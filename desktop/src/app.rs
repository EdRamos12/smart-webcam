//! The controller: owns settings and the connection, reacts to tray commands,
//! discovery changes and session events, and keeps the tray and placeholder in sync.

use crate::adb::{self, UsbDevice};
use crate::client::{self, Hello, SessionCmd, SessionEnd, SessionEvent, StreamParams};
use crate::config::{Config, Target};
use crate::discovery::Phones;
use crate::protocol::{Caps, Started, PORT};
use crate::setup;
use crate::tray::{Command, Stats, Tray, TrayIcon, TrayState};
use crate::video::{VideoEvent, VideoMsg, VideoSettings, VideoSink};
use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

const STANDARD_RESOLUTIONS: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (3840, 2160)];

pub enum Event {
    Cmd(Command),
    Session(u64, SessionEvent),
    Conn(u64, ConnState),
    Video(VideoEvent),
    WifiChanged,
    UsbChanged(Vec<UsbDevice>),
    /// Once a second, to update the stats.
    Tick,
}

#[derive(Debug)]
pub enum ConnState {
    Connecting,
    Retrying(String),
    Ended(SessionEnd),
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Idle,
    Connecting,
    Pairing,
    Starting,
    Streaming(Started),
    Retrying(String),
    Denied(String),
    /// The phone reported a camera problem (e.g. another app took the camera); it retries itself.
    PhoneError(String),
}

struct Session {
    id: u64,
    target: Target,
    cmd: UnboundedSender<SessionCmd>,
    params: Arc<Mutex<(StreamParams, bool)>>,
}

pub struct App {
    cfg: Config,
    tray: Box<dyn Tray>,
    video: VideoSink,
    phones: Phones,
    usb: Vec<UsbDevice>,
    tx: UnboundedSender<Event>,
    session: Option<Session>,
    next_id: u64,
    status: Status,
    caps: Option<Caps>,
    torch: bool,
    /// Set by an explicit Disconnect (or denial) so auto-connect doesn't kick in again.
    hold_off: bool,
    device_error: Option<String>,
    device_info: String,
    hello: Arc<Hello>,
    /// Full state for windows; the tray gets the same minus the stats.
    ui_tx: watch::Sender<TrayState>,
    last_tray: Option<TrayState>,
    stats: Stats,
    last_counts: (u64, Instant),
    window: Option<std::process::Child>,
    phone_stats: (f32, u32),
    rtt_ms: f32,
}

impl App {
    pub fn new(
        cfg: Config,
        tray: Box<dyn Tray>,
        video: VideoSink,
        phones: Phones,
        tx: UnboundedSender<Event>,
        ui_tx: watch::Sender<TrayState>,
    ) -> Self {
        let hello = Arc::new(Hello { pc_id: cfg.pc_id.clone(), pc_name: crate::config::hostname() });
        App {
            cfg,
            tray,
            video,
            phones,
            usb: Vec::new(),
            tx,
            session: None,
            next_id: 0,
            status: Status::Idle,
            caps: None,
            torch: false,
            hold_off: false,
            device_error: None,
            device_info: String::new(),
            hello,
            ui_tx,
            last_tray: None,
            stats: Stats::default(),
            last_counts: (0, Instant::now()),
            window: None,
            phone_stats: (0.0, 0),
            rtt_ms: 0.0,
        }
    }

    pub fn video_settings(cfg: &Config) -> VideoSettings {
        let (w, h) = if cfg.rotation % 180 == 90 { (cfg.height, cfg.width) } else { (cfg.width, cfg.height) };
        VideoSettings {
            width: w as usize,
            height: h as usize,
            fps: cfg.fps,
            pixel: cfg.pixel_format,
            mirror: cfg.mirror,
            rotation: cfg.rotation,
        }
    }

    fn stream_params(&self) -> StreamParams {
        let c = &self.cfg;
        // Scale the configured (1080p30) bitrate with pixel count and frame rate.
        let scale = (c.width * c.height) as f64 / (1920.0 * 1080.0) * (c.fps as f64 / 30.0).max(1.0);
        let bitrate = (c.bitrate as f64 * scale.max(0.4)).clamp(1.5e6, 40e6) as u32;
        StreamParams { facing: c.camera, width: c.width, height: c.height, fps: c.fps, bitrate }
    }

    pub async fn run(mut self, mut rx: UnboundedReceiver<Event>) {
        self.refresh();
        self.maybe_auto_connect();
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Cmd(Command::Quit) => break,
                Event::Cmd(c) => self.on_command(c),
                Event::Session(id, ev) if self.is_current(id) => self.on_session_event(ev),
                Event::Conn(id, st) if self.is_current(id) => self.on_conn_state(st),
                Event::Session(..) | Event::Conn(..) => {}
                Event::Video(VideoEvent::NeedKeyframe) => {
                    if let Some(s) = &self.session {
                        let _ = s.cmd.send(SessionCmd::RequestKeyframe);
                    }
                }
                Event::Video(VideoEvent::DeviceReady(d)) => {
                    log::info!("virtual camera ready: {d}");
                    self.device_error = None;
                    self.device_info = d;
                }
                Event::Video(VideoEvent::DeviceError(e)) => {
                    self.device_info = "no virtual camera".into();
                    self.device_error = Some(e);
                }
                Event::Tick => {
                    if self.window.as_mut().is_some_and(|w| !matches!(w.try_wait(), Ok(None))) {
                        self.window = None; // reap a closed window
                    }
                    if !self.update_stats() {
                        continue;
                    }
                }
                Event::WifiChanged => self.maybe_auto_connect(),
                Event::UsbChanged(list) => {
                    self.usb = list;
                    self.maybe_auto_connect();
                }
            }
            self.refresh();
        }
        self.disconnect();
        self.video.send(VideoMsg::Shutdown);
        if let Some(mut w) = self.window.take() {
            let _ = w.kill();
            let _ = w.wait();
        }
    }

    fn is_current(&self, id: u64) -> bool {
        self.session.as_ref().is_some_and(|s| s.id == id)
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::Connect(t) => {
                self.hold_off = false;
                self.connect(t);
            }
            Command::Disconnect => {
                self.hold_off = true;
                self.disconnect();
            }
            Command::Toggle => {
                if self.session.is_some() {
                    self.hold_off = true;
                    self.disconnect();
                } else if let Some(t) = self.cfg.last_device.clone().filter(|t| self.available(t)).or_else(|| self.targets().into_iter().next()) {
                    self.hold_off = false;
                    self.connect(t);
                }
            }
            Command::SetFacing(f) => {
                self.cfg.camera = f;
                self.torch = false;
                self.push_params();
            }
            Command::SetResolution(w, h) => {
                self.cfg.width = w;
                self.cfg.height = h;
                self.push_params();
            }
            Command::SetFps(f) => {
                self.cfg.fps = f;
                self.push_params();
            }
            Command::SetRotation(r) => {
                self.cfg.rotation = r % 360;
                self.push_video_settings();
            }
            Command::SetMirror(m) => {
                self.cfg.mirror = m;
                self.push_video_settings();
            }
            Command::SetTorch(on) => {
                self.torch = on;
                if let Some(s) = &self.session {
                    s.params.lock().unwrap().1 = on;
                    let _ = s.cmd.send(SessionCmd::Torch(on));
                }
            }
            Command::SetBitrate(b) => {
                self.cfg.bitrate = b;
                self.push_params();
            }
            Command::OpenWindow => self.open_window(),
            Command::SetAutoConnect(on) => {
                self.cfg.auto_connect = on;
                self.save();
            }
            Command::SetAutostart(on) => match setup::set_autostart(on) {
                Ok(()) => {
                    self.cfg.autostart = on;
                    self.save();
                }
                Err(e) => log::error!("autostart: {e:#}"),
            },
            Command::RunSetup => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let ev = match setup::ensure_device() {
                        Ok(p) => VideoEvent::DeviceReady(p.display().to_string()),
                        Err(e) => VideoEvent::DeviceError(format!("{e:#}")),
                    };
                    let _ = tx.send(Event::Video(ev));
                });
            }
            Command::Quit => {}
        }
    }

    fn open_window(&mut self) {
        if let Some(child) = &mut self.window {
            if matches!(child.try_wait(), Ok(None)) {
                return; // already open
            }
        }
        match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("window").spawn()) {
            Ok(child) => self.window = Some(child),
            Err(e) => log::error!("cannot open window: {e}"),
        }
    }

    /// Turns the video counters into rates. Returns true if anything visible changed.
    fn update_stats(&mut self) -> bool {
        let c = &self.video.counters;
        let frames = c.frames.load(Ordering::Relaxed);
        let bytes = c.bytes.swap(0, Ordering::Relaxed);
        let (last_frames, at) = self.last_counts;
        let secs = at.elapsed().as_secs_f32().max(0.001);
        self.last_counts = (frames, Instant::now());
        let stats = if matches!(self.status, Status::Streaming(_)) {
            Stats {
                fps: (frames - last_frames) as f32 / secs,
                mbps: bytes as f32 * 8.0 / secs / 1e6,
                dropped: c.dropped.load(Ordering::Relaxed),
                encode_ms: self.phone_stats.0,
                network_ms: self.rtt_ms / 2.0,
                pc_ms: {
                    let (sum, n) = (c.latency_us.swap(0, Ordering::Relaxed), c.latency_n.swap(0, Ordering::Relaxed));
                    if n > 0 { sum as f32 / n as f32 / 1000.0 } else { 0.0 }
                },
                phone_queue: self.phone_stats.1,
            }
        } else {
            Stats::default()
        };
        let changed = stats != self.stats;
        self.stats = stats;
        changed
    }

    fn save(&self) {
        if let Err(e) = self.cfg.save() {
            log::warn!("saving config: {e:#}");
        }
    }

    fn push_params(&mut self) {
        self.save();
        self.push_video_settings();
        let p = self.stream_params();
        if let Some(s) = &self.session {
            s.params.lock().unwrap().0 = p.clone();
            let _ = s.cmd.send(SessionCmd::Configure(p));
        }
    }

    fn push_video_settings(&mut self) {
        self.save();
        self.video.send(VideoMsg::Settings(Self::video_settings(&self.cfg)));
    }

    // ---- connection management ----

    fn targets(&self) -> Vec<Target> {
        let mut v: Vec<Target> = self
            .usb
            .iter()
            .filter(|d| d.state == "device")
            .map(|d| Target::Usb { serial: d.serial.clone(), name: d.model.clone() })
            .collect();
        v.extend(self.phones.list().into_iter().map(|p| Target::Wifi { id: p.id, name: p.name }));
        if let Some(t @ Target::Addr { .. }) = &self.cfg.last_device {
            v.push(t.clone());
        }
        v
    }

    fn available(&self, t: &Target) -> bool {
        match t {
            Target::Usb { serial, .. } => self.usb.iter().any(|d| &d.serial == serial && d.state == "device"),
            Target::Wifi { id, .. } => self.phones.find(id).is_some(),
            Target::Addr { .. } => true,
        }
    }

    fn maybe_auto_connect(&mut self) {
        if self.session.is_some() || self.hold_off || !self.cfg.auto_connect {
            return;
        }
        if let Some(t) = self.cfg.last_device.clone().filter(|t| self.available(t)) {
            log::info!("auto-connecting to {}", t.label());
            self.connect(t);
        }
    }

    fn connect(&mut self, target: Target) {
        self.disconnect();
        self.next_id += 1;
        let id = self.next_id;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let params = Arc::new(Mutex::new((self.stream_params(), false)));
        self.torch = false;
        self.caps = None;
        self.status = Status::Connecting;
        self.cfg.last_device = Some(target.clone());
        self.save();
        tokio::spawn(connection_loop(
            id,
            target.clone(),
            self.hello.clone(),
            params.clone(),
            self.video.clone(),
            self.phones.clone(),
            self.tx.clone(),
            cmd_rx,
        ));
        self.session = Some(Session { id, target, cmd: cmd_tx, params });
    }

    /// Dropping the command sender makes the session say bye and stop.
    fn disconnect(&mut self) {
        if self.session.take().is_some() {
            self.video.send(VideoMsg::Stopped);
        }
        self.status = Status::Idle;
        self.caps = None;
        self.torch = false;
    }

    fn on_session_event(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::PairingPending => self.status = Status::Pairing,
            SessionEvent::Caps(c) => {
                // Remember the phone's real model name (CLI/auto targets may only know an id).
                if let Some(s) = &mut self.session {
                    if let Target::Wifi { name, .. } = &mut s.target {
                        if !c.device_name.is_empty() && *name != c.device_name {
                            *name = c.device_name.clone();
                            self.cfg.last_device = Some(s.target.clone());
                            self.save();
                        }
                    }
                }
                self.caps = Some(c);
                self.status = Status::Starting;
            }
            SessionEvent::Started(s) => {
                log::info!("streaming {}x{}@{} from camera {} ({})", s.w, s.h, s.fps, s.camera, s.facing);
                self.status = Status::Streaming(s);
            }
            SessionEvent::PhoneStats { encode_ms, queued } => {
                self.phone_stats = (encode_ms, queued);
                return;
            }
            SessionEvent::Rtt(ms) => {
                self.rtt_ms = ms;
                return;
            }
            SessionEvent::PhoneError(e) => {
                log::warn!("phone: {e}");
                self.status = Status::PhoneError(e);
            }
        }
    }

    fn on_conn_state(&mut self, st: ConnState) {
        match st {
            ConnState::Connecting => {
                if !matches!(self.status, Status::Retrying(_)) {
                    self.status = Status::Connecting;
                }
            }
            ConnState::Retrying(e) => {
                self.video.send(VideoMsg::Stopped);
                self.status = Status::Retrying(e);
            }
            ConnState::Ended(SessionEnd::Denied(r)) => {
                self.session = None;
                self.hold_off = true;
                self.video.send(VideoMsg::Stopped);
                self.status = Status::Denied(r);
            }
            ConnState::Ended(SessionEnd::Closed) => {
                self.session = None;
                self.video.send(VideoMsg::Stopped);
                self.status = Status::Idle;
            }
        }
    }

    // ---- presentation ----

    fn refresh(&mut self) {
        let name = self.session.as_ref().map(|s| s.target.name().to_string()).unwrap_or_default();
        let (status, placeholder, icon) = match &self.status {
            Status::Idle => ("Idle".to_string(), "Waiting for phone…".to_string(), TrayIcon::Idle),
            Status::Connecting => (format!("Connecting to {name}…"), format!("Connecting to {name}…"), TrayIcon::Busy),
            Status::Pairing => (
                format!("Allow this PC on {name}…"),
                "Tap Allow on your phone".to_string(),
                TrayIcon::Busy,
            ),
            Status::Starting => (format!("Starting camera on {name}…"), "Starting camera…".to_string(), TrayIcon::Busy),
            Status::Streaming(s) => (
                format!("Streaming {}p{} from {name}", s.h.min(s.w), s.fps),
                "Waiting for video…".to_string(),
                TrayIcon::Streaming,
            ),
            Status::Retrying(e) => (format!("Reconnecting to {name}… ({e})"), format!("Reconnecting to {name}…"), TrayIcon::Busy),
            Status::PhoneError(e) => (format!("{name}: {e}"), "Camera busy on the phone…".to_string(), TrayIcon::Busy),
            Status::Denied(_) => ("Access denied on the phone".to_string(), "Access denied on the phone".to_string(), TrayIcon::Error),
        };
        let (status, icon) = match &self.device_error {
            Some(_) => (format!("{status} — no virtual camera (run setup)"), TrayIcon::Error),
            None => (status, icon),
        };
        self.video.send(VideoMsg::Status(placeholder));

        let cam = self.caps.as_ref().and_then(|c| c.cameras.iter().find(|x| x.facing == self.cfg.camera.as_str()));
        let mut resolutions: Vec<(u32, u32)> = STANDARD_RESOLUTIONS
            .iter()
            .copied()
            .filter(|r| r.1 <= 1080 || cam.is_some_and(|c| c.sizes.contains(r)))
            .collect();
        if !resolutions.contains(&(self.cfg.width, self.cfg.height)) {
            resolutions.push((self.cfg.width, self.cfg.height));
        }
        let mut fps_options: Vec<u32> = match cam {
            Some(c) => c.fps.iter().copied().filter(|&f| f >= 15).collect(),
            None => vec![30, 60],
        };
        if !fps_options.contains(&self.cfg.fps) {
            fps_options.push(self.cfg.fps);
            fps_options.sort();
        }
        let has_front = self.caps.as_ref().is_none_or(|c| c.cameras.iter().any(|x| x.facing == "front"));

        let state = TrayState {
            status,
            icon,
            current: self.session.as_ref().map(|s| s.target.clone()),
            targets: self.targets(),
            facing: Some(self.cfg.camera),
            has_front,
            resolution: (self.cfg.width, self.cfg.height),
            resolutions,
            fps: self.cfg.fps,
            fps_options,
            rotation: self.cfg.rotation,
            mirror: self.cfg.mirror,
            torch: self.torch,
            torch_available: matches!(self.status, Status::Streaming(_)) && cam.is_some_and(|c| c.torch),
            auto_connect: self.cfg.auto_connect,
            autostart: self.cfg.autostart,
            device_missing: self.device_error.is_some(),
            device: self.device_info.clone(),
            bitrate: self.cfg.bitrate,
            stats: self.stats.clone(),
        };
        // The tray doesn't show stats; don't spam D-Bus with them.
        let tray_view = TrayState { stats: Stats::default(), ..state.clone() };
        if self.last_tray.as_ref() != Some(&tray_view) {
            self.tray.update(tray_view.clone());
            self.last_tray = Some(tray_view);
        }
        self.ui_tx.send_replace(state);
    }
}

/// Resolves a target to socket addresses. For USB this sets up an `adb forward`.
async fn resolve(target: &Target, phones: &Phones) -> Result<(Vec<SocketAddr>, Option<(String, u16)>)> {
    match target {
        Target::Usb { serial, .. } => {
            let port = adb::forward(serial).await?;
            Ok((vec![SocketAddr::from(([127, 0, 0, 1], port))], Some((serial.clone(), port))))
        }
        Target::Wifi { id, name } => match phones.find(id) {
            Some(p) => Ok((p.addrs, None)),
            None => bail!("{name} not found on the network"),
        },
        Target::Addr { addr } => {
            let with_port = if addr.contains(':') { addr.clone() } else { format!("{addr}:{PORT}") };
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(with_port).await.context("resolving address")?.collect();
            Ok((addrs, None))
        }
    }
}

/// Keeps a session to `target` alive with backoff until the command channel is dropped.
#[allow(clippy::too_many_arguments)]
async fn connection_loop(
    id: u64,
    target: Target,
    hello: Arc<Hello>,
    params: Arc<Mutex<(StreamParams, bool)>>,
    video: VideoSink,
    phones: Phones,
    tx: UnboundedSender<Event>,
    mut cmds: UnboundedReceiver<SessionCmd>,
) {
    let send = |st: ConnState| {
        let _ = tx.send(Event::Conn(id, st));
    };
    let mut attempt: u32 = 0;
    let mut launched_app = false;
    loop {
        send(ConnState::Connecting);
        let started = std::time::Instant::now();
        let result = async {
            let (addrs, forward) = resolve(&target, &phones).await?;
            let mut last_err = anyhow::anyhow!("no address");
            for addr in addrs {
                let (p, torch) = params.lock().unwrap().clone();
                let ev_tx = tx.clone();
                let res = client::run(addr, &hello, p, torch, video.clone(), &mut cmds, |ev| {
                    let _ = ev_tx.send(Event::Session(id, ev));
                })
                .await;
                if let Some((serial, port)) = &forward {
                    adb::remove_forward(serial, *port).await;
                }
                match res {
                    Ok(end) => return Ok(end),
                    Err(e) => last_err = e,
                }
            }
            Err(last_err)
        }
        .await;

        if cmds.is_closed() {
            return; // disconnected by the user
        }
        let err = match result {
            Ok(SessionEnd::Denied(r)) => {
                send(ConnState::Ended(SessionEnd::Denied(r)));
                return;
            }
            Ok(SessionEnd::Closed) => "phone closed the stream".to_string(),
            Err(e) => format!("{e:#}"),
        };
        log::info!("{}: {err}", target.label());

        // Over USB an immediately closed connection means the app isn't running: launch it.
        if let Target::Usb { serial, .. } = &target {
            if !launched_app && started.elapsed() < Duration::from_secs(2) {
                launched_app = true;
                if adb::app_installed(serial).await {
                    log::info!("starting Smart Webcam app on {serial}");
                    let _ = adb::start_app(serial).await;
                } else {
                    send(ConnState::Retrying("app not installed on the phone".into()));
                }
            }
        }

        if started.elapsed() > Duration::from_secs(10) {
            attempt = 0; // it was working; retry quickly
            launched_app = false; // and the app may have been killed since, so allow relaunching it again
        }
        attempt += 1;
        send(ConnState::Retrying(short_error(&err)));
        let delay = Duration::from_millis((500 * 2u64.pow(attempt.min(4))).min(5000));
        // Sleep, but wake up immediately if the user disconnects.
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = wait_closed(&mut cmds) => return,
        }
    }
}

async fn wait_closed(cmds: &mut UnboundedReceiver<SessionCmd>) {
    while cmds.recv().await.is_some() {}
}

fn short_error(e: &str) -> String {
    let e = e.split(": ").last().unwrap_or(e);
    if e.len() > 60 { format!("{}…", &e[..e.floor_char_boundary(60)]) } else { e.to_string() }
}

/// Polls `adb devices` and reports changes.
pub async fn poll_usb(tx: UnboundedSender<Event>) {
    let mut last: Option<Vec<UsbDevice>> = None;
    let mut warned = false;
    loop {
        let list = match adb::devices().await {
            Ok(l) => l,
            Err(e) => {
                if !warned {
                    log::warn!("USB discovery disabled: {e:#}");
                    warned = true;
                }
                Vec::new()
            }
        };
        if last.as_ref() != Some(&list) {
            if tx.send(Event::UsbChanged(list.clone())).is_err() {
                return;
            }
            last = Some(list);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
