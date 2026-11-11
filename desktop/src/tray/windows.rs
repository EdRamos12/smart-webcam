//! Windows notification-area icon. Left click opens the window, middle click
//! connects or disconnects, right click is the menu.

use super::{Command, Tray, TrayIcon as IconKind, TrayState};
use crate::config::Facing;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

pub struct WindowsTray {
    tx: Sender<TrayState>,
}

impl WindowsTray {
    pub async fn spawn(cmd: UnboundedSender<Command>) -> Result<Self> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                match build(&TrayState { status: "Starting…".into(), ..TrayState::default() }) {
                    Ok((tray, initial)) => {
                        let _ = ready_tx.send(Ok(tx));
                        run(tray, rx, cmd, initial);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .context("spawning the tray thread")?;
        let tx = ready_rx.await.context("tray thread exited")??;
        Ok(WindowsTray { tx })
    }
}

impl Tray for WindowsTray {
    fn update(&self, state: TrayState) {
        let _ = self.tx.send(state);
    }
}

fn build(state: &TrayState) -> Result<(tray_icon::TrayIcon, HashMap<String, Command>)> {
    let (menu, commands) = menu(state);
    let tray = TrayIconBuilder::new()
        .with_tooltip(&state.status)
        .with_icon(icon(state.icon))
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| anyhow::anyhow!("cannot create tray icon: {e}"))?;
    Ok((tray, commands))
}

fn run(tray: tray_icon::TrayIcon, rx: Receiver<TrayState>, cmd: UnboundedSender<Command>, initial: HashMap<String, Command>) {
    let mut commands = initial;
    let mut shown = IconKind::Idle;
    loop {
        while let Ok(state) = rx.try_recv() {
            let (menu, map) = menu(&state);
            commands = map;
            tray.set_menu(Some(Box::new(menu)));
            if tray.set_tooltip(Some(&state.status)).is_err() {
                log::debug!("could not update the tray tooltip");
            }
            if state.icon != shown {
                shown = state.icon;
                if tray.set_icon(Some(icon(state.icon))).is_err() {
                    log::warn!("could not update the tray icon");
                }
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button, button_state: MouseButtonState::Up, .. } = event {
                let command = match button {
                    MouseButton::Left => Some(Command::OpenWindow),
                    MouseButton::Middle => Some(Command::Toggle),
                    MouseButton::Right => None,
                };
                if let Some(command) = command {
                    let _ = cmd.send(command);
                }
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(command) = commands.get(event.id.as_ref()) {
                let _ = cmd.send(command.clone());
            }
        }
        pump_messages();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
    unsafe {
        let mut msg = std::mem::zeroed::<MSG>();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn menu(state: &TrayState) -> (Menu, HashMap<String, Command>) {
    let mut commands = HashMap::new();
    let menu = Menu::new();
    let mut append = |item: &dyn tray_icon::menu::IsMenuItem| {
        if let Err(e) = menu.append(item) {
            log::warn!("tray menu: {e}");
        }
    };

    append(&MenuItem::with_id("status", &state.status, false, None));
    let open = MenuItem::with_id("open", "Open Smart Webcam…", true, None);
    commands.insert("open".into(), Command::OpenWindow);
    append(&open);
    append(&PredefinedMenuItem::separator());

    let mut phone_items: Vec<CheckMenuItem> = Vec::new();
    if state.targets.is_empty() {
        phone_items.push(CheckMenuItem::with_id("no-phones", "No phones found", false, false, None));
    } else {
        for (i, target) in state.targets.iter().enumerate() {
            let id = format!("phone-{i}");
            let current = state.current.as_ref() == Some(target);
            phone_items.push(CheckMenuItem::with_id(&id, target.label(), true, current, None));
            commands.insert(id, Command::Connect(target.clone()));
        }
    }
    let phone_refs: Vec<&dyn tray_icon::menu::IsMenuItem> = phone_items.iter().map(|item| item as _).collect();
    if let Ok(connect) = Submenu::with_items("Connect", true, &phone_refs) {
        append(&connect);
    }
    let disconnect = MenuItem::with_id("disconnect", "Disconnect", state.current.is_some(), None);
    commands.insert("disconnect".into(), Command::Disconnect);
    append(&disconnect);
    append(&PredefinedMenuItem::separator());

    let mut cams = vec![("Back", Facing::Back)];
    if state.has_front {
        cams.push(("Front", Facing::Front));
    }
    append_checks(&mut append, &mut commands, "Camera", &cams, &state.facing.unwrap_or(Facing::Back), "cam", Command::SetFacing);

    let res: Vec<(String, (u32, u32))> = state.resolutions.iter().copied().map(|r| (res_label(r), r)).collect();
    append_checks(&mut append, &mut commands, "Resolution", &res, &state.resolution, "res", |(w, h)| Command::SetResolution(w, h));

    let rates: Vec<(String, u32)> = state.fps_options.iter().copied().map(|f| (format!("{f} fps"), f)).collect();
    append_checks(&mut append, &mut commands, "Frame rate", &rates, &state.fps, "fps", Command::SetFps);

    let rotations: Vec<(String, u32)> = [0, 90, 180, 270].into_iter().map(|r| (format!("{r}°"), r)).collect();
    append_checks(&mut append, &mut commands, "Rotation", &rotations, &state.rotation, "rot", Command::SetRotation);

    let mirror = CheckMenuItem::with_id("mirror", "Mirror", true, state.mirror, None);
    commands.insert("mirror".into(), Command::SetMirror(!state.mirror));
    append(&mirror);
    let torch = CheckMenuItem::with_id("torch", "Torch", state.torch_available, state.torch, None);
    commands.insert("torch".into(), Command::SetTorch(!state.torch));
    append(&torch);
    append(&PredefinedMenuItem::separator());

    let auto = CheckMenuItem::with_id("auto", "Auto-connect to last phone", true, state.auto_connect, None);
    commands.insert("auto".into(), Command::SetAutoConnect(!state.auto_connect));
    append(&auto);
    let autostart = CheckMenuItem::with_id("autostart", "Start on login", true, state.autostart, None);
    commands.insert("autostart".into(), Command::SetAutostart(!state.autostart));
    append(&autostart);
    if state.device_missing {
        let setup = MenuItem::with_id("setup", "Set up virtual camera…", true, None);
        commands.insert("setup".into(), Command::RunSetup);
        append(&setup);
    }
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    commands.insert("quit".into(), Command::Quit);
    append(&quit);
    (menu, commands)
}

fn append_checks<V, T, F>(
    append: &mut dyn FnMut(&dyn tray_icon::menu::IsMenuItem),
    commands: &mut HashMap<String, Command>,
    label: &str,
    options: &[(T, V)],
    current: &V,
    prefix: &str,
    cmd: F,
) where
    V: Clone + PartialEq,
    T: AsRef<str>,
    F: Fn(V) -> Command,
{
    let mut items = Vec::new();
    for (i, (text, value)) in options.iter().enumerate() {
        let id = format!("{prefix}-{i}");
        items.push(CheckMenuItem::with_id(&id, text.as_ref(), true, value == current, None));
        commands.insert(id, cmd(value.clone()));
    }
    let refs: Vec<&dyn tray_icon::menu::IsMenuItem> = items.iter().map(|item| item as _).collect();
    if let Ok(submenu) = Submenu::with_items(label, true, &refs) {
        append(&submenu);
    }
}

fn res_label((w, h): (u32, u32)) -> String {
    match h {
        2160 => "4K (3840×2160)".into(),
        _ => format!("{h}p ({w}×{h})"),
    }
}

fn icon(kind: IconKind) -> Icon {
    let (r, g, b) = match kind {
        IconKind::Idle => (140, 140, 140),
        IconKind::Busy => (230, 180, 40),
        IconKind::Streaming => (60, 200, 90),
        IconKind::Error => (220, 70, 60),
    };
    const N: u32 = 32;
    let mut pixels = vec![0u8; (N * N * 4) as usize];
    let center = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let d = (dx * dx + dy * dy).sqrt();
            let i = ((y * N + x) * 4) as usize;
            if d <= 14.0 {
                pixels[i] = r;
                pixels[i + 1] = g;
                pixels[i + 2] = b;
                pixels[i + 3] = 255;
            }
            if (6.0..9.0).contains(&d) {
                pixels[i] = 255;
                pixels[i + 1] = 255;
                pixels[i + 2] = 255;
                pixels[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(pixels, N, N).expect("32x32 icon")
}
