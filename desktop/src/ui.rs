//! `smart-webcam window`: live preview and settings, talking to the tray app over [`crate::ipc`].

use crate::config::{Facing, Target};
use crate::ipc::{self, Incoming};
use crate::tray::{Command, TrayIcon, TrayState};
use crate::video::Preview;
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bitrate presets (at 1080p30; the app scales them for other modes).
const QUALITY: [(&str, u32); 4] = [("Low", 4_000_000), ("Normal", 8_000_000), ("High", 12_000_000), ("Very high", 16_000_000)];

#[derive(Default)]
struct Shared {
    state: Option<TrayState>,
    preview: Option<Preview>,
    writer: Option<UnixStream>,
}

pub fn run() -> Result<()> {
    let client = connect_or_launch()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Smart Webcam")
            .with_app_id("smart-webcam")
            .with_inner_size([1040.0, 600.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Smart Webcam",
        options,
        Box::new(move |cc| {
            let shared = Arc::new(Mutex::new(Shared { writer: client.writer().ok(), ..Default::default() }));
            spawn_reader(client, shared.clone(), cc.egui_ctx.clone());
            let screenshot = std::env::var_os("SMART_WEBCAM_SCREENSHOT").map(std::path::PathBuf::from);
            let now = std::time::Instant::now();
            Ok(Box::new(Window { shared, texture: None, screenshot, started: now, preview_age: (0, 0), preview_age_at: now, preview_ms: 0.0 }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("window: {e}"))
}

/// Connects to the running tray app, starting it first if needed.
fn connect_or_launch() -> Result<ipc::Client> {
    if let Ok(c) = ipc::Client::connect() {
        return Ok(c);
    }
    log::info!("starting the Smart Webcam tray app");
    std::process::Command::new(std::env::current_exe()?)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("starting smart-webcam")?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(c) = ipc::Client::connect() {
            return Ok(c);
        }
    }
    anyhow::bail!("the Smart Webcam tray app did not start")
}

/// Reads state and previews; reconnects if the tray app restarts.
fn spawn_reader(mut client: ipc::Client, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    std::thread::spawn(move || loop {
        match client.read() {
            Ok(Incoming::State(s)) => shared.lock().unwrap().state = Some(*s),
            Ok(Incoming::Preview(p)) => shared.lock().unwrap().preview = Some(p),
            Err(_) => {
                {
                    let mut sh = shared.lock().unwrap();
                    sh.state = None;
                    sh.writer = None;
                }
                ctx.request_repaint();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if let Ok(c) = ipc::Client::connect() {
                        shared.lock().unwrap().writer = c.writer().ok();
                        client = c;
                        break;
                    }
                }
            }
        }
        ctx.request_repaint();
    });
}

struct Window {
    shared: Arc<Mutex<Shared>>,
    texture: Option<egui::TextureHandle>,
    /// Debug aid: `SMART_WEBCAM_SCREENSHOT=out.ppm` saves the window after a few seconds.
    screenshot: Option<std::path::PathBuf>,
    started: std::time::Instant,
    preview_age: (u64, u32),
    preview_age_at: std::time::Instant,
    preview_ms: f32,
}

impl Window {
    fn send(&self, cmd: Command) {
        if let Some(w) = self.shared.lock().unwrap().writer.as_mut() {
            if let Err(e) = ipc::send_command(w, &cmd) {
                log::warn!("sending {cmd:?}: {e}");
            }
        }
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.debug_screenshot(ui.ctx());
        let (state, preview) = {
            let mut sh = self.shared.lock().unwrap();
            (sh.state.clone(), sh.preview.take())
        };
        if let Some(p) = preview {
            // How long after reaching the virtual camera this frame is shown.
            let age = crate::video::now_us().saturating_sub(p.written_us);
            self.preview_age.0 += age;
            self.preview_age.1 += 1;
            if self.preview_age_at.elapsed() >= Duration::from_secs(1) {
                self.preview_ms = self.preview_age.0 as f32 / self.preview_age.1.max(1) as f32 / 1000.0;
                log::debug!("preview: {} frames/s, {:.1} ms behind the virtual camera", self.preview_age.1, self.preview_ms);
                self.preview_age = (0, 0);
                self.preview_age_at = std::time::Instant::now();
            }
            let image = egui::ColorImage::from_rgb([p.width, p.height], &p.rgb);
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ui.ctx().load_texture("preview", image, egui::TextureOptions::LINEAR)),
            }
        }

        let Some(st) = state else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Waiting for the Smart Webcam tray app…").size(18.0));
                });
            });
            return;
        };

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, &st));
        egui::Panel::right("settings").resizable(false).exact_size(320.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.settings(ui, &st));
        });
        egui::CentralPanel::default().show(ui, |ui| self.preview(ui));
    }
}

impl Window {
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        if self.screenshot.is_none() {
            return;
        }
        if self.started.elapsed() < Duration::from_secs(4) {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        let path = self.screenshot.take().unwrap();
        ctx.request_screenshot(move |img| {
            let mut ppm = format!("P6 {} {} 255\n", img.size[0], img.size[1]).into_bytes();
            ppm.extend(img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]));
            let _ = std::fs::write(path, ppm);
        });
    }

    fn preview(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, Color32::BLACK);
        match &self.texture {
            Some(t) => {
                let [w, h] = t.size();
                let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
                let img = egui::Rect::from_center_size(rect.center(), egui::vec2(w as f32 * scale, h as f32 * scale));
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                painter.image(t.id(), img, uv, Color32::WHITE);
            }
            None => {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No preview yet", egui::FontId::proportional(16.0), Color32::GRAY);
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui, st: &TrayState) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let color = match st.icon {
                TrayIcon::Streaming => Color32::from_rgb(60, 200, 90),
                TrayIcon::Busy => Color32::from_rgb(230, 180, 40),
                TrayIcon::Error => Color32::from_rgb(220, 70, 60),
                TrayIcon::Idle => Color32::GRAY,
            };
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 5.0, color);
            ui.label(RichText::new(&st.status).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&st.device).weak());
                if st.icon == TrayIcon::Streaming {
                    ui.separator();
                    let x = &st.stats;
                    ui.label(format!("{:.1} fps · {:.1} Mbit/s · {} dropped", x.fps, x.mbps, x.dropped));
                    ui.separator();
                    ui.label(format!("delay ≈ {:.0} ms", x.encode_ms + x.network_ms + x.pc_ms)).on_hover_text(format!(
                        "Camera → encoded on the phone: {:.0} ms\nNetwork (½ round trip): {:.0} ms\nPC decode → virtual camera: {:.0} ms\nPhone send queue: {} packets\n\nThis preview: +{:.0} ms after the virtual camera\n(other apps add their own buffering)",
                        x.encode_ms, x.network_ms, x.pc_ms, x.phone_queue, self.preview_ms
                    ));
                }
            });
        });
        ui.add_space(4.0);
    }

    fn settings(&self, ui: &mut egui::Ui, st: &TrayState) {
        ui.add_space(6.0);
        ui.heading("Phone");
        ui.add_space(4.0);
        if st.targets.is_empty() {
            ui.label(RichText::new("No phones found. Open Smart Webcam on the phone (same Wi-Fi), or plug it in with USB debugging on.").weak());
        }
        for t in &st.targets {
            ui.horizontal(|ui| {
                let current = st.current.as_ref() == Some(t);
                ui.label(if current { RichText::new(target_label(t)).strong() } else { RichText::new(target_label(t)) });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if current {
                        if ui.button("Disconnect").clicked() {
                            self.send(Command::Disconnect);
                        }
                    } else if ui.button("Connect").clicked() {
                        self.send(Command::Connect(t.clone()));
                    }
                });
            });
        }
        if let Some(cur) = st.current.as_ref().filter(|c| !st.targets.contains(c)) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(target_label(cur)).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        self.send(Command::Disconnect);
                    }
                });
            });
        }

        ui.add_space(10.0);
        ui.separator();
        ui.heading("Video");
        ui.add_space(4.0);
        egui::Grid::new("video").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Camera");
            ui.horizontal(|ui| {
                let mut facing = st.facing.unwrap_or(Facing::Back);
                ui.selectable_value(&mut facing, Facing::Back, "Back");
                ui.add_enabled_ui(st.has_front, |ui| ui.selectable_value(&mut facing, Facing::Front, "Front"));
                if Some(facing) != st.facing {
                    self.send(Command::SetFacing(facing));
                }
            });
            ui.end_row();

            ui.label("Resolution");
            let mut res = st.resolution;
            egui::ComboBox::from_id_salt("res").selected_text(res_label(res)).show_ui(ui, |ui| {
                for &r in &st.resolutions {
                    ui.selectable_value(&mut res, r, res_label(r));
                }
            });
            if res != st.resolution {
                self.send(Command::SetResolution(res.0, res.1));
            }
            ui.end_row();

            ui.label("Frame rate");
            let mut fps = st.fps;
            egui::ComboBox::from_id_salt("fps").selected_text(format!("{fps} fps")).show_ui(ui, |ui| {
                for &f in &st.fps_options {
                    ui.selectable_value(&mut fps, f, format!("{f} fps"));
                }
            });
            if fps != st.fps {
                self.send(Command::SetFps(fps));
            }
            ui.end_row();

            ui.label("Quality");
            let mut bitrate = st.bitrate;
            let current = QUALITY.iter().find(|q| q.1 == bitrate).map(|q| q.0.to_string()).unwrap_or_else(|| format!("{} Mbit/s", bitrate / 1_000_000));
            egui::ComboBox::from_id_salt("quality").selected_text(current).show_ui(ui, |ui| {
                for (name, b) in QUALITY {
                    ui.selectable_value(&mut bitrate, b, format!("{name} ({} Mbit/s at 1080p)", b / 1_000_000));
                }
            });
            if bitrate != st.bitrate {
                self.send(Command::SetBitrate(bitrate));
            }
            ui.end_row();

            ui.label("Rotation");
            ui.horizontal(|ui| {
                let mut rot = st.rotation;
                for r in [0, 90, 180, 270] {
                    ui.selectable_value(&mut rot, r, format!("{r}°"));
                }
                if rot != st.rotation {
                    self.send(Command::SetRotation(rot));
                }
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        let mut mirror = st.mirror;
        if ui.checkbox(&mut mirror, "Mirror").changed() {
            self.send(Command::SetMirror(mirror));
        }
        let mut torch = st.torch;
        if ui.add_enabled(st.torch_available, egui::Checkbox::new(&mut torch, "Torch")).changed() {
            self.send(Command::SetTorch(torch));
        }

        ui.add_space(10.0);
        ui.separator();
        ui.heading("General");
        ui.add_space(4.0);
        let mut auto = st.auto_connect;
        if ui.checkbox(&mut auto, "Auto-connect to last phone").changed() {
            self.send(Command::SetAutoConnect(auto));
        }
        let mut autostart = st.autostart;
        if ui.checkbox(&mut autostart, "Start on login").changed() {
            self.send(Command::SetAutostart(autostart));
        }
        if st.device_missing {
            ui.add_space(6.0);
            ui.colored_label(Color32::from_rgb(220, 120, 60), "The virtual camera device is missing.");
            if ui.button("Set up virtual camera…").clicked() {
                self.send(Command::RunSetup);
            }
        }
        ui.add_space(10.0);
        ui.separator();
        if ui.button("Quit Smart Webcam").clicked() {
            self.send(Command::Quit);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn target_label(t: &Target) -> String {
    match t {
        Target::Usb { name, .. } => format!("{name} (USB)"),
        Target::Wifi { name, .. } => format!("{name} (Wi-Fi)"),
        Target::Addr { addr } => addr.clone(),
    }
}

fn res_label((w, h): (u32, u32)) -> String {
    if h == 2160 { "4K".into() } else { format!("{h}p ({w}×{h})") }
}
