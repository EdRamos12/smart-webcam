//! KDE/freedesktop StatusNotifierItem tray (native on Plasma Wayland).

use super::{Command, Tray, TrayIcon, TrayState};
use crate::config::Facing;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, TrayMethods};
use tokio::sync::mpsc::UnboundedSender;

struct SniTray {
    state: TrayState,
    cmd: UnboundedSender<Command>,
}

impl SniTray {
    fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }
}

fn item(label: impl Into<String>, enabled: bool, cmd: Command) -> MenuItem<SniTray> {
    StandardItem {
        label: label.into(),
        enabled,
        activate: Box::new(move |t: &mut SniTray| t.send(cmd.clone())),
        ..Default::default()
    }
    .into()
}

fn check(label: &str, checked: bool, enabled: bool, cmd: impl Fn(bool) -> Command + Send + 'static) -> MenuItem<SniTray> {
    CheckmarkItem {
        label: label.into(),
        checked,
        enabled,
        activate: Box::new(move |t: &mut SniTray| t.send(cmd(!checked))),
        ..Default::default()
    }
    .into()
}

fn radio<V: Clone + PartialEq + Send + 'static>(
    label: &str,
    options: Vec<(String, V)>,
    current: &V,
    cmd: impl Fn(V) -> Command + Send + 'static,
) -> MenuItem<SniTray> {
    let selected = options.iter().position(|(_, v)| v == current).unwrap_or(usize::MAX);
    let labels = options.iter().map(|(l, _)| RadioItem { label: l.clone(), ..Default::default() }).collect();
    let values: Vec<V> = options.into_iter().map(|(_, v)| v).collect();
    SubMenu {
        label: label.into(),
        submenu: vec![RadioGroup {
            selected,
            select: Box::new(move |t: &mut SniTray, i| {
                if let Some(v) = values.get(i) {
                    t.send(cmd(v.clone()));
                }
            }),
            options: labels,
        }
        .into()],
        ..Default::default()
    }
    .into()
}

fn res_label((w, h): (u32, u32)) -> String {
    match h {
        2160 => "4K (3840×2160)".into(),
        _ => format!("{h}p ({w}×{h})"),
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "smart-webcam".into()
    }

    fn title(&self) -> String {
        "Smart Webcam".into()
    }

    fn icon_name(&self) -> String {
        match self.state.icon {
            TrayIcon::Idle => "camera-off",
            TrayIcon::Busy => "camera-ready",
            TrayIcon::Streaming => "camera-on",
            TrayIcon::Error => "dialog-warning",
        }
        .into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Smart Webcam".into(), description: self.state.status.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::OpenWindow);
    }

    /// Middle click.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let mut menu = vec![
            StandardItem { label: s.status.clone(), enabled: false, ..Default::default() }.into(),
            item("Open Smart Webcam…", true, Command::OpenWindow),
            MenuItem::Separator,
        ];

        let mut connect: Vec<MenuItem<Self>> = s
            .targets
            .iter()
            .map(|t| {
                let current = s.current.as_ref() == Some(t);
                let target = t.clone();
                CheckmarkItem {
                    label: t.label(),
                    checked: current,
                    activate: Box::new(move |tray: &mut SniTray| tray.send(Command::Connect(target.clone()))),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        if connect.is_empty() {
            connect.push(StandardItem { label: "No phones found".into(), enabled: false, ..Default::default() }.into());
        }
        menu.push(SubMenu { label: "Connect".into(), submenu: connect, ..Default::default() }.into());
        menu.push(item("Disconnect", s.current.is_some(), Command::Disconnect));
        menu.push(MenuItem::Separator);

        let mut cams = vec![("Back".to_string(), Facing::Back)];
        if s.has_front {
            cams.push(("Front".to_string(), Facing::Front));
        }
        menu.push(radio("Camera", cams, &s.facing.unwrap_or(Facing::Back), Command::SetFacing));
        menu.push(radio(
            "Resolution",
            s.resolutions.iter().map(|&r| (res_label(r), r)).collect(),
            &s.resolution,
            |(w, h)| Command::SetResolution(w, h),
        ));
        menu.push(radio(
            "Frame rate",
            s.fps_options.iter().map(|&f| (format!("{f} fps"), f)).collect(),
            &s.fps,
            Command::SetFps,
        ));
        menu.push(radio(
            "Rotation",
            [0, 90, 180, 270].into_iter().map(|r| (format!("{r}°"), r)).collect(),
            &s.rotation,
            Command::SetRotation,
        ));
        menu.push(check("Mirror", s.mirror, true, Command::SetMirror));
        menu.push(check("Torch", s.torch, s.torch_available, Command::SetTorch));
        menu.push(MenuItem::Separator);
        menu.push(check("Auto-connect to last phone", s.auto_connect, true, Command::SetAutoConnect));
        menu.push(check("Start on login", s.autostart, true, Command::SetAutostart));
        if s.device_missing {
            menu.push(item("Set up virtual camera…", true, Command::RunSetup));
        }
        menu.push(item("Quit", true, Command::Quit));
        menu
    }
}

pub struct KsniTray {
    tx: tokio::sync::watch::Sender<TrayState>,
}

impl KsniTray {
    pub async fn spawn(cmd: UnboundedSender<Command>) -> anyhow::Result<Self> {
        let initial = TrayState { status: "Starting…".into(), ..Default::default() };
        let tray = SniTray { state: initial.clone(), cmd };
        let handle = tray.spawn().await.map_err(|e| anyhow::anyhow!("cannot create tray icon: {e}"))?;
        // A single task applies updates in order; intermediate states may be skipped.
        let (tx, mut rx) = tokio::sync::watch::channel(initial);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let state = rx.borrow_and_update().clone();
                if handle.update(|t| t.state = state).await.is_none() {
                    break;
                }
            }
        });
        Ok(KsniTray { tx })
    }
}

impl Tray for KsniTray {
    fn update(&self, state: TrayState) {
        self.tx.send_replace(state);
    }
}
