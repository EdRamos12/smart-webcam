//! System tray abstraction. Linux uses `ksni` (StatusNotifierItem); Windows can plug in `tray-icon`.

use crate::config::{Facing, Target};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod ksni;
#[cfg(windows)]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TrayIcon {
    #[default]
    Idle,
    Busy,
    Streaming,
    Error,
}

/// Live numbers shown in the window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub fps: f32,
    pub mbps: f32,
    pub dropped: u64,
    /// Camera → encoded on the phone.
    pub encode_ms: f32,
    /// Half the ping round trip.
    pub network_ms: f32,
    /// Network arrival → written to the virtual camera.
    pub pc_ms: f32,
    /// Packets waiting in the phone's send queue.
    pub phone_queue: u32,
}

/// Everything the tray and the window need to render themselves; rebuilt by the app on every change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrayState {
    pub status: String,
    pub icon: TrayIcon,
    /// Phone we are connected (or trying to connect) to.
    pub current: Option<Target>,
    /// Phones that can be connected right now.
    pub targets: Vec<Target>,
    pub facing: Option<Facing>,
    pub has_front: bool,
    pub resolution: (u32, u32),
    pub resolutions: Vec<(u32, u32)>,
    pub fps: u32,
    pub fps_options: Vec<u32>,
    pub rotation: u32,
    pub mirror: bool,
    pub torch: bool,
    pub torch_available: bool,
    pub auto_connect: bool,
    pub autostart: bool,
    pub device_missing: bool,
    /// Virtual camera description, e.g. "/dev/video10 1920x1080", or the error.
    pub device: String,
    /// Configured bitrate at 1080p30 (scaled for other modes).
    pub bitrate: u32,
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Connect(Target),
    Disconnect,
    /// Left click: connect to the last phone or disconnect.
    Toggle,
    SetFacing(Facing),
    SetResolution(u32, u32),
    SetFps(u32),
    SetRotation(u32),
    SetMirror(bool),
    SetTorch(bool),
    SetBitrate(u32),
    OpenWindow,
    SetAutoConnect(bool),
    SetAutostart(bool),
    RunSetup,
    Quit,
}

pub trait Tray: Send {
    fn update(&self, state: TrayState);
}

/// Used in headless mode: just logs status changes.
#[derive(Default)]
pub struct LogTray {
    last: std::sync::Mutex<String>,
}

impl Tray for LogTray {
    fn update(&self, state: TrayState) {
        let mut last = self.last.lock().unwrap();
        if *last != state.status {
            log::info!("status: {}", state.status);
            *last = state.status;
        }
    }
}
