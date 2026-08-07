use std::sync::atomic::AtomicBool;

use crate::{
    error::Result,
    models::{
        Bitstreams, CaptureData, CaptureMetadata, CaptureProgress, CaptureSettings, DeviceInfo,
    },
};

pub type CaptureCancel = AtomicBool;

pub trait CaptureBackend: Send + Sync {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>>;
    fn prepare_device(&self, device_id: &str, bitstreams: Option<&Bitstreams>) -> Result<()>;
    fn capture(
        &self,
        settings: &CaptureSettings,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(CaptureProgress),
    ) -> Result<CaptureData>;

    fn capture_streaming(
        &self,
        settings: &CaptureSettings,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(CaptureProgress),
        started: &mut dyn FnMut(&CaptureMetadata),
        samples: &mut dyn FnMut(&[u8]),
    ) -> Result<CaptureData> {
        let capture = self.capture(settings, cancel, progress)?;
        started(&capture.metadata);
        samples(&capture.samples);
        Ok(capture)
    }

    fn stop_capture(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }
}
