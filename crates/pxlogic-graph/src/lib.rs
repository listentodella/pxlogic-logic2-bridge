use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use parking_lot::{Mutex, RwLock};
use pxlogic_core::{
    resolve_enabled_channels, AnalyzerDecodeSettings, CaptureData, CaptureMetadata,
    CaptureTriggerMetadata, DecodedChannelValue, DecodedFrame, DecoderBackend, DecoderBackendKind,
    I2cDecodeSettings, SaleaeNativeDecoder, SigrokNativeDecoder, SparseCaptureData,
    SparseCaptureView, SparseDigitalChannel, SparseDigitalChannelView, SpiDecodeSettings,
    UartDecodeSettings,
};
use pxlogic_waveform::{ChannelTile, DigitalSegment, WaveformBin, WaveformRequest, WaveformTile};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_CHANNELS: u8 = 32;
const MAX_DECODED_FRAMES: usize = 100_000;

// Logic 2.4.43 BaseAnalyzerManager render limits.
const MAX_ANALYZER_MARKER_RESULTS: usize = 300;
const MAX_MULTI_BUBBLE_STRINGS: usize = 400;
const MAX_PIXEL_WIDTH_FOR_MERGEABLE_FRAME: f64 = 15.0;
const MAX_PIXELS_BETWEEN_MERGEABLE_FRAMES: f64 = 30.0;
const DENSE_BUBBLE_THRESHOLD: usize = MAX_ANALYZER_MARKER_RESULTS * 4;
const MAX_COARSE_EDGE_MARKERS: usize = 400;
const PACKED_DIGITAL_BIN_BYTES: usize = 16;
const MAX_EXACT_SAMPLES_PER_PIXEL: u64 = 8;
const PIXELS_PER_EXACT_TRANSITION: u32 = 10;
const NATIVE_VIEW_EDGE_BUDGET: usize = 32_768;
const NATIVE_FIT_MAX_CHUNKS: usize = 16;
const NATIVE_FIT_EDGES_PER_CHUNK: usize = 1_024;
const NATIVE_CONTEXT_EDGES: usize = 256;
const MAX_ANALYZER_WINDOW_CACHE_ENTRIES: usize = 64;
const MAX_PARALLEL_ANALYZER_RENDERS: usize = 4;
const DEFAULT_MAX_PARALLEL_NATIVE_WINDOW_DECODES: usize = 4;
const MAX_PARALLEL_ANALYZER_TRACK_RENDERS: usize = 4;
const NATIVE_WINDOW_DECODE_WORKERS_ENV: &str = "PXLOGIC_NATIVE_WINDOW_DECODE_WORKERS";

fn trace_graph_timing(stage: &str, started: Instant, detail: &str) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    tracing::debug!(
        target: "pxlogic::decoder_performance",
        component = "graph",
        stage,
        elapsed_ms,
        detail
    );
    if std::env::var_os("PXLOGIC_DECODER_TIMING").is_some() {
        eprintln!("[pxlogic-graph] stage={stage} elapsed_ms={elapsed_ms:.3} {detail}");
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("capture graph is empty")]
    EmptyGraph,
    #[error("capture metadata is invalid")]
    InvalidMetadata,
    #[error("sample chunk is not aligned to the capture unit size")]
    UnalignedSamples,
    #[error("waveform request is invalid")]
    InvalidWaveformRequest,
    #[error("capture trim range is invalid")]
    InvalidTrimRange,
    #[error("analyzer settings are invalid: {0}")]
    InvalidAnalyzer(String),
    #[error("failed to serialize analyzer settings: {0}")]
    AnalyzerCacheKey(#[from] serde_json::Error),
    #[error("decoder backend failed: {0}")]
    DecoderBackend(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerTrackRequest {
    pub channel: u8,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerRenderRequest {
    pub analyzer_id: String,
    #[serde(default)]
    pub backend: DecoderBackendKind,
    pub settings: AnalyzerDecodeSettings,
    #[serde(default)]
    pub tracks: Vec<AnalyzerTrackRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerFrameTableRequest {
    pub analyzer_id: String,
    #[serde(default)]
    pub backend: DecoderBackendKind,
    pub settings: AnalyzerDecodeSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerFrameTableRow {
    pub analyzer_id: String,
    pub frame: DecodedFrame,
    pub frame_index: u64,
    pub frame_number: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerFrameTableWindow {
    pub roles: Vec<String>,
    pub row_count: usize,
    pub rows: Vec<AnalyzerFrameTableRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphFrameRequest {
    pub frame_id: u64,
    pub waveform: WaveformRequest,
    #[serde(default)]
    pub analyzer_view: Option<WaveformRequest>,
    #[serde(default)]
    pub analyzers: Vec<AnalyzerRenderRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphFrameResponse {
    pub frame_id: u64,
    pub revision: u64,
    pub sample_count: u64,
    pub tile: WaveformTile,
    pub analyzers: Vec<AnalyzerRenderResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerRenderResult {
    pub analyzer_id: String,
    pub tracks: Vec<AnalyzerTrackWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerTrackWindow {
    pub channel: u8,
    pub role: String,
    pub bubbles: Vec<AnalyzerBubble>,
    pub protocol_markers: Vec<AnalyzerProtocolMarker>,
    pub frames: Vec<DecodedFrame>,
    pub previous_frame: Option<DecodedFrame>,
    pub next_frame: Option<DecodedFrame>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerProtocolMarker {
    pub sample: u64,
    pub kind: AnalyzerProtocolMarkerKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzerProtocolMarkerKind {
    Dot,
    ErrorDot,
    Square,
    ErrorSquare,
    UpArrow,
    DownArrow,
    X,
    ErrorX,
    Start,
    Stop,
    One,
    Zero,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalyzerBubble {
    #[serde(rename = "type")]
    pub bubble_type: AnalyzerBubbleType,
    pub frame_id_interval: FrameIdInterval,
    pub start_sample: u64,
    pub end_sample: u64,
    pub display_text: Vec<String>,
    pub result_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnalyzerBubbleType {
    SingleBubble,
    MultiBubble,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrameIdInterval {
    pub begin: u64,
    pub end: u64,
}

#[derive(Debug, Clone)]
struct DigitalChannelIndex {
    initial_high: bool,
    edges: Vec<u64>,
}

impl DigitalChannelIndex {
    fn new() -> Self {
        Self {
            initial_high: false,
            edges: Vec::new(),
        }
    }

    fn edge_count_at_or_before(&self, sample: u64) -> usize {
        self.edges.partition_point(|edge| *edge <= sample)
    }

    fn first_edge_after(&self, sample: u64) -> usize {
        self.edge_count_at_or_before(sample)
    }

    fn first_edge_at_or_after(&self, sample: u64) -> usize {
        self.edges.partition_point(|edge| *edge < sample)
    }

    fn level_at(&self, sample: u64) -> bool {
        self.initial_high ^ (self.edge_count_at_or_before(sample) % 2 == 1)
    }

    fn level_after_edge(&self, edge_index: usize) -> bool {
        self.initial_high ^ ((edge_index + 1) % 2 == 1)
    }

    fn edge(&self, edge_index: usize) -> u64 {
        self.edges[edge_index]
    }
}

/// Incremental, 32-channel digital graph used by capture, rendering and analyzers.
///
/// Raw samples are inspected exactly once while they are appended. All viewport
/// requests afterwards use sorted edge indexes and therefore scale with pixels,
/// not with capture duration.
#[derive(Debug, Clone)]
pub struct DigitalGraph {
    metadata: CaptureMetadata,
    channels: Vec<DigitalChannelIndex>,
    sample_count: u64,
    last_word: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct NearestDigitalTransition {
    pub previous_sample: Option<u64>,
    pub sample: u64,
    pub next_sample: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DigitalPointMeasurement {
    pub channel: u8,
    pub cursor_sample: u64,
    pub high: bool,
    pub width_start_sample: u64,
    pub width_end_sample: u64,
    pub period_end_sample: Option<u64>,
}

impl DigitalGraph {
    pub fn new(mut metadata: CaptureMetadata) -> Result<Self> {
        validate_metadata(&metadata)?;
        metadata.sample_count = 0;
        Ok(Self {
            channels: (0..metadata.channel_count)
                .map(|_| DigitalChannelIndex::new())
                .collect(),
            metadata,
            sample_count: 0,
            last_word: None,
        })
    }

    pub fn from_capture(capture: &CaptureData) -> Result<Self> {
        let expected_bytes = usize::try_from(capture.metadata.sample_count)
            .ok()
            .and_then(|samples| samples.checked_mul(usize::from(capture.metadata.unitsize)))
            .ok_or(GraphError::InvalidMetadata)?;
        if capture.samples.len() < expected_bytes {
            return Err(GraphError::InvalidMetadata);
        }

        let mut graph = Self::new(capture.metadata.clone())?;
        graph.append_interleaved(&capture.samples[..expected_bytes])?;
        graph.metadata.sample_count = graph.sample_count;
        Ok(graph)
    }

    pub fn from_sparse_capture(capture: &SparseCaptureData) -> Result<Self> {
        let mut graph = Self::new(capture.metadata.clone())?;
        graph.sample_count = capture.metadata.sample_count;
        graph.metadata.sample_count = capture.metadata.sample_count;
        for sparse_channel in &capture.channels {
            if sparse_channel.channel >= capture.metadata.channel_count {
                return Err(GraphError::InvalidMetadata);
            }
            let index = &mut graph.channels[usize::from(sparse_channel.channel)];
            index.initial_high = sparse_channel.initial_high;
            if sparse_channel
                .transitions
                .windows(2)
                .any(|window| window[0] >= window[1])
                || sparse_channel
                    .transitions
                    .iter()
                    .any(|sample| *sample >= capture.metadata.sample_count)
            {
                return Err(GraphError::InvalidMetadata);
            }
            index.edges.clone_from(&sparse_channel.transitions);
        }
        Ok(graph)
    }

    fn sparse_view(&self) -> SparseCaptureView<'_> {
        SparseCaptureView {
            metadata: &self.metadata,
            channels: self
                .channels
                .iter()
                .enumerate()
                .map(|(channel, index)| SparseDigitalChannelView {
                    channel: channel as u8,
                    initial_high: index.initial_high,
                    transitions: &index.edges,
                })
                .collect(),
        }
    }

    fn trim(&mut self, start: u64, end: u64) -> Result<()> {
        if start >= end || end > self.sample_count {
            return Err(GraphError::InvalidTrimRange);
        }

        for channel in &mut self.channels {
            const fn shifted_edge(edge: u64, start: u64) -> u64 {
                edge - start
            }

            let initial_high = channel.level_at(start);
            let first_edge = channel.edges.partition_point(|edge| *edge <= start);
            let end_edge = channel.edges.partition_point(|edge| *edge < end);
            let retained_edges = end_edge.saturating_sub(first_edge);
            channel.edges.copy_within(first_edge..end_edge, 0);
            channel.edges.truncate(retained_edges);
            channel
                .edges
                .iter_mut()
                .for_each(|edge| *edge = shifted_edge(*edge, start));
            channel.initial_high = initial_high;
        }

        self.metadata = trimmed_metadata(&self.metadata, start, end)?;
        self.sample_count = end - start;
        self.last_word = Some(self.channels.iter().enumerate().fold(
            0u32,
            |word, (channel_index, channel)| {
                word | (u32::from(channel.level_at(self.sample_count - 1)) << channel_index)
            },
        ));
        Ok(())
    }

    pub fn append_interleaved(&mut self, samples: &[u8]) -> Result<u64> {
        let unit_size = usize::from(self.metadata.unitsize);
        if unit_size == 0 || samples.len() % unit_size != 0 {
            return Err(GraphError::UnalignedSamples);
        }
        for bytes in samples.chunks_exact(unit_size) {
            let word =
                read_word(bytes, self.metadata.unitsize).ok_or(GraphError::InvalidMetadata)?;
            let sample = self.sample_count;
            if let Some(previous) = self.last_word {
                let valid_mask = channel_mask(self.metadata.channel_count);
                let mut changed = (previous ^ word) & valid_mask;
                while changed != 0 {
                    let channel = changed.trailing_zeros() as usize;
                    self.channels[channel].edges.push(sample);
                    changed &= changed - 1;
                }
            } else {
                for channel in 0..self.metadata.channel_count {
                    self.channels[usize::from(channel)].initial_high =
                        word & (1u32 << channel) != 0;
                }
            }
            self.last_word = Some(word);
            self.sample_count = self.sample_count.saturating_add(1);
        }
        self.metadata.sample_count = self.sample_count;
        Ok(self.sample_count)
    }

    /// Appends PXLogic `LA_CROSS_DATA` without first transposing every sample
    /// into an interleaved word. Each lane is a little-endian 64-sample bitset.
    pub fn append_cross_lanes(
        &mut self,
        enabled_channels: &[u8],
        input: &[u8],
        reverse_bits_in_word: bool,
    ) -> Result<u64> {
        let enabled_channels =
            resolve_enabled_channels(self.metadata.channel_count, enabled_channels)
                .map_err(|_| GraphError::InvalidMetadata)?;
        if enabled_channels != self.metadata.enabled_channels {
            return Err(GraphError::InvalidMetadata);
        }
        let stripe_bytes = enabled_channels
            .len()
            .checked_mul(8)
            .ok_or(GraphError::InvalidMetadata)?;
        if stripe_bytes == 0 || input.len() % stripe_bytes != 0 {
            return Err(GraphError::UnalignedSamples);
        }

        for stripe in input.chunks_exact(stripe_bytes) {
            let stripe_start = self.sample_count;
            let mut next_word = 0u32;
            for (lane, physical_channel) in enabled_channels.iter().copied().enumerate() {
                let offset = lane * 8;
                let mut word = u64::from_le_bytes(
                    stripe[offset..offset + 8]
                        .try_into()
                        .expect("fixed cross-lane word"),
                );
                if reverse_bits_in_word {
                    word = word.reverse_bits();
                }

                let first_high = word & 1 != 0;
                let channel = &mut self.channels[usize::from(physical_channel)];
                if let Some(previous_word) = self.last_word {
                    let previous_high = previous_word & (1u32 << physical_channel) != 0;
                    if previous_high != first_high {
                        channel.edges.push(stripe_start);
                    }
                } else {
                    channel.initial_high = first_high;
                }

                // Bit n is set when sample n differs from sample n - 1. Bit 0
                // represents the boundary handled above and is discarded.
                let mut transitions = (word ^ (word << 1)) & !1u64;
                while transitions != 0 {
                    let bit = transitions.trailing_zeros() as u64;
                    channel.edges.push(stripe_start.saturating_add(bit));
                    transitions &= transitions - 1;
                }
                if word >> 63 != 0 {
                    next_word |= 1u32 << physical_channel;
                }
            }
            self.last_word = Some(next_word);
            self.sample_count = self.sample_count.saturating_add(64);
        }
        self.metadata.sample_count = self.sample_count;
        Ok(self.sample_count)
    }

    pub fn metadata(&self) -> &CaptureMetadata {
        &self.metadata
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    pub fn nearest_transition(
        &self,
        channel: u8,
        sample: u64,
    ) -> Result<Option<NearestDigitalTransition>> {
        let index = self
            .channels
            .get(usize::from(channel))
            .ok_or(GraphError::InvalidWaveformRequest)?;
        let right = index.first_edge_at_or_after(sample);
        let left = right.checked_sub(1);
        let nearest_index = match (left, index.edges.get(right)) {
            (Some(left), Some(right_sample)) => {
                let left_sample = index.edge(left);
                if sample.saturating_sub(left_sample) <= right_sample.saturating_sub(sample) {
                    left
                } else {
                    right
                }
            }
            (Some(left), None) => left,
            (None, Some(_)) => right,
            (None, None) => return Ok(None),
        };
        Ok(Some(NearestDigitalTransition {
            previous_sample: nearest_index
                .checked_sub(1)
                .and_then(|previous| index.edges.get(previous).copied()),
            sample: index.edge(nearest_index),
            next_sample: index.edges.get(nearest_index + 1).copied(),
        }))
    }

    pub fn measure_point(
        &self,
        channel: u8,
        sample: u64,
    ) -> Result<Option<DigitalPointMeasurement>> {
        let index = self
            .channels
            .get(usize::from(channel))
            .ok_or(GraphError::InvalidWaveformRequest)?;
        if self.sample_count == 0 {
            return Ok(None);
        }

        let cursor_sample = sample.min(self.sample_count - 1);
        let width_end_index = index.first_edge_after(cursor_sample);
        let Some(width_start_index) = width_end_index.checked_sub(1) else {
            return Ok(None);
        };
        let Some(&width_end_sample) = index.edges.get(width_end_index) else {
            return Ok(None);
        };
        let width_start_sample = index.edge(width_start_index);

        Ok(Some(DigitalPointMeasurement {
            channel,
            cursor_sample,
            high: index.level_at(cursor_sample),
            width_start_sample,
            width_end_sample,
            period_end_sample: index.edges.get(width_end_index + 1).copied(),
        }))
    }

    fn set_trigger(&mut self, trigger: CaptureTriggerMetadata) {
        self.metadata.trigger = Some(trigger);
    }

    fn finish_metadata(&mut self, metadata: CaptureMetadata) -> Result<()> {
        validate_metadata(&metadata)?;
        let enabled_channels = metadata_enabled_channels(&metadata)?;
        let current_enabled_channels = metadata_enabled_channels(&self.metadata)?;
        if metadata.sample_count != self.sample_count
            || metadata.channel_count != self.metadata.channel_count
            || enabled_channels != current_enabled_channels
            || metadata.unitsize != self.metadata.unitsize
            || metadata.sample_rate_hz != self.metadata.sample_rate_hz
        {
            return Err(GraphError::InvalidMetadata);
        }
        self.metadata = metadata;
        Ok(())
    }

    pub fn build_tile(&self, request: &WaveformRequest) -> Result<WaveformTile> {
        if request.pixels == 0 || request.sample_count == 0 {
            return Err(GraphError::InvalidWaveformRequest);
        }

        let start = request.start_sample.min(self.sample_count);
        let end = start
            .saturating_add(request.sample_count)
            .min(self.sample_count);
        let visible_samples = end.saturating_sub(start);
        if visible_samples == 0 {
            return Ok(WaveformTile {
                start_sample: start,
                sample_count: 0,
                pixels: request.pixels,
                samples_per_pixel: 0.0,
                channels: Vec::new(),
            });
        }

        let bin_count = u64::from(request.pixels).min(visible_samples).max(1) as u32;
        let samples_per_bin = visible_samples as f64 / f64::from(bin_count);
        let requested_channels = requested_channels(request, self.metadata.channel_count);
        let exact_limit = u64::from(request.pixels)
            .saturating_mul(MAX_EXACT_SAMPLES_PER_PIXEL)
            .max(4096);
        let include_exact = visible_samples <= exact_limit;
        let build_channel = |channel| {
            self.build_channel_tile(
                channel,
                start,
                end,
                bin_count,
                samples_per_bin,
                include_exact,
            )
        };
        let channels = if requested_channels.len() > 4 {
            requested_channels
                .into_par_iter()
                .map(build_channel)
                .collect()
        } else {
            requested_channels.into_iter().map(build_channel).collect()
        };

        Ok(WaveformTile {
            start_sample: start,
            sample_count: visible_samples,
            pixels: bin_count,
            samples_per_pixel: samples_per_bin,
            channels,
        })
    }

    fn build_channel_tile(
        &self,
        channel: u8,
        start: u64,
        end: u64,
        bin_count: u32,
        samples_per_bin: f64,
        include_exact: bool,
    ) -> ChannelTile {
        let index = &self.channels[usize::from(channel)];
        let visible_samples = end - start;
        let mut bins = Vec::with_capacity(bin_count as usize);
        for x in 0..bin_count {
            let bin_start =
                start + ((f64::from(x) * samples_per_bin).floor() as u64).min(visible_samples);
            let bin_end = start
                + ((f64::from(x + 1) * samples_per_bin).ceil() as u64)
                    .min(visible_samples)
                    .max(1);
            bins.push(summarize_indexed_bin(index, x, bin_start, bin_end));
        }

        // PXView switches from individual transition lines to pixel-level
        // activity once the edge count exceeds roughly one transition per ten
        // pixels. This also bounds IPC payloads and frontend Path2D work.
        let first_edge = index.first_edge_after(start);
        let edge_end = index.first_edge_at_or_after(end);
        let exact_transition_budget = (bin_count / PIXELS_PER_EXACT_TRANSITION).max(1) as usize;
        let include_exact =
            include_exact && edge_end.saturating_sub(first_edge) < exact_transition_budget;
        let (segments, rising_edges, falling_edges) = if include_exact {
            exact_channel_data(index, start, end)
        } else {
            let (rising, falling) = sampled_channel_edges(index, start, end);
            (Vec::new(), rising, falling)
        };
        let previous_transition = index
            .first_edge_at_or_after(start)
            .checked_sub(1)
            .map(|edge_index| index.edge(edge_index));
        let next_edge_index = index.first_edge_at_or_after(end);
        let next_transition =
            (next_edge_index < index.edges.len()).then(|| index.edge(next_edge_index));
        let label = self
            .metadata
            .labels
            .get(usize::from(channel))
            .cloned()
            .unwrap_or_else(|| format!("D{channel}"));

        let packed_bins = pack_digital_bins(&bins);
        ChannelTile {
            channel,
            label,
            bins: Vec::new(),
            packed_bins: Some(packed_bins),
            segments,
            rising_edges,
            falling_edges,
            previous_transition,
            next_transition,
        }
    }

    fn decode(&self, settings: &AnalyzerDecodeSettings) -> Result<Vec<DecodedFrame>> {
        self.decode_from(settings, 0)
    }

    fn decode_from(
        &self,
        settings: &AnalyzerDecodeSettings,
        start_sample: u64,
    ) -> Result<Vec<DecodedFrame>> {
        match settings {
            AnalyzerDecodeSettings::Uart(settings) => self.decode_uart_from(settings, start_sample),
            AnalyzerDecodeSettings::I2c(settings) => self.decode_i2c_from(settings, start_sample),
            AnalyzerDecodeSettings::Spi(settings) => self.decode_spi_from(settings, start_sample),
            AnalyzerDecodeSettings::Native(settings) => Err(GraphError::InvalidAnalyzer(format!(
                "{} requires a Saleae Native or Sigrok Native decoder backend",
                settings.protocol_name
            ))),
        }
    }

    fn decode_uart_from(
        &self,
        settings: &UartDecodeSettings,
        start_sample: u64,
    ) -> Result<Vec<DecodedFrame>> {
        self.validate_channel(settings.channel)?;
        if settings.baud_rate == 0 {
            return Err(GraphError::InvalidAnalyzer(
                "UART baud rate must be non-zero".to_string(),
            ));
        }
        let bit_period = self.metadata.sample_rate_hz as f64 / f64::from(settings.baud_rate);
        if bit_period < 2.0 {
            return Err(GraphError::InvalidAnalyzer(
                "sample rate is too low for the selected UART baud rate".to_string(),
            ));
        }

        let channel = &self.channels[usize::from(settings.channel)];
        let mut collector = FrameCollector::default();
        let resume_sample = start_sample.saturating_sub((bit_period * 11.0).ceil() as u64);
        let mut edge_index = channel.first_edge_at_or_after(resume_sample);
        while edge_index < channel.edges.len() {
            let sample = channel.edge(edge_index);
            let physical_after = channel.level_after_edge(edge_index);
            let logical_after = physical_after ^ settings.inverted;
            if logical_after
                || sample.saturating_add((10.0 * bit_period).ceil() as u64) >= self.sample_count
                || (self.level_at_offset(settings.channel, sample, bit_period * 0.5)?
                    ^ settings.inverted)
            {
                edge_index += 1;
                continue;
            }

            let mut value = 0u8;
            let mut valid = true;
            for bit_index in 0..8 {
                let bit_sample =
                    sample + (bit_period * (1.5 + f64::from(bit_index))).round() as u64;
                if bit_sample >= self.sample_count {
                    valid = false;
                    break;
                }
                if self.level_at(settings.channel, bit_sample)? ^ settings.inverted {
                    value |= 1u8 << bit_index;
                }
            }
            let stop_sample = sample + (bit_period * 9.5).round() as u64;
            if valid
                && stop_sample < self.sample_count
                && (self.level_at(settings.channel, stop_sample)? ^ settings.inverted)
            {
                let label = label_for_value(u64::from(value), 8);
                collector.push(DecodedFrame {
                    frame_id: 0,
                    start_sample: sample,
                    end_sample: sample + (bit_period * 10.0).ceil() as u64,
                    frame_type: "data".to_string(),
                    label: label.clone(),
                    value: u64::from(value),
                    channel_values: vec![DecodedChannelValue {
                        channel: settings.channel,
                        role: "Input".to_string(),
                        label: label.clone(),
                        texts: vec![label],
                        value: u64::from(value),
                    }],
                    protocol_markers: Vec::new(),
                });
                let skip_until = sample + (bit_period * 9.0).floor() as u64;
                edge_index = channel.edges.partition_point(|edge| *edge < skip_until);
            } else {
                edge_index += 1;
            }
        }
        Ok(collector.finish())
    }

    fn decode_i2c_from(
        &self,
        settings: &I2cDecodeSettings,
        start_sample: u64,
    ) -> Result<Vec<DecodedFrame>> {
        self.validate_channel(settings.sda_channel)?;
        self.validate_channel(settings.scl_channel)?;
        if settings.sda_channel == settings.scl_channel {
            return Err(GraphError::InvalidAnalyzer(
                "I2C SDA and SCL must use different channels".to_string(),
            ));
        }
        if self.sample_count < 2 {
            return Ok(Vec::new());
        }

        let sda_index = &self.channels[usize::from(settings.sda_channel)];
        let scl_index = &self.channels[usize::from(settings.scl_channel)];
        let resume_sample = self.i2c_resume_sample(settings, start_sample);
        let mut sda_edge = sda_index.first_edge_at_or_after(resume_sample);
        let mut scl_edge = scl_index.first_edge_at_or_after(resume_sample);
        let previous_sample = resume_sample.saturating_sub(1);
        let mut previous_sda = sda_index.level_at(previous_sample);
        let mut previous_scl = scl_index.level_at(previous_sample);
        let mut active = false;
        let mut byte = 0u8;
        let mut bit_count = 0u8;
        let mut byte_index = 0u64;
        let mut byte_start = 0u64;
        let mut collector = FrameCollector::default();

        while sda_edge < sda_index.edges.len() || scl_edge < scl_index.edges.len() {
            let next_sda = sda_index.edges.get(sda_edge).copied().unwrap_or(u64::MAX);
            let next_scl = scl_index.edges.get(scl_edge).copied().unwrap_or(u64::MAX);
            let sample = next_sda.min(next_scl);
            if sample == u64::MAX {
                break;
            }

            let mut sda = previous_sda;
            let mut scl = previous_scl;
            if next_sda == sample {
                sda = !sda;
                sda_edge += 1;
            }
            if next_scl == sample {
                scl = !scl;
                scl_edge += 1;
            }

            let start = previous_sda && !sda && scl;
            let stop = !previous_sda && sda && scl;
            if start {
                active = true;
                byte = 0;
                bit_count = 0;
                byte_index = 0;
                byte_start = sample;
            } else if stop && active {
                active = false;
                bit_count = 0;
            } else if active && !previous_scl && scl {
                if bit_count < 8 {
                    if bit_count == 0 {
                        byte_start = sample;
                    }
                    byte = (byte << 1) | u8::from(sda);
                    bit_count += 1;
                } else {
                    let acknowledged = !sda;
                    let bubble_label = if byte_index == 0 {
                        format!(
                            "Setup {} to [0x{:02X}] + {}",
                            if byte & 1 == 0 { "Write" } else { "Read" },
                            byte >> 1,
                            if acknowledged { "ACK" } else { "NAK" }
                        )
                    } else {
                        format!(
                            "{} + {}",
                            label_for_value(u64::from(byte), 8),
                            if acknowledged { "ACK" } else { "NAK" }
                        )
                    };
                    let label = if byte_index == 0 {
                        format!(
                            "Address 0x{:02X} {} {}",
                            byte >> 1,
                            if byte & 1 == 0 { "Write" } else { "Read" },
                            if acknowledged { "ACK" } else { "NAK" }
                        )
                    } else {
                        format!(
                            "Data {} {}",
                            label_for_value(u64::from(byte), 8),
                            if acknowledged { "ACK" } else { "NAK" }
                        )
                    };
                    collector.push(DecodedFrame {
                        frame_id: 0,
                        start_sample: byte_start,
                        end_sample: sample.saturating_add(1),
                        frame_type: if byte_index == 0 { "address" } else { "data" }.to_string(),
                        label,
                        value: u64::from(byte),
                        channel_values: vec![DecodedChannelValue {
                            channel: settings.sda_channel,
                            role: "SDA".to_string(),
                            label: bubble_label.clone(),
                            texts: vec![bubble_label],
                            value: u64::from(byte),
                        }],
                        protocol_markers: Vec::new(),
                    });
                    byte = 0;
                    bit_count = 0;
                    byte_index += 1;
                }
            }
            previous_sda = sda;
            previous_scl = scl;
        }
        Ok(collector.finish())
    }

    fn i2c_resume_sample(&self, settings: &I2cDecodeSettings, start_sample: u64) -> u64 {
        if start_sample == 0 {
            return 0;
        }
        let sda = &self.channels[usize::from(settings.sda_channel)];
        let scl = &self.channels[usize::from(settings.scl_channel)];
        let mut edge = sda.first_edge_at_or_after(start_sample);
        while edge > 0 {
            edge -= 1;
            let sample = sda.edge(edge);
            if scl.level_at(sample) {
                return sample;
            }
        }
        0
    }

    fn decode_spi_from(
        &self,
        settings: &SpiDecodeSettings,
        start_sample: u64,
    ) -> Result<Vec<DecodedFrame>> {
        self.validate_channel(settings.clock_channel)?;
        for channel in [
            settings.mosi_channel,
            settings.miso_channel,
            settings.enable_channel,
        ]
        .into_iter()
        .flatten()
        {
            self.validate_channel(channel)?;
            if channel == settings.clock_channel {
                return Err(GraphError::InvalidAnalyzer(
                    "SPI clock must use a different channel than data and enable".to_string(),
                ));
            }
        }
        if settings.mosi_channel.is_none() && settings.miso_channel.is_none() {
            return Err(GraphError::InvalidAnalyzer(
                "SPI requires a MOSI or MISO channel".to_string(),
            ));
        }
        if !(1..=64).contains(&settings.bits_per_transfer) {
            return Err(GraphError::InvalidAnalyzer(
                "SPI bits per transfer must be between 1 and 64".to_string(),
            ));
        }

        let clock = &self.channels[usize::from(settings.clock_channel)];
        let sample_rising_edge = settings.clock_polarity == settings.clock_phase;
        let mut collector = FrameCollector::default();
        let mut transfer_start = 0u64;
        let mut bit_count = 0u8;
        let mut mosi_value = 0u64;
        let mut miso_value = 0u64;

        let resume_edge = settings.enable_channel.map_or(0, |enable_channel| {
            let enable = &self.channels[usize::from(enable_channel)];
            let mut edge = enable.first_edge_at_or_after(start_sample);
            while edge > 0 {
                edge -= 1;
                let asserted = enable.level_after_edge(edge) != settings.enable_active_low;
                if asserted {
                    return clock.first_edge_at_or_after(enable.edge(edge));
                }
            }
            0
        });
        for (edge_index, sample) in clock.edges.iter().copied().enumerate().skip(resume_edge) {
            let rising = clock.level_after_edge(edge_index);
            if rising != sample_rising_edge {
                continue;
            }
            let enabled = settings.enable_channel.map_or(Ok(true), |channel| {
                self.level_at(channel, sample)
                    .map(|level| level != settings.enable_active_low)
            })?;
            if !enabled {
                bit_count = 0;
                mosi_value = 0;
                miso_value = 0;
                continue;
            }
            if bit_count == 0 {
                transfer_start = sample;
            }
            if let Some(channel) = settings.mosi_channel {
                mosi_value = append_spi_bit(
                    mosi_value,
                    self.level_at(channel, sample)?,
                    bit_count,
                    settings.bits_per_transfer,
                    settings.msb_first,
                );
            }
            if let Some(channel) = settings.miso_channel {
                miso_value = append_spi_bit(
                    miso_value,
                    self.level_at(channel, sample)?,
                    bit_count,
                    settings.bits_per_transfer,
                    settings.msb_first,
                );
            }
            bit_count += 1;

            if bit_count == settings.bits_per_transfer {
                let mut channel_values = Vec::new();
                if let Some(channel) = settings.mosi_channel {
                    channel_values.push(DecodedChannelValue {
                        channel,
                        role: "MOSI".to_string(),
                        label: label_for_value(mosi_value, settings.bits_per_transfer),
                        texts: vec![label_for_value(mosi_value, settings.bits_per_transfer)],
                        value: mosi_value,
                    });
                }
                if let Some(channel) = settings.miso_channel {
                    channel_values.push(DecodedChannelValue {
                        channel,
                        role: "MISO".to_string(),
                        label: label_for_value(miso_value, settings.bits_per_transfer),
                        texts: vec![label_for_value(miso_value, settings.bits_per_transfer)],
                        value: miso_value,
                    });
                }
                let label = match (settings.mosi_channel, settings.miso_channel) {
                    (Some(_), Some(_)) => format!(
                        "MOSI {} / MISO {}",
                        label_for_value(mosi_value, settings.bits_per_transfer),
                        label_for_value(miso_value, settings.bits_per_transfer)
                    ),
                    (Some(_), None) => format!(
                        "MOSI {}",
                        label_for_value(mosi_value, settings.bits_per_transfer)
                    ),
                    (None, Some(_)) => format!(
                        "MISO {}",
                        label_for_value(miso_value, settings.bits_per_transfer)
                    ),
                    (None, None) => unreachable!(),
                };
                collector.push(DecodedFrame {
                    frame_id: 0,
                    start_sample: transfer_start,
                    end_sample: sample.saturating_add(1),
                    frame_type: "result".to_string(),
                    label,
                    value: settings.mosi_channel.map_or(miso_value, |_| mosi_value),
                    channel_values,
                    protocol_markers: Vec::new(),
                });
                bit_count = 0;
                mosi_value = 0;
                miso_value = 0;
            }
        }
        Ok(collector.finish())
    }

    fn validate_channel(&self, channel: u8) -> Result<()> {
        if channel < self.metadata.channel_count {
            Ok(())
        } else {
            Err(GraphError::InvalidAnalyzer(format!(
                "channel {channel} is outside the capture"
            )))
        }
    }

    fn level_at(&self, channel: u8, sample: u64) -> Result<bool> {
        self.validate_channel(channel)?;
        if sample >= self.sample_count {
            return Err(GraphError::InvalidAnalyzer(format!(
                "sample {sample} is outside the capture"
            )));
        }
        Ok(self.channels[usize::from(channel)].level_at(sample))
    }

    fn level_at_offset(&self, channel: u8, start: u64, offset: f64) -> Result<bool> {
        self.level_at(channel, start.saturating_add(offset.round() as u64))
    }
}

fn sparse_capture_view(capture: &SparseCaptureData) -> SparseCaptureView<'_> {
    SparseCaptureView {
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
    }
}

/// Per-UI-session graph state. Analyzer frames are cached by settings and are
/// invalidated whenever new samples arrive.
#[derive(Debug, Default)]
pub struct GraphSession {
    graph: RwLock<Option<DigitalGraph>>,
    capture: RwLock<Option<Arc<CaptureData>>>,
    sparse_capture: RwLock<Option<Arc<SparseCaptureData>>>,
    analyzer_cache: Mutex<HashMap<String, AnalyzerCacheEntry>>,
    revision: AtomicU64,
}

#[derive(Debug, Clone)]
struct AnalyzerCacheEntry {
    decoded_sample_count: u64,
    frames: Arc<Vec<DecodedFrame>>,
}

#[derive(Debug, Clone)]
struct AnalyzerFrameTableSource {
    analyzer_id: String,
    frames: Arc<Vec<DecodedFrame>>,
}

fn decoded_frames_allocated_bytes(frames: &Arc<Vec<DecodedFrame>>) -> u64 {
    frames.iter().fold(
        u64::try_from(
            frames
                .capacity()
                .saturating_mul(std::mem::size_of::<DecodedFrame>()),
        )
        .unwrap_or(u64::MAX),
        |bytes, frame| {
            let frame_strings = frame
                .frame_type
                .capacity()
                .saturating_add(frame.label.capacity());
            let channel_values = frame.channel_values.iter().fold(
                frame
                    .channel_values
                    .capacity()
                    .saturating_mul(std::mem::size_of::<DecodedChannelValue>()),
                |channel_bytes, value| {
                    channel_bytes
                        .saturating_add(value.role.capacity())
                        .saturating_add(value.label.capacity())
                },
            );
            bytes
                .saturating_add(u64::try_from(frame_strings).unwrap_or(u64::MAX))
                .saturating_add(u64::try_from(channel_values).unwrap_or(u64::MAX))
        },
    )
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureMemoryUsage {
    pub buffer_bytes: u64,
    pub sample_bytes: u64,
    pub index_bytes: u64,
    pub analyzer_bytes: u64,
    pub total_bytes: u64,
}

impl GraphSession {
    pub fn capture_memory_usage(
        &self,
        buffer_bytes: u64,
        in_flight_sample_bytes: Option<u64>,
    ) -> CaptureMemoryUsage {
        let sample_bytes = in_flight_sample_bytes.unwrap_or_else(|| {
            if let Some(capture) = self.capture.read().as_ref() {
                return u64::try_from(capture.samples.capacity()).unwrap_or(u64::MAX);
            }
            self.sparse_capture
                .read()
                .as_ref()
                .map(|capture| {
                    capture.channels.iter().fold(0u64, |bytes, channel| {
                        bytes.saturating_add(
                            u64::try_from(channel.transitions.capacity())
                                .unwrap_or(u64::MAX)
                                .saturating_mul(std::mem::size_of::<u64>() as u64),
                        )
                    })
                })
                .unwrap_or(0)
        });
        let index_bytes = self
            .graph
            .read()
            .as_ref()
            .map(|graph| {
                graph.channels.iter().fold(0u64, |bytes, channel| {
                    bytes.saturating_add(
                        u64::try_from(channel.edges.capacity())
                            .unwrap_or(u64::MAX)
                            .saturating_mul(std::mem::size_of::<u64>() as u64),
                    )
                })
            })
            .unwrap_or(0);
        let analyzer_bytes = self
            .analyzer_cache
            .lock()
            .values()
            .fold(0u64, |bytes, entry| {
                bytes.saturating_add(decoded_frames_allocated_bytes(&entry.frames))
            });
        CaptureMemoryUsage {
            buffer_bytes,
            sample_bytes,
            index_bytes,
            analyzer_bytes,
            total_bytes: sample_bytes
                .saturating_add(index_bytes)
                .saturating_add(analyzer_bytes),
        }
    }

    pub fn begin_capture(&self, metadata: CaptureMetadata) -> Result<()> {
        *self.graph.write() = Some(DigitalGraph::new(metadata)?);
        *self.capture.write() = None;
        *self.sparse_capture.write() = None;
        self.analyzer_cache.lock().clear();
        self.bump_revision();
        Ok(())
    }

    pub fn append_samples(&self, samples: &[u8]) -> Result<u64> {
        let count = self
            .graph
            .write()
            .as_mut()
            .ok_or(GraphError::EmptyGraph)?
            .append_interleaved(samples)?;
        self.bump_revision();
        Ok(count)
    }

    pub fn append_cross_lanes(
        &self,
        enabled_channels: &[u8],
        samples: &[u8],
        reverse_bits_in_word: bool,
    ) -> Result<u64> {
        let count = self
            .graph
            .write()
            .as_mut()
            .ok_or(GraphError::EmptyGraph)?
            .append_cross_lanes(enabled_channels, samples, reverse_bits_in_word)?;
        self.bump_revision();
        Ok(count)
    }

    pub fn set_trigger(&self, trigger: CaptureTriggerMetadata) -> Result<()> {
        self.graph
            .write()
            .as_mut()
            .ok_or(GraphError::EmptyGraph)?
            .set_trigger(trigger);
        self.bump_revision();
        Ok(())
    }

    pub fn replace_capture(&self, capture: CaptureData) -> Result<()> {
        let graph = DigitalGraph::from_capture(&capture)?;
        *self.graph.write() = Some(graph);
        *self.capture.write() = Some(Arc::new(capture));
        *self.sparse_capture.write() = None;
        self.analyzer_cache.lock().clear();
        self.bump_revision();
        Ok(())
    }

    pub fn finish_capture(&self, capture: CaptureData) -> Result<()> {
        let indexed_samples = self
            .graph
            .read()
            .as_ref()
            .map(DigitalGraph::sample_count)
            .unwrap_or(0);
        if indexed_samples != capture.metadata.sample_count {
            return self.replace_capture(capture);
        }
        let finalized = self
            .graph
            .write()
            .as_mut()
            .ok_or(GraphError::EmptyGraph)
            .and_then(|graph| graph.finish_metadata(capture.metadata.clone()));
        if finalized.is_err() {
            return self.replace_capture(capture);
        }
        *self.capture.write() = Some(Arc::new(capture));
        *self.sparse_capture.write() = None;
        self.bump_revision();
        Ok(())
    }

    pub fn replace_sparse_capture(&self, capture: SparseCaptureData) -> Result<()> {
        let graph = DigitalGraph::from_sparse_capture(&capture)?;
        *self.graph.write() = Some(graph);
        *self.capture.write() = None;
        *self.sparse_capture.write() = Some(Arc::new(capture));
        self.analyzer_cache.lock().clear();
        self.bump_revision();
        Ok(())
    }

    pub fn trim_capture(&self, start: u64, end: u64) -> Result<(CaptureMetadata, usize)> {
        let mut graph = self.graph.write();
        let graph = graph.as_mut().ok_or(GraphError::EmptyGraph)?;
        if start >= end || end > graph.sample_count() {
            return Err(GraphError::InvalidTrimRange);
        }
        let metadata = trimmed_metadata(graph.metadata(), start, end)?;

        let mut packed_capture = self.capture.write();
        let mut sparse_capture = self.sparse_capture.write();
        let bytes = if let Some(capture) = packed_capture.as_mut() {
            trim_packed_capture(capture, metadata.clone(), start, end)?
        } else if let Some(capture) = sparse_capture.as_mut() {
            trim_sparse_capture(capture, metadata.clone(), start, end)?
        } else {
            return Err(GraphError::EmptyGraph);
        };

        graph.trim(start, end)?;
        self.analyzer_cache.lock().clear();
        self.bump_revision();
        Ok((metadata, bytes))
    }

    pub fn capture(&self) -> Option<Arc<CaptureData>> {
        self.capture.read().clone()
    }

    pub fn sparse_capture(&self) -> Option<Arc<SparseCaptureData>> {
        self.sparse_capture.read().clone()
    }

    pub fn metadata(&self) -> Option<CaptureMetadata> {
        self.graph
            .read()
            .as_ref()
            .map(|graph| graph.metadata().clone())
    }

    pub fn sample_count(&self) -> u64 {
        self.graph
            .read()
            .as_ref()
            .map(DigitalGraph::sample_count)
            .unwrap_or(0)
    }

    pub fn nearest_transition(
        &self,
        channel: u8,
        sample: u64,
    ) -> Result<Option<NearestDigitalTransition>> {
        self.graph
            .read()
            .as_ref()
            .ok_or(GraphError::EmptyGraph)?
            .nearest_transition(channel, sample)
    }

    pub fn measure_point(
        &self,
        channel: u8,
        sample: u64,
    ) -> Result<Option<DigitalPointMeasurement>> {
        self.graph
            .read()
            .as_ref()
            .ok_or(GraphError::EmptyGraph)?
            .measure_point(channel, sample)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn decode_frames(
        &self,
        settings: &AnalyzerDecodeSettings,
    ) -> Result<Arc<Vec<DecodedFrame>>> {
        let key = serde_json::to_string(settings)?;
        let sample_count = self.sample_count();
        let cached = self.analyzer_cache.lock().remove(&key);
        if let Some(entry) = cached {
            if entry.decoded_sample_count == sample_count {
                let frames = entry.frames.clone();
                self.analyzer_cache.lock().insert(key, entry);
                return Ok(frames);
            }
            let frames = {
                let graph_guard = self.graph.read();
                let graph = graph_guard.as_ref().ok_or(GraphError::EmptyGraph)?;
                let resume_sample = entry
                    .frames
                    .last()
                    .map(|frame| frame.start_sample)
                    .unwrap_or(entry.decoded_sample_count.saturating_sub(1));
                let incremental = graph.decode_from(settings, resume_sample)?;
                let cached_frames =
                    Arc::try_unwrap(entry.frames).unwrap_or_else(|frames| frames.as_ref().clone());
                Arc::new(merge_incremental_frames(
                    cached_frames,
                    incremental,
                    resume_sample,
                ))
            };
            let result = frames.clone();
            self.analyzer_cache.lock().insert(
                key,
                AnalyzerCacheEntry {
                    decoded_sample_count: sample_count,
                    frames,
                },
            );
            return Ok(result);
        }

        let frames = Arc::new(
            self.graph
                .read()
                .as_ref()
                .ok_or(GraphError::EmptyGraph)?
                .decode(settings)?,
        );
        let mut cache = self.analyzer_cache.lock();
        let entry = cache.entry(key).or_insert_with(|| AnalyzerCacheEntry {
            decoded_sample_count: sample_count,
            frames: frames.clone(),
        });
        if entry.decoded_sample_count < sample_count {
            *entry = AnalyzerCacheEntry {
                decoded_sample_count: sample_count,
                frames: frames.clone(),
            };
        }
        Ok(entry.frames.clone())
    }

    pub fn decode_frames_with_backend(
        &self,
        settings: &AnalyzerDecodeSettings,
        backend: DecoderBackendKind,
    ) -> Result<Arc<Vec<DecodedFrame>>> {
        if matches!(
            backend,
            DecoderBackendKind::Auto | DecoderBackendKind::LegacyRust
        ) {
            if let AnalyzerDecodeSettings::Native(settings) = settings {
                return Err(GraphError::InvalidAnalyzer(format!(
                    "{} cannot use the Legacy Rust decoder backend",
                    settings.protocol_name
                )));
            }
            return self.decode_frames(settings);
        }
        let packed_capture = self.capture();
        let sparse_capture = self.sparse_capture();
        if packed_capture.is_none() && sparse_capture.is_none() {
            // Native sidecars are batch decoders. Keep live bubbles available
            // from the indexed graph, then replace them after finish_capture().
            if matches!(settings, AnalyzerDecodeSettings::Native(_)) {
                return Ok(Arc::new(Vec::new()));
            }
            return self.decode_frames(settings);
        }

        let key = native_full_analyzer_cache_key(backend, settings)?;
        let sample_count = self.sample_count();
        if let Some(entry) = self.analyzer_cache.lock().get(&key) {
            if entry.decoded_sample_count == sample_count {
                return Ok(entry.frames.clone());
            }
        }

        let decode_indexed = |decoder: &dyn DecoderBackend| {
            if packed_capture.is_some() {
                let graph = self.graph.read();
                let capture = graph
                    .as_ref()
                    .ok_or_else(|| {
                        pxlogic_core::CoreError::Decode("capture graph is empty".to_string())
                    })?
                    .sparse_view();
                decoder.decode_sparse_view(&capture, settings)
            } else if let Some(capture) = &sparse_capture {
                decoder.decode_sparse(capture.as_ref(), settings)
            } else {
                unreachable!();
            }
        };
        let decode_started = Instant::now();
        let output = match backend {
            DecoderBackendKind::SaleaeNative => {
                let decoder = SaleaeNativeDecoder::from_env()
                    .map_err(|error| GraphError::DecoderBackend(error.to_string()))?;
                decode_indexed(&decoder)
            }
            DecoderBackendKind::SigrokNative => {
                let decoder = SigrokNativeDecoder::from_env()
                    .map_err(|error| GraphError::DecoderBackend(error.to_string()))?;
                decode_indexed(&decoder)
            }
            DecoderBackendKind::Auto | DecoderBackendKind::LegacyRust => unreachable!(),
        }
        .map_err(|error| GraphError::DecoderBackend(error.to_string()))?;
        trace_graph_timing(
            "backend_decode",
            decode_started,
            &format!("backend={backend:?} output_frames={}", output.frames.len()),
        );
        let conversion_started = Instant::now();
        let frames = Arc::new(output.into_decoded_frames());
        trace_graph_timing(
            "frame_conversion",
            conversion_started,
            &format!("backend={backend:?} frames={}", frames.len()),
        );
        self.analyzer_cache.lock().insert(
            key,
            AnalyzerCacheEntry {
                decoded_sample_count: sample_count,
                frames: frames.clone(),
            },
        );
        Ok(frames)
    }

    pub fn decode_frame_count_with_backend(
        &self,
        settings: &AnalyzerDecodeSettings,
        backend: DecoderBackendKind,
    ) -> Result<usize> {
        self.decode_frames_with_backend(settings, backend)
            .map(|frames| frames.len())
    }

    pub fn decode_frame_slice_with_backend(
        &self,
        settings: &AnalyzerDecodeSettings,
        backend: DecoderBackendKind,
        start: usize,
        end: usize,
    ) -> Result<(usize, Vec<DecodedFrame>)> {
        let frames = self.decode_frames_with_backend(settings, backend)?;
        let total = frames.len();
        let start = start.min(total);
        let end = end.min(total).max(start);
        Ok((total, frames[start..end].to_vec()))
    }

    pub fn decode_frame_table_window(
        &self,
        requests: &[AnalyzerFrameTableRequest],
        start: usize,
        end: usize,
    ) -> Result<AnalyzerFrameTableWindow> {
        let decoded_sources = if requests.len() <= 1 {
            requests
                .iter()
                .map(|request| self.decode_frame_table_source(request))
                .collect::<Result<Vec<_>>>()?
        } else {
            requests
                .par_iter()
                .map(|request| self.decode_frame_table_source(request))
                .collect::<Result<Vec<_>>>()?
        };
        let sources = decoded_sources.into_iter().flatten().collect::<Vec<_>>();
        Ok(analyzer_frame_table_window_from_sources(
            &sources, start, end,
        ))
    }

    fn decode_frame_table_source(
        &self,
        request: &AnalyzerFrameTableRequest,
    ) -> Result<Option<AnalyzerFrameTableSource>> {
        let frames = self.decode_frames_with_backend(&request.settings, request.backend)?;
        Ok((!frames.is_empty()).then(|| AnalyzerFrameTableSource {
            analyzer_id: request.analyzer_id.clone(),
            frames,
        }))
    }

    pub fn decode_frames_with_backend_for_view(
        &self,
        settings: &AnalyzerDecodeSettings,
        backend: DecoderBackendKind,
        waveform: &WaveformRequest,
    ) -> Result<Arc<Vec<DecodedFrame>>> {
        if matches!(
            backend,
            DecoderBackendKind::SaleaeNative | DecoderBackendKind::SigrokNative
        ) {
            if let Some(capture) = self.sparse_capture() {
                let capture = sparse_capture_view(capture.as_ref());
                return self.decode_sparse_frames_for_view(&capture, settings, backend, waveform);
            }
            if self.capture().is_some() {
                let graph = self.graph.read();
                let graph = graph.as_ref().ok_or(GraphError::EmptyGraph)?;
                let capture = graph.sparse_view();
                return self.decode_sparse_frames_for_view(&capture, settings, backend, waveform);
            }
        }
        self.decode_frames_with_backend(settings, backend)
    }

    pub fn render_frame(&self, request: &GraphFrameRequest) -> Result<GraphFrameResponse> {
        let total_started = Instant::now();
        let tile_started = Instant::now();
        let tile = self
            .graph
            .read()
            .as_ref()
            .ok_or(GraphError::EmptyGraph)?
            .build_tile(&request.waveform)?;
        trace_graph_timing(
            "waveform_tile",
            tile_started,
            &format!("channels={}", tile.channels.len()),
        );
        let analyzers_started = Instant::now();
        let analyzers = self.render_analyzers(
            &request.analyzers,
            request.analyzer_view.as_ref().unwrap_or(&request.waveform),
        )?;
        trace_graph_timing(
            "analyzer_render",
            analyzers_started,
            &format!("analyzers={}", analyzers.len()),
        );

        let response = GraphFrameResponse {
            frame_id: request.frame_id,
            revision: self.revision(),
            sample_count: self.sample_count(),
            tile,
            analyzers,
        };
        trace_graph_timing(
            "graph_frame_total",
            total_started,
            &format!("frame_id={}", request.frame_id),
        );
        Ok(response)
    }

    fn render_analyzer(
        &self,
        request: &AnalyzerRenderRequest,
        waveform: &WaveformRequest,
    ) -> Result<AnalyzerRenderResult> {
        let decode_started = Instant::now();
        let frames =
            self.decode_frames_with_backend_for_view(&request.settings, request.backend, waveform)?;
        trace_graph_timing(
            "analyzer_frames",
            decode_started,
            &format!(
                "analyzer={} backend={:?} frames={}",
                request.analyzer_id,
                request.backend,
                frames.len()
            ),
        );
        let aggregate_started = Instant::now();
        let tracks = render_analyzer_tracks(&frames, &request.tracks, waveform)?;
        trace_graph_timing(
            "bubble_aggregation",
            aggregate_started,
            &format!("analyzer={} tracks={}", request.analyzer_id, tracks.len()),
        );
        Ok(AnalyzerRenderResult {
            analyzer_id: request.analyzer_id.clone(),
            tracks,
        })
    }

    fn render_analyzers(
        &self,
        requests: &[AnalyzerRenderRequest],
        waveform: &WaveformRequest,
    ) -> Result<Vec<AnalyzerRenderResult>> {
        if requests.len() <= 1 {
            return requests
                .iter()
                .map(|request| self.render_analyzer(request, waveform))
                .collect();
        }

        let worker_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .min(MAX_PARALLEL_ANALYZER_RENDERS)
            .min(requests.len());
        if worker_count <= 1 {
            return requests
                .iter()
                .map(|request| self.render_analyzer(request, waveform))
                .collect();
        }

        let chunk_size = requests.len().div_ceil(worker_count);
        let worker_outputs = requests
            .par_chunks(chunk_size)
            .enumerate()
            .map(|(chunk_index, chunk)| {
                let start_index = chunk_index * chunk_size;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(index, request)| {
                            self.render_analyzer(request, waveform)
                                .map(|result| (start_index + index, result))
                        })
                        .collect::<Result<Vec<_>>>()
                }))
                .unwrap_or_else(|_| {
                    Err(GraphError::DecoderBackend(
                        "analyzer render worker panicked".to_string(),
                    ))
                })
            })
            .collect::<Vec<_>>();

        let mut indexed_results: Vec<(usize, AnalyzerRenderResult)> =
            Vec::with_capacity(requests.len());
        for output in worker_outputs {
            let mut results = output?;
            indexed_results.append(&mut results);
        }
        indexed_results.sort_by_key(|(index, _)| *index);
        Ok(indexed_results
            .into_iter()
            .map(|(_, result)| result)
            .collect())
    }

    fn decode_sparse_frames_for_view(
        &self,
        capture: &SparseCaptureView<'_>,
        settings: &AnalyzerDecodeSettings,
        backend: DecoderBackendKind,
        waveform: &WaveformRequest,
    ) -> Result<Arc<Vec<DecodedFrame>>> {
        let sample_count = self.sample_count();
        let full_key = native_full_analyzer_cache_key(backend, settings)?;
        if let Some(entry) = self.analyzer_cache.lock().get(&full_key) {
            if entry.decoded_sample_count == sample_count {
                trace_graph_timing(
                    "native_full_decode_cache_hit",
                    Instant::now(),
                    &format!("backend={backend:?} frames={}", entry.frames.len()),
                );
                return Ok(entry.frames.clone());
            }
        }

        let windows = sparse_native_analysis_windows(capture, settings, waveform)?;
        if windows.is_empty() {
            return Ok(Arc::new(Vec::new()));
        }

        let key = serde_json::to_string(&("sparse-native-window-v1", backend, settings, &windows))?;
        if let Some(entry) = self.analyzer_cache.lock().get(&key) {
            if entry.decoded_sample_count == sample_count {
                return Ok(entry.frames.clone());
            }
        }

        let mut frames = Vec::new();
        let mut first_error = None;
        let window_decode_started = Instant::now();
        let worker_count = native_window_decode_worker_count(windows.len());
        if worker_count <= 1 {
            let decoder = native_decoder_for_backend(backend)?;
            for &(start, end) in &windows {
                match decoder.decode_sparse_view_window(capture, start, end, settings) {
                    Ok(output) => {
                        frames.extend(
                            output
                                .into_decoded_frames()
                                .into_iter()
                                .map(|frame| offset_decoded_frame_samples(frame, start)),
                        );
                    }
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            }
        } else {
            let chunk_size = windows.len().div_ceil(worker_count);
            let worker_outputs = windows
                .par_chunks(chunk_size)
                .map(|chunk| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let decoder = native_decoder_for_backend(backend)?;
                        let mut chunk_frames = Vec::new();
                        let mut chunk_error = None;
                        for &(start, end) in chunk {
                            match decoder.decode_sparse_view_window(capture, start, end, settings) {
                                Ok(output) => {
                                    chunk_frames.extend(
                                        output.into_decoded_frames().into_iter().map(|frame| {
                                            offset_decoded_frame_samples(frame, start)
                                        }),
                                    );
                                }
                                Err(error) => {
                                    chunk_error.get_or_insert_with(|| error.to_string());
                                }
                            }
                        }
                        Ok::<_, GraphError>((chunk_frames, chunk_error))
                    }))
                    .unwrap_or_else(|_| {
                        Err(GraphError::DecoderBackend(
                            "native window decode worker panicked".to_string(),
                        ))
                    })
                })
                .collect::<Vec<_>>();
            for output in worker_outputs {
                match output {
                    Ok((mut chunk_frames, chunk_error)) => {
                        frames.append(&mut chunk_frames);
                        if let Some(error) = chunk_error {
                            first_error.get_or_insert(error);
                        }
                    }
                    Err(error) => {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            }
        }
        trace_graph_timing(
            "native_window_decode",
            window_decode_started,
            &format!(
                "backend={backend:?} windows={} workers={} frames={}",
                windows.len(),
                worker_count,
                frames.len()
            ),
        );
        if frames.is_empty() {
            if let Some(error) = first_error {
                return Err(GraphError::DecoderBackend(error));
            }
        }

        normalize_native_decoded_frames(&mut frames);
        for frame in &mut frames {
            frame.frame_id = frame.start_sample;
        }
        let uncapped_frame_count = frames.len();
        if frames.len() > MAX_DECODED_FRAMES {
            let stride = frames.len().div_ceil(MAX_DECODED_FRAMES);
            frames = frames.into_iter().step_by(stride).collect();
            for frame in &mut frames {
                frame.frame_id = frame.start_sample;
            }
        }

        let frames = Arc::new(frames);
        let mut cache = self.analyzer_cache.lock();
        if cache.len() >= MAX_ANALYZER_WINDOW_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            key,
            AnalyzerCacheEntry {
                decoded_sample_count: sample_count,
                frames: frames.clone(),
            },
        );
        if uncapped_frame_count <= MAX_DECODED_FRAMES
            && native_view_covers_capture(waveform, capture.metadata.sample_count)
            && native_windows_cover_all_driver_edges(capture, settings, &windows)?
        {
            cache.insert(
                full_key,
                AnalyzerCacheEntry {
                    decoded_sample_count: sample_count,
                    frames: frames.clone(),
                },
            );
        }
        Ok(frames)
    }

    fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
}

fn offset_decoded_frame_samples(mut frame: DecodedFrame, offset: u64) -> DecodedFrame {
    frame.start_sample = frame.start_sample.saturating_add(offset);
    frame.end_sample = frame.end_sample.saturating_add(offset);
    for marker in &mut frame.protocol_markers {
        marker.sample = marker.sample.saturating_add(offset);
    }
    frame
}

fn native_full_analyzer_cache_key(
    backend: DecoderBackendKind,
    settings: &AnalyzerDecodeSettings,
) -> Result<String> {
    serde_json::to_string(&(backend, settings)).map_err(GraphError::from)
}

fn native_view_covers_capture(waveform: &WaveformRequest, sample_count: u64) -> bool {
    waveform.start_sample == 0 && waveform.sample_count >= sample_count
}

fn native_windows_cover_all_driver_edges(
    capture: &SparseCaptureView<'_>,
    settings: &AnalyzerDecodeSettings,
    windows: &[(u64, u64)],
) -> Result<bool> {
    let driver_channel = match settings {
        AnalyzerDecodeSettings::Uart(settings) => settings.channel,
        AnalyzerDecodeSettings::I2c(settings) => settings.scl_channel,
        AnalyzerDecodeSettings::Spi(settings) => settings.clock_channel,
        AnalyzerDecodeSettings::Native(settings) => settings.primary_channel,
    };
    let driver = capture
        .channels
        .iter()
        .find(|channel| channel.channel == driver_channel)
        .ok_or_else(|| {
            GraphError::InvalidAnalyzer(format!(
                "native decoder clock/input channel D{driver_channel} is not present"
            ))
        })?;
    let (Some(first), Some(last)) = (driver.transitions.first(), driver.transitions.last()) else {
        return Ok(false);
    };
    Ok(windows.len() == 1
        && windows[0].0 <= first.saturating_sub(1)
        && windows[0].1 >= last.saturating_add(2).min(capture.metadata.sample_count))
}

fn native_window_decode_worker_count(window_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(DEFAULT_MAX_PARALLEL_NATIVE_WINDOW_DECODES);
    let configured = std::env::var(NATIVE_WINDOW_DECODE_WORKERS_ENV).ok();
    native_window_decode_worker_count_value(window_count, configured.as_deref(), available)
}

fn native_window_decode_worker_count_value(
    window_count: usize,
    configured: Option<&str>,
    available: usize,
) -> usize {
    if window_count <= 1 {
        return 1;
    }
    let requested = configured
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_MAX_PARALLEL_NATIVE_WINDOW_DECODES);
    requested.min(available.max(1)).min(window_count).max(1)
}

fn native_decoder_for_backend(backend: DecoderBackendKind) -> Result<Box<dyn DecoderBackend>> {
    match backend {
        DecoderBackendKind::SaleaeNative => Ok(Box::new(
            SaleaeNativeDecoder::from_env()
                .map_err(|error| GraphError::DecoderBackend(error.to_string()))?,
        )),
        DecoderBackendKind::SigrokNative => Ok(Box::new(
            SigrokNativeDecoder::from_env()
                .map_err(|error| GraphError::DecoderBackend(error.to_string()))?,
        )),
        DecoderBackendKind::Auto | DecoderBackendKind::LegacyRust => unreachable!(),
    }
}

fn trimmed_metadata(metadata: &CaptureMetadata, start: u64, end: u64) -> Result<CaptureMetadata> {
    if start >= end || end > metadata.sample_count {
        return Err(GraphError::InvalidTrimRange);
    }
    let mut trimmed = metadata.clone();
    trimmed.sample_count = end - start;
    trimmed.trigger = trimmed.trigger.and_then(|mut trigger| {
        if trigger.sample_index < start || trigger.sample_index >= end {
            return None;
        }
        trigger.sample_index -= start;
        Some(trigger)
    });
    Ok(trimmed)
}

fn trim_packed_capture(
    capture: &mut Arc<CaptureData>,
    metadata: CaptureMetadata,
    start: u64,
    end: u64,
) -> Result<usize> {
    let unit_size = usize::from(capture.metadata.unitsize);
    let byte_start = usize::try_from(start)
        .ok()
        .and_then(|sample| sample.checked_mul(unit_size))
        .ok_or(GraphError::InvalidMetadata)?;
    let byte_end = usize::try_from(end)
        .ok()
        .and_then(|sample| sample.checked_mul(unit_size))
        .ok_or(GraphError::InvalidMetadata)?;
    if unit_size == 0 || byte_end > capture.samples.len() || byte_start > byte_end {
        return Err(GraphError::InvalidMetadata);
    }

    if let Some(capture) = Arc::get_mut(capture) {
        if byte_start > 0 {
            capture.samples.copy_within(byte_start..byte_end, 0);
        }
        capture.samples.truncate(byte_end - byte_start);
        capture.metadata = metadata;
    } else {
        *capture = Arc::new(CaptureData {
            metadata,
            samples: capture.samples[byte_start..byte_end].to_vec(),
        });
    }
    Ok(byte_end - byte_start)
}

fn trim_sparse_capture(
    capture: &mut Arc<SparseCaptureData>,
    metadata: CaptureMetadata,
    start: u64,
    end: u64,
) -> Result<usize> {
    let trim_channel = |channel: &mut SparseDigitalChannel| {
        let first_edge = channel.transitions.partition_point(|edge| *edge <= start);
        let end_edge = channel.transitions.partition_point(|edge| *edge < end);
        channel.initial_high ^= first_edge % 2 == 1;
        let retained_edges = end_edge.saturating_sub(first_edge);
        channel.transitions.copy_within(first_edge..end_edge, 0);
        channel.transitions.truncate(retained_edges);
        channel
            .transitions
            .iter_mut()
            .for_each(|edge| *edge -= start);
    };

    if let Some(capture) = Arc::get_mut(capture) {
        capture.channels.iter_mut().for_each(trim_channel);
        capture.metadata = metadata;
    } else {
        let mut trimmed = capture.as_ref().clone();
        trimmed.channels.iter_mut().for_each(trim_channel);
        trimmed.metadata = metadata;
        *capture = Arc::new(trimmed);
    }
    Ok(capture
        .channels
        .iter()
        .map(|channel| channel.transitions.len().saturating_mul(8))
        .sum())
}

fn sparse_native_analysis_windows(
    capture: &SparseCaptureView<'_>,
    settings: &AnalyzerDecodeSettings,
    waveform: &WaveformRequest,
) -> Result<Vec<(u64, u64)>> {
    if capture.metadata.sample_count == 0 || waveform.sample_count == 0 {
        return Ok(Vec::new());
    }
    let driver_channel = match settings {
        AnalyzerDecodeSettings::Uart(settings) => settings.channel,
        AnalyzerDecodeSettings::I2c(settings) => settings.scl_channel,
        AnalyzerDecodeSettings::Spi(settings) => settings.clock_channel,
        AnalyzerDecodeSettings::Native(settings) => settings.primary_channel,
    };
    let driver = capture
        .channels
        .iter()
        .find(|channel| channel.channel == driver_channel)
        .ok_or_else(|| {
            GraphError::InvalidAnalyzer(format!(
                "native decoder clock/input channel D{driver_channel} is not present"
            ))
        })?;
    if driver.transitions.is_empty() {
        return Ok(Vec::new());
    }

    let view_start = waveform
        .start_sample
        .min(capture.metadata.sample_count.saturating_sub(1));
    let view_end = waveform
        .start_sample
        .saturating_add(waveform.sample_count)
        .min(capture.metadata.sample_count)
        .max(view_start.saturating_add(1));
    let edge_begin = driver
        .transitions
        .partition_point(|sample| *sample < view_start);
    let edge_end = driver
        .transitions
        .partition_point(|sample| *sample < view_end);
    let visible_edges = edge_end - edge_begin;
    let mut edge_windows = Vec::new();
    if visible_edges == 0 {
        let center = edge_begin.min(driver.transitions.len() - 1);
        edge_windows.push((
            center.saturating_sub(NATIVE_CONTEXT_EDGES),
            center
                .saturating_add(NATIVE_CONTEXT_EDGES)
                .saturating_add(1)
                .min(driver.transitions.len()),
        ));
    } else if visible_edges <= NATIVE_VIEW_EDGE_BUDGET {
        edge_windows.push((
            edge_begin.saturating_sub(NATIVE_CONTEXT_EDGES),
            edge_end
                .saturating_add(NATIVE_CONTEXT_EDGES)
                .min(driver.transitions.len()),
        ));
    } else {
        let chunk_count = NATIVE_FIT_MAX_CHUNKS
            .min((waveform.pixels.max(1) as usize).div_ceil(96))
            .max(2);
        for chunk in 0..chunk_count {
            let center = edge_begin
                + visible_edges.saturating_sub(1) * chunk / chunk_count.saturating_sub(1);
            let body_start = center.saturating_sub(NATIVE_FIT_EDGES_PER_CHUNK / 2);
            let body_end = center
                .saturating_add(NATIVE_FIT_EDGES_PER_CHUNK / 2)
                .saturating_add(1);
            edge_windows.push((
                body_start.saturating_sub(NATIVE_CONTEXT_EDGES),
                body_end
                    .saturating_add(NATIVE_CONTEXT_EDGES)
                    .min(driver.transitions.len()),
            ));
        }
    }

    let mut windows = edge_windows
        .into_iter()
        .filter(|(begin, end)| begin < end)
        .map(|(begin, end)| {
            let start = driver.transitions[begin].saturating_sub(1);
            let finish = driver.transitions[end - 1]
                .saturating_add(2)
                .min(capture.metadata.sample_count);
            align_spi_window_to_enable(capture, settings, start, finish)
        })
        .filter(|(start, end)| start < end)
        .collect::<Vec<_>>();
    windows.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(windows.len());
    for (start, end) in windows {
        if let Some(previous) = merged.last_mut() {
            if start <= previous.1 {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    Ok(merged)
}

fn align_spi_window_to_enable(
    capture: &SparseCaptureView<'_>,
    settings: &AnalyzerDecodeSettings,
    mut start: u64,
    mut end: u64,
) -> (u64, u64) {
    let AnalyzerDecodeSettings::Spi(settings) = settings else {
        return (start, end);
    };
    let Some(enable_channel) = settings.enable_channel else {
        return (start, end);
    };
    let Some(enable) = capture
        .channels
        .iter()
        .find(|channel| channel.channel == enable_channel)
    else {
        return (start, end);
    };
    let is_active = |high: bool| high != settings.enable_active_low;
    if is_active(sparse_level_at(enable, start)) {
        let previous = enable
            .transitions
            .partition_point(|sample| *sample <= start);
        if previous > 0 {
            start = enable.transitions[previous - 1].saturating_sub(1);
        }
    }
    if end > 0 && is_active(sparse_level_at(enable, end - 1)) {
        let next = enable.transitions.partition_point(|sample| *sample < end);
        if let Some(sample) = enable.transitions.get(next) {
            end = sample.saturating_add(1).min(capture.metadata.sample_count);
        }
    }
    (start, end)
}

fn sparse_level_at(channel: &SparseDigitalChannelView<'_>, sample: u64) -> bool {
    channel.initial_high
        ^ (channel
            .transitions
            .partition_point(|transition| *transition <= sample)
            % 2
            == 1)
}

#[cfg(test)]
fn slice_sparse_capture(
    capture: &SparseCaptureData,
    start: u64,
    end: u64,
) -> Result<SparseCaptureData> {
    if start >= end || end > capture.metadata.sample_count {
        return Err(GraphError::InvalidWaveformRequest);
    }
    let channels = capture
        .channels
        .iter()
        .map(|channel| {
            let begin = channel
                .transitions
                .partition_point(|sample| *sample <= start);
            let finish = channel.transitions.partition_point(|sample| *sample < end);
            SparseDigitalChannel {
                channel: channel.channel,
                initial_high: channel.initial_high ^ (begin % 2 == 1),
                transitions: channel.transitions[begin..finish]
                    .iter()
                    .map(|sample| sample - start)
                    .collect(),
            }
        })
        .collect();
    let mut metadata = capture.metadata.clone();
    metadata.sample_count = end - start;
    metadata.trigger = None;
    Ok(SparseCaptureData { metadata, channels })
}

fn merge_incremental_frames(
    mut cached: Vec<DecodedFrame>,
    mut incremental: Vec<DecodedFrame>,
    resume_sample: u64,
) -> Vec<DecodedFrame> {
    let frame_id_offset = incremental
        .iter()
        .find_map(|candidate| {
            cached
                .iter()
                .rev()
                .find(|existing| same_decoded_frame(existing, candidate))
                .map(|existing| existing.frame_id.saturating_sub(candidate.frame_id))
        })
        .unwrap_or_else(|| {
            cached
                .last()
                .map(|frame| frame.frame_id.saturating_add(1))
                .unwrap_or(0)
        });
    for frame in &mut incremental {
        frame.frame_id = frame.frame_id.saturating_add(frame_id_offset);
    }
    let keep = cached.partition_point(|frame| frame.end_sample <= resume_sample);
    cached.truncate(keep);
    cached.reserve(incremental.len());
    cached.extend(incremental);
    let mut merged = cached;
    normalize_native_decoded_frames(&mut merged);
    if merged.len() > MAX_DECODED_FRAMES {
        let stride = merged.len().div_ceil(MAX_DECODED_FRAMES);
        let last = merged.last().cloned();
        merged = merged.into_iter().step_by(stride).collect();
        if let Some(last) = last {
            if merged.last().map(|frame| frame.start_sample) != Some(last.start_sample) {
                if merged.len() >= MAX_DECODED_FRAMES {
                    merged.pop();
                }
                merged.push(last);
            }
        }
    }
    merged
}

fn normalize_native_decoded_frames(frames: &mut Vec<DecodedFrame>) {
    if frames.len() <= 1 {
        return;
    }
    if !decoded_frames_are_ordered(frames) {
        frames.sort_by_key(|frame| (frame.start_sample, frame.end_sample));
    }
    frames.dedup_by(|right, left| same_decoded_frame(left, right));
}

fn decoded_frames_are_ordered(frames: &[DecodedFrame]) -> bool {
    frames.windows(2).all(|pair| {
        let left = &pair[0];
        let right = &pair[1];
        left.start_sample < right.start_sample
            || (left.start_sample == right.start_sample && left.end_sample <= right.end_sample)
    })
}

fn same_decoded_frame(left: &DecodedFrame, right: &DecodedFrame) -> bool {
    left.start_sample == right.start_sample
        && left.end_sample == right.end_sample
        && left.channel_values == right.channel_values
        && left.value == right.value
}

fn validate_metadata(metadata: &CaptureMetadata) -> Result<()> {
    if !(1..=MAX_CHANNELS).contains(&metadata.channel_count) {
        return Err(GraphError::InvalidMetadata);
    }
    let expected_unit_size = match metadata.channel_count {
        1..=8 => 1,
        9..=16 => 2,
        17..=32 => 4,
        _ => unreachable!(),
    };
    if metadata.unitsize != expected_unit_size || metadata.sample_rate_hz == 0 {
        return Err(GraphError::InvalidMetadata);
    }
    let enabled_channels = metadata_enabled_channels(metadata)?;
    if metadata.trigger.as_ref().is_some_and(|trigger| {
        trigger.channel >= metadata.channel_count || !enabled_channels.contains(&trigger.channel)
    }) {
        return Err(GraphError::InvalidMetadata);
    }
    Ok(())
}

fn metadata_enabled_channels(metadata: &CaptureMetadata) -> Result<Vec<u8>> {
    resolve_enabled_channels(metadata.channel_count, &metadata.enabled_channels)
        .map_err(|_| GraphError::InvalidMetadata)
}

fn channel_mask(channel_count: u8) -> u32 {
    if channel_count == 32 {
        u32::MAX
    } else {
        (1u32 << channel_count) - 1
    }
}

fn read_word(bytes: &[u8], unit_size: u8) -> Option<u32> {
    match unit_size {
        1 => bytes.first().copied().map(u32::from),
        2 => Some(u32::from(u16::from_le_bytes(bytes.try_into().ok()?))),
        4 => Some(u32::from_le_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

fn requested_channels(request: &WaveformRequest, channel_count: u8) -> Vec<u8> {
    let source = if request.channels.is_empty() {
        (0..channel_count).collect()
    } else {
        request.channels.clone()
    };
    source
        .into_iter()
        .filter(|channel| *channel < channel_count)
        .collect()
}

fn summarize_indexed_bin(index: &DigitalChannelIndex, x: u32, start: u64, end: u64) -> WaveformBin {
    let first_edge = index.first_edge_after(start);
    let edge_end = index.edge_count_at_or_before(end.saturating_sub(1));
    let first = index.initial_high ^ (first_edge % 2 == 1);
    let last = index.initial_high ^ (edge_end % 2 == 1);
    let edge_count = edge_end.saturating_sub(first_edge);
    let first_edge_offset = (edge_count > 0).then(|| {
        index
            .edge(first_edge)
            .saturating_sub(start)
            .min(u64::from(u32::MAX)) as u32
    });
    let last_edge_offset = (edge_count > 0).then(|| {
        index
            .edge(edge_end - 1)
            .saturating_sub(start)
            .min(u64::from(u32::MAX)) as u32
    });

    WaveformBin {
        x,
        first,
        last,
        has_high: first || edge_count > 0,
        has_low: !first || edge_count > 0,
        edges: edge_count.min(u32::MAX as usize) as u32,
        first_edge_offset,
        last_edge_offset,
    }
}

fn pack_digital_bins(bins: &[WaveformBin]) -> String {
    let mut packed = Vec::with_capacity(bins.len() * PACKED_DIGITAL_BIN_BYTES);
    for bin in bins {
        let flags = u32::from(bin.first)
            | (u32::from(bin.last) << 1)
            | (u32::from(bin.has_high) << 2)
            | (u32::from(bin.has_low) << 3);
        packed.extend_from_slice(&flags.to_le_bytes());
        packed.extend_from_slice(&bin.edges.to_le_bytes());
        packed.extend_from_slice(&bin.first_edge_offset.unwrap_or(u32::MAX).to_le_bytes());
        packed.extend_from_slice(&bin.last_edge_offset.unwrap_or(u32::MAX).to_le_bytes());
    }
    BASE64.encode(packed)
}

#[cfg(test)]
fn unpack_digital_bins(packed: &str) -> Vec<WaveformBin> {
    let bytes = BASE64.decode(packed).expect("valid packed bins");
    bytes
        .chunks_exact(PACKED_DIGITAL_BIN_BYTES)
        .enumerate()
        .map(|(x, chunk)| {
            let flags = u32::from_le_bytes(chunk[0..4].try_into().expect("flags"));
            let edges = u32::from_le_bytes(chunk[4..8].try_into().expect("edges"));
            let first = u32::from_le_bytes(chunk[8..12].try_into().expect("first edge"));
            let last = u32::from_le_bytes(chunk[12..16].try_into().expect("last edge"));
            WaveformBin {
                x: x as u32,
                first: flags & 1 != 0,
                last: flags & 2 != 0,
                has_high: flags & 4 != 0,
                has_low: flags & 8 != 0,
                edges,
                first_edge_offset: (first != u32::MAX).then_some(first),
                last_edge_offset: (last != u32::MAX).then_some(last),
            }
        })
        .collect()
}

fn exact_channel_data(
    index: &DigitalChannelIndex,
    start: u64,
    end: u64,
) -> (Vec<DigitalSegment>, Vec<u64>, Vec<u64>) {
    let first_edge = index.first_edge_after(start);
    let edge_end = index.first_edge_at_or_after(end);
    let mut segments = Vec::with_capacity(edge_end.saturating_sub(first_edge) + 1);
    let mut rising = Vec::new();
    let mut falling = Vec::new();
    let mut run_start = start;
    let mut high = index.level_at(start);

    for edge_index in first_edge..edge_end {
        let edge = index.edge(edge_index);
        segments.push(DigitalSegment {
            start_sample: run_start,
            end_sample: edge,
            high,
        });
        high = index.level_after_edge(edge_index);
        if high {
            rising.push(edge);
        } else {
            falling.push(edge);
        }
        run_start = edge;
    }
    segments.push(DigitalSegment {
        start_sample: run_start,
        end_sample: end,
        high,
    });
    (segments, rising, falling)
}

fn sampled_channel_edges(
    index: &DigitalChannelIndex,
    start: u64,
    end: u64,
) -> (Vec<u64>, Vec<u64>) {
    let begin = index.first_edge_after(start);
    let finish = index.first_edge_at_or_after(end);
    let count = finish.saturating_sub(begin);
    if count == 0 {
        return (Vec::new(), Vec::new());
    }
    let stride = count.div_ceil(MAX_COARSE_EDGE_MARKERS).max(1);
    let mut rising = Vec::with_capacity(count.min(MAX_COARSE_EDGE_MARKERS));
    let mut falling = Vec::with_capacity(count.min(MAX_COARSE_EDGE_MARKERS));
    let mut edge_index = begin;
    while edge_index < finish {
        let edge = index.edge(edge_index);
        if index.level_after_edge(edge_index) {
            rising.push(edge);
        } else {
            falling.push(edge);
        }
        edge_index = edge_index.saturating_add(stride);
    }
    (rising, falling)
}

#[derive(Debug, Clone)]
struct BubbleAccumulator {
    bubble: AnalyzerBubble,
    last_frame_end_sample: u64,
    last_frame_width_px: f64,
}

fn render_analyzer_track(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    waveform: &WaveformRequest,
) -> AnalyzerTrackWindow {
    let start = waveform.start_sample;
    let end = start.saturating_add(waveform.sample_count);
    let begin = lower_bound_frame_end(frames, start);
    let finish = upper_bound_frame_start(frames, end, begin);
    let visible_count = finish.saturating_sub(begin);

    let bubbles = if visible_count > DENSE_BUBBLE_THRESHOLD {
        build_dense_bubbles(frames, track, begin, finish)
    } else {
        build_pixel_bubbles(
            frames,
            track,
            begin,
            finish,
            waveform.sample_count,
            waveform.pixels,
        )
    };
    let visible_frames = (begin..finish)
        .filter_map(|index| frame_for_track(&frames[index], track))
        .take(MAX_ANALYZER_MARKER_RESULTS)
        .collect::<Vec<_>>();
    let previous_frame = (0..begin)
        .rev()
        .find_map(|index| frame_for_track(&frames[index], track));
    let next_start = if visible_count > MAX_ANALYZER_MARKER_RESULTS {
        begin
            .saturating_add(MAX_ANALYZER_MARKER_RESULTS)
            .min(frames.len())
    } else {
        finish
    };
    let next_frame =
        (next_start..frames.len()).find_map(|index| frame_for_track(&frames[index], track));
    let protocol_markers = protocol_markers_for_track(frames, track, begin, finish);

    AnalyzerTrackWindow {
        channel: track.channel,
        role: track.role.clone(),
        bubbles,
        protocol_markers,
        frames: visible_frames,
        previous_frame,
        next_frame,
    }
}

fn render_analyzer_tracks(
    frames: &[DecodedFrame],
    tracks: &[AnalyzerTrackRequest],
    waveform: &WaveformRequest,
) -> Result<Vec<AnalyzerTrackWindow>> {
    if tracks.len() <= 1 {
        return Ok(tracks
            .iter()
            .map(|track| render_analyzer_track(frames, track, waveform))
            .collect());
    }

    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(MAX_PARALLEL_ANALYZER_TRACK_RENDERS)
        .min(tracks.len());
    if worker_count <= 1 {
        return Ok(tracks
            .iter()
            .map(|track| render_analyzer_track(frames, track, waveform))
            .collect());
    }

    let chunk_size = tracks.len().div_ceil(worker_count);
    let worker_outputs = tracks
        .par_chunks(chunk_size)
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let start_index = chunk_index * chunk_size;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                chunk
                    .iter()
                    .enumerate()
                    .map(|(index, track)| {
                        (
                            start_index + index,
                            render_analyzer_track(frames, track, waveform),
                        )
                    })
                    .collect::<Vec<_>>()
            }))
            .map_err(|_| GraphError::DecoderBackend("analyzer track worker panicked".to_string()))
        })
        .collect::<Vec<_>>();

    let mut indexed_results: Vec<(usize, AnalyzerTrackWindow)> = Vec::with_capacity(tracks.len());
    for output in worker_outputs {
        for (index, result) in output? {
            indexed_results.push((index, result));
        }
    }
    indexed_results.sort_by_key(|(index, _)| *index);
    Ok(indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect())
}

fn analyzer_frame_table_window_from_sources(
    sources: &[AnalyzerFrameTableSource],
    start: usize,
    end: usize,
) -> AnalyzerFrameTableWindow {
    let row_count = sources
        .iter()
        .map(|source| source.frames.len())
        .sum::<usize>();
    let roles = analyzer_frame_table_roles(sources);
    let start = start.min(row_count);
    let end = end.min(row_count).max(start);
    if start >= end || sources.is_empty() {
        return AnalyzerFrameTableWindow {
            roles,
            row_count,
            rows: Vec::new(),
        };
    }

    if sources.len() == 1 {
        let source = &sources[0];
        return AnalyzerFrameTableWindow {
            roles,
            row_count,
            rows: source.frames[start..end]
                .iter()
                .enumerate()
                .map(|(offset, frame)| analyzer_frame_table_row(source, start + offset, frame))
                .collect(),
        };
    }

    let mut positions = vec![0usize; sources.len()];
    let mut row_index = 0usize;
    let mut rows = Vec::with_capacity(end - start);
    while row_index < end {
        let Some(source_index) = next_analyzer_table_source_index(sources, &positions) else {
            break;
        };
        let frame_position = positions[source_index];
        let frame = &sources[source_index].frames[frame_position];
        if row_index >= start {
            rows.push(analyzer_frame_table_row(
                &sources[source_index],
                frame_position,
                frame,
            ));
        }
        positions[source_index] += 1;
        row_index += 1;
    }

    AnalyzerFrameTableWindow {
        roles,
        row_count,
        rows,
    }
}

fn analyzer_frame_table_roles(sources: &[AnalyzerFrameTableSource]) -> Vec<String> {
    let mut roles = Vec::new();
    for source in sources {
        for frame in source.frames.iter() {
            for value in &frame.channel_values {
                if !roles.iter().any(|role| role == &value.role) {
                    roles.push(value.role.clone());
                }
            }
        }
    }
    roles
}

fn next_analyzer_table_source_index(
    sources: &[AnalyzerFrameTableSource],
    positions: &[usize],
) -> Option<usize> {
    let mut best_source_index = None;
    let mut best_frame: Option<&DecodedFrame> = None;
    for (source_index, source) in sources.iter().enumerate() {
        let Some(frame) = source.frames.get(positions[source_index]) else {
            continue;
        };
        if best_frame.is_none_or(|candidate| {
            frame.start_sample < candidate.start_sample
                || (frame.start_sample == candidate.start_sample
                    && source_index < best_source_index.unwrap_or(usize::MAX))
        }) {
            best_source_index = Some(source_index);
            best_frame = Some(frame);
        }
    }
    best_source_index
}

fn analyzer_frame_table_row(
    source: &AnalyzerFrameTableSource,
    frame_position: usize,
    frame: &DecodedFrame,
) -> AnalyzerFrameTableRow {
    AnalyzerFrameTableRow {
        analyzer_id: source.analyzer_id.clone(),
        frame: frame.clone(),
        frame_index: frame.frame_id,
        frame_number: frame_position + 1,
    }
}

fn protocol_markers_for_track(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
) -> Vec<AnalyzerProtocolMarker> {
    let (ordered_count, ordered) =
        ordered_protocol_marker_count_for_track(frames, track, begin, end);
    if ordered_count > 0 && ordered {
        return collect_ordered_limited_protocol_markers_for_track(
            frames,
            track,
            begin,
            end,
            ordered_count,
        );
    }

    let mut markers = Vec::new();
    visit_protocol_markers_for_track(frames, track, begin, end, |marker| markers.push(marker));
    if markers.is_empty() && track.role.eq_ignore_ascii_case("sda") {
        for frame in &frames[begin..end] {
            let kind = if frame.frame_type.eq_ignore_ascii_case("start") {
                AnalyzerProtocolMarkerKind::Start
            } else if frame.frame_type.eq_ignore_ascii_case("stop") {
                AnalyzerProtocolMarkerKind::Stop
            } else {
                continue;
            };
            markers.push(AnalyzerProtocolMarker {
                sample: frame.start_sample,
                kind,
            });
        }
    }
    normalize_analyzer_protocol_markers(&mut markers);
    limit_analyzer_protocol_markers(markers)
}

fn visit_protocol_markers_for_track(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
    mut visit: impl FnMut(AnalyzerProtocolMarker),
) {
    for frame in &frames[begin..end] {
        for marker in &frame.protocol_markers {
            if marker.channel != Some(track.channel) {
                continue;
            }
            let Some(kind) = analyzer_protocol_marker_kind(marker.kind.as_str()) else {
                continue;
            };
            visit(AnalyzerProtocolMarker {
                sample: marker.sample,
                kind,
            });
        }
    }
}

fn analyzer_protocol_marker_kind(kind: &str) -> Option<AnalyzerProtocolMarkerKind> {
    Some(match kind {
        "dot" => AnalyzerProtocolMarkerKind::Dot,
        "error_dot" => AnalyzerProtocolMarkerKind::ErrorDot,
        "square" => AnalyzerProtocolMarkerKind::Square,
        "error_square" => AnalyzerProtocolMarkerKind::ErrorSquare,
        "up_arrow" => AnalyzerProtocolMarkerKind::UpArrow,
        "down_arrow" => AnalyzerProtocolMarkerKind::DownArrow,
        "x" => AnalyzerProtocolMarkerKind::X,
        "error_x" => AnalyzerProtocolMarkerKind::ErrorX,
        "start" => AnalyzerProtocolMarkerKind::Start,
        "stop" => AnalyzerProtocolMarkerKind::Stop,
        "one" => AnalyzerProtocolMarkerKind::One,
        "zero" => AnalyzerProtocolMarkerKind::Zero,
        _ => return None,
    })
}

fn ordered_protocol_marker_count_for_track(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
) -> (usize, bool) {
    let mut count = 0usize;
    let mut ordered = true;
    let mut previous: Option<AnalyzerProtocolMarker> = None;
    visit_protocol_markers_for_track(frames, track, begin, end, |marker| {
        if let Some(previous_marker) = previous {
            if marker.sample < previous_marker.sample {
                ordered = false;
            }
            if marker == previous_marker {
                return;
            }
        }
        previous = Some(marker);
        count += 1;
    });
    (count, ordered)
}

fn collect_ordered_limited_protocol_markers_for_track(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
    marker_count: usize,
) -> Vec<AnalyzerProtocolMarker> {
    let target_count = marker_count.min(MAX_ANALYZER_MARKER_RESULTS);
    if target_count == 0 {
        return Vec::new();
    }
    let mut markers = Vec::with_capacity(target_count);
    let last = marker_count - 1;
    let mut next_output_index = 0usize;
    let mut next_target_index = 0usize;
    let mut unique_index = 0usize;
    let mut previous: Option<AnalyzerProtocolMarker> = None;
    visit_protocol_markers_for_track(frames, track, begin, end, |marker| {
        if previous == Some(marker) {
            return;
        }
        previous = Some(marker);
        if unique_index == next_target_index {
            markers.push(marker);
            next_output_index += 1;
            next_target_index = if next_output_index >= target_count {
                usize::MAX
            } else if target_count == 1 {
                0
            } else {
                next_output_index.saturating_mul(last) / (target_count - 1)
            };
        }
        unique_index += 1;
    });
    markers
}

fn normalize_analyzer_protocol_markers(markers: &mut Vec<AnalyzerProtocolMarker>) {
    if markers.len() <= 1 {
        return;
    }
    if !analyzer_protocol_markers_are_ordered(markers) {
        markers.sort_by_key(|marker| marker.sample);
    }
    markers.dedup();
}

fn analyzer_protocol_markers_are_ordered(markers: &[AnalyzerProtocolMarker]) -> bool {
    markers
        .windows(2)
        .all(|pair| pair[0].sample <= pair[1].sample)
}

fn limit_analyzer_protocol_markers(
    markers: Vec<AnalyzerProtocolMarker>,
) -> Vec<AnalyzerProtocolMarker> {
    if markers.len() <= MAX_ANALYZER_MARKER_RESULTS {
        return markers;
    }
    let last = markers.len() - 1;
    (0..MAX_ANALYZER_MARKER_RESULTS)
        .map(|index| markers[index * last / (MAX_ANALYZER_MARKER_RESULTS - 1)])
        .collect()
}

fn build_pixel_bubbles(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
    viewport_samples: u64,
    width_px: u32,
) -> Vec<AnalyzerBubble> {
    let safe_samples = viewport_samples.max(1) as f64;
    let width = width_px.max(1) as f64;
    let mut accumulators: Vec<BubbleAccumulator> = Vec::new();
    for frame in &frames[begin..end] {
        let Some(frame) = display_frame_for_track(frame, track) else {
            continue;
        };
        let frame_width_px =
            frame.end_sample.saturating_sub(frame.start_sample) as f64 / safe_samples * width;
        let should_merge = accumulators.last().is_some_and(|previous| {
            let gap_px = frame
                .start_sample
                .saturating_sub(previous.last_frame_end_sample) as f64
                / safe_samples
                * width;
            frame_width_px <= MAX_PIXEL_WIDTH_FOR_MERGEABLE_FRAME
                && previous.last_frame_width_px <= MAX_PIXEL_WIDTH_FOR_MERGEABLE_FRAME
                && gap_px <= MAX_PIXELS_BETWEEN_MERGEABLE_FRAMES
        });

        if should_merge {
            let previous = accumulators.last_mut().expect("checked above");
            previous.bubble.bubble_type = AnalyzerBubbleType::MultiBubble;
            previous.bubble.end_sample = previous.bubble.end_sample.max(frame.end_sample);
            previous.bubble.frame_id_interval.end = previous
                .bubble
                .frame_id_interval
                .end
                .max(frame.frame_id.saturating_add(1));
            previous.bubble.result_count = previous.bubble.result_count.saturating_add(1);
            if previous.bubble.display_text.len() < MAX_MULTI_BUBBLE_STRINGS {
                previous.bubble.display_text.push(frame.label.to_string());
            }
            previous.last_frame_end_sample = frame.end_sample;
            previous.last_frame_width_px = frame_width_px;
        } else {
            accumulators.push(BubbleAccumulator {
                bubble: AnalyzerBubble {
                    bubble_type: AnalyzerBubbleType::SingleBubble,
                    frame_id_interval: FrameIdInterval {
                        begin: frame.frame_id,
                        end: frame.frame_id.saturating_add(1),
                    },
                    start_sample: frame.start_sample,
                    end_sample: frame.end_sample,
                    display_text: if frame.texts.is_empty() {
                        vec![frame.label.to_string()]
                    } else {
                        frame.texts.to_vec()
                    },
                    result_count: 1,
                },
                last_frame_end_sample: frame.end_sample,
                last_frame_width_px: frame_width_px,
            });
        }
    }
    enforce_maximum_bubbles(accumulators.into_iter().map(|item| item.bubble).collect())
}

fn build_dense_bubbles(
    frames: &[DecodedFrame],
    track: &AnalyzerTrackRequest,
    begin: usize,
    end: usize,
) -> Vec<AnalyzerBubble> {
    let count = end.saturating_sub(begin);
    if count == 0 {
        return Vec::new();
    }
    let bucket_size = count.div_ceil(MAX_ANALYZER_MARKER_RESULTS).max(1);
    let mut bubbles = Vec::with_capacity(count.div_ceil(bucket_size));
    let mut bucket_begin = begin;
    while bucket_begin < end {
        let bucket_end = (bucket_begin + bucket_size).min(end);
        let mut first = None;
        let mut last = None;
        let mut display_text = Vec::new();
        let mut result_count = 0u64;
        for frame in &frames[bucket_begin..bucket_end] {
            let Some(frame) = display_frame_for_track(frame, track) else {
                continue;
            };
            first.get_or_insert(frame);
            last = Some(frame);
            result_count = result_count.saturating_add(1);
            if display_text.len() < 12 {
                display_text.push(frame.label.to_string());
            }
        }
        if let (Some(first), Some(last)) = (first, last) {
            bubbles.push(AnalyzerBubble {
                bubble_type: if result_count == 1 {
                    AnalyzerBubbleType::SingleBubble
                } else {
                    AnalyzerBubbleType::MultiBubble
                },
                frame_id_interval: FrameIdInterval {
                    begin: first.frame_id,
                    end: last.frame_id.saturating_add(1),
                },
                start_sample: first.start_sample,
                end_sample: last.end_sample,
                display_text,
                result_count,
            });
        }
        bucket_begin = bucket_end;
    }
    bubbles
}

fn enforce_maximum_bubbles(bubbles: Vec<AnalyzerBubble>) -> Vec<AnalyzerBubble> {
    if bubbles.len() <= MAX_ANALYZER_MARKER_RESULTS {
        return bubbles;
    }
    let bucket_size = bubbles.len().div_ceil(MAX_ANALYZER_MARKER_RESULTS);
    bubbles
        .chunks(bucket_size)
        .map(merge_bubbles)
        .collect::<Vec<_>>()
}

fn merge_bubbles(bubbles: &[AnalyzerBubble]) -> AnalyzerBubble {
    let first = bubbles.first().expect("bubble bucket is not empty");
    let last = bubbles.last().expect("bubble bucket is not empty");
    let display_text = bubbles
        .iter()
        .flat_map(|bubble| bubble.display_text.iter().cloned())
        .take(MAX_MULTI_BUBBLE_STRINGS)
        .collect();
    AnalyzerBubble {
        bubble_type: AnalyzerBubbleType::MultiBubble,
        frame_id_interval: FrameIdInterval {
            begin: first.frame_id_interval.begin,
            end: last.frame_id_interval.end,
        },
        start_sample: first.start_sample,
        end_sample: last.end_sample,
        display_text,
        result_count: bubbles.iter().map(|bubble| bubble.result_count).sum(),
    }
}

#[derive(Debug, Clone, Copy)]
struct DisplayFrame<'a> {
    frame_id: u64,
    start_sample: u64,
    end_sample: u64,
    label: &'a str,
    texts: &'a [String],
}

fn display_frame_for_track<'a>(
    frame: &'a DecodedFrame,
    track: &AnalyzerTrackRequest,
) -> Option<DisplayFrame<'a>> {
    let label = channel_value_for_track(frame, track)?.label.as_str();
    Some(DisplayFrame {
        frame_id: frame.frame_id,
        start_sample: frame.start_sample,
        end_sample: frame.end_sample,
        label,
        texts: &channel_value_for_track(frame, track)?.texts,
    })
}

fn frame_for_track(frame: &DecodedFrame, track: &AnalyzerTrackRequest) -> Option<DecodedFrame> {
    let value = channel_value_for_track(frame, track)?;
    let mut result = frame.clone();
    result.label = value.label.clone();
    result.value = value.value;
    result.channel_values = vec![value.clone()];
    Some(result)
}

fn channel_value_for_track<'a>(
    frame: &'a DecodedFrame,
    track: &AnalyzerTrackRequest,
) -> Option<&'a DecodedChannelValue> {
    frame
        .channel_values
        .iter()
        .find(|value| value.channel == track.channel && value.role == track.role)
        .or_else(|| {
            frame
                .channel_values
                .iter()
                .find(|value| value.channel == track.channel)
        })
}

fn lower_bound_frame_end(frames: &[DecodedFrame], sample: u64) -> usize {
    frames.partition_point(|frame| frame.end_sample < sample)
}

fn upper_bound_frame_start(frames: &[DecodedFrame], sample: u64, low: usize) -> usize {
    low + frames[low..].partition_point(|frame| frame.start_sample <= sample)
}

fn append_spi_bit(value: u64, bit: bool, bit_index: u8, bits: u8, msb_first: bool) -> u64 {
    if msb_first {
        (value << 1) | u64::from(bit)
    } else {
        value | (u64::from(bit) << u32::from(bit_index.min(bits - 1)))
    }
}

fn label_for_value(value: u64, bits: u8) -> String {
    let digits = usize::from(bits.div_ceil(4));
    format!("0x{value:0digits$X}")
}

#[derive(Default)]
struct FrameCollector {
    frames: Vec<DecodedFrame>,
    last_frame: Option<DecodedFrame>,
    seen: u64,
    stride: u64,
}

impl FrameCollector {
    fn push(&mut self, mut frame: DecodedFrame) {
        if self.stride == 0 {
            self.stride = 1;
        }
        frame.frame_id = self.seen;
        self.seen = self.seen.saturating_add(1);
        self.last_frame = Some(frame.clone());
        if frame.frame_id % self.stride != 0 {
            return;
        }
        self.frames.push(frame);
        if self.frames.len() > MAX_DECODED_FRAMES {
            self.frames = self
                .frames
                .drain(..)
                .enumerate()
                .filter_map(|(index, frame)| (index % 2 == 0).then_some(frame))
                .collect();
            self.stride = self.stride.saturating_mul(2);
        }
    }

    fn finish(mut self) -> Vec<DecodedFrame> {
        if let Some(last) = self.last_frame {
            if self.frames.last().map(|frame| frame.frame_id) != Some(last.frame_id) {
                if self.frames.len() >= MAX_DECODED_FRAMES {
                    self.frames.pop();
                }
                self.frames.push(last);
            }
        }
        self.frames
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use chrono::Utc;
    use pxlogic_core::{
        capture::generate_sample_words_with_count, decode_analyzer, CaptureSettings,
        SparseCaptureData, SparseDigitalChannel,
    };

    use super::*;

    fn demo_capture(channel_count: u8, sample_count: u64) -> CaptureData {
        let mut settings = CaptureSettings::default();
        settings.channel_count = channel_count;
        let unit_size = if channel_count > 16 {
            4
        } else if channel_count > 8 {
            2
        } else {
            1
        };
        generate_sample_words_with_count(&settings, sample_count, unit_size).unwrap()
    }

    fn decoded_frame_for_graph_test(frame_id: u64, start_sample: u64) -> DecodedFrame {
        DecodedFrame {
            frame_id,
            start_sample,
            end_sample: start_sample + 10,
            frame_type: "data".to_string(),
            label: format!("frame-{frame_id}"),
            value: frame_id,
            channel_values: vec![DecodedChannelValue {
                channel: 0,
                role: "Data".to_string(),
                label: format!("frame-{frame_id}"),
                texts: vec![format!("frame-{frame_id}")],
                value: frame_id,
            }],
            protocol_markers: Vec::new(),
        }
    }

    #[test]
    fn capture_memory_usage_tracks_samples_and_edge_index() {
        let capture = demo_capture(8, 80_000);
        let session = GraphSession::default();
        session.begin_capture(capture.metadata.clone()).unwrap();
        session.append_samples(&capture.samples).unwrap();

        let usage =
            session.capture_memory_usage(512 * 1024 * 1024, Some(capture.samples.len() as u64));
        assert_eq!(usage.buffer_bytes, 512 * 1024 * 1024);
        assert_eq!(usage.sample_bytes, capture.samples.len() as u64);
        assert!(usage.index_bytes > 0);
        assert_eq!(usage.analyzer_bytes, 0);
        assert_eq!(
            usage.total_bytes,
            usage.sample_bytes + usage.index_bytes + usage.analyzer_bytes
        );

        session.finish_capture(capture.clone()).unwrap();
        let finalized = session.capture_memory_usage(512 * 1024 * 1024, None);
        assert_eq!(finalized.sample_bytes, capture.samples.capacity() as u64);
        assert!(finalized.sample_bytes >= capture.samples.len() as u64);
        assert_eq!(finalized.index_bytes, usage.index_bytes);
    }

    #[test]
    fn indexed_tile_matches_scanner() {
        let capture = demo_capture(8, 80_000);
        let graph = DigitalGraph::from_capture(&capture).unwrap();
        for request in [
            WaveformRequest {
                start_sample: 0,
                sample_count: 80_000,
                pixels: 800,
                channels: vec![],
            },
            WaveformRequest {
                start_sample: 12_345,
                sample_count: 2_048,
                pixels: 600,
                channels: vec![0, 1, 3, 7],
            },
        ] {
            let indexed = graph.build_tile(&request).unwrap();
            let scanned = pxlogic_waveform::build_tile(&capture, &request).unwrap();
            assert_eq!(indexed.start_sample, scanned.start_sample);
            assert_eq!(indexed.sample_count, scanned.sample_count);
            assert_eq!(indexed.pixels, scanned.pixels);
            assert_eq!(indexed.samples_per_pixel, scanned.samples_per_pixel);
            assert_eq!(indexed.channels.len(), scanned.channels.len());
            for (indexed_channel, scanned_channel) in indexed.channels.iter().zip(&scanned.channels)
            {
                assert_eq!(indexed_channel.channel, scanned_channel.channel);
                assert_eq!(indexed_channel.label, scanned_channel.label);
                assert_eq!(
                    unpack_digital_bins(indexed_channel.packed_bins.as_deref().unwrap()),
                    scanned_channel.bins
                );
                assert_eq!(indexed_channel.segments, scanned_channel.segments);
                assert_eq!(
                    indexed_channel.previous_transition,
                    scanned_channel.previous_transition
                );
                assert_eq!(
                    indexed_channel.next_transition,
                    scanned_channel.next_transition
                );
                if !scanned_channel.rising_edges.is_empty()
                    || !scanned_channel.falling_edges.is_empty()
                {
                    assert_eq!(indexed_channel.rising_edges, scanned_channel.rising_edges);
                    assert_eq!(indexed_channel.falling_edges, scanned_channel.falling_edges);
                }
            }
        }
    }

    #[test]
    fn packed_trim_reuses_the_existing_edge_index() {
        let original = demo_capture(8, 10_000);
        let unit_size = usize::from(original.metadata.unitsize);
        let expected_samples = original.samples[2_500 * unit_size..9_000 * unit_size].to_vec();
        let session = GraphSession::default();
        session.replace_capture(original).unwrap();

        let (metadata, bytes) = session.trim_capture(2_500, 9_000).unwrap();
        let capture = session.capture().unwrap();
        assert_eq!(metadata.sample_count, 6_500);
        assert_eq!(bytes, expected_samples.len());
        assert_eq!(capture.samples, expected_samples);

        let rebuilt = DigitalGraph::from_capture(&capture).unwrap();
        let indexed = session.graph.read();
        let indexed = indexed.as_ref().unwrap();
        assert_eq!(indexed.metadata, rebuilt.metadata);
        assert_eq!(indexed.sample_count, rebuilt.sample_count);
        for (trimmed, expected) in indexed.channels.iter().zip(&rebuilt.channels) {
            assert_eq!(trimmed.initial_high, expected.initial_high);
            assert_eq!(trimmed.edges, expected.edges);
        }
    }

    #[test]
    fn sparse_trim_updates_initial_levels_and_transition_offsets() {
        let mut metadata = demo_capture(2, 1_000).metadata;
        metadata.sample_count = 1_000;
        let sparse = SparseCaptureData {
            metadata,
            channels: vec![
                SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: vec![100, 300, 700, 900],
                },
                SparseDigitalChannel {
                    channel: 1,
                    initial_high: true,
                    transitions: vec![200, 400, 600, 800],
                },
            ],
        };
        let session = GraphSession::default();
        session.replace_sparse_capture(sparse).unwrap();

        let (metadata, bytes) = session.trim_capture(250, 800).unwrap();
        let capture = session.sparse_capture().unwrap();
        assert_eq!(metadata.sample_count, 550);
        assert_eq!(bytes, 4 * 8);
        assert!(capture.channels[0].initial_high);
        assert_eq!(capture.channels[0].transitions, vec![50, 450]);
        assert!(!capture.channels[1].initial_high);
        assert_eq!(capture.channels[1].transitions, vec![150, 350]);

        let rebuilt = DigitalGraph::from_sparse_capture(&capture).unwrap();
        let indexed = session.graph.read();
        let indexed = indexed.as_ref().unwrap();
        for (trimmed, expected) in indexed.channels.iter().zip(&rebuilt.channels) {
            assert_eq!(trimmed.initial_high, expected.initial_high);
            assert_eq!(trimmed.edges, expected.edges);
        }
    }

    #[test]
    fn nearest_transition_uses_the_index_without_a_viewport_tile() {
        let mut metadata = demo_capture(1, 1_000).metadata;
        metadata.sample_count = 1_000;
        let session = GraphSession::default();
        session
            .replace_sparse_capture(SparseCaptureData {
                metadata,
                channels: vec![SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: vec![100, 300, 700, 900],
                }],
            })
            .unwrap();

        assert_eq!(
            session.nearest_transition(0, 260).unwrap(),
            Some(NearestDigitalTransition {
                previous_sample: Some(100),
                sample: 300,
                next_sample: Some(700),
            }),
        );
        assert_eq!(
            session.nearest_transition(0, 995).unwrap(),
            Some(NearestDigitalTransition {
                previous_sample: Some(700),
                sample: 900,
                next_sample: None,
            }),
        );
    }

    #[test]
    fn point_measurement_uses_the_full_edge_index_outside_the_viewport() {
        let mut metadata = demo_capture(1, 1_000).metadata;
        metadata.sample_count = 1_000;
        let session = GraphSession::default();
        session
            .replace_sparse_capture(SparseCaptureData {
                metadata,
                channels: vec![SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: vec![100, 300, 700, 900],
                }],
            })
            .unwrap();

        assert_eq!(
            session.measure_point(0, 500).unwrap(),
            Some(DigitalPointMeasurement {
                channel: 0,
                cursor_sample: 500,
                high: false,
                width_start_sample: 300,
                width_end_sample: 700,
                period_end_sample: Some(900),
            }),
        );
        assert_eq!(session.measure_point(0, 50).unwrap(), None);
        assert_eq!(session.measure_point(0, 950).unwrap(), None);
    }

    #[test]
    fn incremental_append_matches_one_shot_and_supports_channel_31() {
        let words = [0u32, 1u32 << 31, (1u32 << 31) | 1, 0];
        let samples = words
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let metadata = CaptureMetadata {
            version: 1,
            source_device: "test-32".to_string(),
            sample_rate_hz: 25_000_000,
            channel_count: 32,
            enabled_channels: (0..32).collect(),
            unitsize: 4,
            sample_count: words.len() as u64,
            captured_at: Utc::now(),
            labels: (0..32).map(|channel| format!("D{channel}")).collect(),
            trigger: None,
        };
        let capture = CaptureData {
            metadata: metadata.clone(),
            samples: samples.clone(),
        };
        let one_shot = DigitalGraph::from_capture(&capture).unwrap();
        let mut streaming = DigitalGraph::new(metadata).unwrap();
        streaming.append_interleaved(&samples[..8]).unwrap();
        streaming.append_interleaved(&samples[8..]).unwrap();

        let request = WaveformRequest {
            start_sample: 0,
            sample_count: 4,
            pixels: 4,
            channels: vec![31],
        };
        assert_eq!(
            one_shot.build_tile(&request).unwrap(),
            streaming.build_tile(&request).unwrap()
        );
        let tile = streaming.build_tile(&request).unwrap();
        assert_eq!(tile.channels[0].rising_edges, vec![1]);
        assert_eq!(tile.channels[0].falling_edges, vec![3]);

        let navigation_tile = streaming
            .build_tile(&WaveformRequest {
                start_sample: 2,
                sample_count: 1,
                pixels: 1,
                channels: vec![31],
            })
            .unwrap();
        assert_eq!(navigation_tile.channels[0].previous_transition, Some(1));
        assert_eq!(navigation_tile.channels[0].next_transition, Some(3));
    }

    #[test]
    fn packed_cross_lane_append_matches_interleaved_index() {
        let enabled_channels = vec![0, 4, 31];
        let lane_words = [
            0x00ff_00ff_00ff_00ffu64,
            0xaaaa_aaaa_5555_5555u64,
            0x8000_0000_0000_0001u64,
            0xff00_ff00_ff00_ff00u64,
            0x5555_5555_aaaa_aaaau64,
            0x7fff_ffff_ffff_fffeu64,
        ];
        let packed = lane_words
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>();
        let interleaved = pxlogic_core::decode_cross_data_to_physical_channels(
            32,
            &enabled_channels,
            &packed,
            false,
        )
        .unwrap();
        let metadata = CaptureMetadata {
            version: 1,
            source_device: "packed-cross-test".to_string(),
            sample_rate_hz: 100_000_000,
            channel_count: 32,
            enabled_channels: enabled_channels.clone(),
            unitsize: 4,
            sample_count: 0,
            captured_at: Utc::now(),
            labels: (0..32).map(|channel| format!("D{channel}")).collect(),
            trigger: None,
        };
        let mut expected = DigitalGraph::new(metadata.clone()).unwrap();
        expected.append_interleaved(&interleaved).unwrap();
        let mut packed_graph = DigitalGraph::new(metadata).unwrap();
        packed_graph
            .append_cross_lanes(&enabled_channels, &packed[..24], false)
            .unwrap();
        packed_graph
            .append_cross_lanes(&enabled_channels, &packed[24..], false)
            .unwrap();

        assert_eq!(packed_graph.sample_count(), 128);
        for channel in [0, 4, 31] {
            let request = WaveformRequest {
                start_sample: 0,
                sample_count: 128,
                pixels: 128,
                channels: vec![channel],
            };
            assert_eq!(
                packed_graph.build_tile(&request).unwrap(),
                expected.build_tile(&request).unwrap()
            );
        }
    }

    #[test]
    fn sparse_graph_indexes_saleae_capture_beyond_u32_samples() {
        let sample_count = 49_940_179_616;
        let metadata = CaptureMetadata {
            version: 1,
            source_device: "Saleae demo.sal".to_string(),
            sample_rate_hz: 125_000_000,
            channel_count: 8,
            enabled_channels: vec![3],
            unitsize: 1,
            sample_count,
            captured_at: Utc::now(),
            labels: (0..8).map(|channel| format!("D{channel}")).collect(),
            trigger: None,
        };
        let capture = SparseCaptureData {
            metadata,
            channels: vec![SparseDigitalChannel {
                channel: 3,
                initial_high: false,
                transitions: vec![10, 4_294_967_300, 49_000_000_000],
            }],
        };
        let graph = DigitalGraph::from_sparse_capture(&capture).unwrap();
        let tile = graph
            .build_tile(&WaveformRequest {
                start_sample: 4_294_967_200,
                sample_count: 300,
                pixels: 300,
                channels: vec![3],
            })
            .unwrap();
        assert_eq!(tile.sample_count, 300);
        assert_eq!(tile.channels[0].falling_edges, vec![4_294_967_300]);
        assert_eq!(tile.channels[0].previous_transition, Some(10));
        assert_eq!(tile.channels[0].next_transition, Some(49_000_000_000));
    }

    #[test]
    fn incremental_finish_preserves_hardware_trigger_metadata() {
        let mut capture = demo_capture(4, 4_096);
        let session = GraphSession::default();
        let mut starting_metadata = capture.metadata.clone();
        starting_metadata.sample_count = 0;
        session.begin_capture(starting_metadata).unwrap();
        session.append_samples(&capture.samples).unwrap();

        let trigger = CaptureTriggerMetadata {
            sample_index: 2_048,
            channel: 1,
            kind: pxlogic_core::CaptureTriggerKind::Rising,
        };
        session.set_trigger(trigger.clone()).unwrap();
        assert_eq!(session.metadata().unwrap().trigger, Some(trigger.clone()));

        capture.metadata.trigger = Some(trigger.clone());
        session.finish_capture(capture).unwrap();
        assert_eq!(session.metadata().unwrap().trigger, Some(trigger));
    }

    #[test]
    fn edge_decoders_match_reference_decoders() {
        let capture = demo_capture(16, 250_000);
        let graph = DigitalGraph::from_capture(&capture).unwrap();
        for settings in [
            AnalyzerDecodeSettings::Uart(UartDecodeSettings::default()),
            AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
            AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
        ] {
            let indexed = graph.decode(&settings).unwrap();
            let scanned = decode_analyzer(&capture, &settings).unwrap();
            assert_eq!(indexed, scanned, "settings={settings:?}");
        }
    }

    #[test]
    fn analyzer_cache_extends_incrementally_during_capture() {
        let capture = demo_capture(16, 500_000);
        for settings in [
            AnalyzerDecodeSettings::Uart(UartDecodeSettings::default()),
            AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
            AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
        ] {
            let session = GraphSession::default();
            session.begin_capture(capture.metadata.clone()).unwrap();
            let split = capture.samples.len() / 2 / usize::from(capture.metadata.unitsize)
                * usize::from(capture.metadata.unitsize);
            session.append_samples(&capture.samples[..split]).unwrap();
            let first = session.decode_frames(&settings).unwrap();
            session.append_samples(&capture.samples[split..]).unwrap();
            let incremental = session.decode_frames(&settings).unwrap();
            let expected = DigitalGraph::from_capture(&capture)
                .unwrap()
                .decode(&settings)
                .unwrap();
            assert!(incremental.len() >= first.len(), "settings={settings:?}");
            assert_eq!(incremental.as_ref(), &expected, "settings={settings:?}");
        }
    }

    #[test]
    fn analyzer_decode_count_and_slice_avoid_full_vec_clones() {
        let capture = demo_capture(16, 250_000);
        let session = GraphSession::default();
        session.finish_capture(capture).unwrap();
        let settings = AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default());
        let frames = session
            .decode_frames_with_backend(&settings, DecoderBackendKind::LegacyRust)
            .unwrap();
        assert!(frames.len() > 6);

        assert_eq!(
            session
                .decode_frame_count_with_backend(&settings, DecoderBackendKind::LegacyRust)
                .unwrap(),
            frames.len(),
        );

        let (frame_count, slice) = session
            .decode_frame_slice_with_backend(&settings, DecoderBackendKind::LegacyRust, 2, 6)
            .unwrap();
        assert_eq!(frame_count, frames.len());
        assert_eq!(slice, frames[2..6]);

        let (frame_count, empty_tail) = session
            .decode_frame_slice_with_backend(
                &settings,
                DecoderBackendKind::LegacyRust,
                frames.len() + 10,
                frames.len() + 20,
            )
            .unwrap();
        assert_eq!(frame_count, frames.len());
        assert!(empty_tail.is_empty());
    }

    #[test]
    fn analyzer_frame_table_window_merges_sources_without_cloning_all_rows() {
        let sources = vec![
            AnalyzerFrameTableSource {
                analyzer_id: "a".to_string(),
                frames: Arc::new(vec![
                    decoded_frame_for_graph_test(10, 0),
                    decoded_frame_for_graph_test(11, 10),
                    decoded_frame_for_graph_test(12, 20),
                    decoded_frame_for_graph_test(13, 30),
                ]),
            },
            AnalyzerFrameTableSource {
                analyzer_id: "b".to_string(),
                frames: Arc::new(vec![
                    decoded_frame_for_graph_test(20, 5),
                    decoded_frame_for_graph_test(21, 10),
                    decoded_frame_for_graph_test(22, 25),
                ]),
            },
        ];

        let window = analyzer_frame_table_window_from_sources(&sources, 2, 6);

        assert_eq!(window.row_count, 7);
        assert_eq!(
            window
                .rows
                .iter()
                .map(|row| (
                    row.analyzer_id.as_str(),
                    row.frame.start_sample,
                    row.frame_number
                ))
                .collect::<Vec<_>>(),
            vec![("a", 10, 2), ("b", 10, 2), ("a", 20, 3), ("b", 25, 3)]
        );
    }

    #[test]
    fn fit_capture_bubbles_are_bounded_and_cover_the_capture() {
        let frames = (0..10_000u64)
            .map(|frame_id| DecodedFrame {
                frame_id,
                start_sample: frame_id * 100,
                end_sample: frame_id * 100 + 80,
                frame_type: "data".to_string(),
                label: format!("0x{:02X}", frame_id & 0xff),
                value: frame_id & 0xff,
                channel_values: vec![DecodedChannelValue {
                    channel: 0,
                    role: "Input".to_string(),
                    label: format!("0x{:02X}", frame_id & 0xff),
                    texts: vec![format!("0x{:02X}", frame_id & 0xff)],
                    value: frame_id & 0xff,
                }],
                protocol_markers: Vec::new(),
            })
            .collect::<Vec<_>>();
        let window = render_analyzer_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 0,
                role: "Input".to_string(),
            },
            &WaveformRequest {
                start_sample: 0,
                sample_count: 1_000_000,
                pixels: 1200,
                channels: vec![0],
            },
        );
        assert!(window.bubbles.len() <= MAX_ANALYZER_MARKER_RESULTS);
        assert_eq!(window.bubbles.first().unwrap().start_sample, 0);
        assert!(window.bubbles.last().unwrap().end_sample > 990_000);
    }

    #[test]
    fn native_decoded_frame_normalization_preserves_ordered_frames() {
        let mut frames = vec![
            decoded_frame_for_graph_test(0, 10),
            decoded_frame_for_graph_test(1, 20),
            decoded_frame_for_graph_test(1, 20),
            decoded_frame_for_graph_test(2, 40),
        ];
        normalize_native_decoded_frames(&mut frames);

        assert_eq!(
            frames
                .iter()
                .map(|frame| (frame.frame_id, frame.start_sample))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 20), (2, 40)]
        );
    }

    #[test]
    fn native_decoded_frame_normalization_sorts_unordered_frames() {
        let mut frames = vec![
            decoded_frame_for_graph_test(2, 40),
            decoded_frame_for_graph_test(0, 10),
            decoded_frame_for_graph_test(1, 20),
            decoded_frame_for_graph_test(1, 20),
        ];
        normalize_native_decoded_frames(&mut frames);

        assert_eq!(
            frames
                .iter()
                .map(|frame| (frame.frame_id, frame.start_sample))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 20), (2, 40)]
        );
    }

    #[test]
    fn sparse_native_fit_windows_are_bounded_and_span_the_view() {
        let clock_edges = (1..=100_000u64).map(|edge| edge * 100).collect::<Vec<_>>();
        let capture = SparseCaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "sparse-window-test".to_string(),
                sample_rate_hz: 100_000_000,
                channel_count: 4,
                enabled_channels: (0..4).collect(),
                unitsize: 1,
                sample_count: 10_000_100,
                captured_at: Utc::now(),
                labels: (0..4).map(|channel| format!("D{channel}")).collect(),
                trigger: None,
            },
            channels: vec![
                SparseDigitalChannel {
                    channel: 0,
                    initial_high: false,
                    transitions: Vec::new(),
                },
                SparseDigitalChannel {
                    channel: 1,
                    initial_high: false,
                    transitions: Vec::new(),
                },
                SparseDigitalChannel {
                    channel: 2,
                    initial_high: false,
                    transitions: clock_edges,
                },
                SparseDigitalChannel {
                    channel: 3,
                    initial_high: true,
                    transitions: Vec::new(),
                },
            ],
        };
        let settings = AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
            mosi_channel: Some(0),
            miso_channel: Some(1),
            clock_channel: 2,
            enable_channel: None,
            ..SpiDecodeSettings::default()
        });
        let view = WaveformRequest {
            start_sample: 0,
            sample_count: capture.metadata.sample_count,
            pixels: 1_200,
            channels: Vec::new(),
        };

        let windows =
            sparse_native_analysis_windows(&sparse_capture_view(&capture), &settings, &view)
                .unwrap();
        assert_eq!(windows.len(), 13);
        assert!(windows.first().unwrap().0 < 1_000);
        assert!(windows.last().unwrap().1 > 9_999_000);
        let selected_clock_edges = windows
            .iter()
            .map(|&(start, end)| {
                slice_sparse_capture(&capture, start, end).unwrap().channels[2]
                    .transitions
                    .len()
            })
            .sum::<usize>();
        assert!(selected_clock_edges < 25_000);
    }

    #[test]
    fn sparse_slice_preserves_level_and_offsets_transitions() {
        let capture = SparseCaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "sparse-slice-test".to_string(),
                sample_rate_hz: 1_000_000,
                channel_count: 1,
                enabled_channels: vec![0],
                unitsize: 1,
                sample_count: 1_000,
                captured_at: Utc::now(),
                labels: vec!["D0".to_string()],
                trigger: None,
            },
            channels: vec![SparseDigitalChannel {
                channel: 0,
                initial_high: false,
                transitions: vec![100, 200, 300, 400],
            }],
        };

        let slice = slice_sparse_capture(&capture, 150, 350).unwrap();
        assert!(slice.channels[0].initial_high);
        assert_eq!(slice.channels[0].transitions, vec![50, 150]);
        assert_eq!(slice.metadata.sample_count, 200);
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_spi_decodes_through_graph_session() {
        let capture = demo_capture(16, 250_000);
        let sample_count = capture.metadata.sample_count;
        let session = GraphSession::default();
        session.finish_capture(capture).unwrap();
        let settings = AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
            mosi_channel: Some(3),
            miso_channel: Some(4),
            clock_channel: 5,
            enable_channel: Some(6),
            ..SpiDecodeSettings::default()
        });
        let frames = session
            .decode_frames_with_backend_for_view(
                &settings,
                DecoderBackendKind::SaleaeNative,
                &WaveformRequest {
                    start_sample: 0,
                    sample_count,
                    pixels: 1_200,
                    channels: Vec::new(),
                },
            )
            .unwrap();
        assert!(!frames.is_empty());
        let zoomed_frames = session
            .decode_frames_with_backend_for_view(
                &settings,
                DecoderBackendKind::SaleaeNative,
                &WaveformRequest {
                    start_sample: sample_count / 4,
                    sample_count: sample_count / 8,
                    pixels: 1_200,
                    channels: Vec::new(),
                },
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&frames, &zoomed_frames));
        assert!(frames.iter().any(|frame| {
            frame
                .channel_values
                .iter()
                .any(|value| value.role == "MOSI")
        }));
        let track = render_analyzer_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 3,
                role: "MOSI".to_string(),
            },
            &WaveformRequest {
                start_sample: 0,
                sample_count,
                pixels: 1_200,
                channels: Vec::new(),
            },
        );
        assert!(!track.bubbles.is_empty());
    }

    #[test]
    #[ignore = "requires PXLOGIC_SALEAE_SIDECAR and PXLOGIC_SALEAE_RUNTIME_ROOT"]
    fn saleae_i2c_and_spi_decode_in_parallel_through_shared_edge_index() {
        let capture = demo_capture(16, 2_500_000);
        let sample_count = capture.metadata.sample_count;
        let session = GraphSession::default();
        session.finish_capture(capture).unwrap();
        let response = session
            .render_frame(&GraphFrameRequest {
                frame_id: 1,
                waveform: WaveformRequest {
                    start_sample: 0,
                    sample_count,
                    pixels: 1_200,
                    channels: vec![1, 2, 3, 4, 5, 6],
                },
                analyzer_view: None,
                analyzers: vec![
                    AnalyzerRenderRequest {
                        analyzer_id: "i2c".to_string(),
                        backend: DecoderBackendKind::SaleaeNative,
                        settings: AnalyzerDecodeSettings::I2c(I2cDecodeSettings {
                            sda_channel: 1,
                            scl_channel: 2,
                        }),
                        tracks: vec![AnalyzerTrackRequest {
                            channel: 1,
                            role: "SDA".to_string(),
                        }],
                    },
                    AnalyzerRenderRequest {
                        analyzer_id: "spi".to_string(),
                        backend: DecoderBackendKind::SaleaeNative,
                        settings: AnalyzerDecodeSettings::Spi(SpiDecodeSettings {
                            mosi_channel: Some(3),
                            miso_channel: Some(4),
                            clock_channel: 5,
                            enable_channel: Some(6),
                            ..SpiDecodeSettings::default()
                        }),
                        tracks: vec![AnalyzerTrackRequest {
                            channel: 3,
                            role: "MOSI".to_string(),
                        }],
                    },
                ],
            })
            .unwrap();
        assert_eq!(response.analyzers.len(), 2);
        assert!(response.analyzers.iter().all(|analyzer| analyzer
            .tracks
            .iter()
            .any(|track| !track.bubbles.is_empty())));
    }

    #[test]
    #[ignore = "requires a GraphServer manifest and both native analyzer sidecars"]
    fn imported_saleae_capture_restores_native_bubbles_and_table_frames() {
        let manifest = std::env::var_os("PXLOGIC_SALEAE_MANIFEST")
            .map(std::path::PathBuf::from)
            .expect("PXLOGIC_SALEAE_MANIFEST");
        let imported = pxlogic_file::open_saleae_transition_session(manifest).unwrap();
        let analyzer = imported
            .session
            .analyzers
            .iter()
            .find(|analyzer| analyzer.analyzer_type == "SPI")
            .unwrap();
        let settings = analyzer.decode_settings.clone().unwrap();
        let sample_count = imported.capture.metadata.sample_count;
        let clock_transitions = &imported
            .capture
            .channels
            .iter()
            .find(|channel| channel.channel == 3)
            .unwrap()
            .transitions;
        let first_data_sample = *clock_transitions.first().unwrap();
        let last_data_sample = *clock_transitions.last().unwrap();
        let data_span = last_data_sample - first_data_sample;
        let session = GraphSession::default();
        session.replace_sparse_capture(imported.capture).unwrap();
        let view = WaveformRequest {
            start_sample: 0,
            sample_count,
            pixels: 1_200,
            channels: Vec::new(),
        };

        for backend in [
            DecoderBackendKind::SaleaeNative,
            DecoderBackendKind::SigrokNative,
        ] {
            let table_frames = session
                .decode_frames_with_backend_for_view(&settings, backend, &view)
                .unwrap();
            assert!(!table_frames.is_empty(), "backend={backend:?} table");
            assert!(table_frames.iter().any(|frame| {
                frame
                    .channel_values
                    .iter()
                    .any(|value| value.role == "MOSI")
                    && frame
                        .channel_values
                        .iter()
                        .any(|value| value.role == "MISO")
            }));
            assert!(table_frames
                .iter()
                .all(
                    |frame| frame.channel_values.iter().all(|value| value.value <= 0xff
                        && value.label.len() == 4
                        && value.label.starts_with("0x"))
                ));
            if backend == DecoderBackendKind::SaleaeNative {
                assert!(table_frames
                    .iter()
                    .any(|frame| frame.label == "Error" && frame.channel_values.is_empty()));
            }
            let result = session
                .render_analyzer(
                    &AnalyzerRenderRequest {
                        analyzer_id: format!("{backend:?}"),
                        backend,
                        settings: settings.clone(),
                        tracks: vec![
                            AnalyzerTrackRequest {
                                channel: 5,
                                role: "MOSI".to_string(),
                            },
                            AnalyzerTrackRequest {
                                channel: 0,
                                role: "MISO".to_string(),
                            },
                        ],
                    },
                    &view,
                )
                .unwrap();
            let bubbles = result
                .tracks
                .iter()
                .flat_map(|track| &track.bubbles)
                .collect::<Vec<_>>();
            assert!(!bubbles.is_empty(), "backend={backend:?}");
            assert!(bubbles.iter().all(|bubble| bubble
                .display_text
                .iter()
                .all(|text| text.len() == 4 && text.starts_with("0x"))));
            assert!(
                bubbles
                    .iter()
                    .any(|bubble| bubble.start_sample < first_data_sample + data_span / 10),
                "backend={backend:?} has no bubbles near the first protocol data"
            );
            assert!(
                bubbles
                    .iter()
                    .any(|bubble| bubble.end_sample > last_data_sample - data_span / 10),
                "backend={backend:?} has no bubbles near the last protocol data"
            );
        }
    }

    #[test]
    fn indexed_viewport_requests_have_a_fixed_pixel_budget() {
        let capture = demo_capture(32, 2_000_000);
        let graph = DigitalGraph::from_capture(&capture).unwrap();
        let request = WaveformRequest {
            start_sample: 250_000,
            sample_count: 1_500_000,
            pixels: 1_800,
            channels: (0..32).collect(),
        };
        let started = Instant::now();
        for _ in 0..20 {
            let tile = graph.build_tile(&request).unwrap();
            assert_eq!(tile.channels.len(), 32);
            assert!(tile.channels.iter().all(|channel| channel.bins.is_empty()
                && channel
                    .packed_bins
                    .as_deref()
                    .is_some_and(|data| !data.is_empty())));
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dense_exact_view_switches_to_pixel_primitives() {
        let sample_count = 4_096u64;
        let metadata = CaptureMetadata {
            version: 1,
            source_device: "dense-render-test".to_string(),
            sample_rate_hz: 100_000_000,
            channel_count: 1,
            enabled_channels: vec![0],
            unitsize: 1,
            sample_count,
            captured_at: Utc::now(),
            labels: vec!["D0".to_string()],
            trigger: None,
        };
        let dense = CaptureData {
            metadata: metadata.clone(),
            samples: (0..sample_count)
                .map(|sample| u8::from(sample % 2 != 0))
                .collect(),
        };
        let sparse = CaptureData {
            metadata,
            samples: (0..sample_count)
                .map(|sample| u8::from(sample >= sample_count / 2))
                .collect(),
        };
        let request = WaveformRequest {
            start_sample: 0,
            sample_count,
            pixels: 1_024,
            channels: vec![0],
        };

        let dense_tile = DigitalGraph::from_capture(&dense)
            .unwrap()
            .build_tile(&request)
            .unwrap();
        assert!(dense_tile.channels[0].segments.is_empty());
        assert!(dense_tile.channels[0].packed_bins.is_some());

        let sparse_tile = DigitalGraph::from_capture(&sparse)
            .unwrap()
            .build_tile(&request)
            .unwrap();
        assert_eq!(sparse_tile.channels[0].segments.len(), 2);
    }

    #[test]
    fn i2c_start_and_stop_frames_render_as_sda_protocol_markers() {
        let frame = |frame_id, sample, frame_type: &str, channel_values| DecodedFrame {
            frame_id,
            start_sample: sample,
            end_sample: sample + 1,
            frame_type: frame_type.to_string(),
            label: frame_type.to_string(),
            value: 0,
            channel_values,
            protocol_markers: Vec::new(),
        };
        let frames = vec![
            frame(0, 100, "start", Vec::new()),
            frame(
                1,
                120,
                "address",
                vec![DecodedChannelValue {
                    channel: 2,
                    role: "SDA".to_string(),
                    label: "W[0x20]".to_string(),
                    texts: vec!["W[0x20]".to_string()],
                    value: 0x20,
                }],
            ),
            frame(2, 300, "stop", Vec::new()),
        ];
        let track = render_analyzer_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 2,
                role: "SDA".to_string(),
            },
            &WaveformRequest {
                start_sample: 0,
                sample_count: 400,
                pixels: 800,
                channels: vec![2],
            },
        );

        assert_eq!(
            track.protocol_markers,
            vec![
                AnalyzerProtocolMarker {
                    sample: 100,
                    kind: AnalyzerProtocolMarkerKind::Start,
                },
                AnalyzerProtocolMarker {
                    sample: 300,
                    kind: AnalyzerProtocolMarkerKind::Stop,
                },
            ]
        );
        assert_eq!(track.bubbles.len(), 1);
    }

    #[test]
    fn dense_protocol_markers_are_bounded_and_preserve_endpoints() {
        let frames = vec![DecodedFrame {
            frame_id: 0,
            start_sample: 0,
            end_sample: 2_000,
            frame_type: "marker".to_string(),
            label: "marker".to_string(),
            value: 0,
            channel_values: Vec::new(),
            protocol_markers: (1..=1_000)
                .map(|sample| pxlogic_core::DecodedProtocolMarker {
                    channel: Some(0),
                    sample,
                    kind: "dot".to_string(),
                })
                .collect(),
        }];
        let markers = protocol_markers_for_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 0,
                role: "Data".to_string(),
            },
            0,
            1,
        );
        assert_eq!(markers.len(), MAX_ANALYZER_MARKER_RESULTS);
        assert_eq!(markers.first().unwrap().sample, 1);
        assert_eq!(markers.last().unwrap().sample, 1_000);
    }

    #[test]
    fn unordered_protocol_markers_are_sorted_and_deduplicated_for_track_rendering() {
        let frames = vec![DecodedFrame {
            frame_id: 0,
            start_sample: 0,
            end_sample: 100,
            frame_type: "marker".to_string(),
            label: "marker".to_string(),
            value: 0,
            channel_values: Vec::new(),
            protocol_markers: vec![
                pxlogic_core::DecodedProtocolMarker {
                    channel: Some(0),
                    sample: 80,
                    kind: "stop".to_string(),
                },
                pxlogic_core::DecodedProtocolMarker {
                    channel: Some(0),
                    sample: 10,
                    kind: "start".to_string(),
                },
                pxlogic_core::DecodedProtocolMarker {
                    channel: Some(0),
                    sample: 10,
                    kind: "start".to_string(),
                },
                pxlogic_core::DecodedProtocolMarker {
                    channel: Some(0),
                    sample: 50,
                    kind: "dot".to_string(),
                },
            ],
        }];
        let markers = protocol_markers_for_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 0,
                role: "Data".to_string(),
            },
            0,
            1,
        );

        assert_eq!(
            markers
                .iter()
                .map(|marker| (marker.sample, marker.kind))
                .collect::<Vec<_>>(),
            vec![
                (10, AnalyzerProtocolMarkerKind::Start),
                (50, AnalyzerProtocolMarkerKind::Dot),
                (80, AnalyzerProtocolMarkerKind::Stop),
            ]
        );
    }

    #[test]
    fn native_analyzer_markers_render_on_their_reported_channel() {
        let frames = vec![DecodedFrame {
            frame_id: 0,
            start_sample: 100,
            end_sample: 200,
            frame_type: "data".to_string(),
            label: "0x12".to_string(),
            value: 0x12,
            channel_values: vec![DecodedChannelValue {
                channel: 1,
                role: "DATA".to_string(),
                label: "0x12".to_string(),
                texts: vec!["0x12".to_string()],
                value: 0x12,
            }],
            protocol_markers: vec![pxlogic_core::DecodedProtocolMarker {
                channel: Some(0),
                sample: 150,
                kind: "up_arrow".to_string(),
            }],
        }];
        let track = render_analyzer_track(
            &frames,
            &AnalyzerTrackRequest {
                channel: 0,
                role: "Clock".to_string(),
            },
            &WaveformRequest {
                start_sample: 0,
                sample_count: 400,
                pixels: 800,
                channels: vec![0],
            },
        );

        assert_eq!(
            track.protocol_markers,
            vec![AnalyzerProtocolMarker {
                sample: 150,
                kind: AnalyzerProtocolMarkerKind::UpArrow,
            }]
        );
        assert!(track.bubbles.is_empty());
    }

    #[test]
    fn native_window_offset_moves_protocol_markers_with_frames() {
        let frame = DecodedFrame {
            frame_id: 4,
            start_sample: 10,
            end_sample: 20,
            frame_type: "data".to_string(),
            label: "0x12".to_string(),
            value: 0x12,
            channel_values: vec![DecodedChannelValue {
                channel: 1,
                role: "DATA".to_string(),
                label: "0x12".to_string(),
                texts: vec!["0x12".to_string()],
                value: 0x12,
            }],
            protocol_markers: vec![pxlogic_core::DecodedProtocolMarker {
                channel: Some(0),
                sample: 15,
                kind: "up_arrow".to_string(),
            }],
        };

        let shifted = offset_decoded_frame_samples(frame, 1_000);

        assert_eq!(shifted.start_sample, 1_010);
        assert_eq!(shifted.end_sample, 1_020);
        assert_eq!(shifted.protocol_markers[0].sample, 1_015);
    }

    #[test]
    fn parallel_analyzer_render_preserves_request_order() {
        let capture = demo_capture(16, 500_000);
        let session = GraphSession::default();
        session.replace_capture(capture).unwrap();
        let view = WaveformRequest {
            start_sample: 0,
            sample_count: 500_000,
            pixels: 1_200,
            channels: vec![0, 1, 3, 4],
        };
        let requests = vec![
            AnalyzerRenderRequest {
                analyzer_id: "uart-first".to_string(),
                backend: DecoderBackendKind::LegacyRust,
                settings: AnalyzerDecodeSettings::Uart(UartDecodeSettings::default()),
                tracks: vec![AnalyzerTrackRequest {
                    channel: 0,
                    role: "RX".to_string(),
                }],
            },
            AnalyzerRenderRequest {
                analyzer_id: "i2c-second".to_string(),
                backend: DecoderBackendKind::LegacyRust,
                settings: AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
                tracks: vec![AnalyzerTrackRequest {
                    channel: 1,
                    role: "SDA".to_string(),
                }],
            },
            AnalyzerRenderRequest {
                analyzer_id: "spi-third".to_string(),
                backend: DecoderBackendKind::LegacyRust,
                settings: AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
                tracks: vec![AnalyzerTrackRequest {
                    channel: 3,
                    role: "MOSI".to_string(),
                }],
            },
        ];

        let rendered = session.render_analyzers(&requests, &view).unwrap();

        assert_eq!(
            rendered
                .iter()
                .map(|result| result.analyzer_id.as_str())
                .collect::<Vec<_>>(),
            vec!["uart-first", "i2c-second", "spi-third"]
        );
    }

    #[test]
    fn i2c_and_spi_render_on_their_assigned_role_tracks() {
        let capture = demo_capture(16, 500_000);
        let session = GraphSession::default();
        session.replace_capture(capture).unwrap();
        let view = WaveformRequest {
            start_sample: 0,
            sample_count: 500_000,
            pixels: 1_200,
            channels: vec![1, 3, 4],
        };
        for request in [
            AnalyzerRenderRequest {
                analyzer_id: "i2c".to_string(),
                backend: DecoderBackendKind::LegacyRust,
                settings: AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
                tracks: vec![AnalyzerTrackRequest {
                    channel: 1,
                    role: "SDA".to_string(),
                }],
            },
            AnalyzerRenderRequest {
                analyzer_id: "spi".to_string(),
                backend: DecoderBackendKind::LegacyRust,
                settings: AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
                tracks: vec![
                    AnalyzerTrackRequest {
                        channel: 3,
                        role: "MOSI".to_string(),
                    },
                    AnalyzerTrackRequest {
                        channel: 4,
                        role: "MISO".to_string(),
                    },
                ],
            },
        ] {
            let result = session.render_analyzer(&request, &view).unwrap();
            assert_eq!(result.tracks.len(), request.tracks.len());
            for (track, expected) in result.tracks.iter().zip(&request.tracks) {
                assert_eq!(track.channel, expected.channel);
                assert_eq!(track.role, expected.role);
                assert!(!track.bubbles.is_empty(), "track={expected:?}");
            }
        }
    }

    #[test]
    fn analyzer_rendering_does_not_change_the_waveform_tile() {
        let capture = demo_capture(16, 250_000);
        let session = GraphSession::default();
        session.replace_capture(capture).unwrap();
        let waveform = WaveformRequest {
            start_sample: 25_000,
            sample_count: 100_000,
            pixels: 1_200,
            channels: (0..6).collect(),
        };
        let baseline = session
            .render_frame(&GraphFrameRequest {
                frame_id: 1,
                waveform: waveform.clone(),
                analyzer_view: None,
                analyzers: Vec::new(),
            })
            .unwrap();
        let with_analyzers = session
            .render_frame(&GraphFrameRequest {
                frame_id: 2,
                waveform: waveform.clone(),
                analyzer_view: Some(waveform),
                analyzers: vec![
                    AnalyzerRenderRequest {
                        analyzer_id: "i2c".to_string(),
                        backend: DecoderBackendKind::LegacyRust,
                        settings: AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
                        tracks: vec![AnalyzerTrackRequest {
                            channel: 1,
                            role: "SDA".to_string(),
                        }],
                    },
                    AnalyzerRenderRequest {
                        analyzer_id: "spi".to_string(),
                        backend: DecoderBackendKind::LegacyRust,
                        settings: AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
                        tracks: vec![AnalyzerTrackRequest {
                            channel: 3,
                            role: "MOSI".to_string(),
                        }],
                    },
                ],
            })
            .unwrap();

        assert_eq!(baseline.tile, with_analyzers.tile);
        assert_eq!(with_analyzers.analyzers.len(), 2);
    }

    #[test]
    fn graph_sessions_keep_capture_data_isolated() {
        let short = GraphSession::default();
        let long = GraphSession::default();
        short.replace_capture(demo_capture(8, 10_000)).unwrap();
        long.replace_capture(demo_capture(32, 50_000)).unwrap();
        assert_eq!(short.sample_count(), 10_000);
        assert_eq!(short.metadata().unwrap().channel_count, 8);
        assert_eq!(long.sample_count(), 50_000);
        assert_eq!(long.metadata().unwrap().channel_count, 32);

        let request = WaveformRequest {
            start_sample: 0,
            sample_count: 50_000,
            pixels: 500,
            channels: vec![31],
        };
        assert!(short
            .graph
            .read()
            .as_ref()
            .unwrap()
            .build_tile(&request)
            .unwrap()
            .channels
            .is_empty());
        assert_eq!(
            long.graph
                .read()
                .as_ref()
                .unwrap()
                .build_tile(&request)
                .unwrap()
                .channels[0]
                .channel,
            31
        );
    }

    #[test]
    fn native_window_decode_worker_count_is_bounded_and_configurable() {
        assert_eq!(native_window_decode_worker_count_value(1, None, 8), 1);
        assert_eq!(native_window_decode_worker_count_value(13, None, 8), 4);
        assert_eq!(native_window_decode_worker_count_value(13, None, 2), 2);
        assert_eq!(native_window_decode_worker_count_value(13, Some("1"), 8), 1);
        assert_eq!(native_window_decode_worker_count_value(13, Some("8"), 4), 4);
        assert_eq!(
            native_window_decode_worker_count_value(13, Some("garbage"), 8),
            4
        );
        assert_eq!(native_window_decode_worker_count_value(13, Some("0"), 8), 4);
    }
}
