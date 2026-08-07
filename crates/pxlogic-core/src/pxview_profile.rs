use crate::{
    capture::supported_samplerates,
    error::{CoreError, Result},
    models::{CaptureMode, HardwareCaptureCapabilities, HardwareChannelModeInfo},
    protocol::{PXLOGIC_LEGACY_PID, PXLOGIC_LEGACY_VID, PXLOGIC_WCH_PID, PXLOGIC_WCH_VID},
};

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PxUsbSpeed {
    High,
    Super,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PxlogicChannelModeId {
    BufferLogic250x32,
    BufferLogic250x16,
    BufferLogic500x16,
    BufferLogic1000x8,
    StreamLogic50x32,
    StreamLogic125x16,
    StreamLogic250x8,
    StreamLogic500x4,
    StreamLogic1000x2,
    StreamLogic200x1,
    StreamLogic100x2,
    StreamLogic50x4,
    StreamLogic25x8,
    StreamLogic10x16,
    StreamLogic5x32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxlogicChannelMode {
    pub id: PxlogicChannelModeId,
    pub stream: bool,
    pub channels: u8,
    pub default_samplerate_hz: u64,
    pub default_samplelimit: u64,
    pub min_samplerate_hz: u64,
    pub max_samplerate_hz: u64,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxlogicDeviceProfile {
    pub vid: u16,
    pub pid: u16,
    pub usb_speed: PxUsbSpeed,
    pub logic_mode: u32,
    pub vendor: &'static str,
    pub model: &'static str,
    pub hardware_depth_bits: u64,
    pub supported_modes: &'static [PxlogicChannelModeId],
    pub default_mode: PxlogicChannelModeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxlogicCapturePlan {
    pub profile: &'static PxlogicDeviceProfile,
    pub channel_mode: &'static PxlogicChannelMode,
    pub channel_count: u8,
    pub sample_rate_hz: u64,
}

impl PxlogicDeviceProfile {
    pub fn supported_channel_modes(
        &self,
    ) -> impl Iterator<Item = &'static PxlogicChannelMode> + '_ {
        self.supported_modes
            .iter()
            .filter_map(|id| pxlogic_channel_mode(*id))
    }
}

const MHZ: u64 = 1_000_000;
const GHZ: u64 = 1_000_000_000;
pub const PXLOGIC_32_CHANNEL_HARDWARE_DEPTH_BITS: u64 = 4 * GHZ;
pub const PXLOGIC_16_PRO_HARDWARE_DEPTH_BITS: u64 = 4 * GHZ;
pub const PXLOGIC_16_PLUS_HARDWARE_DEPTH_BITS: u64 = 2 * GHZ;
pub const PXLOGIC_16_BASE_HARDWARE_DEPTH_BITS: u64 = GHZ;
const DEFAULT_SAMPLELIMIT: u64 = 250 * MHZ;
const MIN_SAMPLERATE: u64 = 2_000;
/// PXView exposes these standard logic-level thresholds while retaining its
/// underlying VTH register as a floating-point voltage.
pub const PXLOGIC_THRESHOLD_VOLTAGES: &[f64] = &[1.8, 2.5, 3.3, 5.0];

pub const PXLOGIC_CHANNEL_MODES: &[PxlogicChannelMode] = &[
    PxlogicChannelMode {
        id: PxlogicChannelModeId::BufferLogic250x32,
        stream: false,
        channels: 32,
        default_samplerate_hz: 250 * MHZ,
        default_samplelimit: DEFAULT_SAMPLELIMIT,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 250 * MHZ,
        description: "Use 32 Channels (Max 250MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::BufferLogic250x16,
        stream: false,
        channels: 16,
        default_samplerate_hz: 250 * MHZ,
        default_samplelimit: DEFAULT_SAMPLELIMIT,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 250 * MHZ,
        description: "Use 16 Channels (Max 250MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::BufferLogic500x16,
        stream: false,
        channels: 16,
        default_samplerate_hz: 500 * MHZ,
        default_samplelimit: 500 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 500 * MHZ,
        description: "Use 16 Channels (Max 500MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::BufferLogic1000x8,
        stream: false,
        channels: 8,
        default_samplerate_hz: GHZ,
        default_samplelimit: GHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: GHZ,
        description: "Use 8 Channels (Max 1000MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic50x32,
        stream: true,
        channels: 32,
        default_samplerate_hz: 50 * MHZ,
        default_samplelimit: 50 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 50 * MHZ,
        description: "Use 32 Channels (Max50MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic125x16,
        stream: true,
        channels: 16,
        default_samplerate_hz: 125 * MHZ,
        default_samplelimit: 125 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 125 * MHZ,
        description: "Use 16 Channels (Max 125MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic250x8,
        stream: true,
        channels: 8,
        default_samplerate_hz: 250 * MHZ,
        default_samplelimit: 250 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 250 * MHZ,
        description: "Use 8 Channels (Max 250MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic500x4,
        stream: true,
        channels: 4,
        default_samplerate_hz: 500 * MHZ,
        default_samplelimit: 500 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 500 * MHZ,
        description: "Use 4 Channels (Max 500MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic1000x2,
        stream: true,
        channels: 2,
        default_samplerate_hz: GHZ,
        default_samplelimit: GHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: GHZ,
        description: "Use 2 Channels (Max 1000MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic200x1,
        stream: true,
        channels: 1,
        default_samplerate_hz: 200 * MHZ,
        default_samplelimit: 200 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 200 * MHZ,
        description: "Use 1 Channels (Max200MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic100x2,
        stream: true,
        channels: 2,
        default_samplerate_hz: 100 * MHZ,
        default_samplelimit: 100 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 100 * MHZ,
        description: "Use 2 Channels (Max100MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic50x4,
        stream: true,
        channels: 4,
        default_samplerate_hz: 50 * MHZ,
        default_samplelimit: 50 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 50 * MHZ,
        description: "Use 4 Channels (Max50MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic25x8,
        stream: true,
        channels: 8,
        default_samplerate_hz: 25 * MHZ,
        default_samplelimit: 25 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 25 * MHZ,
        description: "Use 8 Channels (Max25MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic10x16,
        stream: true,
        channels: 16,
        default_samplerate_hz: 10 * MHZ,
        default_samplelimit: 10 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 10 * MHZ,
        description: "Use 16 Channels (Max10MHz)",
    },
    PxlogicChannelMode {
        id: PxlogicChannelModeId::StreamLogic5x32,
        stream: true,
        channels: 32,
        default_samplerate_hz: 5 * MHZ,
        default_samplelimit: 5 * MHZ,
        min_samplerate_hz: MIN_SAMPLERATE,
        max_samplerate_hz: 5 * MHZ,
        description: "Use 32 Channels (Max5MHz)",
    },
];

const MODES_32_SUPER: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic250x32,
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::BufferLogic1000x8,
    PxlogicChannelModeId::StreamLogic50x32,
    PxlogicChannelModeId::StreamLogic125x16,
    PxlogicChannelModeId::StreamLogic250x8,
    PxlogicChannelModeId::StreamLogic500x4,
    PxlogicChannelModeId::StreamLogic1000x2,
];
const MODES_32_HIGH: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic250x32,
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::BufferLogic1000x8,
    PxlogicChannelModeId::StreamLogic200x1,
    PxlogicChannelModeId::StreamLogic100x2,
    PxlogicChannelModeId::StreamLogic50x4,
    PxlogicChannelModeId::StreamLogic25x8,
    PxlogicChannelModeId::StreamLogic10x16,
    PxlogicChannelModeId::StreamLogic5x32,
];
const MODES_16_PRO_SUPER: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::BufferLogic1000x8,
    PxlogicChannelModeId::StreamLogic125x16,
    PxlogicChannelModeId::StreamLogic250x8,
    PxlogicChannelModeId::StreamLogic500x4,
    PxlogicChannelModeId::StreamLogic1000x2,
];
const MODES_16_PRO_HIGH: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::BufferLogic1000x8,
    PxlogicChannelModeId::StreamLogic200x1,
    PxlogicChannelModeId::StreamLogic100x2,
    PxlogicChannelModeId::StreamLogic50x4,
    PxlogicChannelModeId::StreamLogic25x8,
    PxlogicChannelModeId::StreamLogic10x16,
];
const MODES_16_PLUS_SUPER: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::StreamLogic125x16,
    PxlogicChannelModeId::StreamLogic250x8,
    PxlogicChannelModeId::StreamLogic500x4,
];
const MODES_16_PLUS_HIGH: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic500x16,
    PxlogicChannelModeId::StreamLogic200x1,
    PxlogicChannelModeId::StreamLogic100x2,
    PxlogicChannelModeId::StreamLogic50x4,
    PxlogicChannelModeId::StreamLogic25x8,
    PxlogicChannelModeId::StreamLogic10x16,
];
const MODES_16_BASE_SUPER: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic250x16,
    PxlogicChannelModeId::StreamLogic125x16,
    PxlogicChannelModeId::StreamLogic250x8,
];
const MODES_16_BASE_HIGH: &[PxlogicChannelModeId] = &[
    PxlogicChannelModeId::BufferLogic250x16,
    PxlogicChannelModeId::StreamLogic200x1,
    PxlogicChannelModeId::StreamLogic100x2,
    PxlogicChannelModeId::StreamLogic50x4,
    PxlogicChannelModeId::StreamLogic25x8,
    PxlogicChannelModeId::StreamLogic10x16,
];

pub const PXLOGIC_DEVICE_PROFILES: &[PxlogicDeviceProfile] = &[
    PxlogicDeviceProfile {
        vid: PXLOGIC_WCH_VID,
        pid: PXLOGIC_WCH_PID,
        usb_speed: PxUsbSpeed::Super,
        logic_mode: 0,
        vendor: "PX_Tool",
        model: "PX-Logic U3 channel 32",
        hardware_depth_bits: PXLOGIC_32_CHANNEL_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_32_SUPER,
        default_mode: PxlogicChannelModeId::BufferLogic250x32,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_WCH_VID,
        pid: PXLOGIC_WCH_PID,
        usb_speed: PxUsbSpeed::High,
        logic_mode: 0,
        vendor: "PX_Tool",
        model: "PX-Logic U2 channel 32",
        hardware_depth_bits: PXLOGIC_32_CHANNEL_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_32_HIGH,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::Super,
        logic_mode: 0,
        vendor: "PX_Tool",
        model: "PX-Logic U3 channel 32",
        hardware_depth_bits: PXLOGIC_32_CHANNEL_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_32_SUPER,
        default_mode: PxlogicChannelModeId::BufferLogic250x32,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::High,
        logic_mode: 0,
        vendor: "PX_Tool",
        model: "PX-Logic U2 channel 32",
        hardware_depth_bits: PXLOGIC_32_CHANNEL_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_32_HIGH,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::Super,
        logic_mode: 1,
        vendor: "PX_Tool",
        model: "PX-Logic U3 channel 16 Pro",
        hardware_depth_bits: PXLOGIC_16_PRO_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_PRO_SUPER,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::High,
        logic_mode: 1,
        vendor: "PX_Tool",
        model: "PX-Logic U2 channel 16 Pro",
        hardware_depth_bits: PXLOGIC_16_PRO_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_PRO_HIGH,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::Super,
        logic_mode: 2,
        vendor: "PX_Tool",
        model: "PX-Logic U3 channel 16 Plus",
        hardware_depth_bits: PXLOGIC_16_PLUS_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_PLUS_SUPER,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::High,
        logic_mode: 2,
        vendor: "PX_Tool",
        model: "PX-Logic U2 channel 16 Plus",
        hardware_depth_bits: PXLOGIC_16_PLUS_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_PLUS_HIGH,
        default_mode: PxlogicChannelModeId::BufferLogic500x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::Super,
        logic_mode: 3,
        vendor: "PX_Tool",
        model: "PX-Logic U3 channel 16 Base",
        hardware_depth_bits: PXLOGIC_16_BASE_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_BASE_SUPER,
        default_mode: PxlogicChannelModeId::BufferLogic250x16,
    },
    PxlogicDeviceProfile {
        vid: PXLOGIC_LEGACY_VID,
        pid: PXLOGIC_LEGACY_PID,
        usb_speed: PxUsbSpeed::High,
        logic_mode: 3,
        vendor: "PX_Tool",
        model: "PX-Logic U2 channel 16 Base",
        hardware_depth_bits: PXLOGIC_16_BASE_HARDWARE_DEPTH_BITS,
        supported_modes: MODES_16_BASE_HIGH,
        default_mode: PxlogicChannelModeId::BufferLogic250x16,
    },
];

pub fn pxlogic_channel_mode(id: PxlogicChannelModeId) -> Option<&'static PxlogicChannelMode> {
    PXLOGIC_CHANNEL_MODES.iter().find(|mode| mode.id == id)
}

pub fn pxlogic_capture_capabilities_from_profile(
    device_id: impl Into<String>,
    profile: &'static PxlogicDeviceProfile,
) -> HardwareCaptureCapabilities {
    let channel_modes = profile
        .supported_channel_modes()
        .map(pxlogic_channel_mode_info)
        .collect::<Vec<_>>();
    let default_mode = pxlogic_channel_mode(profile.default_mode)
        .or_else(|| profile.supported_channel_modes().next());
    let max_channels = channel_modes
        .iter()
        .map(|mode| mode.channels)
        .max()
        .unwrap_or(1);

    HardwareCaptureCapabilities {
        device_id: device_id.into(),
        profile_vendor: Some(profile.vendor.to_string()),
        profile_model: Some(profile.model.to_string()),
        usb_speed: Some(profile.usb_speed.label().to_string()),
        logic_mode: Some(profile.logic_mode),
        max_channels,
        default_channel_count: max_channels.min(8),
        default_sample_rate_hz: default_mode
            .map(|mode| mode.default_samplerate_hz)
            .unwrap_or(25 * MHZ),
        sample_rates: union_sample_rates(&channel_modes),
        threshold_volts: PXLOGIC_THRESHOLD_VOLTAGES.to_vec(),
        channel_modes,
    }
}

pub fn generic_logic_capture_capabilities(
    device_id: impl Into<String>,
    label: impl Into<String>,
) -> HardwareCaptureCapabilities {
    const GENERIC_MAX_SAMPLE_RATE: u64 = 100 * MHZ;
    let sample_rates = supported_samplerates()
        .iter()
        .copied()
        .filter(|rate| *rate <= GENERIC_MAX_SAMPLE_RATE)
        .collect::<Vec<_>>();
    let profile_model = label.into();
    let channel_modes = vec![
        HardwareChannelModeInfo {
            id: "DEMO_STREAM32".to_string(),
            stream: true,
            channels: 32,
            default_sample_rate_hz: 25 * MHZ,
            default_sample_limit: 0,
            min_sample_rate_hz: MIN_SAMPLERATE,
            max_sample_rate_hz: GENERIC_MAX_SAMPLE_RATE,
            sample_rates: sample_rates.clone(),
            description: "Use 32 Channels (Max 100MHz)".to_string(),
        },
        HardwareChannelModeInfo {
            id: "DEMO_BUFFER32".to_string(),
            stream: false,
            channels: 32,
            default_sample_rate_hz: 25 * MHZ,
            default_sample_limit: DEFAULT_SAMPLELIMIT,
            min_sample_rate_hz: MIN_SAMPLERATE,
            max_sample_rate_hz: GENERIC_MAX_SAMPLE_RATE,
            sample_rates: sample_rates.clone(),
            description: "Use 32 Channels (Max 100MHz)".to_string(),
        },
    ];

    HardwareCaptureCapabilities {
        device_id: device_id.into(),
        profile_vendor: None,
        profile_model: Some(profile_model),
        usb_speed: None,
        logic_mode: None,
        max_channels: 32,
        default_channel_count: 8,
        default_sample_rate_hz: 25 * MHZ,
        sample_rates,
        threshold_volts: PXLOGIC_THRESHOLD_VOLTAGES.to_vec(),
        channel_modes,
    }
}

fn pxlogic_channel_mode_info(mode: &'static PxlogicChannelMode) -> HardwareChannelModeInfo {
    HardwareChannelModeInfo {
        id: mode.id.pxview_name().to_string(),
        stream: mode.stream,
        channels: mode.channels,
        default_sample_rate_hz: mode.default_samplerate_hz,
        default_sample_limit: mode.default_samplelimit,
        min_sample_rate_hz: mode.min_samplerate_hz,
        max_sample_rate_hz: mode.max_samplerate_hz,
        sample_rates: sample_rates_for_mode(mode),
        description: mode.description.to_string(),
    }
}

fn sample_rates_for_mode(mode: &PxlogicChannelMode) -> Vec<u64> {
    supported_samplerates()
        .iter()
        .copied()
        .filter(|rate| *rate >= mode.min_samplerate_hz && *rate <= mode.max_samplerate_hz)
        .collect()
}

fn union_sample_rates(channel_modes: &[HardwareChannelModeInfo]) -> Vec<u64> {
    channel_modes
        .iter()
        .flat_map(|mode| mode.sample_rates.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl PxUsbSpeed {
    pub fn label(self) -> &'static str {
        match self {
            PxUsbSpeed::High => "high",
            PxUsbSpeed::Super => "super",
        }
    }
}

impl PxlogicChannelModeId {
    pub fn pxview_name(self) -> &'static str {
        match self {
            PxlogicChannelModeId::BufferLogic250x32 => "BUFFER_LOGIC250x32",
            PxlogicChannelModeId::BufferLogic250x16 => "BUFFER_LOGIC250x16",
            PxlogicChannelModeId::BufferLogic500x16 => "BUFFER_LOGIC500x16",
            PxlogicChannelModeId::BufferLogic1000x8 => "BUFFER_LOGIC1000x8",
            PxlogicChannelModeId::StreamLogic50x32 => "STREAM_LOGIC50x32",
            PxlogicChannelModeId::StreamLogic125x16 => "STREAM_LOGIC125x16",
            PxlogicChannelModeId::StreamLogic250x8 => "STREAM_LOGIC250x8",
            PxlogicChannelModeId::StreamLogic500x4 => "STREAM_LOGIC500x4",
            PxlogicChannelModeId::StreamLogic1000x2 => "STREAM_LOGIC1000x2",
            PxlogicChannelModeId::StreamLogic200x1 => "STREAM_LOGIC200x1",
            PxlogicChannelModeId::StreamLogic100x2 => "STREAM_LOGIC100x2",
            PxlogicChannelModeId::StreamLogic50x4 => "STREAM_LOGIC50x4",
            PxlogicChannelModeId::StreamLogic25x8 => "STREAM_LOGIC25x8",
            PxlogicChannelModeId::StreamLogic10x16 => "STREAM_LOGIC10x16",
            PxlogicChannelModeId::StreamLogic5x32 => "STREAM_LOGIC5x32",
        }
    }
}

pub fn resolve_pxlogic_device_profile(
    vid: u16,
    pid: u16,
    speed: PxUsbSpeed,
    logic_mode: Option<u32>,
) -> Option<&'static PxlogicDeviceProfile> {
    let fallback = PXLOGIC_DEVICE_PROFILES
        .iter()
        .find(|profile| profile.vid == vid && profile.pid == pid && profile.usb_speed == speed)?;
    if let Some(logic_mode) = logic_mode {
        PXLOGIC_DEVICE_PROFILES
            .iter()
            .find(|profile| {
                profile.vid == vid
                    && profile.pid == pid
                    && profile.usb_speed == speed
                    && profile.logic_mode == logic_mode
            })
            .or(Some(fallback))
    } else {
        Some(fallback)
    }
}

pub fn resolve_pxlogic_capture_plan(
    profile: &'static PxlogicDeviceProfile,
    capture_mode: CaptureMode,
    requested_channels: u8,
    requested_samplerate_hz: u64,
) -> Result<PxlogicCapturePlan> {
    if requested_channels == 0 {
        return Err(CoreError::InvalidChannelCount(requested_channels));
    }

    let stream = matches!(capture_mode, CaptureMode::Stream);
    let modes = profile
        .supported_channel_modes()
        .filter(|mode| mode.stream == stream)
        .collect::<Vec<_>>();
    if modes.is_empty() {
        return Err(CoreError::UnsupportedDevice);
    }

    let max_channels = modes
        .iter()
        .map(|mode| mode.channels)
        .max()
        .ok_or(CoreError::UnsupportedDevice)?;
    let channel_count = requested_channels.min(max_channels);
    let selected = modes
        .iter()
        .copied()
        .filter(|mode| mode.channels >= channel_count)
        .filter(|mode| {
            requested_samplerate_hz >= mode.min_samplerate_hz
                && requested_samplerate_hz <= mode.max_samplerate_hz
        })
        .min_by_key(|mode| (mode.channels, mode.max_samplerate_hz))
        .or_else(|| {
            modes
                .iter()
                .copied()
                .filter(|mode| mode.channels >= channel_count)
                .max_by_key(|mode| (mode.max_samplerate_hz, std::cmp::Reverse(mode.channels)))
        })
        .ok_or(CoreError::UnsupportedDevice)?;

    Ok(PxlogicCapturePlan {
        profile,
        channel_mode: selected,
        channel_count,
        sample_rate_hz: requested_samplerate_hz
            .clamp(selected.min_samplerate_hz, selected.max_samplerate_hz),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(speed: PxUsbSpeed, logic_mode: u32) -> &'static PxlogicDeviceProfile {
        resolve_pxlogic_device_profile(
            PXLOGIC_LEGACY_VID,
            PXLOGIC_LEGACY_PID,
            speed,
            Some(logic_mode),
        )
        .expect("profile")
    }

    #[test]
    fn selects_usb3_32_channel_stream_mode_and_caps_rate() {
        let plan = resolve_pxlogic_capture_plan(
            profile(PxUsbSpeed::Super, 0),
            CaptureMode::Stream,
            32,
            100 * MHZ,
        )
        .unwrap();

        assert_eq!(plan.channel_mode.id, PxlogicChannelModeId::StreamLogic50x32);
        assert_eq!(plan.channel_count, 32);
        assert_eq!(plan.sample_rate_hz, 50 * MHZ);
    }

    #[test]
    fn selects_usb2_32_channel_stream_mode_and_caps_rate() {
        let plan = resolve_pxlogic_capture_plan(
            profile(PxUsbSpeed::High, 0),
            CaptureMode::Stream,
            32,
            25 * MHZ,
        )
        .unwrap();

        assert_eq!(plan.channel_mode.id, PxlogicChannelModeId::StreamLogic5x32);
        assert_eq!(plan.channel_count, 32);
        assert_eq!(plan.sample_rate_hz, 5 * MHZ);
    }

    #[test]
    fn selects_smaller_fast_stream_mode_for_few_channels() {
        let plan = resolve_pxlogic_capture_plan(
            profile(PxUsbSpeed::Super, 0),
            CaptureMode::Stream,
            4,
            500 * MHZ,
        )
        .unwrap();

        assert_eq!(plan.channel_mode.id, PxlogicChannelModeId::StreamLogic500x4);
        assert_eq!(plan.channel_count, 4);
        assert_eq!(plan.sample_rate_hz, 500 * MHZ);
    }

    #[test]
    fn caps_16_channel_profiles_to_16_enabled_channels() {
        let plan = resolve_pxlogic_capture_plan(
            profile(PxUsbSpeed::High, 1),
            CaptureMode::Stream,
            32,
            10 * MHZ,
        )
        .unwrap();

        assert_eq!(plan.channel_mode.id, PxlogicChannelModeId::StreamLogic10x16);
        assert_eq!(plan.channel_count, 16);
        assert_eq!(plan.sample_rate_hz, 10 * MHZ);
    }

    #[test]
    fn selects_buffer_mode_by_requested_channels_and_rate() {
        let plan = resolve_pxlogic_capture_plan(
            profile(PxUsbSpeed::Super, 0),
            CaptureMode::Buffer,
            16,
            500 * MHZ,
        )
        .unwrap();

        assert_eq!(
            plan.channel_mode.id,
            PxlogicChannelModeId::BufferLogic500x16
        );
        assert_eq!(plan.channel_count, 16);
        assert_eq!(plan.sample_rate_hz, 500 * MHZ);
    }

    #[test]
    fn exposes_pxview_channel_modes_as_capture_capabilities() {
        let caps =
            pxlogic_capture_capabilities_from_profile("usb:test", profile(PxUsbSpeed::High, 0));
        assert_eq!(
            caps.profile_model.as_deref(),
            Some("PX-Logic U2 channel 32")
        );
        assert_eq!(caps.max_channels, 32);
        assert_eq!(caps.threshold_volts, PXLOGIC_THRESHOLD_VOLTAGES);

        let stream_32 = caps
            .channel_modes
            .iter()
            .find(|mode| mode.id == "STREAM_LOGIC5x32")
            .expect("stream 32 channel mode");
        assert!(stream_32.stream);
        assert_eq!(stream_32.channels, 32);
        assert_eq!(stream_32.max_sample_rate_hz, 5 * MHZ);
        assert!(stream_32.sample_rates.contains(&(5 * MHZ)));
        assert!(!stream_32.sample_rates.contains(&(10 * MHZ)));
    }
}
