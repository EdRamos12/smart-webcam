//! Virtual camera backends. Linux uses v4l2loopback; Windows writes frames into
//! the OBS Virtual Camera shared-memory queue.

use crate::convert::OutputFormat;
use anyhow::Result;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub mod v4l2loopback;
#[cfg(windows)]
pub mod obs;

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

/// Creates the platform's virtual camera. `device` overrides auto-detection on Linux.
pub fn create(device: Option<PathBuf>) -> Result<Box<dyn VirtualCamera>> {
    #[cfg(target_os = "linux")]
    {
        let path = match device {
            Some(p) => p,
            None => v4l2loopback::find_device().ok_or_else(|| {
                anyhow::anyhow!("no \"{}\" v4l2loopback device; run `smart-webcam setup`", v4l2loopback::CARD_LABEL)
            })?,
        };
        Ok(Box::new(v4l2loopback::V4l2Loopback::new(path)))
    }
    #[cfg(windows)]
    {
        let _ = device;
        Ok(Box::new(obs::ObsCamera::new()))
    }
}

/// True when the virtual camera still has to be installed.
pub fn needs_setup(device: &Option<PathBuf>) -> bool {
    #[cfg(target_os = "linux")]
    {
        device.is_none() && v4l2loopback::find_device().is_none()
    }
    #[cfg(windows)]
    {
        let _ = device;
        !obs::is_registered()
    }
}

/// Video devices this process can see. The smart-webcam one is [`our_device_name`].
pub fn devices() -> Vec<(PathBuf, String)> {
    #[cfg(target_os = "linux")]
    {
        v4l2loopback::devices()
    }
    #[cfg(windows)]
    {
        if obs::is_registered() {
            vec![(PathBuf::from(obs::FRIENDLY_NAME), obs::FRIENDLY_NAME.into())]
        } else {
            Vec::new()
        }
    }
}

pub fn our_device_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        v4l2loopback::CARD_LABEL
    }
    #[cfg(windows)]
    {
        obs::FRIENDLY_NAME
    }
}
