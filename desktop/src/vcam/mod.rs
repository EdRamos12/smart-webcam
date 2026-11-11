//! Virtual camera backends. Linux uses v4l2loopback; Windows will get its own implementation.

use crate::convert::OutputFormat;
use anyhow::Result;

#[cfg(target_os = "linux")]
pub mod v4l2loopback;

pub trait VirtualCamera: Send {
    /// Opens the device with the requested format. Returns the format actually in effect,
    /// which can differ when a consumer already holds the device with another size.
    fn open(&mut self, format: OutputFormat, fps: u32) -> Result<OutputFormat>;
    /// Writes one frame of `format.frame_size()` bytes.
    fn write_frame(&mut self, frame: &[u8]) -> Result<()>;
    fn close(&mut self);
    /// Human readable device description, e.g. `/dev/video10`.
    fn describe(&self) -> String;
}

/// Creates the platform's virtual camera. `device` overrides auto-detection.
#[cfg(target_os = "linux")]
pub fn create(device: Option<std::path::PathBuf>) -> Result<Box<dyn VirtualCamera>> {
    let path = match device {
        Some(p) => p,
        None => v4l2loopback::find_device()
            .ok_or_else(|| anyhow::anyhow!("no \"{}\" v4l2loopback device; run `smart-webcam setup`", v4l2loopback::CARD_LABEL))?,
    };
    Ok(Box::new(v4l2loopback::V4l2Loopback::new(path)))
}
