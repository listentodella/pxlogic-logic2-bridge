use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use chrono::Utc;
use pxlogic_core::{
    capture::read_sample_word, resolve_enabled_channels, AnalyzerDecodeSettings, CaptureData,
    CaptureMetadata, DecoderBackendKind, I2cDecodeSettings, NativeDecodeSettings,
    NativeOptionValue, SparseCaptureData, SparseDigitalChannel, SpiDecodeSettings,
    UartDecodeSettings, UartParity, UartStopBits,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

pub type Result<T> = std::result::Result<T, FileError>;

#[derive(Debug, Error)]
pub enum FileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("capture has invalid sample metadata")]
    InvalidCapture,
    #[error("invalid Saleae capture: {0}")]
    InvalidSaleae(String),
}

#[derive(Debug, Deserialize)]
struct SaleaeTransitionManifest {
    format: String,
    source: String,
    sample_rate_hz: u64,
    channel_count: u8,
    duration_seconds: f64,
    sample_count: u64,
    channels: Vec<SaleaeTransitionChannel>,
    #[serde(default)]
    session: SaleaeTransitionSession,
}

#[derive(Debug, Deserialize)]
struct SaleaeTransitionChannel {
    channel: u8,
    file: String,
}

#[derive(Debug, Default, Deserialize)]
struct SaleaeTransitionSession {
    #[serde(default)]
    name: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    analyzers: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaleaeSessionMetadata {
    pub name: String,
    pub notes: String,
    pub analyzers: Vec<SaleaeAnalyzerMetadata>,
}

impl Default for SaleaeSessionMetadata {
    fn default() -> Self {
        Self {
            name: "PXLogic Capture".to_string(),
            notes: String::new(),
            analyzers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaleaeAnalyzerMetadata {
    #[serde(rename = "type")]
    pub analyzer_type: String,
    pub name: String,
    pub color: String,
    pub display_radix: String,
    pub show_in_data_table: bool,
    pub stream_to_terminal: bool,
    pub decoder_backend: DecoderBackendKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_settings: Option<AnalyzerDecodeSettings>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub saleae_metadata: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SaleaeImportedCapture {
    pub capture: SparseCaptureData,
    pub session: SaleaeSessionMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PxlogicProjectCapture {
    Packed(CaptureData),
    Sparse(SparseCaptureData),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PxlogicProject {
    pub capture: PxlogicProjectCapture,
    pub session: SaleaeSessionMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
struct PxlogicProjectManifest {
    format: String,
    session: SaleaeSessionMetadata,
    capture: PxlogicProjectCaptureManifest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PxlogicProjectCaptureManifest {
    Packed {
        metadata: CaptureMetadata,
        samples_file: String,
    },
    Sparse {
        metadata: CaptureMetadata,
        channels: Vec<PxlogicProjectSparseChannel>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct PxlogicProjectSparseChannel {
    channel: u8,
    initial_high: bool,
    transitions_file: String,
}

#[derive(Debug, Serialize)]
struct SaleaeExportManifest {
    format: &'static str,
    name: String,
    source_device: String,
    sample_rate_hz: u64,
    channel_count: u8,
    sample_count: u64,
    captured_at_unix_milliseconds: i64,
    channels: Vec<SaleaeExportChannel>,
    session: SaleaeSessionMetadata,
}

#[derive(Debug, Serialize)]
struct SaleaeExportChannel {
    channel: u8,
    label: String,
    initial_high: bool,
    transitions_file: String,
}

pub fn write_saleae_packed_export_manifest(
    directory: impl AsRef<Path>,
    capture: &CaptureData,
) -> Result<PathBuf> {
    write_saleae_packed_export_manifest_with_session(
        directory,
        capture,
        &SaleaeSessionMetadata::default(),
    )
}

pub fn write_saleae_packed_export_manifest_with_session(
    directory: impl AsRef<Path>,
    capture: &CaptureData,
    session: &SaleaeSessionMetadata,
) -> Result<PathBuf> {
    validate_saleae_export_metadata(&capture.metadata)?;
    let channels = export_channels(&capture.metadata)?;
    let initial_word = read_sample_word(&capture.samples, capture.metadata.unitsize, 0)
        .ok_or(FileError::InvalidCapture)?;
    let directory = directory.as_ref();
    fs::create_dir_all(directory)?;

    struct ChannelWriter {
        channel: u8,
        initial_high: bool,
        file_name: String,
        writer: BufWriter<File>,
    }

    let mut writers = channels
        .iter()
        .map(|&channel| {
            let file_name = format!("transitions-{channel}.u64");
            Ok(ChannelWriter {
                channel,
                initial_high: initial_word & (1u32 << channel) != 0,
                writer: BufWriter::new(File::create(directory.join(&file_name))?),
                file_name,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut previous_word = initial_word;
    for sample in 1..capture.metadata.sample_count {
        let word = read_sample_word(&capture.samples, capture.metadata.unitsize, sample)
            .ok_or(FileError::InvalidCapture)?;
        let changed = previous_word ^ word;
        if changed != 0 {
            for channel in &mut writers {
                if changed & (1u32 << channel.channel) != 0 {
                    channel.writer.write_all(&sample.to_le_bytes())?;
                }
            }
        }
        previous_word = word;
    }

    let manifest_channels = writers
        .into_iter()
        .map(|mut writer| {
            writer.writer.flush()?;
            Ok(SaleaeExportChannel {
                channel: writer.channel,
                label: capture
                    .metadata
                    .labels
                    .get(usize::from(writer.channel))
                    .cloned()
                    .unwrap_or_else(|| format!("D{}", writer.channel)),
                initial_high: writer.initial_high,
                transitions_file: writer.file_name,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    write_saleae_export_manifest(directory, &capture.metadata, manifest_channels, session)
}

pub fn write_saleae_sparse_export_manifest(
    directory: impl AsRef<Path>,
    capture: &SparseCaptureData,
) -> Result<PathBuf> {
    write_saleae_sparse_export_manifest_with_session(
        directory,
        capture,
        &SaleaeSessionMetadata::default(),
    )
}

pub fn write_saleae_sparse_export_manifest_with_session(
    directory: impl AsRef<Path>,
    capture: &SparseCaptureData,
    session: &SaleaeSessionMetadata,
) -> Result<PathBuf> {
    validate_saleae_export_metadata(&capture.metadata)?;
    let enabled_channels = export_channels(&capture.metadata)?;
    let mut channels = capture.channels.iter().collect::<Vec<_>>();
    channels.sort_by_key(|channel| channel.channel);
    if channels.len() != enabled_channels.len()
        || channels
            .iter()
            .map(|channel| channel.channel)
            .ne(enabled_channels.iter().copied())
    {
        return Err(FileError::InvalidSaleae(
            "sparse channels do not match enabled capture channels".to_string(),
        ));
    }

    let directory = directory.as_ref();
    fs::create_dir_all(directory)?;
    let mut manifest_channels = Vec::with_capacity(channels.len());
    for channel in channels {
        if channel.channel >= capture.metadata.channel_count
            || channel
                .transitions
                .iter()
                .any(|&sample| sample >= capture.metadata.sample_count)
            || channel
                .transitions
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(FileError::InvalidSaleae(format!(
                "D{} contains invalid sparse transitions",
                channel.channel
            )));
        }
        let file_name = format!("transitions-{}.u64", channel.channel);
        let mut writer = BufWriter::new(File::create(directory.join(&file_name))?);
        for &sample in &channel.transitions {
            writer.write_all(&sample.to_le_bytes())?;
        }
        writer.flush()?;
        manifest_channels.push(SaleaeExportChannel {
            channel: channel.channel,
            label: capture
                .metadata
                .labels
                .get(usize::from(channel.channel))
                .cloned()
                .unwrap_or_else(|| format!("D{}", channel.channel)),
            initial_high: channel.initial_high,
            transitions_file: file_name,
        });
    }
    write_saleae_export_manifest(directory, &capture.metadata, manifest_channels, session)
}

fn validate_saleae_export_metadata(metadata: &CaptureMetadata) -> Result<()> {
    let expected_unitsize = match metadata.channel_count {
        1..=8 => 1,
        9..=16 => 2,
        17..=32 => 4,
        _ => return Err(FileError::InvalidCapture),
    };
    if !(1..=32).contains(&metadata.channel_count)
        || metadata.sample_rate_hz == 0
        || metadata.sample_count == 0
        || metadata.unitsize != expected_unitsize
    {
        return Err(FileError::InvalidCapture);
    }
    Ok(())
}

fn write_saleae_export_manifest(
    directory: &Path,
    metadata: &CaptureMetadata,
    channels: Vec<SaleaeExportChannel>,
    session: &SaleaeSessionMetadata,
) -> Result<PathBuf> {
    if channels.is_empty() {
        return Err(FileError::InvalidCapture);
    }
    let manifest = SaleaeExportManifest {
        format: "pxlogic.saleae-export.v1",
        name: if session.name.trim().is_empty() {
            "PXLogic Capture".to_string()
        } else {
            session.name.clone()
        },
        source_device: metadata.source_device.clone(),
        sample_rate_hz: metadata.sample_rate_hz,
        channel_count: metadata.channel_count,
        sample_count: metadata.sample_count,
        captured_at_unix_milliseconds: metadata.captured_at.timestamp_millis(),
        channels,
        session: session.clone(),
    };
    let path = directory.join("manifest.json");
    serde_json::to_writer_pretty(File::create(&path)?, &manifest)?;
    Ok(path)
}

pub fn open_saleae_transition_manifest(path: impl AsRef<Path>) -> Result<SparseCaptureData> {
    Ok(open_saleae_transition_session(path)?.capture)
}

pub fn open_saleae_transition_session(path: impl AsRef<Path>) -> Result<SaleaeImportedCapture> {
    let path = path.as_ref();
    let manifest: SaleaeTransitionManifest = serde_json::from_reader(File::open(path)?)?;
    if manifest.format != "pxlogic.saleae-transition.v1"
        || !(1..=32).contains(&manifest.channel_count)
        || manifest.sample_rate_hz == 0
        || manifest.sample_count == 0
        || !manifest.duration_seconds.is_finite()
        || manifest.duration_seconds <= 0.0
    {
        return Err(FileError::InvalidSaleae(
            "transition manifest metadata is invalid".to_string(),
        ));
    }
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut channels = manifest
        .channels
        .iter()
        .map(|channel| {
            if channel.channel >= manifest.channel_count {
                return Err(FileError::InvalidSaleae(format!(
                    "D{} is outside the declared channel count",
                    channel.channel
                )));
            }
            read_saleae_digital_binary(
                directory.join(&channel.file),
                channel.channel,
                manifest.sample_rate_hz,
                manifest.sample_count,
                manifest.duration_seconds,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    channels.sort_by_key(|channel| channel.channel);
    if channels.is_empty()
        || channels
            .windows(2)
            .any(|pair| pair[0].channel == pair[1].channel)
    {
        return Err(FileError::InvalidSaleae(
            "transition manifest has no channels or duplicate channels".to_string(),
        ));
    }

    let enabled_channels = channels.iter().map(|channel| channel.channel).collect();
    let unitsize = match manifest.channel_count {
        1..=8 => 1,
        9..=16 => 2,
        17..=32 => 4,
        _ => unreachable!(),
    };
    let labels = (0..manifest.channel_count)
        .map(|channel| {
            manifest
                .session
                .labels
                .get(usize::from(channel))
                .filter(|label| !label.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("D{channel}"))
        })
        .collect();
    let metadata = CaptureMetadata {
        version: 1,
        source_device: format!("Saleae {}", manifest.source),
        sample_rate_hz: manifest.sample_rate_hz,
        channel_count: manifest.channel_count,
        enabled_channels,
        unitsize,
        sample_count: manifest.sample_count,
        captured_at: Utc::now(),
        labels,
        trigger: None,
    };
    let analyzers = manifest
        .session
        .analyzers
        .into_iter()
        .filter_map(normalize_saleae_analyzer)
        .collect();
    Ok(SaleaeImportedCapture {
        capture: SparseCaptureData { metadata, channels },
        session: SaleaeSessionMetadata {
            name: manifest.session.name,
            notes: manifest.session.notes,
            analyzers,
        },
    })
}

fn normalize_saleae_analyzer(raw: Value) -> Option<SaleaeAnalyzerMetadata> {
    let analyzer_type = raw.get("type")?.as_str()?.trim().to_string();
    if analyzer_type.is_empty() {
        return None;
    }
    let decoder_backend = match analyzer_type.as_str() {
        "SPI" | "Async Serial" | "Serial" | "I2C" | "I2S" | "I2S / PCM" | "CAN" | "LIN" => {
            DecoderBackendKind::SaleaeNative
        }
        _ => DecoderBackendKind::LegacyRust,
    };
    let decode_settings = normalize_saleae_decode_settings(&analyzer_type, &raw);
    Some(SaleaeAnalyzerMetadata {
        name: raw
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&analyzer_type)
            .to_string(),
        color: raw
            .get("color")
            .and_then(Value::as_str)
            .unwrap_or("#7EA0F8")
            .to_string(),
        display_radix: raw
            .get("displayRadix")
            .and_then(Value::as_str)
            .unwrap_or("Hexadecimal")
            .to_string(),
        show_in_data_table: raw
            .get("showInDataTable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        stream_to_terminal: raw
            .get("streamToTerminal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        analyzer_type,
        decoder_backend,
        decode_settings,
        saleae_metadata: raw,
    })
}

fn normalize_saleae_decode_settings(
    analyzer_type: &str,
    analyzer: &Value,
) -> Option<AnalyzerDecodeSettings> {
    match analyzer_type {
        "SPI" => {
            let mosi_channel = saleae_setting_u8(analyzer, &["MOSI"]);
            let miso_channel = saleae_setting_u8(analyzer, &["MISO"]);
            if mosi_channel.is_none() && miso_channel.is_none() {
                return None;
            }
            Some(AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
                mosi_channel,
                miso_channel,
                clock_channel: saleae_setting_u8(analyzer, &["Clock"])?,
                enable_channel: saleae_setting_u8(analyzer, &["Enable"]),
                enable_active_low: saleae_setting_u64(analyzer, &["Enable Line"]).unwrap_or(0) == 0,
                clock_polarity: saleae_setting_u64(analyzer, &["Clock State"]).unwrap_or(0) != 0,
                clock_phase: saleae_setting_u64(analyzer, &["Clock Phase"]).unwrap_or(0) != 0,
                bits_per_transfer: u8::try_from(
                    saleae_setting_u64(analyzer, &["Bits per Transfer"]).unwrap_or(8),
                )
                .ok()?,
                msb_first: saleae_setting_u64(analyzer, &["Significant Bit"]).unwrap_or(0) == 0,
            }))
        }
        "I2C" => Some(AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
            sda_channel: saleae_setting_u8(analyzer, &["Serial Data Line", "SDA"])?,
            scl_channel: saleae_setting_u8(analyzer, &["Serial Clock Line", "SCL"])?,
        })),
        "Async Serial" | "Serial" => {
            let parity = match saleae_setting_u64(analyzer, &["Parity Bit"]).unwrap_or(0) {
                1 => UartParity::Even,
                2 => UartParity::Odd,
                _ => UartParity::None,
            };
            Some(AnalyzerDecodeSettings::Uart(UartDecodeSettings {
                channel: saleae_setting_u8(analyzer, &["Input Channel"])?,
                baud_rate: u32::try_from(saleae_setting_u64(analyzer, &["Bit Rate (Bits/s)"])?)
                    .ok()?,
                inverted: saleae_setting_u64(analyzer, &["Signal inversion"]).unwrap_or(0) != 0,
                data_bits: u8::try_from(
                    saleae_setting_u64(analyzer, &["Bits per Frame"]).unwrap_or(8),
                )
                .ok()?,
                stop_bits: UartStopBits::from_f64(
                    saleae_setting_number(analyzer, &["Stop Bits"]).unwrap_or(1.0),
                )?,
                parity,
                msb_first: saleae_setting_u64(analyzer, &["Significant Bit"]).unwrap_or(0) != 0,
            }))
        }
        "I2S" | "I2S / PCM" => {
            let sck = saleae_setting_u8(analyzer, &["CLOCK channel", "PCM CLOCK", "SCK"])?;
            let ws = saleae_setting_u8(analyzer, &["FRAME", "PCM FRAME", "WS"])?;
            let sd = saleae_setting_u8(analyzer, &["DATA", "PCM DATA", "SD"])?;
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "i2s".to_string(),
                protocol_name: "I2S".to_string(),
                channels: BTreeMap::from([
                    ("sck".to_string(), Some(sck)),
                    ("ws".to_string(), Some(ws)),
                    ("sd".to_string(), Some(sd)),
                ]),
                options: BTreeMap::from(
                    [
                        (
                            "bit_order".to_string(),
                            NativeOptionValue::Text(
                                if saleae_setting_u64(analyzer, &["DATA Significant Bit"])
                                    .unwrap_or(0)
                                    == 0
                                {
                                    "msb-first"
                                } else {
                                    "lsb-first"
                                }
                                .to_string(),
                            ),
                        ),
                        (
                            "clk_edge".to_string(),
                            NativeOptionValue::Text(
                                if saleae_setting_u64(analyzer, &["CLOCK State"]).unwrap_or(1) == 0
                                {
                                    "rising-edge"
                                } else {
                                    "falling-edge"
                                }
                                .to_string(),
                            ),
                        ),
                        (
                            "word_size".to_string(),
                            NativeOptionValue::Integer(
                                saleae_setting_u64(analyzer, &["Audio Bit Depth (bits/sample)"])
                                    .unwrap_or(16) as i64,
                            ),
                        ),
                        (
                            "frame_transitions".to_string(),
                            NativeOptionValue::Text(
                                match saleae_setting_u64(analyzer, &["FRAME Signal Transitions"])
                                    .unwrap_or(1)
                                {
                                    0 => "twice-each-word",
                                    2 => "twice-every-4-words",
                                    _ => "once-each-word",
                                }
                                .to_string(),
                            ),
                        ),
                        (
                            "bit_align".to_string(),
                            NativeOptionValue::Text(
                                if saleae_setting_u64(analyzer, &["DATA Bits Alignment"])
                                    .unwrap_or(0)
                                    == 0
                                {
                                    "left-aligned"
                                } else {
                                    "right-aligned"
                                }
                                .to_string(),
                            ),
                        ),
                        (
                            "bit_shift".to_string(),
                            NativeOptionValue::Text(
                                if saleae_setting_u64(analyzer, &["DATA Bits Shift"]).unwrap_or(0)
                                    == 0
                                {
                                    "right-shifted by one"
                                } else {
                                    "none"
                                }
                                .to_string(),
                            ),
                        ),
                        (
                            "signed".to_string(),
                            NativeOptionValue::Boolean(
                                saleae_setting_u64(analyzer, &["Signed/Unsigned"]).unwrap_or(0)
                                    != 0,
                            ),
                        ),
                        (
                            "ws_polarity".to_string(),
                            NativeOptionValue::Text(
                                if saleae_setting_u64(analyzer, &["Word Select High"]).unwrap_or(1)
                                    == 1
                                {
                                    "right-high"
                                } else {
                                    "left-high"
                                }
                                .to_string(),
                            ),
                        ),
                    ],
                ),
                primary_channel: sck,
            }))
        }
        "CAN" => {
            let can_rx = saleae_setting_u8(analyzer, &["CAN"])?;
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "can".to_string(),
                protocol_name: "CAN".to_string(),
                channels: BTreeMap::from([("can_rx".to_string(), Some(can_rx))]),
                options: BTreeMap::from([
                    (
                        "bitrate".to_string(),
                        NativeOptionValue::Integer(
                            saleae_setting_u64(analyzer, &["Bit Rate (Bits/s)"])
                                .unwrap_or(1_000_000) as i64,
                        ),
                    ),
                    ("sample_point".to_string(), NativeOptionValue::Integer(70)),
                    (
                        "inverted".to_string(),
                        NativeOptionValue::Boolean(
                            saleae_setting_bool(analyzer, &[""]).unwrap_or(false),
                        ),
                    ),
                ]),
                primary_channel: can_rx,
            }))
        }
        "LIN" => {
            let rx = saleae_setting_u8(analyzer, &["Serial"])?;
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "lin".to_string(),
                protocol_name: "LIN".to_string(),
                channels: BTreeMap::from([("rx".to_string(), Some(rx))]),
                options: BTreeMap::from([
                    (
                        "baudrate".to_string(),
                        NativeOptionValue::Integer(
                            saleae_setting_u64(analyzer, &["Bit Rate (Bits/s)"]).unwrap_or(20_000)
                                as i64,
                        ),
                    ),
                    (
                        "version".to_string(),
                        NativeOptionValue::Integer(
                            saleae_setting_u64(analyzer, &["LIN Version"]).unwrap_or(2) as i64,
                        ),
                    ),
                ]),
                primary_channel: rx,
            }))
        }
        "Simple Parallel" | "Parallel" => {
            let mut channels = BTreeMap::new();
            let mut primary_channel = None;
            for bit in 0..16 {
                let title = format!("D{bit}");
                let channel = saleae_setting_u8(analyzer, &[title.as_str()]);
                if primary_channel.is_none() {
                    primary_channel = channel;
                }
                channels.insert(format!("d{bit}"), channel);
            }
            let clock = saleae_setting_u8(analyzer, &["Clock"])?;
            let primary_channel = primary_channel?;
            channels.insert("clk".to_string(), Some(clock));
            let clock_state = saleae_setting_u64(analyzer, &["Clock State"]).unwrap_or(0);
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "parallel".to_string(),
                protocol_name: "Parallel".to_string(),
                channels,
                options: BTreeMap::from([
                    (
                        "clock_edge".to_string(),
                        NativeOptionValue::Text(
                            match clock_state {
                                1 => "falling",
                                2 => "dual",
                                _ => "rising",
                            }
                            .to_string(),
                        ),
                    ),
                    (
                        "clock_state".to_string(),
                        NativeOptionValue::Integer(clock_state as i64),
                    ),
                    ("word_size".to_string(), NativeOptionValue::Integer(0)),
                    (
                        "endianness".to_string(),
                        NativeOptionValue::Text("little".to_string()),
                    ),
                ]),
                primary_channel,
            }))
        }
        _ => None,
    }
}

fn saleae_setting_value<'a>(analyzer: &'a Value, titles: &[&str]) -> Option<&'a Value> {
    analyzer
        .get("settings")?
        .as_array()?
        .iter()
        .find_map(|entry| {
            let title = entry.get("title")?.as_str()?;
            titles
                .iter()
                .any(|candidate| title.eq_ignore_ascii_case(candidate))
                .then(|| entry.get("setting")?.get("value"))?
        })
}

fn saleae_setting_number(analyzer: &Value, titles: &[&str]) -> Option<f64> {
    saleae_setting_value(analyzer, titles)?.as_f64()
}

fn saleae_setting_u64(analyzer: &Value, titles: &[&str]) -> Option<u64> {
    saleae_setting_value(analyzer, titles)?.as_u64()
}

fn saleae_setting_bool(analyzer: &Value, titles: &[&str]) -> Option<bool> {
    let value = saleae_setting_value(analyzer, titles)?;
    value
        .as_bool()
        .or_else(|| value.as_u64().map(|value| value != 0))
}

fn saleae_setting_u8(analyzer: &Value, titles: &[&str]) -> Option<u8> {
    u8::try_from(saleae_setting_u64(analyzer, titles)?).ok()
}

fn read_saleae_digital_binary(
    path: impl AsRef<Path>,
    channel: u8,
    sample_rate_hz: u64,
    sample_count: u64,
    manifest_duration: f64,
) -> Result<SparseDigitalChannel> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header)?;
    if &header[..8] != b"<SALEAE>"
        || u32::from_le_bytes(header[8..12].try_into().expect("header width")) != 0
        || u32::from_le_bytes(header[12..16].try_into().expect("header width")) != 0
    {
        return Err(FileError::InvalidSaleae(format!(
            "{} is not an initial-version digital binary",
            path.display()
        )));
    }
    let initial_high = u32::from_le_bytes(header[16..20].try_into().expect("header width")) != 0;
    let duration_seconds = f64::from_le_bytes(header[28..36].try_into().expect("header width"));
    let transition_count = u64::from_le_bytes(header[36..44].try_into().expect("header width"));
    if !duration_seconds.is_finite()
        || duration_seconds <= 0.0
        || (duration_seconds - manifest_duration).abs() > (1.0 / sample_rate_hz as f64).max(1e-9)
    {
        return Err(FileError::InvalidSaleae(format!(
            "{} duration does not match the manifest",
            path.display()
        )));
    }
    let transition_bytes = transition_count
        .checked_mul(8)
        .ok_or_else(|| FileError::InvalidSaleae("transition byte count overflowed".to_string()))?;
    let expected_size = 44u64
        .checked_add(transition_bytes)
        .ok_or_else(|| FileError::InvalidSaleae("digital binary size overflowed".to_string()))?;
    if file.seek(SeekFrom::End(0))? != expected_size {
        return Err(FileError::InvalidSaleae(format!(
            "{} has an invalid transition payload length",
            path.display()
        )));
    }
    file.seek(SeekFrom::Start(44))?;
    let capacity = usize::try_from(transition_count).map_err(|_| {
        FileError::InvalidSaleae("transition count exceeds this platform".to_string())
    })?;
    let mut transitions = Vec::with_capacity(capacity);
    let mut time_bytes = [0u8; 8];
    for _ in 0..transition_count {
        file.read_exact(&mut time_bytes)?;
        let time = f64::from_le_bytes(time_bytes);
        if !time.is_finite() || time < 0.0 || time > duration_seconds {
            return Err(FileError::InvalidSaleae(format!(
                "{} contains an invalid transition time",
                path.display()
            )));
        }
        let sample = (time * sample_rate_hz as f64).round() as u64;
        if sample >= sample_count
            || transitions
                .last()
                .is_some_and(|previous| *previous >= sample)
        {
            return Err(FileError::InvalidSaleae(format!(
                "{} transitions are not strictly increasing sample indexes",
                path.display()
            )));
        }
        transitions.push(sample);
    }
    Ok(SparseDigitalChannel {
        channel,
        initial_high,
        transitions,
    })
}

pub fn save_pxcap(path: impl AsRef<Path>, capture: &CaptureData) -> Result<()> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("metadata.json", options)?;
    zip.write_all(&serde_json::to_vec_pretty(&capture.metadata)?)?;
    zip.start_file("samples.bin", options)?;
    zip.write_all(&capture.samples)?;
    zip.finish()?;
    Ok(())
}

pub fn open_pxcap(path: impl AsRef<Path>) -> Result<CaptureData> {
    let file = File::open(path)?;
    let mut zip = ZipArchive::new(file)?;

    let mut metadata_json = String::new();
    zip.by_name("metadata.json")?
        .read_to_string(&mut metadata_json)?;
    let mut metadata: CaptureMetadata = serde_json::from_str(&metadata_json)?;
    metadata.enabled_channels =
        resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
            .map_err(|_| FileError::InvalidCapture)?;

    let mut samples = Vec::new();
    zip.by_name("samples.bin")?.read_to_end(&mut samples)?;

    Ok(CaptureData { metadata, samples })
}

pub fn save_pxl_packed(
    path: impl AsRef<Path>,
    capture: &CaptureData,
    session: &SaleaeSessionMetadata,
) -> Result<()> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let manifest = PxlogicProjectManifest {
        format: "pxlogic.project.v1".to_string(),
        session: session.clone(),
        capture: PxlogicProjectCaptureManifest::Packed {
            metadata: capture.metadata.clone(),
            samples_file: "capture/samples.bin".to_string(),
        },
    };

    zip.start_file("project.json", options)?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    zip.start_file("capture/samples.bin", options)?;
    zip.write_all(&capture.samples)?;
    zip.finish()?;
    Ok(())
}

pub fn save_pxl_sparse(
    path: impl AsRef<Path>,
    capture: &SparseCaptureData,
    session: &SaleaeSessionMetadata,
) -> Result<()> {
    validate_sparse_project_capture(capture)?;
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut channels = capture.channels.iter().collect::<Vec<_>>();
    channels.sort_by_key(|channel| channel.channel);
    let manifest_channels = channels
        .iter()
        .map(|channel| PxlogicProjectSparseChannel {
            channel: channel.channel,
            initial_high: channel.initial_high,
            transitions_file: format!("capture/transitions-{}.u64", channel.channel),
        })
        .collect::<Vec<_>>();
    let manifest = PxlogicProjectManifest {
        format: "pxlogic.project.v1".to_string(),
        session: session.clone(),
        capture: PxlogicProjectCaptureManifest::Sparse {
            metadata: capture.metadata.clone(),
            channels: manifest_channels,
        },
    };

    zip.start_file("project.json", options)?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    for channel in channels {
        zip.start_file(
            format!("capture/transitions-{}.u64", channel.channel),
            options,
        )?;
        for &sample in &channel.transitions {
            zip.write_all(&sample.to_le_bytes())?;
        }
    }
    zip.finish()?;
    Ok(())
}

pub fn open_pxl(path: impl AsRef<Path>) -> Result<PxlogicProject> {
    let file = File::open(path)?;
    let mut zip = ZipArchive::new(file)?;
    let mut manifest_json = String::new();
    zip.by_name("project.json")?
        .read_to_string(&mut manifest_json)?;
    let manifest: PxlogicProjectManifest = serde_json::from_str(&manifest_json)?;
    if manifest.format != "pxlogic.project.v1" {
        return Err(FileError::InvalidCapture);
    }

    let capture = match manifest.capture {
        PxlogicProjectCaptureManifest::Packed {
            mut metadata,
            samples_file,
        } => {
            validate_project_file_name(&samples_file)?;
            metadata.enabled_channels =
                resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
                    .map_err(|_| FileError::InvalidCapture)?;
            let mut samples = Vec::new();
            zip.by_name(&samples_file)?.read_to_end(&mut samples)?;
            PxlogicProjectCapture::Packed(CaptureData { metadata, samples })
        }
        PxlogicProjectCaptureManifest::Sparse {
            mut metadata,
            channels,
        } => {
            metadata.enabled_channels =
                resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
                    .map_err(|_| FileError::InvalidCapture)?;
            let mut restored_channels = channels
                .into_iter()
                .map(|channel| {
                    validate_project_file_name(&channel.transitions_file)?;
                    let transitions =
                        read_project_transition_file(&mut zip, &channel.transitions_file)?;
                    Ok(SparseDigitalChannel {
                        channel: channel.channel,
                        initial_high: channel.initial_high,
                        transitions,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            restored_channels.sort_by_key(|channel| channel.channel);
            let capture = SparseCaptureData {
                metadata,
                channels: restored_channels,
            };
            validate_sparse_project_capture(&capture)?;
            PxlogicProjectCapture::Sparse(capture)
        }
    };

    Ok(PxlogicProject {
        capture,
        session: manifest.session,
    })
}

fn validate_project_file_name(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(FileError::InvalidCapture);
    }
    Ok(())
}

fn read_project_transition_file(zip: &mut ZipArchive<File>, path: &str) -> Result<Vec<u64>> {
    let mut bytes = Vec::new();
    zip.by_name(path)?.read_to_end(&mut bytes)?;
    if bytes.len() % 8 != 0 {
        return Err(FileError::InvalidCapture);
    }
    Ok(bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("transition width")))
        .collect())
}

fn validate_sparse_project_capture(capture: &SparseCaptureData) -> Result<()> {
    validate_saleae_export_metadata(&capture.metadata)?;
    let enabled_channels = export_channels(&capture.metadata)?;
    let mut channels = capture.channels.iter().collect::<Vec<_>>();
    channels.sort_by_key(|channel| channel.channel);
    if channels.is_empty()
        || channels.len() != enabled_channels.len()
        || channels
            .iter()
            .map(|channel| channel.channel)
            .ne(enabled_channels.iter().copied())
        || channels
            .windows(2)
            .any(|pair| pair[0].channel == pair[1].channel)
    {
        return Err(FileError::InvalidCapture);
    }
    for channel in channels {
        if channel.channel >= capture.metadata.channel_count
            || channel
                .transitions
                .iter()
                .any(|sample| *sample >= capture.metadata.sample_count)
            || channel
                .transitions
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(FileError::InvalidCapture);
        }
    }
    Ok(())
}

pub fn export_sr(path: impl AsRef<Path>, capture: &CaptureData) -> Result<()> {
    let enabled_channels = export_channels(&capture.metadata)?;
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("version", options)?;
    zip.write_all(b"2")?;
    zip.start_file("metadata", options)?;
    zip.write_all(
        render_sr_metadata_for_channels(&capture.metadata, &enabled_channels).as_bytes(),
    )?;
    zip.start_file("logic-1", options)?;
    zip.write_all(&capture.samples)?;
    zip.finish()?;
    Ok(())
}

pub fn render_sr_metadata(metadata: &CaptureMetadata) -> String {
    let enabled_channels =
        resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
            .unwrap_or_else(|_| (0..metadata.channel_count).collect());
    render_sr_metadata_for_channels(metadata, &enabled_channels)
}

fn render_sr_metadata_for_channels(metadata: &CaptureMetadata, enabled_channels: &[u8]) -> String {
    let mut output = String::new();
    output.push_str("[global]\n");
    output.push_str("sigrok version=0.5.2\n\n");
    output.push_str("[device 1]\n");
    output.push_str("capturefile=logic-1\n");
    output.push_str(&format!("total probes={}\n", metadata.channel_count));
    output.push_str(&format!("samplerate={}\n", metadata.sample_rate_hz));
    output.push_str(&format!("unitsize={}\n", metadata.unitsize));
    // PXView's srzip.c keeps the physical probe count, but only emits probe
    // entries for enabled channels. Probe numbers remain physical indices.
    for &index in enabled_channels {
        let label = metadata
            .labels
            .get(index as usize)
            .cloned()
            .unwrap_or_else(|| format!("D{index}"));
        output.push_str(&format!("probe{}={}\n", index + 1, label));
    }
    output
}

pub fn export_vcd(path: impl AsRef<Path>, capture: &CaptureData) -> Result<()> {
    let mut file = File::create(path)?;
    let metadata = &capture.metadata;
    if metadata.unitsize == 0 {
        return Err(FileError::InvalidCapture);
    }
    let enabled_channels = export_channels(metadata)?;

    writeln!(file, "$date")?;
    writeln!(file, "  {}", metadata.captured_at)?;
    writeln!(file, "$end")?;
    writeln!(file, "$version PXLogic Studio 0.1 $end")?;
    writeln!(file, "$timescale 1 ns $end")?;
    writeln!(file, "$scope module logic $end")?;
    for &channel in &enabled_channels {
        let id = vcd_identifier(channel);
        let label = metadata
            .labels
            .get(channel as usize)
            .cloned()
            .unwrap_or_else(|| format!("D{channel}"));
        writeln!(file, "$var wire 1 {id} {label} $end")?;
    }
    writeln!(file, "$upscope $end")?;
    writeln!(file, "$enddefinitions $end")?;
    writeln!(file, "#0")?;

    let mut previous = u32::MAX;
    for sample in 0..metadata.sample_count {
        let word = read_sample_word(&capture.samples, metadata.unitsize, sample)
            .ok_or(FileError::InvalidCapture)?;
        if sample == 0 {
            dump_vcd_word(&mut file, &enabled_channels, word, 0)?;
        } else {
            let changed = previous ^ word;
            if changed != 0 {
                let timestamp_ns =
                    sample.saturating_mul(1_000_000_000) / metadata.sample_rate_hz.max(1);
                writeln!(file, "#{timestamp_ns}")?;
                dump_vcd_word(&mut file, &enabled_channels, word, changed)?;
            }
        }
        previous = word;
    }
    Ok(())
}

fn export_channels(metadata: &CaptureMetadata) -> Result<Vec<u8>> {
    resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
        .map_err(|_| FileError::InvalidCapture)
}

fn dump_vcd_word(
    file: &mut File,
    enabled_channels: &[u8],
    word: u32,
    changed_mask: u32,
) -> Result<()> {
    for &channel in enabled_channels {
        let bit = 1u32 << channel;
        if changed_mask != 0 && changed_mask & bit == 0 {
            continue;
        }
        let value = if word & bit == 0 { '0' } else { '1' };
        writeln!(file, "{value}{}", vcd_identifier(channel))?;
    }
    Ok(())
}

fn vcd_identifier(channel: u8) -> String {
    let mut n = channel as usize;
    let mut chars = Vec::new();
    loop {
        chars.push((33 + (n % 94)) as u8 as char);
        n /= 94;
        if n == 0 {
            break;
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use pxlogic_core::{
        capture::generate_sample_words, CaptureSettings, CaptureTriggerKind, CaptureTriggerMetadata,
    };

    use super::*;

    #[test]
    fn pxcap_round_trips_capture() {
        let mut capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        capture.metadata.trigger = Some(CaptureTriggerMetadata {
            sample_index: 50,
            channel: 0,
            kind: CaptureTriggerKind::Rising,
        });
        let path = temp_file("roundtrip.pxcap");
        save_pxcap(&path, &capture).unwrap();
        let opened = open_pxcap(&path).unwrap();
        assert_eq!(opened.metadata.sample_count, capture.metadata.sample_count);
        assert_eq!(
            opened.metadata.enabled_channels,
            capture.metadata.enabled_channels
        );
        assert_eq!(opened.metadata.trigger, capture.metadata.trigger);
        assert_eq!(opened.samples, capture.samples);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pxl_round_trips_packed_capture_and_analyzers() {
        let mut capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        capture.metadata.trigger = Some(CaptureTriggerMetadata {
            sample_index: 50,
            channel: 0,
            kind: CaptureTriggerKind::Rising,
        });
        let session = project_session();
        let path = temp_file("roundtrip.pxl");

        save_pxl_packed(&path, &capture, &session).unwrap();
        let opened = open_pxl(&path).unwrap();

        assert_eq!(opened.session, session);
        match opened.capture {
            PxlogicProjectCapture::Packed(opened_capture) => {
                assert_eq!(opened_capture.metadata, capture.metadata);
                assert_eq!(opened_capture.samples, capture.samples);
            }
            PxlogicProjectCapture::Sparse(_) => panic!("packed project opened as sparse"),
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pxl_round_trips_sparse_capture_and_sigrok_metadata() {
        let mut metadata = sparse_capture().metadata;
        metadata.sample_count = 4;
        let capture = SparseCaptureData {
            metadata,
            channels: vec![
                SparseDigitalChannel {
                    channel: 4,
                    initial_high: true,
                    transitions: vec![2],
                },
                SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: vec![1, 3],
                },
            ],
        };
        let session = project_session();
        let path = temp_file("sparse-roundtrip.pxl");

        save_pxl_sparse(&path, &capture, &session).unwrap();
        let opened = open_pxl(&path).unwrap();

        assert_eq!(opened.session, session);
        match opened.capture {
            PxlogicProjectCapture::Sparse(opened_capture) => {
                assert_eq!(opened_capture.metadata, capture.metadata);
                assert_eq!(opened_capture.channels.len(), 2);
                assert_eq!(opened_capture.channels[0].channel, 0);
                assert_eq!(opened_capture.channels[0].transitions, vec![1, 3]);
                assert_eq!(opened_capture.channels[1].channel, 4);
                assert_eq!(opened_capture.channels[1].initial_high, true);
            }
            PxlogicProjectCapture::Packed(_) => panic!("sparse project opened as packed"),
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn old_metadata_without_trigger_remains_readable() {
        let metadata: CaptureMetadata = serde_json::from_str(
            r#"{
                "version": 1,
                "source_device": "legacy",
                "sample_rate_hz": 1000000,
                "channel_count": 1,
                "unitsize": 1,
                "sample_count": 4,
                "captured_at": "2026-07-23T00:00:00Z",
                "labels": ["D0"]
            }"#,
        )
        .unwrap();
        assert_eq!(metadata.trigger, None);
        assert!(metadata.enabled_channels.is_empty());
    }

    #[test]
    fn open_pxcap_restores_legacy_contiguous_enabled_channels() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();
        let path = temp_file("legacy-enabled-channels.pxcap");
        save_pxcap(&path, &capture).unwrap();

        let file = File::open(&path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut metadata_json = String::new();
        archive
            .by_name("metadata.json")
            .unwrap()
            .read_to_string(&mut metadata_json)
            .unwrap();
        drop(archive);
        let mut metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        metadata.as_object_mut().unwrap().remove("enabled_channels");

        let legacy_path = temp_file("legacy-enabled-channels-rewritten.pxcap");
        let legacy_file = File::create(&legacy_path).unwrap();
        let mut zip = ZipWriter::new(legacy_file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("metadata.json", options).unwrap();
        zip.write_all(&serde_json::to_vec_pretty(&metadata).unwrap())
            .unwrap();
        zip.start_file("samples.bin", options).unwrap();
        zip.write_all(&capture.samples).unwrap();
        zip.finish().unwrap();

        let opened = open_pxcap(&legacy_path).unwrap();
        assert_eq!(opened.metadata.enabled_channels, vec![0, 1, 2, 3]);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(legacy_path);
    }

    #[test]
    fn sr_metadata_contains_sigrok_fields() {
        let capture = generate_sample_words(&CaptureSettings {
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        let rendered = render_sr_metadata(&capture.metadata);
        assert!(rendered.contains("capturefile=logic-1"));
        assert!(rendered.contains("probe1=D0"));
        assert!(rendered.contains("unitsize=2"));
    }

    #[test]
    fn sparse_sr_export_uses_physical_probe_numbers_without_inventing_channels() {
        let capture = sparse_capture();
        let path = temp_file("sparse.sr");
        export_sr(&path, &capture).unwrap();

        let file = File::open(&path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut metadata = String::new();
        archive
            .by_name("metadata")
            .unwrap()
            .read_to_string(&mut metadata)
            .unwrap();
        assert!(metadata.contains("total probes=5"));
        assert!(metadata.contains("probe1=D0"));
        assert!(metadata.contains("probe5=D4"));
        assert!(!metadata.lines().any(|line| line.starts_with("probe2=")));

        let mut samples = Vec::new();
        archive
            .by_name("logic-1")
            .unwrap()
            .read_to_end(&mut samples)
            .unwrap();
        assert_eq!(samples, capture.samples);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn exports_vcd_file() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();
        let path = temp_file("capture.vcd");
        export_vcd(&path, &capture).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("$timescale 1 ns $end"));
        assert!(text.contains("$var wire 1 ! D0 $end"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sparse_vcd_export_declares_and_updates_only_enabled_physical_channels() {
        let capture = sparse_capture();
        let path = temp_file("sparse.vcd");
        export_vcd(&path, &capture).unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert!(text.contains("$var wire 1 ! D0 $end"));
        assert!(text.contains("$var wire 1 % D4 $end"));
        assert!(!text.contains("$var wire 1 \" D1 $end"));
        assert!(text.contains("#0\n0!\n1%"));
        assert!(text.contains("#100\n1!\n0%"));
        assert!(text.contains("#200\n1%"));
        let _ = fs::remove_file(path);
    }

    fn sparse_capture() -> CaptureData {
        let mut capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000_000,
            duration_ms: 1,
            channel_count: 5,
            enabled_channels: vec![0, 4],
            ..CaptureSettings::default()
        })
        .unwrap();
        capture.metadata.sample_count = 3;
        capture.samples = vec![0x10, 0x01, 0x11];
        capture
    }

    #[test]
    fn opens_saleae_transition_manifest_without_expanding_samples() {
        let directory =
            std::env::temp_dir().join(format!("pxlogic-saleae-manifest-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let binary_path = directory.join("digital_3.bin");
        let mut binary = Vec::new();
        binary.extend_from_slice(b"<SALEAE>");
        binary.extend_from_slice(&0u32.to_le_bytes());
        binary.extend_from_slice(&0u32.to_le_bytes());
        binary.extend_from_slice(&1u32.to_le_bytes());
        binary.extend_from_slice(&0.0f64.to_le_bytes());
        binary.extend_from_slice(&0.001f64.to_le_bytes());
        binary.extend_from_slice(&2u64.to_le_bytes());
        binary.extend_from_slice(&0.0001f64.to_le_bytes());
        binary.extend_from_slice(&0.0004f64.to_le_bytes());
        fs::write(&binary_path, binary).unwrap();
        let manifest_path = directory.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec(&serde_json::json!({
                "format": "pxlogic.saleae-transition.v1",
                "source": "fixture.sal",
                "sample_rate_hz": 1_000_000,
                "channel_count": 8,
                "duration_seconds": 0.001,
                "sample_count": 1_000,
                "channels": [{ "channel": 3, "file": "digital_3.bin" }],
                "session": {
                    "name": "SPI fixture",
                    "notes": "restored note",
                    "labels": ["D0", "D1", "D2", "clock", "D4", "D5", "D6", "D7"],
                    "analyzers": [{
                        "name": "SPI",
                        "type": "SPI",
                        "color": "#7ACB8C",
                        "displayRadix": "Hexadecimal",
                        "showInDataTable": true,
                        "streamToTerminal": false,
                        "settings": [
                            { "title": "MOSI", "setting": { "type": "Channel", "value": 3 } },
                            { "title": "MISO", "setting": { "type": "Channel" } },
                            { "title": "Clock", "setting": { "type": "Channel", "value": 3 } },
                            { "title": "Enable", "setting": { "type": "Channel" } },
                            { "title": "Significant Bit", "setting": { "type": "NumberList", "value": 0 } },
                            { "title": "Bits per Transfer", "setting": { "type": "NumberList", "value": 8 } },
                            { "title": "Clock State", "setting": { "type": "NumberList", "value": 1 } },
                            { "title": "Clock Phase", "setting": { "type": "NumberList", "value": 0 } },
                            { "title": "Enable Line", "setting": { "type": "NumberList", "value": 0 } }
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let imported = open_saleae_transition_session(&manifest_path).unwrap();
        let capture = imported.capture;
        assert_eq!(capture.metadata.sample_count, 1_000);
        assert_eq!(capture.metadata.enabled_channels, vec![3]);
        assert_eq!(capture.metadata.labels[3], "clock");
        assert!(capture.channels[0].initial_high);
        assert_eq!(capture.channels[0].transitions, vec![100, 400]);
        assert_eq!(imported.session.name, "SPI fixture");
        assert_eq!(imported.session.notes, "restored note");
        assert_eq!(imported.session.analyzers.len(), 1);
        let analyzer = &imported.session.analyzers[0];
        assert_eq!(analyzer.name, "SPI");
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(
            analyzer.decode_settings,
            Some(AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
                mosi_channel: Some(3),
                miso_channel: None,
                clock_channel: 3,
                enable_channel: None,
                enable_active_low: true,
                clock_polarity: true,
                clock_phase: false,
                bits_per_transfer: 8,
                msb_first: true,
            }))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normalizes_saleae_i2c_analyzer_settings() {
        let raw = serde_json::json!({
            "name": "I2C [sensors]",
            "type": "I2C",
            "color": "#75CACA",
            "displayRadix": "Hexadecimal",
            "showInDataTable": false,
            "streamToTerminal": true,
            "settings": [
                {
                    "title": "Serial Data Line",
                    "setting": { "type": "Channel", "value": 7 }
                },
                {
                    "title": "Serial Clock Line",
                    "setting": { "type": "Channel", "value": 9 }
                }
            ]
        });

        let analyzer = normalize_saleae_analyzer(raw.clone()).unwrap();
        assert_eq!(analyzer.name, "I2C [sensors]");
        assert_eq!(analyzer.color, "#75CACA");
        assert!(!analyzer.show_in_data_table);
        assert!(analyzer.stream_to_terminal);
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(analyzer.saleae_metadata, raw);
        assert_eq!(
            analyzer.decode_settings,
            Some(AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
                sda_channel: 7,
                scl_channel: 9,
            }))
        );
    }

    #[test]
    fn normalizes_saleae_i2s_analyzer_settings_to_native_contract() {
        let raw = serde_json::json!({
            "name": "I2S / PCM [codec]",
            "type": "I2S / PCM",
            "color": "#E6A45A",
            "displayRadix": "Hexadecimal",
            "showInDataTable": true,
            "streamToTerminal": false,
            "settings": [
                { "title": "CLOCK channel", "setting": { "type": "Channel", "value": 4 } },
                { "title": "FRAME", "setting": { "type": "Channel", "value": 5 } },
                { "title": "DATA", "setting": { "type": "Channel", "value": 6 } },
                { "title": "DATA Significant Bit", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "CLOCK State", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "Audio Bit Depth (bits/sample)", "setting": { "type": "Integer", "value": 24 } },
                { "title": "FRAME Signal Transitions", "setting": { "type": "NumberList", "value": 2 } },
                { "title": "DATA Bits Alignment", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "DATA Bits Shift", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "Signed/Unsigned", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "Word Select High", "setting": { "type": "NumberList", "value": 0 } }
            ]
        });

        let analyzer = normalize_saleae_analyzer(raw.clone()).unwrap();
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(analyzer.saleae_metadata, raw);
        assert_eq!(
            analyzer.decode_settings,
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "i2s".to_string(),
                protocol_name: "I2S".to_string(),
                channels: BTreeMap::from([
                    ("sck".to_string(), Some(4)),
                    ("ws".to_string(), Some(5)),
                    ("sd".to_string(), Some(6)),
                ]),
                options: BTreeMap::from([
                    (
                        "bit_order".to_string(),
                        NativeOptionValue::Text("lsb-first".to_string()),
                    ),
                    (
                        "clk_edge".to_string(),
                        NativeOptionValue::Text("falling-edge".to_string()),
                    ),
                    ("word_size".to_string(), NativeOptionValue::Integer(24)),
                    (
                        "frame_transitions".to_string(),
                        NativeOptionValue::Text("twice-every-4-words".to_string()),
                    ),
                    (
                        "bit_align".to_string(),
                        NativeOptionValue::Text("right-aligned".to_string()),
                    ),
                    (
                        "bit_shift".to_string(),
                        NativeOptionValue::Text("none".to_string()),
                    ),
                    ("signed".to_string(), NativeOptionValue::Boolean(true)),
                    (
                        "ws_polarity".to_string(),
                        NativeOptionValue::Text("left-high".to_string()),
                    ),
                ]),
                primary_channel: 4,
            }))
        );
    }

    #[test]
    fn normalizes_saleae_can_analyzer_settings_to_native_contract() {
        let raw = serde_json::json!({
            "name": "CAN [powertrain]",
            "type": "CAN",
            "color": "#7ACB8C",
            "displayRadix": "Hexadecimal",
            "showInDataTable": true,
            "streamToTerminal": false,
            "settings": [
                { "title": "CAN", "setting": { "type": "Channel", "value": 12 } },
                { "title": "Bit Rate (Bits/s)", "setting": { "type": "Integer", "value": 500000 } },
                { "title": "", "setting": { "type": "Bool", "value": true } }
            ]
        });

        let analyzer = normalize_saleae_analyzer(raw.clone()).unwrap();
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(analyzer.saleae_metadata, raw);
        assert_eq!(
            analyzer.decode_settings,
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "can".to_string(),
                protocol_name: "CAN".to_string(),
                channels: BTreeMap::from([("can_rx".to_string(), Some(12))]),
                options: BTreeMap::from([
                    ("bitrate".to_string(), NativeOptionValue::Integer(500_000)),
                    ("sample_point".to_string(), NativeOptionValue::Integer(70)),
                    ("inverted".to_string(), NativeOptionValue::Boolean(true)),
                ]),
                primary_channel: 12,
            }))
        );
    }

    #[test]
    fn normalizes_saleae_lin_analyzer_settings_to_native_contract() {
        let raw = serde_json::json!({
            "name": "LIN [body bus]",
            "type": "LIN",
            "color": "#D881F7",
            "displayRadix": "Hexadecimal",
            "showInDataTable": true,
            "streamToTerminal": false,
            "settings": [
                { "title": "Serial", "setting": { "type": "Channel", "value": 7 } },
                { "title": "LIN Version", "setting": { "type": "NumberList", "value": 1 } },
                { "title": "Bit Rate (Bits/s)", "setting": { "type": "Integer", "value": 19200 } }
            ]
        });

        let analyzer = normalize_saleae_analyzer(raw.clone()).unwrap();
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(analyzer.saleae_metadata, raw);
        assert_eq!(
            analyzer.decode_settings,
            Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "lin".to_string(),
                protocol_name: "LIN".to_string(),
                channels: BTreeMap::from([("rx".to_string(), Some(7))]),
                options: BTreeMap::from([
                    ("baudrate".to_string(), NativeOptionValue::Integer(19_200)),
                    ("version".to_string(), NativeOptionValue::Integer(1)),
                ]),
                primary_channel: 7,
            }))
        );
    }

    #[test]
    fn normalizes_and_serializes_complete_saleae_uart_settings() {
        let raw = serde_json::json!({
            "name": "Async Serial [console]",
            "type": "Async Serial",
            "color": "#D881F7",
            "displayRadix": "ASCII",
            "showInDataTable": true,
            "streamToTerminal": false,
            "settings": [
                {
                    "title": "Input Channel",
                    "setting": { "type": "Channel", "value": 12 }
                },
                {
                    "title": "Bit Rate (Bits/s)",
                    "setting": { "type": "Integer", "value": 230400 }
                },
                {
                    "title": "Use Autobaud",
                    "setting": { "type": "Bool", "value": false }
                },
                {
                    "title": "Bits per Frame",
                    "setting": { "type": "NumberList", "value": 7 }
                },
                {
                    "title": "Stop Bits",
                    "setting": { "type": "NumberList", "value": 1.5 }
                },
                {
                    "title": "Parity Bit",
                    "setting": { "type": "NumberList", "value": 2 }
                },
                {
                    "title": "Significant Bit",
                    "setting": { "type": "NumberList", "value": 1 }
                },
                {
                    "title": "Signal inversion",
                    "setting": { "type": "NumberList", "value": 1 }
                },
                {
                    "title": "Mode",
                    "setting": { "type": "NumberList", "value": 2 }
                }
            ]
        });

        let analyzer = normalize_saleae_analyzer(raw.clone()).unwrap();
        let expected = AnalyzerDecodeSettings::Uart(UartDecodeSettings {
            channel: 12,
            baud_rate: 230_400,
            inverted: true,
            data_bits: 7,
            stop_bits: UartStopBits::OnePointFive,
            parity: UartParity::Odd,
            msb_first: true,
        });
        assert_eq!(analyzer.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(analyzer.decode_settings, Some(expected.clone()));
        assert_eq!(analyzer.saleae_metadata, raw);

        let serialized = serde_json::to_value(&expected).unwrap();
        assert_eq!(serialized["settings"]["stop_bits"], 1.5);
        assert_eq!(serialized["settings"]["parity"], "odd");
        assert_eq!(
            serde_json::from_value::<AnalyzerDecodeSettings>(serialized).unwrap(),
            expected
        );
    }

    #[test]
    fn writes_packed_saleae_export_manifest_as_transition_indexes() {
        let directory = temp_directory("saleae-packed-export");
        let mut capture = sparse_capture();
        capture.metadata.sample_count = 4;
        capture.samples = vec![0x10, 0x01, 0x11, 0x00];

        let manifest_path = write_saleae_packed_export_manifest(&directory, &capture).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_reader(File::open(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["format"], "pxlogic.saleae-export.v1");
        assert_eq!(manifest["sample_count"], 4);
        assert_eq!(manifest["channels"][0]["channel"], 0);
        assert_eq!(manifest["channels"][0]["initial_high"], false);
        assert_eq!(manifest["channels"][1]["channel"], 4);
        assert_eq!(manifest["channels"][1]["initial_high"], true);
        assert_eq!(
            read_u64_file(directory.join("transitions-0.u64")),
            vec![1, 3]
        );
        assert_eq!(
            read_u64_file(directory.join("transitions-4.u64")),
            vec![1, 2, 3]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn writes_sparse_saleae_export_without_expanding_long_capture() {
        let directory = temp_directory("saleae-sparse-export");
        let mut metadata = sparse_capture().metadata;
        metadata.sample_count = 49_000_000_001;
        let capture = SparseCaptureData {
            metadata,
            channels: vec![
                SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: vec![100, 4_294_967_300, 49_000_000_000],
                },
                SparseDigitalChannel {
                    channel: 4,
                    initial_high: true,
                    transitions: vec![200],
                },
            ],
        };

        write_saleae_sparse_export_manifest(&directory, &capture).unwrap();
        assert_eq!(
            read_u64_file(directory.join("transitions-0.u64")),
            vec![100, 4_294_967_300, 49_000_000_000]
        );
        assert_eq!(
            read_u64_file(directory.join("transitions-4.u64")),
            vec![200]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_MANIFEST from the native GraphServer sidecar"]
    fn opens_native_saleae_graphserver_export() {
        let manifest = std::env::var_os("PXLOGIC_SALEAE_MANIFEST")
            .map(PathBuf::from)
            .expect("PXLOGIC_SALEAE_MANIFEST");
        let imported = open_saleae_transition_session(manifest).unwrap();
        let capture = imported.capture;
        assert!(capture.metadata.sample_count > u64::from(u32::MAX));
        assert!(!capture.channels.is_empty());
        assert!(capture
            .channels
            .iter()
            .all(|channel| channel.transitions.windows(2).all(|pair| pair[0] < pair[1])));
        let spi = imported
            .session
            .analyzers
            .iter()
            .find(|analyzer| analyzer.analyzer_type == "SPI")
            .expect("demo-spi.sal should restore its SPI analyzer");
        assert_eq!(spi.name, "SPI");
        assert_eq!(spi.decoder_backend, DecoderBackendKind::SaleaeNative);
        assert_eq!(
            spi.decode_settings,
            Some(AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
                mosi_channel: Some(5),
                miso_channel: Some(0),
                clock_channel: 3,
                enable_channel: Some(4),
                enable_active_low: true,
                clock_polarity: false,
                clock_phase: false,
                bits_per_transfer: 8,
                msb_first: true,
            }))
        );
    }

    fn project_session() -> SaleaeSessionMetadata {
        SaleaeSessionMetadata {
            name: "PXLogic Bench".to_string(),
            notes: "round-trip notes".to_string(),
            analyzers: vec![
                SaleaeAnalyzerMetadata {
                    analyzer_type: "I2C".to_string(),
                    name: "I2C bus".to_string(),
                    color: "#75CACA".to_string(),
                    display_radix: "Hexadecimal".to_string(),
                    show_in_data_table: true,
                    stream_to_terminal: false,
                    decoder_backend: DecoderBackendKind::SaleaeNative,
                    decode_settings: Some(AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
                        sda_channel: 0,
                        scl_channel: 4,
                    })),
                    saleae_metadata: serde_json::json!({"type": "I2C", "nodeId": 20000}),
                },
                SaleaeAnalyzerMetadata {
                    analyzer_type: "LIN".to_string(),
                    name: "LIN via sigrok".to_string(),
                    color: "#D881F7".to_string(),
                    display_radix: "Binary".to_string(),
                    show_in_data_table: false,
                    stream_to_terminal: true,
                    decoder_backend: DecoderBackendKind::SigrokNative,
                    decode_settings: Some(AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                        decoder_id: "lin".to_string(),
                        protocol_name: "LIN".to_string(),
                        channels: BTreeMap::from([("rx".to_string(), Some(0))]),
                        options: BTreeMap::from([(
                            "baudrate".to_string(),
                            NativeOptionValue::Integer(19_200),
                        )]),
                        primary_channel: 0,
                    })),
                    saleae_metadata: serde_json::json!({
                        "nativeCatalogKind": "python",
                        "nativeRunnerStatus": "wired"
                    }),
                },
            ],
        }
    }

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pxlogic-file-{}-{name}", std::process::id()))
    }

    fn temp_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pxlogic-file-{}-{name}", std::process::id()))
    }

    fn read_u64_file(path: impl AsRef<Path>) -> Vec<u64> {
        fs::read(path)
            .unwrap()
            .chunks_exact(8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .collect()
    }
}
