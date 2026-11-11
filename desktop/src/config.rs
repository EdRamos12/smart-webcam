//! `~/.config/smart-webcam/config.toml`

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where to find a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    /// Over USB through `adb forward`.
    Usb { serial: String, name: String },
    /// Over Wi-Fi, found through mDNS by its stable id.
    Wifi { id: String, name: String },
    /// A fixed `host:port`.
    Addr { addr: String },
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Usb { name, .. } => format!("{name} (USB)"),
            Target::Wifi { name, .. } => format!("{name} (Wi-Fi)"),
            Target::Addr { addr } => addr.clone(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Target::Usb { name, .. } | Target::Wifi { name, .. } => name,
            Target::Addr { addr } => addr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Facing {
    Back,
    Front,
}

impl Facing {
    pub fn as_str(self) -> &'static str {
        match self {
            Facing::Back => "back",
            Facing::Front => "front",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    Yuyv,
    I420,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Identifies this PC to phones for pairing.
    pub pc_id: String,
    pub last_device: Option<Target>,
    pub auto_connect: bool,
    pub camera: Facing,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub mirror: bool,
    /// Extra clockwise rotation applied on top of the automatic sensor correction.
    pub rotation: u32,
    pub pixel_format: PixelFormat,
    /// Explicit /dev/videoN; by default the device labelled "Smart Webcam" is used.
    pub device: Option<PathBuf>,
    pub autostart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pc_id: random_id(),
            last_device: None,
            auto_connect: true,
            camera: Facing::Back,
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 8_000_000,
            mirror: false,
            rotation: 0,
            pixel_format: PixelFormat::Yuyv,
            device: None,
            autostart: false,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("smart-webcam").join("config.toml")
    }

    /// Loads the config, creating it (with a fresh `pc_id`) if missing.
    pub fn load() -> Config {
        let path = Self::path();
        let cfg = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                log::warn!("ignoring invalid {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        if let Err(e) = cfg.save() {
            log::warn!("cannot save config: {e:#}");
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?).context("writing config")?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

fn random_id() -> String {
    let mut buf = [0u8; 16];
    if !fill_random(&mut buf) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        buf[..16].copy_from_slice(&t.as_nanos().to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn fill_random(buf: &mut [u8]) -> bool {
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, buf)).is_ok()
    }
    #[cfg(windows)]
    {
        // RtlGenRandom. Documented for this use by the Win32 `SystemFunction036` export.
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn SystemFunction036(buf: *mut u8, len: u32) -> u8;
        }
        unsafe { SystemFunction036(buf.as_mut_ptr(), buf.len() as u32) != 0 }
    }
}

pub fn hostname() -> String {
    #[cfg(windows)]
    {
        return std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "PC".into());
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "PC".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.last_device = Some(Target::Usb { serial: "R58".into(), name: "Pixel".into() });
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.last_device, c.last_device);
        assert_eq!(back.pc_id, c.pc_id);
        assert_eq!(c.pc_id.len(), 32);
        // Missing fields fall back to defaults.
        let partial: Config = toml::from_str("fps = 60").unwrap();
        assert_eq!(partial.fps, 60);
        assert_eq!(partial.width, 1920);
    }
}
