//! First-run system setup and login autostart.
//!
//! Linux loads v4l2loopback. Windows registers OBS Studio's virtual camera
//! (the DirectShow filter the app writes frames into).

use anyhow::{Context, Result};
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::bail;
    use crate::vcam::v4l2loopback::{find_device, CARD_LABEL};
    use std::time::Duration;

    pub const VIDEO_NR: u32 = 10;

    /// Loaded at boot. A second device keeps OBS's virtual camera working alongside ours.
    pub const MODPROBE_CONF: &str = "options v4l2loopback devices=2 video_nr=10,11 \
card_label=\"Smart Webcam\",\"OBS Virtual Camera\" exclusive_caps=1,1\n";
    pub const MODULES_LOAD_CONF: &str = "v4l2loopback\n";

    /// Shell script run as root through pkexec.
    fn root_script() -> String {
        format!(
            r#"set -e
printf '%s' '{modules}' > /etc/modules-load.d/smart-webcam.conf
printf '%s' '{modprobe}' > /etc/modprobe.d/smart-webcam.conf
if grep -q '^v4l2loopback ' /proc/modules; then
    # Module already loaded (e.g. by OBS): add our device next to the existing ones.
    v4l2loopback-ctl add -n '{label}' -x 1 /dev/video{nr} || v4l2loopback-ctl add -n '{label}' -x 1
else
    modprobe v4l2loopback
fi
"#,
            modules = MODULES_LOAD_CONF.replace('\'', "'\\''"),
            modprobe = MODPROBE_CONF.replace('\'', "'\\''"),
            label = CARD_LABEL,
            nr = VIDEO_NR,
        )
    }

    /// Makes sure a "Smart Webcam" loopback device exists, asking for admin rights if needed.
    pub fn ensure_device() -> Result<PathBuf> {
        if let Some(p) = find_device() {
            return Ok(p);
        }
        log::info!("no \"{CARD_LABEL}\" device; installing v4l2loopback config via pkexec");
        let status = std::process::Command::new("pkexec")
            .args(["sh", "-c", &root_script()])
            .status()
            .context("running pkexec")?;
        if !status.success() {
            bail!("setup was cancelled or failed ({status})");
        }
        for _ in 0..30 {
            if let Some(p) = find_device() {
                log::info!("created {}", p.display());
                return Ok(p);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("v4l2loopback loaded but no \"{CARD_LABEL}\" device appeared (an older module may be loaded with other options; reboot or `sudo modprobe -r v4l2loopback`)")
    }

    fn autostart_path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("autostart").join("smart-webcam.desktop")
    }

    pub fn set_autostart(enabled: bool) -> Result<()> {
        let path = autostart_path();
        if !enabled {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=Smart Webcam\nComment=Use your phone as a webcam\n\
                 Exec={}\nIcon=camera-web\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
        Ok(())
    }

    pub fn autostart_enabled() -> bool {
        autostart_path().exists()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn script_quotes_cleanly() {
            let s = super::root_script();
            assert!(s.contains(r#"card_label="Smart Webcam","OBS Virtual Camera""#));
            assert!(s.contains("/dev/video10"));
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "Smart Webcam";

    pub fn ensure_device() -> Result<PathBuf> {
        crate::vcam::obs::register()?;
        Ok(PathBuf::from(crate::vcam::obs::FRIENDLY_NAME))
    }

    fn run_key(write: bool) -> Result<RegKey> {
        let flags = if write { KEY_READ | KEY_SET_VALUE } else { KEY_READ };
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, flags).context("opening the startup registry key")
    }

    pub fn set_autostart(enabled: bool) -> Result<()> {
        let key = run_key(true)?;
        if !enabled {
            match key.delete_value(RUN_VALUE) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))?;
        Ok(())
    }

    pub fn autostart_enabled() -> bool {
        run_key(false).ok().and_then(|key| key.get_value::<String, _>(RUN_VALUE).ok()).is_some()
    }
}

#[cfg(target_os = "linux")]
pub use linux::{autostart_enabled, ensure_device, set_autostart};
#[cfg(windows)]
pub use windows::{autostart_enabled, ensure_device, set_autostart};
