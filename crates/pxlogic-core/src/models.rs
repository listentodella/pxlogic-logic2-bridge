use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeviceKind {
    Fake,
    Usb,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub kind: DeviceKind,
    pub vid: u16,
    pub pid: u16,
    pub bus: Option<u8>,
    pub address: Option<u8>,
    pub label: String,
    pub ready: bool,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
    pub usb_speed: Option<String>,
    pub logic_mode: Option<u32>,
    pub profile_model: Option<String>,
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareChannelModeInfo {
    pub id: String,
    pub stream: bool,
    pub channels: u8,
    pub default_sample_rate_hz: u64,
    pub default_sample_limit: u64,
    pub min_sample_rate_hz: u64,
    pub max_sample_rate_hz: u64,
    pub sample_rates: Vec<u64>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardwareCaptureCapabilities {
    pub device_id: String,
    pub profile_vendor: Option<String>,
    pub profile_model: Option<String>,
    pub usb_speed: Option<String>,
    pub logic_mode: Option<u32>,
    pub max_channels: u8,
    pub default_channel_count: u8,
    pub default_sample_rate_hz: u64,
    pub sample_rates: Vec<u64>,
    #[serde(default)]
    pub threshold_volts: Vec<f64>,
    pub channel_modes: Vec<HardwareChannelModeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureSettings {
    pub device_id: String,
    pub sample_rate_hz: u64,
    pub channel_count: u8,
    #[serde(default)]
    pub enabled_channels: Vec<u8>,
    pub duration_ms: u64,
    pub buffer_size_mb: u64,
    pub decode_cross: bool,
    pub threshold_volts: f64,
    #[serde(default)]
    pub external_trigger_mode: ExternalTriggerMode,
    pub mode: CaptureMode,
    pub trigger_enabled: bool,
    pub trigger_channel: u8,
    pub trigger_kind: CaptureTriggerKind,
    /// Additional level conditions, matching PXView's per-channel simple trigger masks.
    #[serde(default)]
    pub trigger_high_mask: u32,
    #[serde(default)]
    pub trigger_low_mask: u32,
    #[serde(default = "default_trigger_position_percent")]
    pub trigger_position_percent: u8,
    pub glitch_filter_enabled: bool,
    pub clock_edge: bool,
    pub trigger_out_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PwmSettings {
    pub enabled: bool,
    pub frequency_hz: f64,
    pub duty_percent: f64,
}

impl Default for PwmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            frequency_hz: 1_000.0,
            duty_percent: 50.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PwmConfiguration {
    pub enabled: bool,
    pub requested_frequency_hz: f64,
    pub requested_duty_percent: f64,
    pub effective_frequency_hz: f64,
    pub effective_duty_percent: f64,
    pub period_ticks: u32,
    pub high_ticks: u32,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            device_id: "fake:pxlogic-demo".to_string(),
            sample_rate_hz: 25_000_000,
            channel_count: 8,
            enabled_channels: Vec::new(),
            duration_ms: 10,
            buffer_size_mb: 16,
            decode_cross: true,
            threshold_volts: 2.0,
            external_trigger_mode: ExternalTriggerMode::Close,
            mode: CaptureMode::Stream,
            trigger_enabled: false,
            trigger_channel: 0,
            trigger_kind: CaptureTriggerKind::Rising,
            trigger_high_mask: 0,
            trigger_low_mask: 0,
            trigger_position_percent: default_trigger_position_percent(),
            glitch_filter_enabled: false,
            clock_edge: false,
            trigger_out_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaptureMode {
    Buffer,
    Stream,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaptureTriggerKind {
    Rising,
    Falling,
    High,
    Low,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum ExternalTriggerMode {
    #[default]
    Close = 0,
    Rising = 1,
    One = 2,
    Falling = 3,
    Zero = 4,
    Edge = 5,
}

impl From<ExternalTriggerMode> for u32 {
    fn from(value: ExternalTriggerMode) -> Self {
        value as u32
    }
}

const fn default_trigger_position_percent() -> u8 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureTriggerMetadata {
    pub sample_index: u64,
    pub channel: u8,
    pub kind: CaptureTriggerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureMetadata {
    pub version: u32,
    pub source_device: String,
    pub sample_rate_hz: u64,
    pub channel_count: u8,
    #[serde(default)]
    pub enabled_channels: Vec<u8>,
    pub unitsize: u8,
    pub sample_count: u64,
    pub captured_at: DateTime<Utc>,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CaptureTriggerMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureProgress {
    pub bytes_read: u64,
    pub bytes_expected: u64,
    pub samples_read: u64,
    pub sample_memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub phase: String,
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureData {
    pub metadata: CaptureMetadata,
    pub samples: Vec<u8>,
}

/// A long Saleae capture represented by its transition indexes instead of a
/// sample-per-clock buffer. `transitions` contains sample indexes at which the
/// channel toggles from its initial level. This is the in-process form of the
/// public Logic binary produced by the native GraphServer sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseDigitalChannel {
    pub channel: u8,
    pub initial_high: bool,
    pub transitions: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseCaptureData {
    pub metadata: CaptureMetadata,
    pub channels: Vec<SparseDigitalChannel>,
}

/// Borrowed transition-indexed capture used by in-process graph owners.
///
/// This lets native decoders consume the graph's existing edge vectors without
/// rescanning packed samples or cloning transitions for every analyzer.
#[derive(Debug)]
pub struct SparseDigitalChannelView<'a> {
    pub channel: u8,
    pub initial_high: bool,
    pub transitions: &'a [u64],
}

#[derive(Debug)]
pub struct SparseCaptureView<'a> {
    pub metadata: &'a CaptureMetadata,
    pub channels: Vec<SparseDigitalChannelView<'a>>,
}

#[derive(Debug, Clone)]
pub struct Bitstreams {
    pub reset: Vec<u8>,
    pub main: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExportFormat {
    Pxcap,
    Sr,
    Vcd,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WaveformSource {
    CurrentCapture,
}
