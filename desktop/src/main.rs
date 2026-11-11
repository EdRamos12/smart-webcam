mod adb;
mod app;
mod client;
mod config;
mod convert;
mod decoder;
mod discovery;
mod ipc;
mod placeholder;
mod protocol;
mod setup;
mod tray;
mod ui;
mod vcam;
mod video;

use anyhow::{bail, Context, Result};
use app::{App, Event};
use config::{Config, Target};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::mpsc;
use tray::{Command, Tray};

const USAGE: &str = "\
smart-webcam — use your phone as a webcam

USAGE:
    smart-webcam [--device /dev/videoN]              run in the system tray
    smart-webcam window                              open the preview/settings window
    smart-webcam headless [TARGET] [--device ...]    run without a tray (logs to stderr)
    smart-webcam setup                               create the \"Smart Webcam\" v4l2loopback device
    smart-webcam dump TARGET OUT.h264 [SECONDS]      save the raw H.264 stream (debugging)
    smart-webcam devices                             list phones and video devices

TARGET:
    usb             first phone connected over USB
    usb:SERIAL      a specific USB phone (see `adb devices`)
    wifi:ID         a phone found over mDNS (see `devices`)
    HOST[:PORT]     connect directly
";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let device = take_flag(&mut args, "--device")?.map(PathBuf::from);
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    let rt = tokio::runtime::Runtime::new()?;
    match args.first().map(String::as_str) {
        None => rt.block_on(run(device, None, true)),
        Some("headless") => {
            let target = match args.get(1) {
                Some(t) => Some(rt.block_on(parse_target(t))?),
                None => None,
            };
            rt.block_on(run(device, target, false))
        }
        Some("window") => {
            drop(rt);
            ui::run()
        }
        Some("setup") => {
            let p = setup::ensure_device()?;
            println!("virtual camera ready at {}", p.display());
            Ok(())
        }
        Some("dump") => {
            let (Some(t), Some(out)) = (args.get(1), args.get(2)) else { bail!("usage: dump TARGET OUT.h264 [SECONDS]") };
            let secs: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(10);
            rt.block_on(dump(t, out, secs))
        }
        Some("devices") => rt.block_on(list_devices()),
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            if i + 1 >= args.len() {
                bail!("{name} needs a value");
            }
            let v = args.remove(i + 1);
            args.remove(i);
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

async fn parse_target(s: &str) -> Result<Target> {
    if s == "usb" {
        let d = adb::devices()
            .await?
            .into_iter()
            .find(|d| d.state == "device")
            .context("no authorized phone connected over USB")?;
        return Ok(Target::Usb { serial: d.serial, name: d.model });
    }
    if let Some(serial) = s.strip_prefix("usb:") {
        let name = adb::devices().await.ok().and_then(|l| l.into_iter().find(|d| d.serial == serial)).map(|d| d.model);
        return Ok(Target::Usb { serial: serial.into(), name: name.unwrap_or_else(|| serial.into()) });
    }
    if let Some(id) = s.strip_prefix("wifi:") {
        return Ok(Target::Wifi { id: id.into(), name: id.into() });
    }
    Ok(Target::Addr { addr: s.into() })
}

/// Holds an exclusive lock for the process lifetime so only one instance drives the device.
fn single_instance() -> Result<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join("smart-webcam.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    use std::os::fd::AsRawFd;
    // SAFETY: plain flock on a file descriptor we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Smart Webcam is already running");
    }
    Ok(file)
}

async fn run(device: Option<PathBuf>, target: Option<Target>, with_tray: bool) -> Result<()> {
    let _lock = single_instance()?;
    let mut cfg = Config::load();
    cfg.autostart = setup::autostart_enabled();

    let (tx, rx) = mpsc::unbounded_channel::<Event>();

    // Virtual camera. If the device is missing, offer the one-time setup.
    // `--device` is a one-off override; it is never written to the config.
    let vcam_device = device.or_else(|| cfg.device.clone());
    if vcam_device.is_none() && vcam::v4l2loopback::find_device().is_none() && with_tray {
        if let Err(e) = tokio::task::spawn_blocking(setup::ensure_device).await? {
            log::warn!("{e:#}");
        }
    }
    let cam: Box<dyn vcam::VirtualCamera> = match vcam::create(vcam_device.clone()) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("{e:#}");
            Box::new(LazyCamera { device: vcam_device, inner: None })
        }
    };
    let video_tx = tx.clone();
    let viewers = Arc::new(AtomicUsize::new(0));
    let (preview_tx, preview_rx) = tokio::sync::watch::channel(None);
    let tap = video::PreviewTap { viewers: viewers.clone(), tx: preview_tx };
    let (video, video_thread) = video::spawn(cam, App::video_settings(&cfg), tap, move |ev| {
        let _ = video_tx.send(Event::Video(ev));
    });

    // Discovery: mDNS for Wi-Fi, adb polling for USB.
    let wifi_tx = tx.clone();
    let (phones, _mdns) = discovery::start(move || {
        let _ = wifi_tx.send(Event::WifiChanged);
    })?;
    tokio::spawn(app::poll_usb(tx.clone()));

    // Tray (or a logger in headless mode). Tray commands go to the same event queue.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
    let tray: Box<dyn Tray> = if with_tray {
        Box::new(tray::ksni::KsniTray::spawn(cmd_tx.clone()).await?)
    } else {
        Box::new(tray::LogTray::default())
    };
    let fwd = tx.clone();
    tokio::spawn(async move {
        while let Some(c) = cmd_rx.recv().await {
            let _ = fwd.send(Event::Cmd(c));
        }
    });
    if let Some(t) = target {
        let _ = cmd_tx.send(Command::Connect(t));
    }

    // Window IPC and the once-a-second stats tick.
    let (ui_tx, ui_rx) = tokio::sync::watch::channel(tray::TrayState::default());
    let ipc_cmds = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = ipc::serve(ui_rx, preview_rx, viewers, ipc_cmds).await {
            log::warn!("window socket: {e:#}");
        }
    });
    let tick = tx.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            every.tick().await;
            if tick.send(Event::Tick).is_err() {
                break;
            }
        }
    });
    let quit = cmd_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = quit.send(Command::Quit);
    });

    App::new(cfg, tray, video, phones, tx, ui_tx).run(rx).await;
    let _ = std::fs::remove_file(ipc::socket_path());
    let _ = tokio::task::spawn_blocking(move || video_thread.join()).await;
    Ok(())
}

/// Virtual camera that appears later (e.g. after setup); retried by the video thread.
struct LazyCamera {
    device: Option<PathBuf>,
    inner: Option<Box<dyn vcam::VirtualCamera>>,
}

impl vcam::VirtualCamera for LazyCamera {
    fn open(&mut self, format: convert::OutputFormat, fps: u32) -> Result<convert::OutputFormat> {
        if self.inner.is_none() {
            self.inner = Some(vcam::create(self.device.clone())?);
        }
        self.inner.as_mut().unwrap().open(format, fps)
    }
    fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        self.inner.as_mut().context("no device")?.write_frame(frame)
    }
    fn close(&mut self) {
        if let Some(c) = &mut self.inner {
            c.close();
        }
    }
    fn describe(&self) -> String {
        self.inner.as_ref().map(|c| c.describe()).unwrap_or_else(|| "(no device)".into())
    }
}

/// Debug helper: connect, request a stream and save the raw Annex-B H.264.
async fn dump(target: &str, out: &str, secs: u64) -> Result<()> {
    use protocol::Frame;
    use serde_json::json;
    use std::io::Write;
    use tokio::net::TcpStream;

    let target = parse_target(target).await?;
    let (addr, forward) = match &target {
        Target::Usb { serial, .. } => {
            let port = adb::forward(serial).await?;
            (format!("127.0.0.1:{port}"), Some((serial.clone(), port)))
        }
        Target::Addr { addr } => (if addr.contains(':') { addr.clone() } else { format!("{addr}:{}", protocol::PORT) }, None),
        Target::Wifi { .. } => bail!("dump supports usb or HOST[:PORT]"),
    };
    let cfg = Config::load();
    let mut s = TcpStream::connect(&addr).await.with_context(|| format!("connecting to {addr}"))?;
    protocol::write_handshake(&mut s).await?;
    protocol::read_handshake(&mut s).await?;
    protocol::write_ctrl(&mut s, &json!({"type":"hello","pc_id":cfg.pc_id,"pc_name":config::hostname()})).await?;
    let mut file = std::fs::File::create(out)?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let (mut packets, mut bytes) = (0usize, 0usize);
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout_at(deadline, protocol::read_frame(&mut s)).await {
            Ok(f) => f?,
            Err(_) => break,
        };
        match frame {
            Frame::Ctrl(v) => {
                eprintln!("ctrl: {v}");
                match v["type"].as_str() {
                    Some("caps") => {
                        let caps: protocol::Caps = serde_json::from_value(v)?;
                        let p = client::StreamParams { facing: cfg.camera, width: cfg.width, height: cfg.height, fps: cfg.fps, bitrate: cfg.bitrate };
                        protocol::write_ctrl(&mut s, &client::build_config(&caps, &p)).await?;
                    }
                    Some("denied") => bail!("denied by the phone"),
                    _ => {}
                }
            }
            Frame::Video(p) => {
                file.write_all(&p.data)?;
                packets += 1;
                bytes += p.data.len();
            }
            Frame::Unknown(_) => {}
        }
    }
    let _ = protocol::write_ctrl(&mut s, &json!({"type":"bye"})).await;
    if let Some((serial, port)) = forward {
        adb::remove_forward(&serial, port).await;
    }
    eprintln!("wrote {packets} packets ({:.1} Mbit/s) to {out}; play with: ffplay {out}", bytes as f64 * 8.0 / secs as f64 / 1e6);
    Ok(())
}

async fn list_devices() -> Result<()> {
    println!("USB phones (adb):");
    match adb::devices().await {
        Ok(l) if l.is_empty() => println!("  none"),
        Ok(l) => l.iter().for_each(|d| println!("  usb:{}  {} [{}]", d.serial, d.model, d.state)),
        Err(e) => println!("  adb unavailable: {e:#}"),
    }
    println!("Wi-Fi phones (mDNS, 3 s scan):");
    let (phones, daemon) = discovery::start(|| {})?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let list = phones.list();
    if list.is_empty() {
        println!("  none");
    }
    for p in list {
        println!("  wifi:{}  {} {:?}", p.id, p.name, p.addrs);
    }
    let _ = daemon.shutdown();
    println!("Video devices:");
    for (path, name) in vcam::v4l2loopback::devices() {
        let mark = if name == vcam::v4l2loopback::CARD_LABEL { "  <- Smart Webcam" } else { "" };
        println!("  {}  {name}{mark}", path.display());
    }
    Ok(())
}
