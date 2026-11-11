//! USB path: drives the `adb` binary (no adb protocol reimplementation).

use crate::protocol::PORT;
use anyhow::{bail, Context, Result};
use tokio::process::Command;

pub const PACKAGE: &str = "com.edu.smartwebcam";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub serial: String,
    pub model: String,
    /// "device", "unauthorized", "offline", ...
    pub state: String,
}

fn adb() -> Command {
    let mut c = Command::new(std::env::var_os("ADB").unwrap_or_else(|| "adb".into()));
    c.kill_on_drop(true);
    c
}

async fn run(args: &[&str]) -> Result<String> {
    let out = adb().args(args).output().await.context("running adb (is it installed?)")?;
    if !out.status.success() {
        bail!("adb {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Devices from `adb devices -l`. Network (adb-over-Wi-Fi) entries are skipped.
pub async fn devices() -> Result<Vec<UsbDevice>> {
    Ok(parse_devices(&run(&["devices", "-l"]).await?))
}

pub fn parse_devices(out: &str) -> Vec<UsbDevice> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            if serial.contains(':') || serial.starts_with("emulator-") {
                return None;
            }
            let model = parts
                .find_map(|p| p.strip_prefix("model:"))
                .map(|m| m.replace('_', " "))
                .unwrap_or_else(|| serial.clone());
            Some(UsbDevice { serial, model, state })
        })
        .collect()
}

/// `adb forward tcp:0 tcp:47800`; returns the local port adb picked.
pub async fn forward(serial: &str) -> Result<u16> {
    let out = run(&["-s", serial, "forward", "tcp:0", &format!("tcp:{PORT}")]).await?;
    out.trim().parse().with_context(|| format!("unexpected adb forward output: {out:?}"))
}

pub async fn remove_forward(serial: &str, local: u16) {
    let _ = run(&["-s", serial, "forward", "--remove", &format!("tcp:{local}")]).await;
}

pub async fn app_installed(serial: &str) -> bool {
    run(&["-s", serial, "shell", "pm", "path", PACKAGE]).await.is_ok_and(|o| o.contains("package:"))
}

/// Launches the phone app, which starts its streaming service.
pub async fn start_app(serial: &str) -> Result<()> {
    run(&["-s", serial, "shell", "am", "start", "-n", &format!("{PACKAGE}/.MainActivity")]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices() {
        let out = "* daemon started successfully\nList of devices attached\n\
            RXCX30531FL            device usb:1-14 product:a35xjvxx model:SM_A356E device:a35x transport_id:1\n\
            0123456789ABCDEF       unauthorized usb:1-2 transport_id:2\n\
            192.168.1.5:5555       device product:x model:Pixel_7 transport_id:3\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], UsbDevice { serial: "RXCX30531FL".into(), model: "SM A356E".into(), state: "device".into() });
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[1].model, "0123456789ABCDEF");
    }
}
