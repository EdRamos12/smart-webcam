# Smart Webcam

Use an Android phone as a real webcam on Linux or Windows. It replaces the IP Webcam + OBS + virtual camera chain with two small apps:

- **`android/`**: Kotlin app. Camera2 feeds the hardware H.264 encoder, which streams over TCP. The camera only opens while a PC is connected.
- **`desktop/`**: Rust tray app (`smart-webcam`). It finds the phone over USB (adb) or Wi-Fi (mDNS) and decodes with OpenH264. On Linux it writes a v4l2loopback device named **"Smart Webcam"**. On Windows it feeds the **OBS Virtual Camera** (install [OBS Studio](https://obsproject.com/) once). Zoom, Meet, Discord, OBS and browsers see it as a normal camera.

The wire protocol is documented in [`protocol/PROTOCOL.md`](protocol/PROTOCOL.md).

## Build & install

### Phone
```sh
cd android
./gradlew assembleDebug        # uses JDK 17 (see gradle.properties); JDK 27 is too new for Gradle 8.11
adb install -r app/build/outputs/apk/debug/app-debug.apk
```
Open the app once. It asks for camera + notification permissions and starts a background service that waits for a PC.

### PC
```sh
cd desktop
cargo build --release
./target/release/smart-webcam setup     # one time: creates /dev/video10 "Smart Webcam" (asks for your password)
./target/release/smart-webcam           # tray app
./target/release/smart-webcam window    # preview/settings window (starts the tray app if needed)
```
Or build an Arch package: `cd packaging && makepkg -si`.

### Windows
Install [OBS Studio](https://obsproject.com/) once, plus Rust and a MinGW UCRT GCC (OpenH264 is compiled from source; NASM is optional and makes decoding faster). Then:

```powershell
cd desktop
cargo build --release
.\target\release\smart-webcam.exe setup    # registers OBS Virtual Camera if it isn't already (UAC prompt)
.\target\release\smart-webcam.exe          # tray app
.\target\release\smart-webcam.exe window   # preview/settings window
```

The camera shows up in other apps as **OBS Virtual Camera**. Quit OBS's own "Start Virtual Camera" while Smart Webcam is running — they share one camera. Settings are saved in `%APPDATA%\smart-webcam\config.toml`. The window talks to the tray app over `127.0.0.1` (the port is in `%TEMP%\smart-webcam.port`).

`setup` writes `/etc/modules-load.d/smart-webcam.conf` and `/etc/modprobe.d/smart-webcam.conf`, so the device survives reboots. It also creates a second device for OBS's own virtual camera. The tray app runs setup automatically on first start if the device is missing.

## Using it

- **USB**: enable USB debugging and plug the phone in, then choose *Connect ▸ <phone> (USB)*. If the app isn't running, the PC launches it through adb.
- **Wi-Fi**: be on the same network; the phone appears under *Connect* by itself. The first time, the phone asks *Allow <PC>?*. USB connections are always trusted. Manage trusted PCs in the phone app.
- **Left click** on the tray icon (or *Open Smart Webcam…*) opens the window: a live preview of exactly what apps receive, the phone list, all settings, and fps/bitrate/dropped-frame stats. **Middle click** connects to or disconnects from the last phone. With *Auto-connect* on, it reconnects whenever that phone shows up.
- The window is a separate process (`smart-webcam window`) that talks to the tray app over `$XDG_RUNTIME_DIR/smart-webcam.sock`. Closing it leaves the webcam running in the tray. The preview is only generated while a window is open.
- Camera (back/front), resolution, frame rate, rotation, mirror and torch can all be changed live from the tray.
- When no phone is streaming, the device shows a "Waiting for phone…" frame, so video apps never lose the camera.
- Settings are saved in `~/.config/smart-webcam/config.toml`.

The video is in landscape: hold the phone sideways with its top to the left (for the back camera). The front camera is corrected automatically. Use *Rotation* for other mounts.

## CLI (debugging)
```
smart-webcam headless [usb|usb:SERIAL|wifi:ID|HOST[:PORT]] [--device /dev/videoN]
smart-webcam dump usb out.h264 5     # raw stream → ffplay out.h264
smart-webcam devices                 # phones (USB + mDNS) and video devices
```

## Tests
```sh
cd desktop && cargo test      # protocol framing, YUV conversion/rotation, placeholder, adb parsing, config
```

## Notes / limits (v1)
- Video only (no audio). Linux uses v4l2loopback; Windows uses the OBS Virtual Camera DirectShow filter. The tray (`tray::Tray`) and the virtual camera (`vcam::VirtualCamera`) are traits.
- Decoding is in software (OpenH264): about 25–30% of one core for 1080p30.
- The phone app has no camera preview; settings are controlled from the PC (tray or window).
- If another app on the phone (e.g. face unlock) takes the camera, the stream pauses and resumes on its own.
