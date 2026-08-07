use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported PXLogic device")]
    UnsupportedDevice,
    #[error("invalid channel count: {0}")]
    InvalidChannelCount(u8),
    #[error("invalid samplerate: {0}")]
    InvalidSamplerate(u64),
    #[error("invalid capture duration")]
    InvalidCaptureDuration,
    #[error("invalid register ACK: 0x{0:08x}")]
    InvalidRegisterAck(u32),
    #[error("capture was cancelled")]
    Cancelled,
    #[error("PXLogic FPGA bitstreams were not provided")]
    MissingBitstreams,
    #[error("USB error: {0}")]
    Usb(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode error: {0}")]
    Decode(String),
}

#[cfg(feature = "rusb-transport")]
impl From<rusb::Error> for CoreError {
    fn from(value: rusb::Error) -> Self {
        Self::Usb(value.to_string())
    }
}
