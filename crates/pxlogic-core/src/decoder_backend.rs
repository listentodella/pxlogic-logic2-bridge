//! Decoder backend contracts shared by the Tauri host and external decoders.
//!
//! The protocol-specific Rust implementation is deliberately kept behind the
//! `LegacyRust` backend.  New protocol support belongs in a Saleae analyzer
//! host or a sigrok backend, so both ecosystems can expose their richer
//! annotations without forcing the UI to know which decoder produced them.

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Instant,
};

use serde::{
    ser::{SerializeSeq, SerializeStruct},
    Deserialize, Serialize,
};

use crate::{
    capture::read_sample_word,
    decode::{AnalyzerDecodeSettings, DecodedChannelValue, UartParity},
    error::{CoreError, Result},
    models::{CaptureData, SparseCaptureData, SparseCaptureView, SparseDigitalChannel},
};

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecoderBackendKind {
    /// Let the host choose the best available native backend.
    #[default]
    Auto,
    /// Saleae Analyzer SDK host plus native analyzer plugins.
    SaleaeNative,
    /// libsigrokdecode, preferring PXView's native C decoders.
    SigrokNative,
    /// Transitional implementation retained only while external backends are
    /// being integrated. No new protocol logic should be added here.
    LegacyRust,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecoderBackendIsolation {
    InProcess,
    Sidecar,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderBackendInfo {
    pub kind: DecoderBackendKind,
    pub display_name: String,
    pub available: bool,
    pub protocols: Vec<String>,
    pub supports_streaming: bool,
    pub isolation: DecoderBackendIsolation,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DecoderFieldValue {
    Text(String),
    Unsigned(u64),
    Signed(i64),
    Boolean(bool),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderField {
    pub name: String,
    pub value: DecoderFieldValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecoderAnnotationKind {
    Data,
    Marker,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderAnnotation {
    pub row: String,
    pub channel: Option<u8>,
    pub start_sample: u64,
    pub end_sample: u64,
    pub kind: DecoderAnnotationKind,
    /// Ordered display candidates. The UI can choose the shortest readable
    /// candidate at the current zoom, matching both Saleae and sigrok output.
    pub texts: Vec<String>,
    pub fields: Vec<DecoderField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderMarker {
    pub row: String,
    pub channel: Option<u8>,
    pub sample: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderOutputFrame {
    pub frame_id: u64,
    pub start_sample: u64,
    pub end_sample: u64,
    pub row: String,
    pub texts: Vec<String>,
    pub fields: Vec<DecoderField>,
    pub channel_values: Vec<DecodedChannelValue>,
    pub markers: Vec<DecoderMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderDiagnostic {
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecoderOutput {
    pub backend: DecoderBackendKind,
    pub protocol: String,
    pub frames: Vec<DecoderOutputFrame>,
    pub diagnostics: Vec<DecoderDiagnostic>,
}

const SALEAE_NATIVE_PROTOCOLS: &[&str] = &["UART", "I2C", "SPI", "I2S", "CAN", "LIN", "Parallel"];
const SIGROK_NATIVE_PROTOCOLS: &[&str] = &[
    "UART", "I2C", "SPI", "I2S", "CAN", "CAN-FD", "LIN", "Parallel",
];
const LEGACY_RUST_PROTOCOLS: &[&str] = &["UART", "I2C", "SPI"];

fn trace_decoder_timing(backend: &str, stage: &str, started: Instant, detail: &str) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    tracing::debug!(
        target: "pxlogic::decoder_performance",
        backend,
        stage,
        elapsed_ms,
        detail
    );
    if std::env::var_os("PXLOGIC_DECODER_TIMING").is_some() {
        eprintln!(
            "[pxlogic-decoder] backend={backend} stage={stage} elapsed_ms={elapsed_ms:.3} {detail}"
        );
    }
}

#[cfg(windows)]
const SIGROK_SIDECAR_EXECUTABLE: &str = "pxlogic-sigrok-sidecar.exe";
#[cfg(not(windows))]
const SIGROK_SIDECAR_EXECUTABLE: &str = "pxlogic-sigrok-sidecar";
#[cfg(windows)]
const SALEAE_SIDECAR_EXECUTABLE: &str = "pxlogic-saleae-analyzer-sidecar.exe";
#[cfg(not(windows))]
const SALEAE_SIDECAR_EXECUTABLE: &str = "pxlogic-saleae-analyzer-sidecar";

/// Stable host-side contract. External implementations may live in a native
/// sidecar; the input remains the same packed capture representation used by
/// PXLogic and the `.sr` exporter.
pub trait DecoderBackend: Send + Sync {
    fn info(&self) -> DecoderBackendInfo;

    fn decode(
        &self,
        capture: &CaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput>;

    /// Decode a transition-indexed capture without expanding it into one
    /// byte per sample. Native backends may override this for long Saleae
    /// captures; the legacy Rust adapter intentionally does not.
    fn decode_sparse(
        &self,
        _capture: &SparseCaptureData,
        _settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        Err(CoreError::Decode(
            "decoder backend does not support sparse captures".to_string(),
        ))
    }

    /// Decode edge vectors borrowed directly from an owning graph. Backends
    /// that do not override this retain compatibility through one owned copy.
    fn decode_sparse_view(
        &self,
        capture: &SparseCaptureView<'_>,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let capture = SparseCaptureData {
            metadata: capture.metadata.clone(),
            channels: capture
                .channels
                .iter()
                .map(|channel| crate::models::SparseDigitalChannel {
                    channel: channel.channel,
                    initial_high: channel.initial_high,
                    transitions: channel.transitions.to_vec(),
                })
                .collect(),
        };
        self.decode_sparse(&capture, settings)
    }

    /// Decode a window from a borrowed sparse capture. Implementations can
    /// serialize borrowed transition slices as window-relative samples without
    /// allocating an owned sparse capture for every viewport.
    fn decode_sparse_view_window(
        &self,
        capture: &SparseCaptureView<'_>,
        start_sample: u64,
        end_sample: u64,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let capture = sparse_view_window_to_owned(capture, start_sample, end_sample)?;
        self.decode_sparse(&capture, settings)
    }
}

#[derive(Debug, Clone)]
pub struct SaleaeNativeDecoder {
    sidecar_path: PathBuf,
    runtime_root: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct SaleaeSidecarProbe {
    available: bool,
    protocols: Vec<String>,
    supports_streaming: bool,
    analyzer_name: String,
    runtime_sha256: String,
    plugin_sha256: String,
    abi_match: bool,
    #[serde(default)]
    edge_encodings: Vec<String>,
    #[serde(default)]
    decoder_catalog: Vec<SigrokDecoderCatalogItem>,
    #[serde(default)]
    c_decoder_count: usize,
    #[serde(default)]
    python_decoder_count: usize,
}

#[derive(Debug)]
struct NativeEdgeSet<'a> {
    channel: u8,
    compact: bool,
    sample_offset: u64,
    samples: Cow<'a, [u64]>,
}

impl Serialize for NativeEdgeSet<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if !self.compact {
            let mut state = serializer.serialize_struct("NativeEdgeSet", 2)?;
            state.serialize_field("channel", &self.channel)?;
            state.serialize_field(
                "samples",
                &OffsetEdgeSamples {
                    samples: &self.samples,
                    sample_offset: self.sample_offset,
                },
            )?;
            return state.end();
        }

        let mut state = serializer.serialize_struct("NativeEdgeSet", 3)?;
        state.serialize_field("channel", &self.channel)?;
        state.serialize_field("encoding", "delta_varint_base64")?;
        state.serialize_field(
            "samples_b64",
            &encode_delta_varint_samples_base64(&self.samples, self.sample_offset),
        )?;
        state.end()
    }
}

const BASE64_STANDARD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

struct Base64ByteSink {
    output: String,
    buffer: [u8; 3],
    buffer_len: usize,
}

impl Base64ByteSink {
    fn with_estimated_input_len(input_len: usize) -> Self {
        Self {
            output: String::with_capacity(input_len.div_ceil(3).saturating_mul(4)),
            buffer: [0; 3],
            buffer_len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        self.buffer[self.buffer_len] = byte;
        self.buffer_len += 1;
        if self.buffer_len == 3 {
            self.flush_full();
        }
    }

    fn finish(mut self) -> String {
        if self.buffer_len > 0 {
            self.flush_partial();
        }
        self.output
    }

    fn flush_full(&mut self) {
        let [a, b, c] = self.buffer;
        self.push_encoded(((a >> 2) & 0x3f) as usize);
        self.push_encoded((((a & 0x03) << 4) | (b >> 4)) as usize);
        self.push_encoded((((b & 0x0f) << 2) | (c >> 6)) as usize);
        self.push_encoded((c & 0x3f) as usize);
        self.buffer_len = 0;
    }

    fn flush_partial(&mut self) {
        let a = self.buffer[0];
        self.push_encoded(((a >> 2) & 0x3f) as usize);
        if self.buffer_len == 1 {
            self.push_encoded(((a & 0x03) << 4) as usize);
            self.output.push_str("==");
        } else {
            let b = self.buffer[1];
            self.push_encoded((((a & 0x03) << 4) | (b >> 4)) as usize);
            self.push_encoded(((b & 0x0f) << 2) as usize);
            self.output.push('=');
        }
        self.buffer_len = 0;
    }

    fn push_encoded(&mut self, index: usize) {
        self.output
            .push(char::from(BASE64_STANDARD_ALPHABET[index]));
    }
}

fn encode_delta_varint_samples_base64(samples: &[u64], sample_offset: u64) -> String {
    let mut sink = Base64ByteSink::with_estimated_input_len(samples.len().saturating_mul(2));
    let mut previous = 0u64;
    for &sample in samples {
        let relative_sample = sample.saturating_sub(sample_offset);
        let mut delta = relative_sample.saturating_sub(previous);
        loop {
            let mut byte = (delta & 0x7f) as u8;
            delta >>= 7;
            if delta != 0 {
                byte |= 0x80;
            }
            sink.push(byte);
            if delta == 0 {
                break;
            }
        }
        previous = relative_sample;
    }
    sink.finish()
}

struct OffsetEdgeSamples<'a> {
    samples: &'a [u64],
    sample_offset: u64,
}

impl Serialize for OffsetEdgeSamples<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.samples.len()))?;
        for sample in self.samples {
            sequence.serialize_element(&sample.saturating_sub(self.sample_offset))?;
        }
        sequence.end()
    }
}

#[derive(Debug, Serialize)]
struct SpiSidecarSettings {
    protocol: &'static str,
    channels: SpiSidecarChannels,
    options: SpiSidecarOptions,
}

#[derive(Debug, Serialize)]
struct SpiSidecarChannels {
    mosi: Option<u8>,
    miso: Option<u8>,
    clk: u8,
    cs: Option<u8>,
}

#[derive(Debug, Serialize)]
struct SpiSidecarOptions {
    cpol: u8,
    cpha: u8,
    bit_order: &'static str,
    word_size: u8,
    cs_polarity: &'static str,
}

#[derive(Debug, Serialize)]
struct NativeSidecarRequest<'a> {
    format: &'static str,
    response_format: &'static str,
    include_fields: bool,
    sample_rate_hz: u64,
    sample_count: u64,
    channel_count: u8,
    initial_levels: Vec<u8>,
    edges: Vec<NativeEdgeSet<'a>>,
    decoder: NativeSidecarSettings,
}

type SaleaeProbeCacheKey = (PathBuf, PathBuf, bool);
static SALEAE_PROBE_CACHE: OnceLock<Mutex<HashMap<SaleaeProbeCacheKey, SaleaeSidecarProbe>>> =
    OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SaleaePersistentPoolKey {
    sidecar_path: PathBuf,
    runtime_root: PathBuf,
}

enum SaleaePersistentPoolState {
    Ready(Arc<SaleaePersistentPool>),
    Disabled(String),
}

static SALEAE_PERSISTENT_POOL_CACHE: OnceLock<
    Mutex<HashMap<SaleaePersistentPoolKey, SaleaePersistentPoolState>>,
> = OnceLock::new();

const DEFAULT_MAX_SALEAE_PERSISTENT_WORKERS: usize = 4;
const SALEAE_PERSISTENT_WORKERS_ENV: &str = "PXLOGIC_SALEAE_WORKERS";

#[derive(Debug, Deserialize)]
struct SaleaePersistentReady {
    ok: bool,
    ready: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SaleaePersistentDecodeResponse {
    ok: bool,
    #[serde(default)]
    output: Option<SaleaeDecodePayload>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SaleaeDecodePayload {
    Compact(SaleaeCompactDecoderOutput),
    Standard(DecoderOutput),
}

#[derive(Debug, Deserialize)]
struct SaleaeCompactDecoderOutput {
    format: String,
    #[serde(rename = "p")]
    protocol: String,
    #[serde(rename = "f")]
    frames: Vec<SaleaeCompactFrame>,
    #[serde(rename = "d", default)]
    diagnostics: Vec<SaleaeCompactDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct SaleaeCompactFrame(
    u64,
    u64,
    u64,
    String,
    Vec<String>,
    Vec<SaleaeCompactField>,
    Vec<SaleaeCompactChannelValue>,
    Vec<SaleaeCompactMarker>,
);

#[derive(Debug, Deserialize)]
struct SaleaeCompactField(String, String, serde_json::Value);

#[derive(Debug, Deserialize)]
struct SaleaeCompactChannelValue(u8, String, String, Vec<String>, u64);

#[derive(Debug, Deserialize)]
struct SaleaeCompactMarker(String, Option<u8>, u64, String);

#[derive(Debug, Deserialize)]
struct SaleaeCompactDiagnostic(String, String);

impl SaleaeDecodePayload {
    fn into_decoder_output(self) -> Result<DecoderOutput> {
        match self {
            SaleaeDecodePayload::Compact(output) => output.into_decoder_output(),
            SaleaeDecodePayload::Standard(output) => Ok(output),
        }
    }
}

impl SaleaeCompactDecoderOutput {
    fn into_decoder_output(self) -> Result<DecoderOutput> {
        if self.format != "pxlogic.decoder.compact_v1" {
            return Err(CoreError::Decode(format!(
                "unsupported Saleae compact decoder output format: {}",
                self.format
            )));
        }
        Ok(DecoderOutput {
            backend: DecoderBackendKind::SaleaeNative,
            protocol: self.protocol,
            frames: self
                .frames
                .into_iter()
                .map(SaleaeCompactFrame::into_frame)
                .collect::<Result<Vec<_>>>()?,
            diagnostics: self
                .diagnostics
                .into_iter()
                .map(|diagnostic| DecoderDiagnostic {
                    level: diagnostic.0,
                    message: diagnostic.1,
                })
                .collect(),
        })
    }
}

impl SaleaeCompactFrame {
    fn into_frame(self) -> Result<DecoderOutputFrame> {
        Ok(DecoderOutputFrame {
            frame_id: self.0,
            start_sample: self.1,
            end_sample: self.2,
            row: self.3,
            texts: self.4,
            fields: self
                .5
                .into_iter()
                .map(SaleaeCompactField::into_field)
                .collect::<Result<Vec<_>>>()?,
            channel_values: self
                .6
                .into_iter()
                .map(|value| DecodedChannelValue {
                    channel: value.0,
                    role: value.1,
                    label: value.2,
                    texts: value.3,
                    value: value.4,
                })
                .collect(),
            markers: self
                .7
                .into_iter()
                .map(|marker| DecoderMarker {
                    row: marker.0,
                    channel: marker.1,
                    sample: marker.2,
                    kind: marker.3,
                })
                .collect(),
        })
    }
}

impl SaleaeCompactField {
    fn into_field(self) -> Result<DecoderField> {
        let value = match self.1.as_str() {
            "t" => DecoderFieldValue::Text(serde_json::from_value(self.2).map_err(|error| {
                CoreError::Decode(format!("invalid compact text field {}: {error}", self.0))
            })?),
            "u" => {
                DecoderFieldValue::Unsigned(serde_json::from_value(self.2).map_err(|error| {
                    CoreError::Decode(format!(
                        "invalid compact unsigned field {}: {error}",
                        self.0
                    ))
                })?)
            }
            "s" => DecoderFieldValue::Signed(serde_json::from_value(self.2).map_err(|error| {
                CoreError::Decode(format!("invalid compact signed field {}: {error}", self.0))
            })?),
            "b" => DecoderFieldValue::Boolean(serde_json::from_value(self.2).map_err(|error| {
                CoreError::Decode(format!("invalid compact boolean field {}: {error}", self.0))
            })?),
            "bytes" => {
                DecoderFieldValue::Bytes(serde_json::from_value(self.2).map_err(|error| {
                    CoreError::Decode(format!("invalid compact bytes field {}: {error}", self.0))
                })?)
            }
            kind => {
                return Err(CoreError::Decode(format!(
                    "unsupported compact decoder field kind {kind}"
                )));
            }
        };
        Ok(DecoderField {
            name: self.0,
            value,
        })
    }
}

fn parse_saleae_decode_output(bytes: &[u8]) -> Result<DecoderOutput> {
    let payload: SaleaeDecodePayload = serde_json::from_slice(bytes).map_err(|error| {
        CoreError::Decode(format!("invalid Saleae sidecar decode response: {error}"))
    })?;
    payload.into_decoder_output()
}

#[derive(Debug, Serialize)]
struct NativeSidecarSettings {
    protocol: String,
    primary_channel: u8,
    channels: serde_json::Value,
    options: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct SaleaeSimulationRequest {
    sample_rate_hz: u64,
    sample_count: u64,
    decoder: NativeSidecarSettings,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SaleaeSimulationChannel {
    pub channel: u8,
    pub initial_high: bool,
    pub sample_count: u64,
    pub transitions: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct SaleaeSimulationResponse {
    channels: Vec<SaleaeSimulationChannel>,
}

enum SaleaePersistentDecodeError {
    Decode(String),
    Ipc(String),
}

struct SaleaePersistentPool {
    next_worker: AtomicUsize,
    workers: Vec<Mutex<SaleaePersistentWorker>>,
}

struct SaleaePersistentWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    runtime_root: PathBuf,
    sidecar_path: PathBuf,
}

impl SaleaePersistentPool {
    fn new(sidecar_path: PathBuf, runtime_root: PathBuf) -> Result<Self> {
        let worker_count = saleae_persistent_worker_count();
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            workers.push(Mutex::new(SaleaePersistentWorker::spawn(
                sidecar_path.clone(),
                runtime_root.clone(),
            )?));
        }
        Ok(Self {
            next_worker: AtomicUsize::new(0),
            workers,
        })
    }

    fn decode(
        &self,
        request: &[u8],
    ) -> std::result::Result<DecoderOutput, SaleaePersistentDecodeError> {
        let start_index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let lock_started = Instant::now();
        let mut worker_index = start_index;
        let mut worker_guard: Option<std::sync::MutexGuard<'_, SaleaePersistentWorker>> = None;
        let mut scanned_workers = 0usize;
        for offset in 0..self.workers.len() {
            let candidate_index = (start_index + offset) % self.workers.len();
            scanned_workers += 1;
            match self.workers[candidate_index].try_lock() {
                Ok(guard) => {
                    worker_index = candidate_index;
                    worker_guard = Some(guard);
                    break;
                }
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    worker_index = candidate_index;
                    worker_guard = Some(poisoned.into_inner());
                    break;
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
        }
        let all_busy = worker_guard.is_none();
        let mut worker = worker_guard.unwrap_or_else(|| {
            self.workers[start_index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        trace_decoder_timing(
            "saleae",
            "persistent_worker_lock_wait",
            lock_started,
            &format!(
                "worker={} start_worker={} scanned={} all_busy={} request_bytes={}",
                worker_index,
                start_index,
                scanned_workers,
                all_busy,
                request.len()
            ),
        );
        worker.decode(request)
    }

    fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

impl SaleaePersistentWorker {
    fn spawn(sidecar_path: PathBuf, runtime_root: PathBuf) -> Result<Self> {
        let stderr = if std::env::var_os("PXLOGIC_DECODER_TIMING").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(&sidecar_path)
            .arg("--runtime-root")
            .arg(&runtime_root)
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            CoreError::Decode("Saleae persistent sidecar stdin was not available".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CoreError::Decode("Saleae persistent sidecar stdout was not available".to_string())
        })?;
        let mut worker = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            runtime_root,
            sidecar_path,
        };
        worker.read_ready()?;
        Ok(worker)
    }

    fn decode(
        &mut self,
        request: &[u8],
    ) -> std::result::Result<DecoderOutput, SaleaePersistentDecodeError> {
        match self.decode_once(request) {
            Ok(output) => Ok(output),
            Err(SaleaePersistentDecodeError::Decode(error)) => {
                Err(SaleaePersistentDecodeError::Decode(error))
            }
            Err(SaleaePersistentDecodeError::Ipc(first_error)) => {
                self.restart()
                    .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
                self.decode_once(request).map_err(|error| match error {
                    SaleaePersistentDecodeError::Decode(error) => SaleaePersistentDecodeError::Decode(error),
                    SaleaePersistentDecodeError::Ipc(retry_error) => SaleaePersistentDecodeError::Ipc(format!(
                        "Saleae persistent sidecar IPC failed after restart: {first_error}; retry: {retry_error}"
                    )),
                })
            }
        }
    }

    fn decode_once(
        &mut self,
        request: &[u8],
    ) -> std::result::Result<DecoderOutput, SaleaePersistentDecodeError> {
        let write_started = Instant::now();
        self.stdin
            .write_all(request)
            .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
        self.stdin
            .flush()
            .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
        trace_decoder_timing(
            "saleae",
            "persistent_request_write",
            write_started,
            &format!("request_bytes={}", request.len()),
        );

        let mut line = String::new();
        let read_started = Instant::now();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
        trace_decoder_timing(
            "saleae",
            "persistent_response_read",
            read_started,
            &format!("response_bytes={bytes}"),
        );
        if bytes == 0 {
            return Err(SaleaePersistentDecodeError::Ipc(
                "Saleae persistent sidecar closed stdout".to_string(),
            ));
        }
        let parse_started = Instant::now();
        let response: SaleaePersistentDecodeResponse =
            serde_json::from_str(&line).map_err(|error| {
                SaleaePersistentDecodeError::Ipc(format!(
                    "invalid Saleae persistent sidecar response: {error}"
                ))
            })?;
        trace_decoder_timing(
            "saleae",
            "persistent_response_json_parse",
            parse_started,
            &format!("response_bytes={bytes} ok={}", response.ok),
        );
        if !response.ok {
            return Err(SaleaePersistentDecodeError::Decode(
                response
                    .error
                    .unwrap_or_else(|| "Saleae persistent sidecar decode failed".to_string()),
            ));
        }
        let output = response.output.ok_or_else(|| {
            SaleaePersistentDecodeError::Ipc(
                "Saleae persistent sidecar response omitted decode output".to_string(),
            )
        })?;
        let convert_started = Instant::now();
        let decoded = output
            .into_decoder_output()
            .map_err(|error| SaleaePersistentDecodeError::Ipc(error.to_string()))?;
        trace_decoder_timing(
            "saleae",
            "persistent_response_convert",
            convert_started,
            &format!("frames={}", decoded.frames.len()),
        );
        Ok(decoded)
    }

    fn read_ready(&mut self) -> Result<()> {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line)?;
        if bytes == 0 {
            return Err(CoreError::Decode(
                "Saleae persistent sidecar exited before ready".to_string(),
            ));
        }
        let ready: SaleaePersistentReady = serde_json::from_str(&line).map_err(|error| {
            CoreError::Decode(format!(
                "invalid Saleae persistent sidecar ready response: {error}"
            ))
        })?;
        if !ready.ok || !ready.ready {
            return Err(CoreError::Decode(ready.error.unwrap_or_else(|| {
                "Saleae persistent sidecar did not become ready".to_string()
            })));
        }
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        let sidecar_path = self.sidecar_path.clone();
        let runtime_root = self.runtime_root.clone();
        let _ = self.child.kill();
        let _ = self.child.wait();
        *self = Self::spawn(sidecar_path, runtime_root)?;
        Ok(())
    }
}

impl Drop for SaleaePersistentWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Serialize)]
struct UartSidecarChannels {
    rx: Option<u8>,
    tx: Option<u8>,
}

#[derive(Debug, Serialize)]
struct UartSidecarOptions {
    baudrate: u32,
    data_bits: u8,
    stop_bits: f64,
    parity: &'static str,
    bit_order: &'static str,
    format: &'static str,
    invert_rx: u8,
    invert_tx: u8,
}

#[derive(Debug, Serialize)]
struct I2cSidecarChannels {
    scl: u8,
    sda: u8,
}

#[derive(Debug, Serialize)]
struct I2cSidecarOptions {
    address_format: &'static str,
    packets_format: &'static str,
    show_data_point: u8,
}

impl SaleaeNativeDecoder {
    pub fn new(sidecar_path: impl Into<PathBuf>, runtime_root: impl Into<PathBuf>) -> Self {
        Self {
            sidecar_path: sidecar_path.into(),
            runtime_root: runtime_root.into(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let sidecar_path = std::env::var_os("PXLOGIC_SALEAE_SIDECAR")
            .map(PathBuf::from)
            .or_else(discover_workspace_saleae_sidecar)
            .ok_or_else(|| {
                CoreError::Decode(
                    "PXLOGIC_SALEAE_SIDECAR is not configured and no bundled or workspace sidecar was found"
                        .to_string(),
                )
            })?;
        let runtime_root = std::env::var_os("PXLOGIC_SALEAE_RUNTIME_ROOT")
            .map(PathBuf::from)
            .or_else(discover_saleae_runtime_root)
            .ok_or_else(|| {
                CoreError::Decode(
                    "PXLOGIC_SALEAE_RUNTIME_ROOT is not configured and Logic 2.4.43 was not found"
                        .to_string(),
                )
            })?;
        Ok(Self::new(sidecar_path, runtime_root))
    }

    fn probe(&self) -> Result<SaleaeSidecarProbe> {
        self.probe_with_catalog(false)
    }

    pub fn decoder_catalog(&self) -> Result<SigrokDecoderCatalog> {
        let probe = self.probe_with_catalog(true)?;
        Ok(SigrokDecoderCatalog {
            decoders: probe.decoder_catalog,
            c_decoder_count: probe.c_decoder_count,
            python_decoder_count: probe.python_decoder_count,
        })
    }

    fn probe_with_catalog(&self, include_catalog: bool) -> Result<SaleaeSidecarProbe> {
        let started = Instant::now();
        let key = (
            self.sidecar_path.clone(),
            self.runtime_root.clone(),
            include_catalog,
        );
        let mut cache = SALEAE_PROBE_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(probe) = cache.get(&key) {
            trace_decoder_timing("saleae", "probe_cache_hit", started, "");
            return Ok(probe.clone());
        }
        if !include_catalog {
            let catalog_key = (self.sidecar_path.clone(), self.runtime_root.clone(), true);
            if let Some(probe) = cache.get(&catalog_key) {
                trace_decoder_timing("saleae", "probe_catalog_cache_hit", started, "");
                return Ok(probe.clone());
            }
        }
        let mut command = Command::new(&self.sidecar_path);
        command
            .arg("--runtime-root")
            .arg(&self.runtime_root)
            .arg("--probe");
        if include_catalog {
            command.arg("--catalog");
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(sidecar_error("Saleae", "probe", &output.stderr));
        }
        let probe: SaleaeSidecarProbe =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                CoreError::Decode(format!("invalid Saleae sidecar probe response: {error}"))
            })?;
        cache.insert(key, probe.clone());
        trace_decoder_timing(
            "saleae",
            "probe_process",
            started,
            if include_catalog {
                "catalog=true"
            } else {
                "catalog=false"
            },
        );
        Ok(probe)
    }

    fn decode_request<T: Serialize>(&self, request: &T) -> Result<DecoderOutput> {
        let total_started = Instant::now();
        let encode_started = Instant::now();
        let request_json = serde_json::to_vec(request).map_err(|error| {
            CoreError::Decode(format!(
                "failed to serialize Saleae decode request: {error}"
            ))
        })?;
        trace_decoder_timing(
            "saleae",
            "request_json_encode",
            encode_started,
            &format!("request_bytes={}", request_json.len()),
        );

        if let Some(pool) = self.persistent_pool() {
            let execute_started = Instant::now();
            match pool.decode(&request_json) {
                Ok(decoded) => {
                    trace_decoder_timing(
                        "saleae",
                        "persistent_sidecar_execute",
                        execute_started,
                        &format!(
                            "frames={} workers={}",
                            decoded.frames.len(),
                            pool.worker_count()
                        ),
                    );
                    trace_decoder_timing(
                        "saleae",
                        "decode_total",
                        total_started,
                        &format!("frames={} mode=persistent", decoded.frames.len()),
                    );
                    return Ok(decoded);
                }
                Err(SaleaePersistentDecodeError::Decode(error)) => {
                    return Err(CoreError::Decode(error));
                }
                Err(SaleaePersistentDecodeError::Ipc(error)) => {
                    trace_decoder_timing(
                        "saleae",
                        "persistent_sidecar_fallback",
                        execute_started,
                        &error,
                    );
                }
            }
        }

        self.decode_request_oneshot(&request_json, total_started)
    }

    fn decode_request_persistent_only<T: Serialize>(
        &self,
        request: &T,
    ) -> Result<Option<DecoderOutput>> {
        let total_started = Instant::now();
        let encode_started = Instant::now();
        let request_json = serde_json::to_vec(request).map_err(|error| {
            CoreError::Decode(format!(
                "failed to serialize Saleae decode request: {error}"
            ))
        })?;
        trace_decoder_timing(
            "saleae",
            "request_json_encode",
            encode_started,
            &format!("request_bytes={}", request_json.len()),
        );

        let Some(pool) = self.persistent_pool() else {
            return Ok(None);
        };
        let execute_started = Instant::now();
        match pool.decode(&request_json) {
            Ok(decoded) => {
                trace_decoder_timing(
                    "saleae",
                    "persistent_sidecar_execute",
                    execute_started,
                    &format!(
                        "frames={} workers={}",
                        decoded.frames.len(),
                        pool.worker_count()
                    ),
                );
                trace_decoder_timing(
                    "saleae",
                    "decode_total",
                    total_started,
                    &format!("frames={} mode=persistent_fast", decoded.frames.len()),
                );
                Ok(Some(decoded))
            }
            Err(SaleaePersistentDecodeError::Decode(error)) => Err(CoreError::Decode(error)),
            Err(SaleaePersistentDecodeError::Ipc(error)) => {
                trace_decoder_timing(
                    "saleae",
                    "persistent_sidecar_fallback",
                    execute_started,
                    &error,
                );
                Ok(None)
            }
        }
    }

    fn decode_request_oneshot(
        &self,
        request_json: &[u8],
        total_started: Instant,
    ) -> Result<DecoderOutput> {
        let spawn_started = Instant::now();
        let mut child = Command::new(&self.sidecar_path)
            .arg("--runtime-root")
            .arg(&self.runtime_root)
            .arg("--decode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        trace_decoder_timing("saleae", "sidecar_spawn", spawn_started, "");
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CoreError::Decode("Saleae sidecar stdin was not available".to_string())
        })?;
        let write_started = Instant::now();
        stdin.write_all(request_json)?;
        stdin.flush()?;
        drop(stdin);
        trace_decoder_timing(
            "saleae",
            "request_json_write",
            write_started,
            &format!("request_bytes={}", request_json.len()),
        );

        let execute_started = Instant::now();
        let output = child.wait_with_output()?;
        trace_decoder_timing(
            "saleae",
            "sidecar_execute",
            execute_started,
            &format!("response_bytes={}", output.stdout.len()),
        );
        if !output.status.success() {
            return Err(sidecar_error("Saleae", "decode", &output.stderr));
        }
        let parse_started = Instant::now();
        let decoded = parse_saleae_decode_output(&output.stdout)?;
        trace_decoder_timing(
            "saleae",
            "response_json_parse",
            parse_started,
            &format!("frames={}", decoded.frames.len()),
        );
        trace_decoder_timing(
            "saleae",
            "decode_total",
            total_started,
            &format!("frames={} mode=oneshot", decoded.frames.len()),
        );
        Ok(decoded)
    }

    fn persistent_pool(&self) -> Option<Arc<SaleaePersistentPool>> {
        if !saleae_persistent_sidecar_enabled() {
            return None;
        }
        let key = SaleaePersistentPoolKey {
            sidecar_path: self.sidecar_path.clone(),
            runtime_root: self.runtime_root.clone(),
        };
        let started = Instant::now();
        let mut cache = SALEAE_PERSISTENT_POOL_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = cache.get(&key) {
            match state {
                SaleaePersistentPoolState::Ready(pool) => {
                    trace_decoder_timing(
                        "saleae",
                        "persistent_pool_cache_hit",
                        started,
                        &format!("workers={}", pool.worker_count()),
                    );
                    return Some(pool.clone());
                }
                SaleaePersistentPoolState::Disabled(reason) => {
                    trace_decoder_timing("saleae", "persistent_pool_disabled", started, reason);
                    return None;
                }
            }
        }

        match SaleaePersistentPool::new(self.sidecar_path.clone(), self.runtime_root.clone()) {
            Ok(pool) => {
                let pool = Arc::new(pool);
                trace_decoder_timing(
                    "saleae",
                    "persistent_pool_start",
                    started,
                    &format!("workers={}", pool.worker_count()),
                );
                cache.insert(key, SaleaePersistentPoolState::Ready(pool.clone()));
                Some(pool)
            }
            Err(error) => {
                let detail = error.to_string();
                trace_decoder_timing("saleae", "persistent_pool_unavailable", started, &detail);
                cache.insert(key, SaleaePersistentPoolState::Disabled(detail));
                None
            }
        }
    }

    pub fn simulate(
        &self,
        settings: &AnalyzerDecodeSettings,
        sample_rate_hz: u64,
        sample_count: u64,
    ) -> Result<Vec<SaleaeSimulationChannel>> {
        let request = SaleaeSimulationRequest {
            sample_rate_hz,
            sample_count,
            decoder: native_sidecar_settings(settings),
        };
        let mut child = Command::new(&self.sidecar_path)
            .arg("--runtime-root")
            .arg(&self.runtime_root)
            .arg("--simulate")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        serde_json::to_writer(
            child.stdin.as_mut().ok_or_else(|| {
                CoreError::Decode("Saleae simulation sidecar stdin was not available".to_string())
            })?,
            &request,
        )
        .map_err(|error| {
            CoreError::Decode(format!(
                "failed to serialize Saleae simulation request: {error}"
            ))
        })?;
        drop(child.stdin.take());
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(sidecar_error("Saleae", "simulation", &output.stderr));
        }
        let mut response: SaleaeSimulationResponse = serde_json::from_slice(&output.stdout)
            .map_err(|error| {
                CoreError::Decode(format!("invalid Saleae simulation response: {error}"))
            })?;
        for channel in &mut response.channels {
            normalize_saleae_simulation_channel(channel);
        }
        Ok(response.channels)
    }
}

impl DecoderBackend for SaleaeNativeDecoder {
    fn info(&self) -> DecoderBackendInfo {
        match self.probe() {
            Ok(probe) => DecoderBackendInfo {
                kind: DecoderBackendKind::SaleaeNative,
                display_name: format!("Saleae Native ({})", probe.analyzer_name),
                available: probe.available && probe.abi_match,
                protocols: probe.protocols,
                supports_streaming: probe.supports_streaming,
                isolation: DecoderBackendIsolation::Sidecar,
                detail: format!(
                    "Logic 2.4.43 ABI {}; runtime {}, plugin {}",
                    if probe.abi_match {
                        "matched"
                    } else {
                        "mismatched"
                    },
                    short_hash(&probe.runtime_sha256),
                    short_hash(&probe.plugin_sha256)
                ),
            },
            Err(error) => DecoderBackendInfo {
                kind: DecoderBackendKind::SaleaeNative,
                display_name: "Saleae Native analyzer sidecar".to_string(),
                available: false,
                protocols: protocol_names(SALEAE_NATIVE_PROTOCOLS),
                supports_streaming: false,
                isolation: DecoderBackendIsolation::Sidecar,
                detail: error.to_string(),
            },
        }
    }

    fn decode(
        &self,
        capture: &CaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let persistent_request = build_native_sidecar_request(capture, settings, true)?;
        if let Some(output) = self.decode_request_persistent_only(&persistent_request)? {
            return Ok(output);
        }

        let probe = self.probe()?;
        if !probe.available || !probe.abi_match {
            return Err(CoreError::Decode(
                "Saleae Logic runtime does not match the pinned 2.4.43 ABI".to_string(),
            ));
        }
        let request = build_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse(
        &self,
        capture: &SparseCaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let persistent_request = build_sparse_native_sidecar_request(capture, settings, true)?;
        if let Some(output) = self.decode_request_persistent_only(&persistent_request)? {
            return Ok(output);
        }

        let probe = self.probe()?;
        if !probe.available || !probe.abi_match {
            return Err(CoreError::Decode(
                "Saleae Logic runtime does not match the pinned 2.4.43 ABI".to_string(),
            ));
        }
        let request = build_sparse_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse_view(
        &self,
        capture: &SparseCaptureView<'_>,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let persistent_request = build_sparse_view_native_sidecar_request(capture, settings, true)?;
        if let Some(output) = self.decode_request_persistent_only(&persistent_request)? {
            return Ok(output);
        }

        let probe = self.probe()?;
        if !probe.available || !probe.abi_match {
            return Err(CoreError::Decode(
                "Saleae Logic runtime does not match the pinned 2.4.43 ABI".to_string(),
            ));
        }
        let request = build_sparse_view_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse_view_window(
        &self,
        capture: &SparseCaptureView<'_>,
        start_sample: u64,
        end_sample: u64,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let persistent_request = build_sparse_view_window_native_sidecar_request(
            capture,
            start_sample,
            end_sample,
            settings,
            true,
        )?;
        if let Some(output) = self.decode_request_persistent_only(&persistent_request)? {
            return Ok(output);
        }

        let probe = self.probe()?;
        if !probe.available || !probe.abi_match {
            return Err(CoreError::Decode(
                "Saleae Logic runtime does not match the pinned 2.4.43 ABI".to_string(),
            ));
        }
        let request = build_sparse_view_window_native_sidecar_request(
            capture,
            start_sample,
            end_sample,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }
}

fn saleae_persistent_sidecar_enabled() -> bool {
    saleae_persistent_sidecar_enabled_value(
        &std::env::var("PXLOGIC_SALEAE_PERSISTENT_SIDECAR").unwrap_or_default(),
    )
}

fn saleae_persistent_sidecar_enabled_value(value: &str) -> bool {
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn saleae_persistent_worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(DEFAULT_MAX_SALEAE_PERSISTENT_WORKERS);
    let configured = std::env::var(SALEAE_PERSISTENT_WORKERS_ENV).ok();
    saleae_persistent_worker_count_value(configured.as_deref(), available)
}

fn saleae_persistent_worker_count_value(configured: Option<&str>, available: usize) -> usize {
    let requested = configured
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_MAX_SALEAE_PERSISTENT_WORKERS);
    requested.min(available.max(1)).max(1)
}

fn normalize_saleae_simulation_channel(channel: &mut SaleaeSimulationChannel) {
    channel.transitions.sort_unstable();
    channel.transitions.dedup();
    if channel.transitions.first() == Some(&0) {
        channel.initial_high = !channel.initial_high;
    }
    channel.transitions.retain(|sample| *sample > 0);
    if let Some(last) = channel.transitions.last() {
        channel.sample_count = channel.sample_count.max(last.saturating_add(1));
    }
}

#[derive(Debug, Clone)]
pub struct SigrokNativeDecoder {
    sidecar_path: PathBuf,
    decoder_root: PathBuf,
    python_decoder_root: Option<PathBuf>,
}

type SigrokProbeCacheKey = (PathBuf, PathBuf, Option<PathBuf>, bool);
static SIGROK_PROBE_CACHE: OnceLock<Mutex<HashMap<SigrokProbeCacheKey, SigrokSidecarProbe>>> =
    OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SigrokDecoderCatalog {
    pub decoders: Vec<SigrokDecoderCatalogItem>,
    pub c_decoder_count: usize,
    pub python_decoder_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SigrokDecoderCatalogItem {
    pub id: Option<String>,
    pub name: Option<String>,
    pub longname: Option<String>,
    pub desc: Option<String>,
    pub license: Option<String>,
    pub kind: Option<String>,
    pub runner_status: Option<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub channels: Vec<SigrokDecoderChannel>,
    #[serde(default)]
    pub optional_channels: Vec<SigrokDecoderChannel>,
    #[serde(default)]
    pub options: Vec<SigrokDecoderOption>,
    #[serde(default)]
    pub annotations: Vec<serde_json::Value>,
    #[serde(default)]
    pub annotation_rows: Vec<serde_json::Value>,
    #[serde(default)]
    pub binary: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SigrokDecoderChannel {
    pub id: Option<String>,
    pub name: Option<String>,
    pub desc: Option<String>,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub r#type: i32,
    pub type_name: Option<String>,
    pub idn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SigrokDecoderOption {
    pub id: Option<String>,
    pub idn: Option<String>,
    pub desc: Option<String>,
    pub value_type: Option<String>,
    #[serde(default)]
    pub default: serde_json::Value,
    #[serde(default)]
    pub values: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct SigrokSidecarProbe {
    available: bool,
    protocols: Vec<String>,
    supports_streaming: bool,
    decoder_name: String,
    decoder_id: String,
    api_version: u32,
    #[serde(default)]
    edge_encodings: Vec<String>,
    #[serde(default)]
    catalog_included: bool,
    #[serde(default)]
    decoder_catalog: Vec<SigrokDecoderCatalogItem>,
    #[serde(default)]
    c_decoder_count: usize,
    #[serde(default)]
    python_decoder_count: usize,
}

impl SigrokNativeDecoder {
    pub fn new(sidecar_path: impl Into<PathBuf>, decoder_root: impl Into<PathBuf>) -> Self {
        Self::new_with_python_decoder_root(sidecar_path, decoder_root, None::<PathBuf>)
    }

    pub fn new_with_python_decoder_root(
        sidecar_path: impl Into<PathBuf>,
        decoder_root: impl Into<PathBuf>,
        python_decoder_root: Option<impl Into<PathBuf>>,
    ) -> Self {
        Self {
            sidecar_path: sidecar_path.into(),
            decoder_root: decoder_root.into(),
            python_decoder_root: python_decoder_root.map(Into::into),
        }
    }

    pub fn from_env() -> Result<Self> {
        let sidecar_path = std::env::var_os("PXLOGIC_SIGROK_SIDECAR")
            .map(PathBuf::from)
            .or_else(|| discover_workspace_sigrok_path(SIGROK_SIDECAR_EXECUTABLE))
            .ok_or_else(|| {
                CoreError::Decode(
                    "PXLOGIC_SIGROK_SIDECAR is not configured and no bundled or workspace sidecar was found"
                        .to_string(),
                )
            })?;
        let decoder_root = std::env::var_os("PXLOGIC_SIGROK_DECODER_ROOT")
            .map(PathBuf::from)
            .or_else(|| discover_workspace_sigrok_path("decoders/c_decoders"))
            .ok_or_else(|| {
                CoreError::Decode(
                    "PXLOGIC_SIGROK_DECODER_ROOT is not configured and no bundled or workspace decoder root was found"
                        .to_string(),
                )
            })?;
        let python_decoder_root = std::env::var_os("PXLOGIC_SIGROK_PYTHON_DECODER_ROOT")
            .map(PathBuf::from)
            .or_else(discover_workspace_sigrok_python_decoder_root);
        Ok(Self::new_with_python_decoder_root(
            sidecar_path,
            decoder_root,
            python_decoder_root,
        ))
    }

    fn probe(&self) -> Result<SigrokSidecarProbe> {
        self.probe_with_catalog(false)
    }

    pub fn decoder_catalog(&self) -> Result<SigrokDecoderCatalog> {
        let probe = self.probe_with_catalog(true)?;
        Ok(SigrokDecoderCatalog {
            decoders: probe.decoder_catalog,
            c_decoder_count: probe.c_decoder_count,
            python_decoder_count: probe.python_decoder_count,
        })
    }

    fn probe_with_catalog(&self, include_catalog: bool) -> Result<SigrokSidecarProbe> {
        let started = Instant::now();
        let key = (
            self.sidecar_path.clone(),
            self.decoder_root.clone(),
            self.python_decoder_root.clone(),
            include_catalog,
        );
        let mut cache = SIGROK_PROBE_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(probe) = cache.get(&key) {
            trace_decoder_timing("sigrok", "probe_cache_hit", started, "");
            return Ok(probe.clone());
        }
        if !include_catalog {
            let catalog_key = (
                self.sidecar_path.clone(),
                self.decoder_root.clone(),
                self.python_decoder_root.clone(),
                true,
            );
            if let Some(probe) = cache.get(&catalog_key) {
                trace_decoder_timing("sigrok", "probe_catalog_cache_hit", started, "");
                return Ok(probe.clone());
            }
        }
        let mut command = self.command();
        command.arg("--probe");
        if include_catalog {
            command.arg("--catalog");
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(sidecar_error("Sigrok", "probe", &output.stderr));
        }
        let probe: SigrokSidecarProbe =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                CoreError::Decode(format!("invalid Sigrok sidecar probe response: {error}"))
            })?;
        cache.insert(key, probe.clone());
        trace_decoder_timing(
            "sigrok",
            "probe_process",
            started,
            if include_catalog {
                "catalog=true"
            } else {
                "catalog=false"
            },
        );
        Ok(probe)
    }

    fn decode_request<T: Serialize>(&self, request: &T) -> Result<DecoderOutput> {
        let total_started = Instant::now();
        let spawn_started = Instant::now();
        let mut command = self.command();
        let mut child = command
            .arg("--decode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        trace_decoder_timing("sigrok", "sidecar_spawn", spawn_started, "");
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CoreError::Decode("Sigrok sidecar stdin was not available".to_string())
        })?;
        let write_started = Instant::now();
        serde_json::to_writer(&mut stdin, request).map_err(|error| {
            CoreError::Decode(format!(
                "failed to serialize Sigrok decode request: {error}"
            ))
        })?;
        stdin.flush()?;
        drop(stdin);
        trace_decoder_timing("sigrok", "request_json_write", write_started, "");

        let execute_started = Instant::now();
        let output = child.wait_with_output()?;
        trace_decoder_timing(
            "sigrok",
            "sidecar_execute",
            execute_started,
            &format!("response_bytes={}", output.stdout.len()),
        );
        if !output.status.success() {
            return Err(sidecar_error("Sigrok", "decode", &output.stderr));
        }
        let parse_started = Instant::now();
        let decoded: DecoderOutput = serde_json::from_slice(&output.stdout).map_err(|error| {
            CoreError::Decode(format!("invalid Sigrok sidecar decode response: {error}"))
        })?;
        trace_decoder_timing(
            "sigrok",
            "response_json_parse",
            parse_started,
            &format!("frames={}", decoded.frames.len()),
        );
        trace_decoder_timing(
            "sigrok",
            "decode_total",
            total_started,
            &format!("frames={}", decoded.frames.len()),
        );
        Ok(decoded)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.sidecar_path);
        command.arg("--decoder-root").arg(&self.decoder_root);
        if let Some(root) = &self.python_decoder_root {
            command.arg("--python-decoder-root").arg(root);
        }
        if let Some(sidecar_dir) = self.sidecar_path.parent() {
            let python_home = sidecar_dir.join("python-runtime");
            if python_home.is_dir() {
                command.env("PYTHONHOME", python_home);
            }
        }
        command
    }
}

fn discover_workspace_sigrok_path(relative: &str) -> Option<PathBuf> {
    native_sidecar_search_paths("sigrok-analyzer-sidecar", relative)
        .into_iter()
        .find(|path| path.exists())
}

fn discover_workspace_sigrok_python_decoder_root() -> Option<PathBuf> {
    decoder_resource_search_paths("sigrok-analyzer-sidecar", "decoders/python_decoders")
        .into_iter()
        .chain(workspace_decoder_search_paths(
            "vendor/pxview-sigrokdecode/libsigrokdecode/decoders",
        ))
        .chain(workspace_decoder_search_paths(
            "pxview-fork/libsigrokdecode/decoders",
        ))
        .find(|path| path.is_dir())
}

fn discover_workspace_saleae_sidecar() -> Option<PathBuf> {
    native_sidecar_search_paths("saleae-analyzer-sidecar", SALEAE_SIDECAR_EXECUTABLE)
        .into_iter()
        .chain(workspace_decoder_search_paths(&format!(
            "target/native/saleae-analyzer-sidecar-{}/{}",
            saleae_build_platform_id(),
            SALEAE_SIDECAR_EXECUTABLE
        )))
        .find(|path| path.is_file())
}

fn saleae_build_platform_id() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        "linux" => "linux",
        other => other,
    };
    let architecture = match (platform, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, architecture) => architecture,
    };
    format!("{platform}-{architecture}")
}

fn native_sidecar_search_paths(subdir: &str, relative: &str) -> Vec<PathBuf> {
    let mut paths = decoder_resource_search_paths(subdir, relative);
    paths.extend(workspace_native_search_paths(subdir, relative));
    dedup_paths(paths)
}

fn decoder_resource_search_paths(subdir: &str, relative: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        push_bundled_native_candidate(&mut paths, &current, subdir, relative);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    push_bundled_native_candidate(&mut paths, &manifest_dir, subdir, relative);
    if let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) {
        push_bundled_native_candidate(&mut paths, workspace_root, subdir, relative);
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().take(8) {
            push_bundled_native_candidate(&mut paths, ancestor, subdir, relative);
        }
    }
    dedup_paths(paths)
}

fn workspace_native_search_paths(subdir: &str, relative: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        push_workspace_native_candidate(&mut paths, &current, subdir, relative);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    push_workspace_native_candidate(&mut paths, &manifest_dir, subdir, relative);
    if let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) {
        push_workspace_native_candidate(&mut paths, workspace_root, subdir, relative);
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().take(8) {
            push_workspace_native_candidate(&mut paths, ancestor, subdir, relative);
        }
    }
    dedup_paths(paths)
}

fn workspace_decoder_search_paths(relative: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        paths.push(current.join(relative));
        paths.push(current.join("..").join(relative));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    paths.push(manifest_dir.join(relative));
    if let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) {
        paths.push(workspace_root.join(relative));
    }

    dedup_paths(paths)
}

fn push_bundled_native_candidate(
    paths: &mut Vec<PathBuf>,
    base: &Path,
    subdir: &str,
    relative: &str,
) {
    paths.push(
        base.join("resources")
            .join("native")
            .join(subdir)
            .join(relative),
    );
    paths.push(
        base.join("Resources")
            .join("resources")
            .join("native")
            .join(subdir)
            .join(relative),
    );
}

fn push_workspace_native_candidate(
    paths: &mut Vec<PathBuf>,
    base: &Path,
    subdir: &str,
    relative: &str,
) {
    paths.push(
        base.join("target")
            .join("native")
            .join(subdir)
            .join(relative),
    );
    paths.push(
        base.join("native")
            .join(subdir)
            .join("target")
            .join(relative),
    );
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn discover_saleae_runtime_root() -> Option<PathBuf> {
    saleae_runtime_search_dirs()
        .into_iter()
        .find(|path| is_saleae_analyzer_runtime(path))
}

fn saleae_runtime_search_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        push_saleae_runtime_candidates(&mut candidates, &current);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    push_saleae_runtime_candidates(&mut candidates, &manifest_dir);
    if let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) {
        push_saleae_runtime_candidates(&mut candidates, workspace_root);
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().take(8) {
            push_saleae_runtime_candidates(&mut candidates, ancestor);
        }
    }

    dedup_paths(candidates)
}

fn push_saleae_runtime_candidates(candidates: &mut Vec<PathBuf>, base: &Path) {
    candidates.push(
        base.join("target")
            .join("native")
            .join(format!(
                "saleae-analyzer-sidecar-{}",
                saleae_build_platform_id()
            ))
            .join("runtime"),
    );
    candidates.push(
        base.join("target")
            .join("native")
            .join("saleae-analyzer-sidecar")
            .join("runtime"),
    );
    candidates.push(
        base.join("resources")
            .join("native")
            .join("saleae-analyzer-sidecar")
            .join("runtime"),
    );
    candidates.push(
        base.join("Resources")
            .join("resources")
            .join("native")
            .join("saleae-analyzer-sidecar")
            .join("runtime"),
    );
}

#[cfg(target_os = "macos")]
fn is_saleae_analyzer_runtime(path: &Path) -> bool {
    path.join("libsaleae_compat.dylib").is_file()
        && path.join("libserial_analyzer.so").is_file()
        && path.join("libswd_analyzer.so").is_file()
}

#[cfg(target_os = "linux")]
fn is_saleae_analyzer_runtime(path: &Path) -> bool {
    path.join("libsaleae_compat.so").is_file()
        && path.join("libserial_analyzer.so").is_file()
        && path.join("libswd_analyzer.so").is_file()
}

#[cfg(windows)]
fn is_saleae_analyzer_runtime(path: &Path) -> bool {
    let compat =
        path.join("libsaleae_compat.dll").is_file() || path.join("saleae_compat.dll").is_file();
    let serial =
        path.join("libserial_analyzer.dll").is_file() || path.join("serial_analyzer.dll").is_file();
    let swd = path.join("libswd_analyzer.dll").is_file() || path.join("swd_analyzer.dll").is_file();
    compat && serial && swd
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn is_saleae_analyzer_runtime(_path: &Path) -> bool {
    false
}

impl DecoderBackend for SigrokNativeDecoder {
    fn info(&self) -> DecoderBackendInfo {
        match self.probe() {
            Ok(probe) => DecoderBackendInfo {
                kind: DecoderBackendKind::SigrokNative,
                display_name: format!("Sigrok Native ({})", probe.decoder_name),
                available: probe.available && probe.api_version == 4,
                protocols: probe.protocols,
                supports_streaming: probe.supports_streaming,
                isolation: DecoderBackendIsolation::Sidecar,
                detail: if probe.catalog_included {
                    format!(
                        "PXView verified sparse C runner {}, API v{}; catalog contains {} C and {} Python decoders",
                        probe.decoder_id,
                        probe.api_version,
                        probe.c_decoder_count,
                        probe.python_decoder_count
                    )
                } else {
                    format!(
                        "PXView verified sparse C runner {}, API v{}; full libsigrokdecode catalog is available through --catalog",
                        probe.decoder_id, probe.api_version
                    )
                },
            },
            Err(error) => DecoderBackendInfo {
                kind: DecoderBackendKind::SigrokNative,
                display_name: "Sigrok Native analyzer sidecar".to_string(),
                available: false,
                protocols: protocol_names(SIGROK_NATIVE_PROTOCOLS),
                supports_streaming: false,
                isolation: DecoderBackendIsolation::Sidecar,
                detail: error.to_string(),
            },
        }
    }

    fn decode(
        &self,
        capture: &CaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let probe = self.probe()?;
        if !probe.available || probe.api_version != 4 {
            return Err(CoreError::Decode(
                "PXView native C decoder API v4 is not available".to_string(),
            ));
        }
        ensure_sigrok_protocol_available(&probe, settings)?;
        let request = build_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse(
        &self,
        capture: &SparseCaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let probe = self.probe()?;
        if !probe.available || probe.api_version != 4 {
            return Err(CoreError::Decode(
                "PXView native C decoder API v4 is not available".to_string(),
            ));
        }
        ensure_sigrok_protocol_available(&probe, settings)?;
        let request = build_sparse_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse_view(
        &self,
        capture: &SparseCaptureView<'_>,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let probe = self.probe()?;
        if !probe.available || probe.api_version != 4 {
            return Err(CoreError::Decode(
                "PXView native C decoder API v4 is not available".to_string(),
            ));
        }
        ensure_sigrok_protocol_available(&probe, settings)?;
        let request = build_sparse_view_native_sidecar_request(
            capture,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }

    fn decode_sparse_view_window(
        &self,
        capture: &SparseCaptureView<'_>,
        start_sample: u64,
        end_sample: u64,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let probe = self.probe()?;
        if !probe.available || probe.api_version != 4 {
            return Err(CoreError::Decode(
                "PXView native C decoder API v4 is not available".to_string(),
            ));
        }
        ensure_sigrok_protocol_available(&probe, settings)?;
        let request = build_sparse_view_window_native_sidecar_request(
            capture,
            start_sample,
            end_sample,
            settings,
            supports_compact_edges(&probe.edge_encodings),
        )?;
        self.decode_request(&request)
    }
}

fn native_protocol_name(settings: &AnalyzerDecodeSettings) -> &str {
    match settings {
        AnalyzerDecodeSettings::Uart(_) => "UART",
        AnalyzerDecodeSettings::I2c(_) => "I2C",
        AnalyzerDecodeSettings::Spi(_) => "SPI",
        AnalyzerDecodeSettings::Native(settings) => &settings.protocol_name,
    }
}

fn supports_compact_edges(edge_encodings: &[String]) -> bool {
    edge_encodings
        .iter()
        .any(|encoding| encoding == "delta_varint_base64")
}

fn protocol_names(protocols: &[&str]) -> Vec<String> {
    protocols
        .iter()
        .map(|protocol| (*protocol).to_string())
        .collect()
}

fn ensure_sigrok_protocol_available(
    probe: &SigrokSidecarProbe,
    settings: &AnalyzerDecodeSettings,
) -> Result<()> {
    if matches!(settings, AnalyzerDecodeSettings::Native(_)) {
        return Ok(());
    }
    ensure_protocol_available(&probe.protocols, settings, "PXView Sigrok C")
}

fn ensure_protocol_available(
    protocols: &[String],
    settings: &AnalyzerDecodeSettings,
    backend: &str,
) -> Result<()> {
    let protocol = native_protocol_name(settings);
    if protocols
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(protocol))
    {
        Ok(())
    } else {
        Err(CoreError::Decode(format!(
            "{backend} decoder for {protocol} is not available"
        )))
    }
}

fn configured_native_channels(
    settings: &AnalyzerDecodeSettings,
    channel_count: u8,
) -> Result<BTreeSet<u8>> {
    let channels = match settings {
        AnalyzerDecodeSettings::Uart(settings) => BTreeSet::from([settings.channel]),
        AnalyzerDecodeSettings::I2c(settings) => {
            BTreeSet::from([settings.sda_channel, settings.scl_channel])
        }
        AnalyzerDecodeSettings::Spi(settings) => configured_spi_channels(settings, channel_count)?,
        AnalyzerDecodeSettings::Native(settings) => {
            let channels: BTreeSet<u8> = settings.channels.values().flatten().copied().collect();
            if !channels.contains(&settings.primary_channel) {
                return Err(CoreError::Decode(format!(
                    "native {} primary channel is not configured",
                    settings.protocol_name
                )));
            }
            channels
        }
    };
    if channels.iter().any(|channel| *channel >= channel_count) {
        return Err(CoreError::Decode(format!(
            "native {} channel is outside the capture",
            native_protocol_name(settings)
        )));
    }
    Ok(channels)
}

fn packed_transition_sets(
    capture: &CaptureData,
    channels: &BTreeSet<u8>,
) -> Result<(Vec<u8>, Vec<NativeEdgeSet<'static>>)> {
    let started = Instant::now();
    let first_word = capture_word(capture, 0)?;
    let initial_levels = (0..capture.metadata.channel_count)
        .map(|channel| u8::from(first_word & (1u32 << channel) != 0))
        .collect();
    let mut edges = channels
        .iter()
        .map(|channel| NativeEdgeSet {
            channel: *channel,
            compact: false,
            sample_offset: 0,
            samples: Cow::Owned(Vec::new()),
        })
        .collect::<Vec<_>>();
    let mut previous_word = first_word;
    for sample in 1..capture.metadata.sample_count {
        let word = capture_word(capture, sample)?;
        let changed = previous_word ^ word;
        if changed != 0 {
            for edge_set in &mut edges {
                if changed & (1u32 << edge_set.channel) != 0 {
                    edge_set.samples.to_mut().push(sample);
                }
            }
        }
        previous_word = word;
    }
    trace_decoder_timing(
        "native",
        "packed_edge_extraction",
        started,
        &format!(
            "samples={} channels={} edges={}",
            capture.metadata.sample_count,
            channels.len(),
            edges.iter().map(|edge| edge.samples.len()).sum::<usize>()
        ),
    );
    Ok((initial_levels, edges))
}

fn sparse_transition_sets<'a, I>(
    channel_count: u8,
    sample_count: u64,
    sparse_channels: I,
    channels: &BTreeSet<u8>,
) -> Result<(Vec<u8>, Vec<NativeEdgeSet<'a>>)>
where
    I: IntoIterator<Item = (u8, bool, &'a [u64])>,
{
    let started = Instant::now();
    let mut initial_levels = vec![0u8; usize::from(channel_count)];
    let mut available = BTreeSet::new();
    let mut edges = Vec::with_capacity(channels.len());
    for (channel, initial_high, transitions) in sparse_channels {
        if channel >= channel_count
            || !available.insert(channel)
            || transitions.windows(2).any(|window| window[0] >= window[1])
            || transitions
                .iter()
                .any(|sample| *sample == 0 || *sample >= sample_count)
        {
            return Err(CoreError::Decode(
                "invalid sparse transition channel data".to_string(),
            ));
        }
        initial_levels[usize::from(channel)] = u8::from(initial_high);
        if channels.contains(&channel) {
            edges.push(NativeEdgeSet {
                channel,
                compact: false,
                sample_offset: 0,
                samples: Cow::Borrowed(transitions),
            });
        }
    }
    for channel in channels {
        if !available.contains(channel) {
            return Err(CoreError::Decode(format!(
                "native decoder channel D{channel} is not present in the sparse capture"
            )));
        }
    }
    edges.sort_by_key(|edge| edge.channel);
    trace_decoder_timing(
        "native",
        "indexed_edge_borrow",
        started,
        &format!(
            "samples={sample_count} channels={} edges={}",
            channels.len(),
            edges.iter().map(|edge| edge.samples.len()).sum::<usize>()
        ),
    );
    Ok((initial_levels, edges))
}

fn validate_sparse_window(
    full_sample_count: u64,
    start_sample: u64,
    end_sample: u64,
) -> Result<()> {
    if start_sample >= end_sample || end_sample > full_sample_count {
        return Err(CoreError::Decode(
            "native decoder sparse window is outside the capture".to_string(),
        ));
    }
    Ok(())
}

fn sparse_view_window_to_owned(
    capture: &SparseCaptureView<'_>,
    start_sample: u64,
    end_sample: u64,
) -> Result<SparseCaptureData> {
    validate_sparse_window(capture.metadata.sample_count, start_sample, end_sample)?;
    let mut metadata = (*capture.metadata).clone();
    metadata.sample_count = end_sample - start_sample;
    metadata.trigger = None;
    let channels = capture
        .channels
        .iter()
        .map(|channel| {
            let first_edge = channel
                .transitions
                .partition_point(|sample| *sample <= start_sample);
            let edge_end = channel
                .transitions
                .partition_point(|sample| *sample < end_sample);
            SparseDigitalChannel {
                channel: channel.channel,
                initial_high: channel.initial_high ^ (first_edge % 2 == 1),
                transitions: channel.transitions[first_edge..edge_end]
                    .iter()
                    .map(|sample| sample - start_sample)
                    .collect(),
            }
        })
        .collect();
    Ok(SparseCaptureData { metadata, channels })
}

fn sparse_window_transition_sets<'a, I>(
    channel_count: u8,
    full_sample_count: u64,
    start_sample: u64,
    end_sample: u64,
    sparse_channels: I,
    channels: &BTreeSet<u8>,
) -> Result<(Vec<u8>, Vec<NativeEdgeSet<'a>>)>
where
    I: IntoIterator<Item = (u8, bool, &'a [u64])>,
{
    validate_sparse_window(full_sample_count, start_sample, end_sample)?;
    let started = Instant::now();
    let mut initial_levels = vec![0u8; usize::from(channel_count)];
    let mut available = BTreeSet::new();
    let mut edges = Vec::with_capacity(channels.len());
    for (channel, initial_high, transitions) in sparse_channels {
        if channel >= channel_count
            || !available.insert(channel)
            || transitions.windows(2).any(|window| window[0] >= window[1])
            || transitions
                .iter()
                .any(|sample| *sample == 0 || *sample >= full_sample_count)
        {
            return Err(CoreError::Decode(
                "invalid sparse transition channel data".to_string(),
            ));
        }
        let first_edge = transitions.partition_point(|sample| *sample <= start_sample);
        let edge_end = transitions.partition_point(|sample| *sample < end_sample);
        initial_levels[usize::from(channel)] = u8::from(initial_high ^ (first_edge % 2 == 1));
        if channels.contains(&channel) {
            edges.push(NativeEdgeSet {
                channel,
                compact: false,
                sample_offset: start_sample,
                samples: Cow::Borrowed(&transitions[first_edge..edge_end]),
            });
        }
    }
    for channel in channels {
        if !available.contains(channel) {
            return Err(CoreError::Decode(format!(
                "native decoder channel D{channel} is not present in the sparse capture"
            )));
        }
    }
    edges.sort_by_key(|edge| edge.channel);
    trace_decoder_timing(
        "native",
        "indexed_edge_window_borrow",
        started,
        &format!(
            "window={start_sample}..{end_sample} channels={} edges={}",
            channels.len(),
            edges.iter().map(|edge| edge.samples.len()).sum::<usize>()
        ),
    );
    Ok((initial_levels, edges))
}

fn native_sidecar_settings(settings: &AnalyzerDecodeSettings) -> NativeSidecarSettings {
    let (protocol, primary_channel, channels, options) = match settings {
        AnalyzerDecodeSettings::Uart(settings) => (
            "uart".to_string(),
            settings.channel,
            serde_json::to_value(UartSidecarChannels {
                rx: Some(settings.channel),
                tx: None,
            })
            .expect("UART sidecar channels serialize"),
            serde_json::to_value(UartSidecarOptions {
                baudrate: settings.baud_rate,
                data_bits: settings.data_bits,
                stop_bits: settings.stop_bits.as_f64(),
                parity: match settings.parity {
                    UartParity::None => "none",
                    UartParity::Even => "even",
                    UartParity::Odd => "odd",
                },
                bit_order: if settings.msb_first {
                    "msb-first"
                } else {
                    "lsb-first"
                },
                format: "hex",
                invert_rx: u8::from(settings.inverted),
                invert_tx: 0,
            })
            .expect("UART sidecar options serialize"),
        ),
        AnalyzerDecodeSettings::I2c(settings) => (
            "i2c".to_string(),
            settings.sda_channel,
            serde_json::to_value(I2cSidecarChannels {
                scl: settings.scl_channel,
                sda: settings.sda_channel,
            })
            .expect("I2C sidecar channels serialize"),
            serde_json::to_value(I2cSidecarOptions {
                address_format: "shifted",
                packets_format: "hex",
                show_data_point: 0,
            })
            .expect("I2C sidecar options serialize"),
        ),
        AnalyzerDecodeSettings::Spi(settings) => {
            let spi = spi_sidecar_settings(settings);
            (
                "spi".to_string(),
                settings
                    .mosi_channel
                    .or(settings.miso_channel)
                    .unwrap_or(settings.clock_channel),
                serde_json::to_value(spi.channels).expect("SPI sidecar channels serialize"),
                serde_json::to_value(spi.options).expect("SPI sidecar options serialize"),
            )
        }
        AnalyzerDecodeSettings::Native(settings) => (
            settings.decoder_id.clone(),
            settings.primary_channel,
            serde_json::to_value(&settings.channels).expect("native sidecar channels serialize"),
            serde_json::to_value(&settings.options).expect("native sidecar options serialize"),
        ),
    };
    NativeSidecarSettings {
        protocol,
        primary_channel,
        channels,
        options,
    }
}

fn build_native_sidecar_request(
    capture: &CaptureData,
    settings: &AnalyzerDecodeSettings,
    compact_edges: bool,
) -> Result<NativeSidecarRequest<'static>> {
    if capture.metadata.sample_count == 0 {
        return Err(CoreError::Decode(
            "native decoding requires at least one sample".to_string(),
        ));
    }
    let channels = configured_native_channels(settings, capture.metadata.channel_count)?;
    let (initial_levels, mut edges) = packed_transition_sets(capture, &channels)?;
    edges
        .iter_mut()
        .for_each(|edge| edge.compact = compact_edges);
    Ok(NativeSidecarRequest {
        format: "pxlogic.decoder.v1",
        response_format: "compact_v1",
        include_fields: false,
        sample_rate_hz: capture.metadata.sample_rate_hz,
        sample_count: capture.metadata.sample_count,
        channel_count: capture.metadata.channel_count,
        initial_levels,
        edges,
        decoder: native_sidecar_settings(settings),
    })
}

fn build_sparse_native_sidecar_request<'a>(
    capture: &'a SparseCaptureData,
    settings: &AnalyzerDecodeSettings,
    compact_edges: bool,
) -> Result<NativeSidecarRequest<'a>> {
    if capture.metadata.sample_count == 0 {
        return Err(CoreError::Decode(
            "native decoding requires at least one sample".to_string(),
        ));
    }
    let channels = configured_native_channels(settings, capture.metadata.channel_count)?;
    let (initial_levels, mut edges) = sparse_transition_sets(
        capture.metadata.channel_count,
        capture.metadata.sample_count,
        capture.channels.iter().map(|channel| {
            (
                channel.channel,
                channel.initial_high,
                channel.transitions.as_slice(),
            )
        }),
        &channels,
    )?;
    edges
        .iter_mut()
        .for_each(|edge| edge.compact = compact_edges);
    Ok(NativeSidecarRequest {
        format: "pxlogic.decoder.v1",
        response_format: "compact_v1",
        include_fields: false,
        sample_rate_hz: capture.metadata.sample_rate_hz,
        sample_count: capture.metadata.sample_count,
        channel_count: capture.metadata.channel_count,
        initial_levels,
        edges,
        decoder: native_sidecar_settings(settings),
    })
}

fn build_sparse_view_native_sidecar_request<'a>(
    capture: &'a SparseCaptureView<'a>,
    settings: &AnalyzerDecodeSettings,
    compact_edges: bool,
) -> Result<NativeSidecarRequest<'a>> {
    if capture.metadata.sample_count == 0 {
        return Err(CoreError::Decode(
            "native decoding requires at least one sample".to_string(),
        ));
    }
    let channels = configured_native_channels(settings, capture.metadata.channel_count)?;
    let (initial_levels, mut edges) = sparse_transition_sets(
        capture.metadata.channel_count,
        capture.metadata.sample_count,
        capture
            .channels
            .iter()
            .map(|channel| (channel.channel, channel.initial_high, channel.transitions)),
        &channels,
    )?;
    edges
        .iter_mut()
        .for_each(|edge| edge.compact = compact_edges);
    Ok(NativeSidecarRequest {
        format: "pxlogic.decoder.v1",
        response_format: "compact_v1",
        include_fields: false,
        sample_rate_hz: capture.metadata.sample_rate_hz,
        sample_count: capture.metadata.sample_count,
        channel_count: capture.metadata.channel_count,
        initial_levels,
        edges,
        decoder: native_sidecar_settings(settings),
    })
}

fn build_sparse_view_window_native_sidecar_request<'a>(
    capture: &'a SparseCaptureView<'a>,
    start_sample: u64,
    end_sample: u64,
    settings: &AnalyzerDecodeSettings,
    compact_edges: bool,
) -> Result<NativeSidecarRequest<'a>> {
    let window_sample_count = end_sample
        .checked_sub(start_sample)
        .ok_or_else(|| CoreError::Decode("native sparse window range is invalid".to_string()))?;
    if window_sample_count == 0 {
        return Err(CoreError::Decode(
            "native decoding requires at least one sample".to_string(),
        ));
    }
    let channels = configured_native_channels(settings, capture.metadata.channel_count)?;
    let (initial_levels, mut edges) = sparse_window_transition_sets(
        capture.metadata.channel_count,
        capture.metadata.sample_count,
        start_sample,
        end_sample,
        capture
            .channels
            .iter()
            .map(|channel| (channel.channel, channel.initial_high, channel.transitions)),
        &channels,
    )?;
    edges
        .iter_mut()
        .for_each(|edge| edge.compact = compact_edges);
    Ok(NativeSidecarRequest {
        format: "pxlogic.decoder.v1",
        response_format: "compact_v1",
        include_fields: false,
        sample_rate_hz: capture.metadata.sample_rate_hz,
        sample_count: window_sample_count,
        channel_count: capture.metadata.channel_count,
        initial_levels,
        edges,
        decoder: native_sidecar_settings(settings),
    })
}

fn configured_spi_channels(
    settings: &crate::decode::SpiDecodeSettings,
    channel_count: u8,
) -> Result<BTreeSet<u8>> {
    let mut channels = BTreeSet::from([settings.clock_channel]);
    channels.extend(settings.mosi_channel);
    channels.extend(settings.miso_channel);
    channels.extend(settings.enable_channel);
    if channels.iter().any(|channel| *channel >= channel_count) {
        return Err(CoreError::Decode(
            "native SPI channel is outside the capture".to_string(),
        ));
    }
    Ok(channels)
}

fn spi_sidecar_settings(settings: &crate::decode::SpiDecodeSettings) -> SpiSidecarSettings {
    SpiSidecarSettings {
        protocol: "spi",
        channels: SpiSidecarChannels {
            mosi: settings.mosi_channel,
            miso: settings.miso_channel,
            clk: settings.clock_channel,
            cs: settings.enable_channel,
        },
        options: SpiSidecarOptions {
            cpol: u8::from(settings.clock_polarity),
            cpha: u8::from(settings.clock_phase),
            bit_order: if settings.msb_first {
                "msb_first"
            } else {
                "lsb_first"
            },
            word_size: settings.bits_per_transfer,
            cs_polarity: if settings.enable_active_low {
                "active_low"
            } else {
                "active_high"
            },
        },
    }
}

fn capture_word(capture: &CaptureData, sample: u64) -> Result<u32> {
    read_sample_word(&capture.samples, capture.metadata.unitsize, sample).ok_or_else(|| {
        CoreError::Decode(format!(
            "capture ended before declared sample {sample} of {}",
            capture.metadata.sample_count
        ))
    })
}

fn sidecar_error(backend: &str, operation: &str, stderr: &[u8]) -> CoreError {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    CoreError::Decode(format!(
        "{backend} sidecar {operation} failed{}",
        if message.is_empty() {
            String::new()
        } else {
            format!(": {message}")
        }
    ))
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

impl DecoderOutput {
    pub fn into_decoded_frames(self) -> Vec<crate::decode::DecodedFrame> {
        let protocol = self.protocol;
        let mut markers = self
            .frames
            .iter()
            .flat_map(|frame| frame.markers.iter())
            .map(|marker| crate::decode::DecodedProtocolMarker {
                channel: marker.channel,
                sample: marker.sample,
                kind: marker.kind.clone(),
            })
            .collect::<Vec<_>>();
        normalize_protocol_markers(&mut markers);
        let mut frames = self
            .frames
            .into_iter()
            .map(|mut frame| {
                let frame_type = frame.row.clone();
                let preferred_label = i2c_display_label(&protocol, &frame);
                if let Some(label) = &preferred_label {
                    for value in &mut frame.channel_values {
                        value.label = label.clone();
                        value.texts = vec![label.clone()];
                    }
                } else {
                    for value in &mut frame.channel_values {
                        if value.texts.is_empty() {
                            value.texts = frame.texts.clone();
                        }
                        if let Some(label) = value.texts.last() {
                            value.label = label.clone();
                        }
                    }
                }
                let label = preferred_label
                    .or_else(|| spi_display_label(&protocol, &frame))
                    .or_else(|| frame.texts.last().cloned())
                    .or_else(|| {
                        frame
                            .channel_values
                            .first()
                            .map(|value| value.label.clone())
                    })
                    .unwrap_or(frame.row);
                let value = frame
                    .channel_values
                    .first()
                    .map(|value| value.value)
                    .unwrap_or(0);
                crate::decode::DecodedFrame {
                    frame_id: frame.frame_id,
                    start_sample: frame.start_sample,
                    end_sample: frame.end_sample,
                    frame_type,
                    label,
                    value,
                    channel_values: frame.channel_values,
                    protocol_markers: Vec::new(),
                }
            })
            .collect::<Vec<_>>();
        attach_protocol_markers(&mut frames, markers);
        frames
    }
}

fn attach_protocol_markers(
    frames: &mut [crate::decode::DecodedFrame],
    markers: Vec<crate::decode::DecodedProtocolMarker>,
) {
    if frames.is_empty() || markers.is_empty() {
        return;
    }
    let mut frame_index = 0usize;
    for marker in markers {
        while frame_index + 1 < frames.len()
            && frames[frame_index + 1].start_sample <= marker.sample
        {
            frame_index += 1;
        }
        frames[frame_index].protocol_markers.push(marker);
    }
}

fn normalize_protocol_markers(markers: &mut Vec<crate::decode::DecodedProtocolMarker>) {
    if markers.len() <= 1 {
        return;
    }
    if !protocol_markers_are_ordered(markers) {
        markers.sort_by(|left, right| {
            left.sample
                .cmp(&right.sample)
                .then_with(|| left.channel.cmp(&right.channel))
                .then_with(|| left.kind.cmp(&right.kind))
        });
    }
    markers.dedup();
}

fn protocol_markers_are_ordered(markers: &[crate::decode::DecodedProtocolMarker]) -> bool {
    markers.windows(2).all(|pair| {
        let left = &pair[0];
        let right = &pair[1];
        left.sample < right.sample
            || (left.sample == right.sample
                && (left.channel < right.channel
                    || (left.channel == right.channel && left.kind <= right.kind)))
    })
}

fn spi_display_label(protocol: &str, frame: &DecoderOutputFrame) -> Option<String> {
    if !protocol.eq_ignore_ascii_case("spi") || frame.channel_values.is_empty() {
        return None;
    }
    let capacity = frame
        .channel_values
        .iter()
        .map(|value| value.role.len() + 1 + value.label.len())
        .sum::<usize>()
        + frame.channel_values.len().saturating_sub(1) * 3;
    let mut label = String::with_capacity(capacity);
    for (index, value) in frame.channel_values.iter().enumerate() {
        if index > 0 {
            label.push_str(" / ");
        }
        label.push_str(&value.role);
        label.push(' ');
        label.push_str(&value.label);
    }
    Some(label)
}

fn i2c_display_label(protocol: &str, frame: &DecoderOutputFrame) -> Option<String> {
    if !protocol.eq_ignore_ascii_case("i2c") {
        return None;
    }
    if frame.row.eq_ignore_ascii_case("address") {
        if let Some(label) = frame
            .texts
            .iter()
            .find(|text| text.starts_with("W[") || text.starts_with("R["))
        {
            return Some(label.clone());
        }
        let address = decoder_unsigned_field(frame, "address")?;
        let direction = if decoder_boolean_field(frame, "read").unwrap_or(false) {
            'R'
        } else {
            'W'
        };
        return Some(format!("{direction}[0x{address:02X}]"));
    }
    if frame.row.eq_ignore_ascii_case("data") {
        if let Some(label) = frame.texts.iter().find(|text| {
            text.contains(" + ACK") || text.contains(" + NAK") || text.contains("Missing ACK/NAK")
        }) {
            return Some(label.clone());
        }
        let data = decoder_unsigned_field(frame, "data")?;
        let acknowledgement = decoder_boolean_field(frame, "ack")
            .map(|acknowledged| if acknowledged { "ACK" } else { "NAK" })
            .or_else(|| {
                decoder_text_field(frame, "error")
                    .filter(|error| error.to_ascii_lowercase().contains("ack"))
                    .map(|_| "Missing ACK/NAK")
            });
        return Some(match acknowledgement {
            Some(acknowledgement) => format!("0x{data:02X} + {acknowledgement}"),
            None => format!("0x{data:02X}"),
        });
    }
    None
}

fn decoder_field<'a>(frame: &'a DecoderOutputFrame, name: &str) -> Option<&'a DecoderFieldValue> {
    frame
        .fields
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case(name))
        .map(|field| &field.value)
}

fn decoder_unsigned_field(frame: &DecoderOutputFrame, name: &str) -> Option<u64> {
    match decoder_field(frame, name)? {
        DecoderFieldValue::Unsigned(value) => Some(*value),
        DecoderFieldValue::Signed(value) => u64::try_from(*value).ok(),
        DecoderFieldValue::Boolean(value) => Some(u64::from(*value)),
        DecoderFieldValue::Text(_) | DecoderFieldValue::Bytes(_) => None,
    }
}

fn decoder_boolean_field(frame: &DecoderOutputFrame, name: &str) -> Option<bool> {
    match decoder_field(frame, name)? {
        DecoderFieldValue::Boolean(value) => Some(*value),
        DecoderFieldValue::Unsigned(value) => Some(*value != 0),
        DecoderFieldValue::Signed(value) => Some(*value != 0),
        DecoderFieldValue::Text(_) | DecoderFieldValue::Bytes(_) => None,
    }
}

fn decoder_text_field<'a>(frame: &'a DecoderOutputFrame, name: &str) -> Option<&'a str> {
    match decoder_field(frame, name)? {
        DecoderFieldValue::Text(value) => Some(value),
        DecoderFieldValue::Unsigned(_)
        | DecoderFieldValue::Signed(_)
        | DecoderFieldValue::Boolean(_)
        | DecoderFieldValue::Bytes(_) => None,
    }
}

/// Compatibility adapter for the current implementation. Keeping this
/// adapter explicit makes the eventual removal of the Rust decoder a backend
/// replacement instead of a UI rewrite.
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyRustDecoder;

impl DecoderBackend for LegacyRustDecoder {
    fn info(&self) -> DecoderBackendInfo {
        DecoderBackendInfo {
            kind: DecoderBackendKind::LegacyRust,
            display_name: "Legacy Rust decoder".to_string(),
            available: true,
            protocols: protocol_names(LEGACY_RUST_PROTOCOLS),
            supports_streaming: true,
            isolation: DecoderBackendIsolation::InProcess,
            detail: "Compatibility fallback only; prefer Saleae Native or Sigrok Native."
                .to_string(),
        }
    }

    fn decode(
        &self,
        capture: &CaptureData,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<DecoderOutput> {
        let protocol = match settings {
            AnalyzerDecodeSettings::Uart(_) => "UART",
            AnalyzerDecodeSettings::I2c(_) => "I2C",
            AnalyzerDecodeSettings::Spi(_) => "SPI",
            AnalyzerDecodeSettings::Native(settings) => {
                return Err(CoreError::Decode(format!(
                    "{} is not implemented by the Legacy Rust decoder",
                    settings.protocol_name
                )))
            }
        };
        let frames = crate::decode::decode_analyzer(capture, settings)?
            .into_iter()
            .map(|frame| DecoderOutputFrame {
                frame_id: frame.frame_id,
                start_sample: frame.start_sample,
                end_sample: frame.end_sample,
                row: protocol.to_string(),
                texts: vec![frame.label],
                fields: Vec::new(),
                channel_values: frame.channel_values,
                markers: Vec::new(),
            })
            .collect();
        Ok(DecoderOutput {
            backend: DecoderBackendKind::LegacyRust,
            protocol: protocol.to_string(),
            frames,
            diagnostics: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::{engine::general_purpose::STANDARD as TEST_BASE64, Engine as _};
    use chrono::Utc;

    use super::*;
    use crate::{
        decode::{
            DecodedFrame, DecodedProtocolMarker, I2cDecodeSettings, NativeDecodeSettings,
            NativeOptionValue, SpiDecodeSettings, UartDecodeSettings,
        },
        models::{
            CaptureMetadata, SparseCaptureView, SparseDigitalChannel, SparseDigitalChannelView,
        },
    };

    #[test]
    fn protocol_markers_attach_to_frames_with_a_linear_scan() {
        let mut frames = vec![
            decoded_test_frame(0, 10, 20),
            decoded_test_frame(1, 40, 50),
            decoded_test_frame(2, 80, 90),
        ];
        attach_protocol_markers(
            &mut frames,
            vec![
                decoded_test_marker(5, "before"),
                decoded_test_marker(40, "second"),
                decoded_test_marker(75, "between"),
                decoded_test_marker(120, "after"),
            ],
        );

        assert_eq!(
            frames
                .iter()
                .map(|frame| {
                    frame
                        .protocol_markers
                        .iter()
                        .map(|marker| (marker.sample, marker.kind.as_str()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
            vec![
                vec![(5, "before")],
                vec![(40, "second"), (75, "between")],
                vec![(120, "after")],
            ]
        );
    }

    #[test]
    fn ordered_protocol_markers_normalize_without_reordering() {
        let mut markers = vec![
            decoded_test_marker(10, "start"),
            decoded_test_marker(10, "start"),
            decoded_test_marker(20, "dot"),
            decoded_test_marker(40, "stop"),
        ];
        normalize_protocol_markers(&mut markers);
        assert_eq!(
            markers
                .iter()
                .map(|marker| (marker.sample, marker.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(10, "start"), (20, "dot"), (40, "stop")]
        );
    }

    #[test]
    fn unordered_protocol_markers_sort_before_attachment() {
        let mut markers = vec![
            decoded_test_marker(80, "stop"),
            decoded_test_marker(20, "start"),
            decoded_test_marker(20, "start"),
            decoded_test_marker(50, "dot"),
        ];
        normalize_protocol_markers(&mut markers);

        let mut frames = vec![
            decoded_test_frame(0, 10, 30),
            decoded_test_frame(1, 40, 70),
            decoded_test_frame(2, 80, 90),
        ];
        attach_protocol_markers(&mut frames, markers);

        assert_eq!(
            frames
                .iter()
                .map(|frame| {
                    frame
                        .protocol_markers
                        .iter()
                        .map(|marker| (marker.sample, marker.kind.as_str()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
            vec![vec![(20, "start")], vec![(50, "dot")], vec![(80, "stop")]]
        );
    }

    #[test]
    fn delta_varint_base64_encoding_streams_without_changing_payload() {
        assert_eq!(encode_delta_varint_samples_base64(&[1, 5], 0), "AQQ=");

        let expected_bytes = [10, 0x8c, 0x01, 0xee, 0x82, 0x01];
        assert_eq!(
            encode_delta_varint_samples_base64(&[110, 250, 17_000], 100),
            TEST_BASE64.encode(expected_bytes)
        );
    }

    #[test]
    fn i2c_adapter_preserves_direction_and_acknowledgement_labels() {
        let frames = DecoderOutput {
            backend: DecoderBackendKind::SaleaeNative,
            protocol: "I2C".to_string(),
            frames: vec![
                i2c_output_frame(
                    0,
                    "address",
                    vec![
                        decoder_test_field("address", DecoderFieldValue::Unsigned(0x20)),
                        decoder_test_field("read", DecoderFieldValue::Boolean(false)),
                        decoder_test_field("ack", DecoderFieldValue::Boolean(true)),
                    ],
                    "Data",
                    0x20,
                ),
                i2c_output_frame(
                    1,
                    "data",
                    vec![
                        decoder_test_field("data", DecoderFieldValue::Unsigned(0x14)),
                        decoder_test_field("ack", DecoderFieldValue::Boolean(true)),
                    ],
                    "Data",
                    0x14,
                ),
                i2c_output_frame(
                    2,
                    "address",
                    vec![
                        decoder_test_field("address", DecoderFieldValue::Unsigned(0x20)),
                        decoder_test_field("read", DecoderFieldValue::Boolean(true)),
                    ],
                    "Address",
                    0x20,
                ),
                i2c_output_frame(
                    3,
                    "data",
                    vec![
                        decoder_test_field("data", DecoderFieldValue::Unsigned(0x2d)),
                        decoder_test_field("ack", DecoderFieldValue::Boolean(false)),
                    ],
                    "Data",
                    0x2d,
                ),
            ],
            diagnostics: Vec::new(),
        }
        .into_decoded_frames();

        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.label.as_str())
                .collect::<Vec<_>>(),
            ["W[0x20]", "0x14 + ACK", "R[0x20]", "0x2D + NAK"]
        );
        assert_eq!(frames[0].channel_values[0].label, "W[0x20]");
        assert_eq!(frames[1].channel_values[0].label, "0x14 + ACK");
        assert_eq!(frames[2].channel_values[0].label, "R[0x20]");
        assert_eq!(frames[3].channel_values[0].label, "0x2D + NAK");
    }

    fn decoder_test_field(name: &str, value: DecoderFieldValue) -> DecoderField {
        DecoderField {
            name: name.to_string(),
            value,
        }
    }

    fn decoded_test_frame(frame_id: u64, start_sample: u64, end_sample: u64) -> DecodedFrame {
        DecodedFrame {
            frame_id,
            start_sample,
            end_sample,
            frame_type: "data".to_string(),
            label: format!("frame-{frame_id}"),
            value: frame_id,
            channel_values: Vec::new(),
            protocol_markers: Vec::new(),
        }
    }

    fn decoded_test_marker(sample: u64, kind: &str) -> DecodedProtocolMarker {
        DecodedProtocolMarker {
            channel: Some(0),
            sample,
            kind: kind.to_string(),
        }
    }

    fn i2c_output_frame(
        frame_id: u64,
        row: &str,
        fields: Vec<DecoderField>,
        role: &str,
        value: u64,
    ) -> DecoderOutputFrame {
        DecoderOutputFrame {
            frame_id,
            start_sample: frame_id * 10,
            end_sample: frame_id * 10 + 9,
            row: row.to_string(),
            texts: Vec::new(),
            fields,
            channel_values: vec![DecodedChannelValue {
                channel: 0,
                role: role.to_string(),
                label: format!("0x{value:02X}"),
                texts: vec![format!("0x{value:02X}")],
                value,
            }],
            markers: Vec::new(),
        }
    }

    #[test]
    fn native_sidecar_candidates_include_macos_app_resource_layout() {
        let contents_dir = PathBuf::from("/Applications/PXLogic Studio.app/Contents");
        let mut paths = Vec::new();
        push_bundled_native_candidate(
            &mut paths,
            &contents_dir,
            "saleae-analyzer-sidecar",
            "pxlogic-saleae-analyzer-sidecar",
        );

        assert!(paths.contains(
            &contents_dir
                .join("Resources")
                .join("resources")
                .join("native")
                .join("saleae-analyzer-sidecar")
                .join("pxlogic-saleae-analyzer-sidecar")
        ));

        let mut runtime_paths = Vec::new();
        push_saleae_runtime_candidates(&mut runtime_paths, &contents_dir);
        assert!(runtime_paths.contains(
            &contents_dir
                .join("Resources")
                .join("resources")
                .join("native")
                .join("saleae-analyzer-sidecar")
                .join("runtime")
        ));
    }

    fn transition_capture(
        source: &str,
        sample_rate_hz: u64,
        sample_count: u64,
        initial_levels: &[u8],
        edge_sets: &[(u8, Vec<u64>)],
    ) -> CaptureData {
        let mut word = initial_levels
            .iter()
            .enumerate()
            .fold(0u8, |word, (channel, level)| word | (*level << channel));
        let mut samples = Vec::with_capacity(sample_count as usize);
        for sample in 0..sample_count {
            for (channel, edges) in edge_sets {
                if edges.contains(&sample) {
                    word ^= 1u8 << channel;
                }
            }
            samples.push(word);
        }
        CaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: source.to_string(),
                sample_rate_hz,
                channel_count: initial_levels.len() as u8,
                enabled_channels: (0..initial_levels.len() as u8).collect(),
                unitsize: 1,
                sample_count,
                captured_at: Utc::now(),
                labels: (0..initial_levels.len())
                    .map(|channel| format!("D{channel}"))
                    .collect(),
                trigger: None,
            },
            samples,
        }
    }

    fn sparse_transition_capture(
        source: &str,
        sample_rate_hz: u64,
        sample_count: u64,
        offset: u64,
        initial_levels: &[u8],
        edge_sets: &[(u8, Vec<u64>)],
    ) -> SparseCaptureData {
        SparseCaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: source.to_string(),
                sample_rate_hz,
                channel_count: initial_levels.len() as u8,
                enabled_channels: (0..initial_levels.len() as u8).collect(),
                unitsize: 1,
                sample_count,
                captured_at: Utc::now(),
                labels: (0..initial_levels.len())
                    .map(|channel| format!("D{channel}"))
                    .collect(),
                trigger: None,
            },
            channels: edge_sets
                .iter()
                .map(|(channel, transitions)| SparseDigitalChannel {
                    channel: *channel,
                    initial_high: initial_levels[usize::from(*channel)] != 0,
                    transitions: transitions.iter().map(|sample| offset + sample).collect(),
                })
                .collect(),
        }
    }

    fn uart_capture_and_settings() -> (CaptureData, AnalyzerDecodeSettings) {
        (
            transition_capture(
                "sigrok-uart-test",
                1_000_000,
                120,
                &[1],
                &[(0, vec![10, 20, 30, 40, 50, 70, 80, 90])],
            ),
            AnalyzerDecodeSettings::Uart(UartDecodeSettings {
                channel: 0,
                baud_rate: 100_000,
                inverted: false,
                ..UartDecodeSettings::default()
            }),
        )
    }

    fn i2c_edge_sets() -> Vec<(u8, Vec<u64>)> {
        vec![
            (
                0,
                vec![
                    20, 40, 50, 70, 80, 100, 110, 130, 140, 160, 170, 190, 200, 220, 230, 250, 260,
                    280, 290, 310, 320, 340, 350, 370, 380, 400, 410, 430, 440, 460, 470, 490, 500,
                    520, 530, 550, 560, 570,
                ],
            ),
            (
                1,
                vec![
                    10, 30, 60, 90, 120, 300, 330, 360, 390, 450, 480, 510, 540, 580,
                ],
            ),
        ]
    }

    fn i2c_capture_and_settings() -> (CaptureData, AnalyzerDecodeSettings) {
        (
            transition_capture("sigrok-i2c-test", 1_000_000, 600, &[1, 1], &i2c_edge_sets()),
            AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
                scl_channel: 0,
                sda_channel: 1,
            }),
        )
    }

    fn i2s_edge_sets() -> Vec<(u8, Vec<u64>)> {
        let mut edges = vec![(0, Vec::new()), (1, Vec::new()), (2, Vec::new())];
        let mut levels = [false; 3];
        let words = [0x1234u16, 0x5678, 0x9abc];
        let bits = words
            .into_iter()
            .flat_map(|word| (0..16).rev().map(move |bit| word & (1 << bit) != 0))
            .collect::<Vec<_>>();
        for sample in 1..400u64 {
            let bit = sample.checked_sub(48).map(|offset| (offset / 10) as usize);
            let next = [
                sample >= 10 && (sample - 10) % 10 < 5,
                (48..208).contains(&sample) || sample >= 368,
                bit.and_then(|index| bits.get(index))
                    .copied()
                    .unwrap_or(false),
            ];
            for channel in 0..3 {
                if next[channel] != levels[channel] {
                    edges[channel].1.push(sample);
                    levels[channel] = next[channel];
                }
            }
        }
        edges
    }

    fn i2s_settings() -> AnalyzerDecodeSettings {
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: "i2s".to_string(),
            protocol_name: "I2S".to_string(),
            channels: BTreeMap::from([
                ("sck".to_string(), Some(0)),
                ("ws".to_string(), Some(1)),
                ("sd".to_string(), Some(2)),
            ]),
            options: BTreeMap::from([
                (
                    "bit_order".to_string(),
                    NativeOptionValue::Text("msb-first".to_string()),
                ),
                (
                    "clk_edge".to_string(),
                    NativeOptionValue::Text("rising-edge".to_string()),
                ),
                ("word_size".to_string(), NativeOptionValue::Integer(16)),
                (
                    "frame_transitions".to_string(),
                    NativeOptionValue::Text("once-each-word".to_string()),
                ),
                (
                    "bit_align".to_string(),
                    NativeOptionValue::Text("left-aligned".to_string()),
                ),
                (
                    "bit_shift".to_string(),
                    NativeOptionValue::Text("none".to_string()),
                ),
                ("signed".to_string(), NativeOptionValue::Boolean(false)),
                (
                    "ws_polarity".to_string(),
                    NativeOptionValue::Text("left-high".to_string()),
                ),
            ]),
            primary_channel: 0,
        })
    }

    fn i2s_capture() -> CaptureData {
        transition_capture(
            "native-i2s-test",
            1_000_000,
            400,
            &[0, 0, 0],
            &i2s_edge_sets(),
        )
    }

    fn assert_i2s_output(output: &DecoderOutput) {
        assert_eq!(
            output
                .frames
                .iter()
                .flat_map(|frame| frame.channel_values.iter())
                .map(|value| value.value)
                .take(2)
                .collect::<Vec<_>>(),
            vec![0x1234, 0x5678]
        );
        assert!(output.frames.iter().all(|frame| !frame.texts.is_empty()));
    }

    fn can_edge_sets() -> Vec<(u8, Vec<u64>)> {
        vec![(
            0,
            vec![
                100, 130, 140, 160, 170, 200, 220, 270, 290, 300, 310, 320, 330, 350, 360, 370,
                380, 390, 400, 410, 430, 440, 450, 460, 470, 490, 500, 520, 530, 540, 550, 560,
                590, 610, 620, 630,
            ],
        )]
    }

    fn can_settings(decoder_id: &str, protocol_name: &str) -> AnalyzerDecodeSettings {
        let mut options = BTreeMap::from([
            ("sample_point".to_string(), NativeOptionValue::Integer(70)),
            ("inverted".to_string(), NativeOptionValue::Boolean(false)),
        ]);
        if decoder_id == "can_fd" {
            options.insert(
                "nominal_bitrate".to_string(),
                NativeOptionValue::Integer(100_000),
            );
            options.insert(
                "fast_bitrate".to_string(),
                NativeOptionValue::Integer(200_000),
            );
        } else {
            options.insert("bitrate".to_string(), NativeOptionValue::Integer(100_000));
        }
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: decoder_id.to_string(),
            protocol_name: protocol_name.to_string(),
            channels: BTreeMap::from([("can_rx".to_string(), Some(0))]),
            options,
            primary_channel: 0,
        })
    }

    fn can_capture() -> CaptureData {
        transition_capture("native-can-test", 1_000_000, 840, &[1], &can_edge_sets())
    }

    fn assert_can_output(output: &DecoderOutput) {
        let values_for = |role: &str| {
            output
                .frames
                .iter()
                .flat_map(|frame| frame.channel_values.iter())
                .filter(|value| value.role == role)
                .map(|value| value.value)
                .collect::<Vec<_>>()
        };
        assert_eq!(values_for("Identifier"), vec![0x123]);
        assert_eq!(values_for("DLC"), vec![2]);
        assert_eq!(values_for("Data"), vec![0xA5, 0x5A]);
        assert_eq!(values_for("CRC"), vec![0x495C]);
        assert_eq!(values_for("ACK"), vec![1]);
    }

    fn lin_edge_sets() -> Vec<(u8, Vec<u64>)> {
        vec![(
            0,
            vec![
                100, 240, 260, 270, 280, 290, 300, 310, 320, 330, 340, 350, 360, 380, 390, 410,
                420, 440, 460, 470, 480, 490, 500, 520, 530, 540, 560, 580, 590, 600, 620, 630,
                640, 650, 660, 670, 680, 690, 710, 720, 740, 750,
            ],
        )]
    }

    fn lin_settings() -> AnalyzerDecodeSettings {
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: "lin".to_string(),
            protocol_name: "LIN".to_string(),
            channels: BTreeMap::from([("rx".to_string(), Some(0))]),
            options: BTreeMap::from([
                ("baudrate".to_string(), NativeOptionValue::Integer(100_000)),
                ("version".to_string(), NativeOptionValue::Integer(2)),
            ]),
            primary_channel: 0,
        })
    }

    fn lin_capture() -> CaptureData {
        transition_capture("native-lin-test", 1_000_000, 860, &[1], &lin_edge_sets())
    }

    fn assert_lin_output(output: &DecoderOutput) {
        assert_eq!(output.protocol, "LIN");
        assert_eq!(
            output
                .frames
                .iter()
                .flat_map(|frame| frame.channel_values.iter())
                .filter(|value| value.role == "LIN")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec![0, 0x55, 0x12, 0xA5, 0x5A, 0x6D]
        );
    }

    fn parallel_settings() -> AnalyzerDecodeSettings {
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: "parallel".to_string(),
            protocol_name: "Parallel".to_string(),
            channels: BTreeMap::from([
                ("d0".to_string(), Some(0)),
                ("d1".to_string(), Some(1)),
                ("clk".to_string(), Some(2)),
            ]),
            options: BTreeMap::from([
                (
                    "clock_edge".to_string(),
                    NativeOptionValue::Text("rising".to_string()),
                ),
                ("clock_state".to_string(), NativeOptionValue::Integer(0)),
                ("word_size".to_string(), NativeOptionValue::Integer(0)),
                (
                    "endianness".to_string(),
                    NativeOptionValue::Text("little".to_string()),
                ),
            ]),
            primary_channel: 0,
        })
    }

    #[test]
    fn legacy_backend_is_explicitly_discoverable() {
        let info = LegacyRustDecoder.info();
        assert_eq!(info.kind, DecoderBackendKind::LegacyRust);
        assert!(info.detail.contains("prefer"));
    }

    #[test]
    fn native_backend_fallback_info_keeps_external_protocol_catalog_visible() {
        let saleae = SaleaeNativeDecoder::new(
            "/definitely/missing/pxlogic-saleae-analyzer-sidecar",
            "/definitely/missing/saleae-runtime",
        )
        .info();
        assert_eq!(saleae.kind, DecoderBackendKind::SaleaeNative);
        assert!(!saleae.available);
        assert!(saleae.protocols.contains(&"LIN".to_string()));
        assert!(saleae.protocols.contains(&"Parallel".to_string()));
        assert!(!saleae.protocols.contains(&"CAN-FD".to_string()));

        let sigrok = SigrokNativeDecoder::new(
            "/definitely/missing/pxlogic-sigrok-sidecar",
            "/definitely/missing/sigrok-decoders",
        )
        .info();
        assert_eq!(sigrok.kind, DecoderBackendKind::SigrokNative);
        assert!(!sigrok.available);
        assert!(sigrok.protocols.contains(&"CAN-FD".to_string()));
        assert!(sigrok.protocols.contains(&"LIN".to_string()));
        assert!(sigrok.protocols.contains(&"Parallel".to_string()));
    }

    #[test]
    fn legacy_adapter_preserves_current_frame_shape() {
        let metadata = CaptureMetadata {
            version: 1,
            source_device: "decoder-contract-test".to_string(),
            sample_rate_hz: 1_000_000,
            channel_count: 8,
            enabled_channels: (0..8).collect(),
            unitsize: 1,
            sample_count: 32,
            captured_at: Utc::now(),
            labels: (0..8).map(|channel| format!("D{channel}")).collect(),
            trigger: None,
        };
        let capture = CaptureData {
            metadata,
            samples: vec![0; 32],
        };
        let output = LegacyRustDecoder
            .decode(
                &capture,
                &AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
            )
            .unwrap();
        assert_eq!(output.backend, DecoderBackendKind::LegacyRust);
        assert_eq!(output.protocol, "SPI");
    }

    #[test]
    fn external_settings_never_fall_back_to_legacy_rust() {
        let error = LegacyRustDecoder
            .decode(&i2s_capture(), &i2s_settings())
            .unwrap_err();
        assert!(error.to_string().contains("not implemented"));
    }

    #[test]
    fn generic_native_request_preserves_i2s_contract() {
        let request = build_native_sidecar_request(&i2s_capture(), &i2s_settings(), true).unwrap();
        let decoder = serde_json::to_value(request.decoder).unwrap();
        assert_eq!(decoder["protocol"], "i2s");
        assert_eq!(decoder["channels"]["sck"], 0);
        assert_eq!(decoder["channels"]["ws"], 1);
        assert_eq!(decoder["channels"]["sd"], 2);
        assert_eq!(decoder["options"]["word_size"], 16);
    }

    #[test]
    fn generic_native_request_preserves_can_contract_without_rust_protocol_logic() {
        let request =
            build_native_sidecar_request(&can_capture(), &can_settings("can", "CAN"), true)
                .unwrap();
        let decoder = serde_json::to_value(request.decoder).unwrap();
        assert_eq!(decoder["protocol"], "can");
        assert_eq!(decoder["channels"]["can_rx"], 0);
        assert_eq!(decoder["options"]["bitrate"], 100_000);
        assert_eq!(decoder["options"]["sample_point"], 70);
        assert_eq!(decoder["options"]["inverted"], false);
    }

    #[test]
    fn generic_native_request_preserves_lin_contract_without_rust_protocol_logic() {
        let request = build_native_sidecar_request(&lin_capture(), &lin_settings(), true).unwrap();
        let decoder = serde_json::to_value(request.decoder).unwrap();
        assert_eq!(decoder["protocol"], "lin");
        assert_eq!(decoder["channels"]["rx"], 0);
        assert_eq!(decoder["options"]["baudrate"], 100_000);
        assert_eq!(decoder["options"]["version"], 2);
    }

    #[test]
    fn generic_native_request_preserves_parallel_contract_without_rust_protocol_logic() {
        let capture = transition_capture(
            "native-parallel-test",
            1_000_000,
            200,
            &[0, 0, 0],
            &[
                (0, vec![30, 70, 110, 150]),
                (1, vec![50, 90, 130, 170]),
                (2, vec![20, 25, 60, 65, 100, 105, 140, 145]),
            ],
        );
        let request = build_native_sidecar_request(&capture, &parallel_settings(), true).unwrap();
        let decoder = serde_json::to_value(request.decoder).unwrap();
        assert_eq!(decoder["protocol"], "parallel");
        assert_eq!(decoder["channels"]["d0"], 0);
        assert_eq!(decoder["channels"]["d1"], 1);
        assert_eq!(decoder["channels"]["clk"], 2);
        assert_eq!(decoder["options"]["clock_edge"], "rising");
        assert_eq!(decoder["options"]["clock_state"], 0);
    }

    #[test]
    fn sidecar_request_extracts_only_configured_spi_edges() {
        let capture = CaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "saleae-request-test".to_string(),
                sample_rate_hz: 1_000_000,
                channel_count: 4,
                enabled_channels: (0..4).collect(),
                unitsize: 1,
                sample_count: 6,
                captured_at: Utc::now(),
                labels: (0..4).map(|channel| format!("D{channel}")).collect(),
                trigger: None,
            },
            samples: vec![0b1000, 0b1001, 0b1011, 0b0011, 0b0111, 0b0110],
        };
        let settings = SpiDecodeSettings {
            mosi_channel: Some(0),
            miso_channel: Some(1),
            clock_channel: 2,
            enable_channel: Some(3),
            ..SpiDecodeSettings::default()
        };

        let request =
            build_native_sidecar_request(&capture, &AnalyzerDecodeSettings::Spi(settings), true)
                .unwrap();
        assert_eq!(request.initial_levels, vec![0, 0, 0, 1]);
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["include_fields"], false);
        assert_eq!(serialized["edges"][0]["encoding"], "delta_varint_base64");
        assert!(serialized["edges"][0].get("samples").is_none());
        let edges = request
            .edges
            .into_iter()
            .map(|edge| (edge.channel, edge.samples))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            edges.get(&0).map(|samples| samples.as_ref()),
            Some(&[1, 5][..])
        );
        assert_eq!(
            edges.get(&1).map(|samples| samples.as_ref()),
            Some(&[2][..])
        );
        assert_eq!(
            edges.get(&2).map(|samples| samples.as_ref()),
            Some(&[4][..])
        );
        assert_eq!(
            edges.get(&3).map(|samples| samples.as_ref()),
            Some(&[3][..])
        );

        let legacy = build_native_sidecar_request(
            &capture,
            &AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
                mosi_channel: Some(0),
                miso_channel: Some(1),
                clock_channel: 2,
                enable_channel: Some(3),
                ..SpiDecodeSettings::default()
            }),
            false,
        )
        .unwrap();
        let serialized = serde_json::to_value(legacy).unwrap();
        assert!(serialized["edges"][0]["samples"].is_array());
        assert!(serialized["edges"][0].get("samples_b64").is_none());
    }

    #[test]
    fn sparse_window_request_borrows_global_edges_but_serializes_window_relative_samples() {
        let capture = sparse_transition_capture(
            "native-window-offset-test",
            1_000_000,
            1_000,
            0,
            &[0, 0, 0],
            &[
                (0, vec![100, 200, 300]),
                (1, vec![50, 150]),
                (2, vec![120, 140, 160]),
            ],
        );
        let view = SparseCaptureView {
            metadata: &capture.metadata,
            channels: capture
                .channels
                .iter()
                .map(|channel| SparseDigitalChannelView {
                    channel: channel.channel,
                    initial_high: channel.initial_high,
                    transitions: &channel.transitions,
                })
                .collect(),
        };
        let settings = AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
            mosi_channel: Some(0),
            miso_channel: None,
            clock_channel: 2,
            enable_channel: None,
            ..SpiDecodeSettings::default()
        });

        let request =
            build_sparse_view_window_native_sidecar_request(&view, 125, 250, &settings, false)
                .unwrap();

        assert_eq!(request.sample_count, 125);
        assert_eq!(request.initial_levels[0], 1);
        assert_eq!(request.initial_levels[2], 1);
        let edge_sets = request
            .edges
            .iter()
            .map(|edge| (edge.channel, edge.samples.as_ref()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(edge_sets.get(&0).copied(), Some(&[200][..]));
        assert_eq!(edge_sets.get(&2).copied(), Some(&[140, 160][..]));

        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["sample_count"], 125);
        assert_eq!(serialized["edges"][0]["samples"], serde_json::json!([75]));
        assert_eq!(
            serialized["edges"][1]["samples"],
            serde_json::json!([15, 35])
        );
    }

    #[test]
    fn native_requests_cover_uart_and_i2c_without_unrelated_channels() {
        let (uart_capture, uart_settings) = uart_capture_and_settings();
        let uart = build_native_sidecar_request(&uart_capture, &uart_settings, true).unwrap();
        assert_eq!(uart.edges.len(), 1);
        assert_eq!(uart.edges[0].channel, 0);
        assert_eq!(uart.decoder.protocol, "uart");

        let (i2c_capture, i2c_settings) = i2c_capture_and_settings();
        let i2c = build_native_sidecar_request(&i2c_capture, &i2c_settings, true).unwrap();
        assert_eq!(
            i2c.edges
                .iter()
                .map(|edge| edge.channel)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(i2c.decoder.protocol, "i2c");
    }

    #[test]
    fn saleae_persistent_sidecar_flag_accepts_common_disable_values() {
        for disabled in ["0", "false", "False", "no", "off"] {
            assert!(!saleae_persistent_sidecar_enabled_value(disabled));
        }
        for enabled in ["", "1", "true", "yes", "on"] {
            assert!(saleae_persistent_sidecar_enabled_value(enabled));
        }
    }

    #[test]
    fn saleae_persistent_worker_count_is_bounded_and_configurable() {
        assert_eq!(saleae_persistent_worker_count_value(None, 8), 4);
        assert_eq!(saleae_persistent_worker_count_value(None, 2), 2);
        assert_eq!(saleae_persistent_worker_count_value(Some("1"), 8), 1);
        assert_eq!(saleae_persistent_worker_count_value(Some("8"), 4), 4);
        assert_eq!(saleae_persistent_worker_count_value(Some("garbage"), 8), 4);
        assert_eq!(saleae_persistent_worker_count_value(Some("0"), 8), 4);
    }

    #[test]
    fn saleae_compact_decoder_output_restores_public_shape() {
        let compact = serde_json::json!({
            "format": "pxlogic.decoder.compact_v1",
            "p": "SPI",
            "f": [[
                7,
                100,
                120,
                "result",
                ["0xA5"],
                [["data", "u", 165], ["valid", "b", true]],
                [[1, "MOSI", "0xA5", ["MOSI", "0xA5"], 165]],
                [["marker", 2, 110, "up_arrow"]]
            ]],
            "d": [["info", "compact parse ok"]]
        });

        let output = parse_saleae_decode_output(compact.to_string().as_bytes()).unwrap();

        assert_eq!(output.backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(output.protocol, "SPI");
        assert_eq!(output.diagnostics[0].message, "compact parse ok");
        let frame = &output.frames[0];
        assert_eq!(frame.frame_id, 7);
        assert_eq!(frame.fields[0].value, DecoderFieldValue::Unsigned(165));
        assert_eq!(frame.fields[1].value, DecoderFieldValue::Boolean(true));
        assert_eq!(frame.channel_values[0].role, "MOSI");
        assert_eq!(frame.markers[0].kind, "up_arrow");
    }

    #[test]
    fn saleae_standard_decoder_output_remains_supported() {
        let standard = DecoderOutput {
            backend: DecoderBackendKind::SaleaeNative,
            protocol: "SPI".to_string(),
            frames: vec![i2c_output_frame(0, "result", Vec::new(), "MOSI", 0xA5)],
            diagnostics: vec![DecoderDiagnostic {
                level: "info".to_string(),
                message: "standard parse ok".to_string(),
            }],
        };
        let bytes = serde_json::to_vec(&standard).unwrap();

        let output = parse_saleae_decode_output(&bytes).unwrap();

        assert_eq!(output, standard);
    }

    fn sparse_mode_zero_capture_and_settings() -> (SparseCaptureData, AnalyzerDecodeSettings) {
        const OFFSET: u64 = 49_000_000_000;
        let (_, settings) = mode_zero_capture_and_settings();
        let edge_sets = [
            (0u8, true, vec![15, 25, 35, 55, 65, 75]),
            (1u8, false, vec![25, 65]),
            (
                2u8,
                false,
                vec![
                    10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85,
                ],
            ),
            (3u8, true, vec![5, 90]),
        ];
        let capture = SparseCaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "sparse-native-sidecar-test".to_string(),
                sample_rate_hz: 1_000_000,
                channel_count: 4,
                enabled_channels: (0..4).collect(),
                unitsize: 1,
                sample_count: 50_000_000_000,
                captured_at: Utc::now(),
                labels: (0..4).map(|channel| format!("D{channel}")).collect(),
                trigger: None,
            },
            channels: edge_sets
                .into_iter()
                .map(
                    |(channel, initial_high, transitions)| SparseDigitalChannel {
                        channel,
                        initial_high,
                        transitions: transitions
                            .into_iter()
                            .map(|sample| OFFSET + sample)
                            .collect(),
                    },
                )
                .collect(),
        };
        (capture, settings)
    }

    #[test]
    fn sparse_sidecar_request_preserves_transition_indexes_beyond_u32() {
        let (capture, settings) = sparse_mode_zero_capture_and_settings();
        let request = build_sparse_native_sidecar_request(&capture, &settings, true).unwrap();
        assert_eq!(request.sample_count, 50_000_000_000);
        assert_eq!(request.initial_levels, vec![1, 0, 0, 1]);
        assert_eq!(request.edges[0].samples[0], 49_000_000_015);
        assert_eq!(request.edges[2].samples.last(), Some(&49_000_000_085));
    }

    fn mode_zero_capture_and_settings() -> (CaptureData, AnalyzerDecodeSettings) {
        let initial_levels = [1u8, 0, 0, 1];
        let edge_sets = [
            (0u8, vec![15, 25, 35, 55, 65, 75]),
            (1u8, vec![25, 65]),
            (
                2u8,
                vec![
                    10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85,
                ],
            ),
            (3u8, vec![5, 90]),
        ];
        let mut word = initial_levels
            .iter()
            .enumerate()
            .fold(0u8, |word, (channel, level)| word | (*level << channel));
        let mut samples = Vec::with_capacity(100);
        for sample in 0..100u64 {
            for (channel, edges) in &edge_sets {
                if edges.contains(&sample) {
                    word ^= 1u8 << channel;
                }
            }
            samples.push(word);
        }
        let capture = CaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "native-sidecar-test".to_string(),
                sample_rate_hz: 1_000_000,
                channel_count: 4,
                enabled_channels: (0..4).collect(),
                unitsize: 1,
                sample_count: 100,
                captured_at: Utc::now(),
                labels: (0..4).map(|channel| format!("D{channel}")).collect(),
                trigger: None,
            },
            samples,
        };
        let settings = AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
            mosi_channel: Some(0),
            miso_channel: Some(1),
            clock_channel: 2,
            enable_channel: Some(3),
            enable_active_low: true,
            clock_polarity: false,
            clock_phase: false,
            bits_per_transfer: 8,
            msb_first: true,
        });
        (capture, settings)
    }

    fn assert_mode_zero_output(output: &DecoderOutput) {
        let result = output
            .frames
            .iter()
            .find(|frame| frame.row == "result")
            .unwrap();
        assert!(result
            .channel_values
            .iter()
            .any(|value| value.role == "MOSI" && value.value == 0xA5));
        assert!(result
            .channel_values
            .iter()
            .any(|value| value.role == "MISO" && value.value == 0x3C));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_mode_zero_fixture() {
        let (capture, settings) = mode_zero_capture_and_settings();

        let output = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode(&capture, &settings)
            .unwrap();
        let result = output
            .frames
            .iter()
            .find(|frame| frame.row == "result")
            .unwrap();
        assert!(result.texts.contains(&"0xA5".to_string()));
        assert!(result.texts.contains(&"0x3C".to_string()));
        assert_mode_zero_output(&output);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_sparse_fixture_beyond_u32() {
        let (capture, settings) = sparse_mode_zero_capture_and_settings();
        let output = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(&capture, &settings)
            .unwrap();
        assert_eq!(output.backend, DecoderBackendKind::SaleaeNative);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
        assert_mode_zero_output(&output);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_uart_and_i2c_fixtures() {
        let (uart_capture, uart_settings) = uart_capture_and_settings();
        let uart = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode(&uart_capture, &uart_settings)
            .unwrap();
        assert_eq!(uart.protocol, "Async Serial");
        assert_eq!(uart.frames.len(), 1);
        assert_eq!(uart.frames[0].channel_values[0].value, 0xA5);

        let (i2c_capture, i2c_settings) = i2c_capture_and_settings();
        let i2c = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode(&i2c_capture, &i2c_settings)
            .unwrap();
        assert_eq!(i2c.protocol, "I2C");
        assert_eq!(
            i2c.frames
                .iter()
                .flat_map(|frame| frame.channel_values.iter())
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec![0x50, 0xA5]
        );
        assert_eq!(i2c.frames.first().unwrap().row, "start");
        assert_eq!(i2c.frames.last().unwrap().row, "stop");
        let decoded = i2c.into_decoded_frames();
        assert_eq!(
            decoded
                .iter()
                .filter(|frame| frame.frame_type == "address" || frame.frame_type == "data")
                .map(|frame| frame.label.as_str())
                .collect::<Vec<_>>(),
            ["W[0x50]", "0xA5 + ACK"]
        );
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_uart_sparse_fixture_preserves_u64_indexes() {
        const OFFSET: u64 = 49_000_000_000;
        let capture = sparse_transition_capture(
            "saleae-uart-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1],
            &[(0, vec![10, 20, 30, 40, 50, 70, 80, 90])],
        );
        let output = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(
                &capture,
                &AnalyzerDecodeSettings::Uart(UartDecodeSettings {
                    channel: 0,
                    baud_rate: 100_000,
                    ..UartDecodeSettings::default()
                }),
            )
            .unwrap();
        assert_eq!(output.frames[0].channel_values[0].value, 0xA5);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_i2s_packed_and_sparse_fixtures() {
        let decoder = SaleaeNativeDecoder::from_env().unwrap();
        let packed = decoder.decode(&i2s_capture(), &i2s_settings()).unwrap();
        assert_eq!(packed.protocol, "I2S / PCM");
        assert_i2s_output(&packed);

        const OFFSET: u64 = 49_000_000_000;
        let sparse = sparse_transition_capture(
            "saleae-i2s-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[0, 0, 0],
            &i2s_edge_sets(),
        );
        let output = decoder.decode_sparse(&sparse, &i2s_settings()).unwrap();
        assert_i2s_output(&output);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_can_packed_and_sparse_fixtures() {
        let decoder = SaleaeNativeDecoder::from_env().unwrap();
        let packed = decoder
            .decode(&can_capture(), &can_settings("can", "CAN"))
            .unwrap();
        assert_eq!(packed.protocol, "CAN");
        assert_can_output(&packed);

        const OFFSET: u64 = 49_000_000_000;
        let sparse = sparse_transition_capture(
            "saleae-can-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1],
            &can_edge_sets(),
        );
        let output = decoder
            .decode_sparse(&sparse, &can_settings("can", "CAN"))
            .unwrap();
        assert_can_output(&output);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_native_sidecar_decodes_lin_packed_and_sparse_fixtures() {
        let decoder = SaleaeNativeDecoder::from_env().unwrap();
        let packed = decoder.decode(&lin_capture(), &lin_settings()).unwrap();
        assert_lin_output(&packed);

        const OFFSET: u64 = 49_000_000_000;
        let sparse = sparse_transition_capture(
            "saleae-lin-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1],
            &lin_edge_sets(),
        );
        let output = decoder.decode_sparse(&sparse, &lin_settings()).unwrap();
        assert_lin_output(&output);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_official_generators_round_trip_core_native_protocols() {
        let decoder = SaleaeNativeDecoder::from_env().unwrap();
        let settings = [
            AnalyzerDecodeSettings::Uart(UartDecodeSettings::default()),
            AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
            AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
            native_settings(
                "can",
                "CAN",
                &[("CAN", 0)],
                &[("Bit Rate (Bits/s)", NativeOptionValue::Integer(1_000_000))],
            ),
            native_settings(
                "lin",
                "LIN",
                &[("Serial", 0)],
                &[
                    ("LIN Version", NativeOptionValue::Integer(2)),
                    ("Bit Rate (Bits/s)", NativeOptionValue::Integer(20_000)),
                ],
            ),
            native_settings(
                "i2s",
                "I2S/PCM",
                &[("CLOCK channel", 0), ("FRAME", 1), ("DATA", 2)],
                &[(
                    "Audio Bit Depth (bits/sample)",
                    NativeOptionValue::Integer(16),
                )],
            ),
            native_settings(
                "simple_parallel",
                "Parallel",
                &[("Clock", 0), ("D0", 1), ("D1", 2)],
                &[("Clock State", NativeOptionValue::Integer(0))],
            ),
            native_settings("swd", "SWD", &[("SWDIO", 0), ("SWCLK", 1)], &[]),
        ];

        for settings in settings {
            let generated = decoder.simulate(&settings, 10_000_000, 2_000_000).unwrap();
            assert!(
                !generated.is_empty(),
                "{} generated no channels",
                native_protocol_name(&settings)
            );
            assert!(generated
                .iter()
                .any(|channel| !channel.transitions.is_empty()));
            let sample_count = generated
                .iter()
                .map(|channel| channel.sample_count)
                .max()
                .unwrap();
            let channel_count = generated
                .iter()
                .map(|channel| channel.channel)
                .max()
                .unwrap()
                + 1;
            let capture = SparseCaptureData {
                metadata: CaptureMetadata {
                    version: 1,
                    source_device: "saleae-official-simulation".to_string(),
                    sample_rate_hz: 10_000_000,
                    channel_count,
                    enabled_channels: (0..channel_count).collect(),
                    unitsize: 1,
                    sample_count,
                    captured_at: Utc::now(),
                    labels: (0..channel_count)
                        .map(|channel| format!("D{channel}"))
                        .collect(),
                    trigger: None,
                },
                channels: generated
                    .into_iter()
                    .map(|channel| SparseDigitalChannel {
                        channel: channel.channel,
                        initial_high: channel.initial_high,
                        transitions: channel.transitions,
                    })
                    .collect(),
            };
            let output = decoder.decode_sparse(&capture, &settings).unwrap();
            assert!(
                !output.frames.is_empty(),
                "{} did not decode its generated waveform",
                native_protocol_name(&settings)
            );
            assert!(output.frames.iter().any(|frame| !frame.texts.is_empty()));
            assert!(output.frames.iter().any(|frame| !frame.markers.is_empty()));
            assert!(output
                .frames
                .iter()
                .flat_map(|frame| frame.channel_values.iter())
                .all(|value| !value.texts.is_empty()));
            let bubble_text = output
                .frames
                .iter()
                .flat_map(|frame| frame.texts.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let expected = match native_protocol_name(&settings) {
                "UART" => "0x",
                "I2C" => "ACK",
                "SPI" => "0x",
                "CAN" => "Identifier",
                "LIN" => "Header Break",
                "I2S/PCM" => "Ch ",
                "Parallel" => "0x",
                "SWD" => "",
                protocol => panic!("unexpected protocol {protocol}"),
            };
            assert!(
                bubble_text.contains(expected),
                "{} bubble text was {bubble_text}",
                native_protocol_name(&settings)
            );
            if native_protocol_name(&settings) == "SWD" {
                let bubble_channels = output
                    .frames
                    .iter()
                    .flat_map(|frame| frame.channel_values.iter().map(|value| value.channel))
                    .collect::<BTreeSet<_>>();
                assert!(bubble_channels.contains(&0) && bubble_channels.contains(&1));
                assert!(output
                    .frames
                    .iter()
                    .flat_map(|frame| &frame.markers)
                    .any(|marker| marker.kind == "one" || marker.kind == "zero"));
            }
        }
    }

    #[test]
    fn saleae_simulation_normalization_preserves_zero_sample_transition() {
        let mut channel = SaleaeSimulationChannel {
            channel: 0,
            initial_high: true,
            sample_count: 8,
            transitions: vec![4, 0, 4, 2],
        };

        normalize_saleae_simulation_channel(&mut channel);

        assert!(!channel.initial_high);
        assert_eq!(channel.transitions, vec![2, 4]);
        assert_eq!(channel.sample_count, 8);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_official_generators_round_trip_entire_catalog() {
        let decoder = SaleaeNativeDecoder::from_env().unwrap();
        let catalog = decoder.decoder_catalog().unwrap();
        let saleae_items = catalog
            .decoders
            .iter()
            .filter(|item| item.kind.as_deref() == Some("saleae"))
            .collect::<Vec<_>>();
        assert!(saleae_items.len() >= 20);

        let mut unsupported_generators = Vec::new();
        for item in saleae_items {
            let settings = catalog_item_native_settings(item);
            let generated = decoder.simulate(&settings, 10_000_000, 2_000_000).unwrap();
            if generated.is_empty() {
                unsupported_generators.push(
                    item.id
                        .clone()
                        .unwrap_or_else(|| native_protocol_name(&settings).to_string()),
                );
                continue;
            }
            assert!(
                generated
                    .iter()
                    .any(|channel| !channel.transitions.is_empty()),
                "{} generated only static simulation channels",
                native_protocol_name(&settings)
            );
            let sample_count = generated
                .iter()
                .map(|channel| channel.sample_count)
                .max()
                .unwrap();
            let channel_count = generated
                .iter()
                .map(|channel| channel.channel)
                .max()
                .unwrap()
                + 1;
            let capture = SparseCaptureData {
                metadata: CaptureMetadata {
                    version: 1,
                    source_device: "saleae-catalog-official-simulation".to_string(),
                    sample_rate_hz: 10_000_000,
                    channel_count,
                    enabled_channels: (0..channel_count).collect(),
                    unitsize: 1,
                    sample_count,
                    captured_at: Utc::now(),
                    labels: (0..channel_count)
                        .map(|channel| format!("D{channel}"))
                        .collect(),
                    trigger: None,
                },
                channels: generated
                    .into_iter()
                    .map(|channel| SparseDigitalChannel {
                        channel: channel.channel,
                        initial_high: channel.initial_high,
                        transitions: channel.transitions,
                    })
                    .collect(),
            };
            let output = decoder.decode_sparse(&capture, &settings).unwrap();
            assert!(
                !output.frames.is_empty(),
                "{} did not decode its generated waveform",
                native_protocol_name(&settings)
            );
        }

        assert_eq!(unsupported_generators, vec!["mcs04"]);
    }

    fn native_settings(
        decoder_id: &str,
        protocol_name: &str,
        channels: &[(&str, u8)],
        options: &[(&str, NativeOptionValue)],
    ) -> AnalyzerDecodeSettings {
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: decoder_id.to_string(),
            protocol_name: protocol_name.to_string(),
            channels: channels
                .iter()
                .map(|(name, channel)| ((*name).to_string(), Some(*channel)))
                .collect(),
            options: options
                .iter()
                .map(|(name, value)| ((*name).to_string(), value.clone()))
                .collect(),
            primary_channel: channels[0].1,
        })
    }

    fn catalog_item_native_settings(item: &SigrokDecoderCatalogItem) -> AnalyzerDecodeSettings {
        let decoder_id = item.id.clone().expect("saleae catalog items have ids");
        let protocol_name = item
            .name
            .clone()
            .or_else(|| item.longname.clone())
            .unwrap_or_else(|| decoder_id.clone());
        let mut sorted_channels = item
            .channels
            .iter()
            .chain(item.optional_channels.iter())
            .collect::<Vec<_>>();
        sorted_channels.sort_by_key(|channel| channel.order);
        let channels = sorted_channels
            .iter()
            .enumerate()
            .map(|(index, channel)| {
                (
                    catalog_channel_key(channel, index),
                    Some(u8::try_from(index).expect("saleae catalog channel fits in u8")),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let options = item
            .options
            .iter()
            .enumerate()
            .filter_map(|(index, option)| {
                let value = native_option_from_json(&option.default)
                    .or_else(|| option.values.iter().find_map(native_option_from_json))?;
                Some((catalog_option_key(option, index), value))
            })
            .collect();
        AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id,
            protocol_name,
            primary_channel: channels.values().flatten().copied().next().unwrap_or(0),
            channels,
            options,
        })
    }

    fn catalog_channel_key(channel: &SigrokDecoderChannel, fallback_index: usize) -> String {
        channel
            .id
            .as_deref()
            .or(channel.name.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| format!("ch{fallback_index}"))
    }

    fn catalog_option_key(option: &SigrokDecoderOption, fallback_index: usize) -> String {
        option
            .id
            .as_deref()
            .or(option.idn.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| format!("option{fallback_index}"))
    }

    fn native_option_from_json(value: &serde_json::Value) -> Option<NativeOptionValue> {
        match value {
            serde_json::Value::Bool(value) => Some(NativeOptionValue::Boolean(*value)),
            serde_json::Value::Number(value) => value
                .as_i64()
                .map(NativeOptionValue::Integer)
                .or_else(|| {
                    value
                        .as_u64()
                        .and_then(|value| i64::try_from(value).ok())
                        .map(NativeOptionValue::Integer)
                })
                .or_else(|| value.as_f64().map(NativeOptionValue::Float)),
            serde_json::Value::String(value) => Some(NativeOptionValue::Text(value.clone())),
            _ => None,
        }
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_mode_zero_fixture() {
        let (capture, settings) = mode_zero_capture_and_settings();
        let output = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode(&capture, &settings)
            .unwrap();
        assert_eq!(output.backend, DecoderBackendKind::SigrokNative);
        assert_mode_zero_output(&output);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR, PXLOGIC_SIGROK_DECODER_ROOT, and PXLOGIC_SIGROK_PYTHON_DECODER_ROOT"]
    fn sigrok_native_sidecar_catalog_enumerates_pxview_c_and_python_decoders() {
        let probe = SigrokNativeDecoder::from_env()
            .unwrap()
            .probe_with_catalog(true)
            .unwrap();

        assert!(probe.available);
        assert!(probe.catalog_included);
        assert!(probe.c_decoder_count >= 200);
        assert!(probe.python_decoder_count >= 200);

        let has = |id: &str, kind: &str| {
            probe
                .decoder_catalog
                .iter()
                .any(|item| item.id.as_deref() == Some(id) && item.kind.as_deref() == Some(kind))
        };
        assert!(has("qspi_c", "c"));
        assert!(has("modbus_c", "c"));
        assert!(has("usb_power_delivery", "python"));
        assert!(has("jtag", "python"));
        assert!(probe.decoder_catalog.iter().any(|item| {
            item.id.as_deref() == Some("spi_c")
                && item.runner_status.as_deref() == Some("verified_sparse_c")
        }));
        assert!(probe.decoder_catalog.iter().any(|item| {
            item.id.as_deref() == Some("guess_bitrate_c")
                && item.runner_status.as_deref() == Some("generic_sparse_c")
        }));
        assert!(probe.decoder_catalog.iter().any(|item| {
            item.id.as_deref() == Some("pwm")
                && item.runner_status.as_deref() == Some("generic_sparse_python")
        }));
        assert!(probe.decoder_catalog.iter().any(|item| {
            item.id.as_deref() == Some("modbus_c")
                && item.runner_status.as_deref() == Some("catalog_only")
        }));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR, PXLOGIC_SIGROK_DECODER_ROOT, and PXLOGIC_SIGROK_PYTHON_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_generic_sparse_python_pwm() {
        let capture = sparse_transition_capture(
            "sigrok-python-pwm-test",
            1_000_000,
            100,
            0,
            &[0],
            &[(0, vec![10, 20, 30, 40, 50, 60])],
        );
        let settings = AnalyzerDecodeSettings::Native(NativeDecodeSettings {
            decoder_id: "python:pwm".to_string(),
            protocol_name: "PWM".to_string(),
            channels: BTreeMap::from([("data".to_string(), Some(0))]),
            options: BTreeMap::from([(
                "polarity".to_string(),
                NativeOptionValue::Text("active-high".to_string()),
            )]),
            primary_channel: 0,
        });

        let output = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(&capture, &settings)
            .unwrap();
        assert_eq!(output.protocol, "Pulse-width modulation");
        assert_eq!(
            output
                .frames
                .iter()
                .filter(|frame| frame.row == "Duty cycle")
                .map(|frame| frame.texts[0].as_str())
                .collect::<Vec<_>>(),
            vec!["50.000000%", "50.000000%"]
        );
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_sparse_fixture_beyond_u32() {
        let (capture, settings) = sparse_mode_zero_capture_and_settings();
        let output = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(&capture, &settings)
            .unwrap();
        assert_eq!(output.backend, DecoderBackendKind::SigrokNative);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
        assert_mode_zero_output(&output);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_uart_and_i2c_fixtures() {
        let (uart_capture, uart_settings) = uart_capture_and_settings();
        let uart = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode(&uart_capture, &uart_settings)
            .unwrap();
        assert_eq!(uart.protocol, "UART");
        assert_eq!(uart.frames.len(), 1);
        assert_eq!(uart.frames[0].channel_values[0].value, 0xA5);

        let (i2c_capture, i2c_settings) = i2c_capture_and_settings();
        let i2c = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode(&i2c_capture, &i2c_settings)
            .unwrap();
        assert_eq!(i2c.protocol, "I2C");
        assert_eq!(
            i2c.frames
                .iter()
                .map(|frame| frame.channel_values[0].value)
                .collect::<Vec<_>>(),
            vec![0x50, 0xA5]
        );
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_uart_and_i2c_sparse_fixtures_preserve_u64_indexes() {
        const OFFSET: u64 = 49_000_000_000;
        let uart_capture = sparse_transition_capture(
            "sigrok-uart-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1],
            &[(0, vec![10, 20, 30, 40, 50, 70, 80, 90])],
        );
        let uart = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(
                &uart_capture,
                &AnalyzerDecodeSettings::Uart(UartDecodeSettings {
                    channel: 0,
                    baud_rate: 100_000,
                    inverted: false,
                    ..UartDecodeSettings::default()
                }),
            )
            .unwrap();
        assert_eq!(uart.frames[0].channel_values[0].value, 0xA5);
        assert!(uart.frames[0].start_sample > u64::from(u32::MAX));

        let i2c_capture = sparse_transition_capture(
            "sigrok-i2c-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1, 1],
            &i2c_edge_sets(),
        );
        let i2c = SigrokNativeDecoder::from_env()
            .unwrap()
            .decode_sparse(
                &i2c_capture,
                &AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
                    scl_channel: 0,
                    sda_channel: 1,
                }),
            )
            .unwrap();
        assert_eq!(i2c.frames[0].channel_values[0].value, 0x50);
        assert!(i2c.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_i2s_packed_and_sparse_fixtures() {
        let decoder = SigrokNativeDecoder::from_env().unwrap();
        let packed = decoder.decode(&i2s_capture(), &i2s_settings()).unwrap();
        assert_eq!(packed.protocol, "I2S");
        assert_i2s_output(&packed);

        const OFFSET: u64 = 49_000_000_000;
        let sparse = sparse_transition_capture(
            "sigrok-i2s-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[0, 0, 0],
            &i2s_edge_sets(),
        );
        let output = decoder.decode_sparse(&sparse, &i2s_settings()).unwrap();
        assert_i2s_output(&output);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_can_and_can_fd_packed_and_sparse_fixtures() {
        let decoder = SigrokNativeDecoder::from_env().unwrap();
        for (decoder_id, protocol_name) in [("can", "CAN"), ("can_fd", "CAN-FD")] {
            let settings = can_settings(decoder_id, protocol_name);
            let packed = decoder.decode(&can_capture(), &settings).unwrap();
            assert_eq!(packed.protocol, protocol_name);
            assert_can_output(&packed);

            const OFFSET: u64 = 49_000_000_000;
            let sparse = sparse_transition_capture(
                "sigrok-can-sparse-test",
                1_000_000,
                50_000_000_000,
                OFFSET,
                &[1],
                &can_edge_sets(),
            );
            let output = decoder.decode_sparse(&sparse, &settings).unwrap();
            assert_can_output(&output);
            assert!(output.frames[0].start_sample > u64::from(u32::MAX));
        }
    }

    #[test]
    #[ignore = "requires PXLOGIC_SIGROK_SIDECAR and PXLOGIC_SIGROK_DECODER_ROOT"]
    fn sigrok_native_sidecar_decodes_lin_packed_and_sparse_fixtures() {
        let decoder = SigrokNativeDecoder::from_env().unwrap();
        let packed = decoder.decode(&lin_capture(), &lin_settings()).unwrap();
        assert_lin_output(&packed);

        const OFFSET: u64 = 49_000_000_000;
        let sparse = sparse_transition_capture(
            "sigrok-lin-sparse-test",
            1_000_000,
            50_000_000_000,
            OFFSET,
            &[1],
            &lin_edge_sets(),
        );
        let output = decoder.decode_sparse(&sparse, &lin_settings()).unwrap();
        assert_lin_output(&output);
        assert!(output.frames[0].start_sample > u64::from(u32::MAX));
    }
}
