pub mod capture;
pub mod decode;
pub mod decoder_backend;
pub mod error;
pub mod fake;
pub mod models;
pub mod protocol;
pub mod pxview_profile;
pub mod transport;

#[cfg(feature = "rusb-transport")]
pub mod rusb_backend;

pub use capture::{
    align_cross_sample_count, build_capture_register_script,
    build_capture_register_script_for_channels, build_pxview_pwm0_register_script,
    capture_channel_mask, capture_profile_from_settings, decode_cross_data,
    decode_cross_data_to_physical_channels, decode_cross_data_with_map, enabled_channel_mask,
    generate_sample_words, generate_sample_words_with_count, gpio_timing_for_samplerate,
    pxview_capture_transfer_size, pxview_cross_raw_byte_count, pxview_trigger_position,
    resolve_enabled_channels, sample_count_from_duration, sample_count_from_settings,
    supported_samplerates, unitsize_for_channel_count, CaptureProfile, DemoAnalyzerSettings,
    RegisterWrite,
};
pub use decode::{
    decode_analyzer, decode_i2c, decode_spi, decode_uart, AnalyzerDecodeSettings,
    DecodedChannelValue, DecodedFrame, DecodedProtocolMarker, I2cDecodeSettings,
    NativeDecodeSettings, NativeOptionValue, SpiDecodeSettings, UartDecodeSettings, UartParity,
    UartStopBits,
};
pub use decoder_backend::{
    DecoderAnnotation, DecoderAnnotationKind, DecoderBackend, DecoderBackendInfo,
    DecoderBackendIsolation, DecoderBackendKind, DecoderDiagnostic, DecoderField,
    DecoderFieldValue, DecoderMarker, DecoderOutput, DecoderOutputFrame, LegacyRustDecoder,
    SaleaeNativeDecoder, SigrokDecoderCatalog, SigrokDecoderCatalogItem, SigrokDecoderChannel,
    SigrokDecoderOption, SigrokNativeDecoder,
};
pub use error::{CoreError, Result};
pub use fake::FakeBackend;
pub use models::{
    Bitstreams, CaptureData, CaptureMetadata, CaptureMode, CaptureProgress, CaptureSettings,
    CaptureTriggerKind, CaptureTriggerMetadata, DeviceInfo, DeviceKind, DiagnosticEvent,
    DiagnosticLevel, ExportFormat, ExternalTriggerMode, HardwareCaptureCapabilities,
    HardwareChannelModeInfo, PwmConfiguration, PwmSettings, SparseCaptureData, SparseCaptureView,
    SparseDigitalChannel, SparseDigitalChannelView, WaveformSource,
};
pub use pxview_profile::{
    generic_logic_capture_capabilities, pxlogic_capture_capabilities_from_profile,
    resolve_pxlogic_capture_plan, resolve_pxlogic_device_profile, PxUsbSpeed, PxlogicCapturePlan,
    PxlogicChannelMode, PxlogicChannelModeId, PxlogicDeviceProfile,
};
pub use transport::{CaptureBackend, CaptureCancel};

#[cfg(feature = "rusb-transport")]
pub use rusb_backend::RusbBackend;
