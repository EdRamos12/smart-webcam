//! Windows virtual camera: the OBS Studio DirectShow filter ("OBS Virtual Camera").
//!
//! The filter lives in whatever app opened the camera and reads NV12 frames from a
//! named file mapping. The layout is OBS's `shared-memory-queue.c` as of OBS 33
//! (`OBSVirtualCamVideo`, 3 slots, 32-byte frame header, 32-byte alignment).
//! OBS's own "Start Virtual Camera" uses the same mapping, so only one producer
//! can run at a time.

use super::VirtualCamera;
use crate::config::PixelFormat;
use crate::convert::OutputFormat;
use anyhow::{bail, Context, Result};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

pub const FRIENDLY_NAME: &str = "OBS Virtual Camera";
const CLSID: &str = r"SOFTWARE\Classes\CLSID\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";

const STATE_STARTING: u32 = 1;
const STATE_READY: u32 = 2;
const STATE_STOPPING: u32 = 3;
const FRAME_HEADER: usize = 32;

/// `struct queue_header` from OBS. `interval` is 8-aligned, so there are 4
/// padding bytes after `cy` (the struct is 80 bytes).
#[repr(C)]
struct QueueHeader {
    write_idx: u32,
    read_idx: u32,
    state: u32,
    offsets: [u32; 3],
    type_: u32,
    cx: u32,
    cy: u32,
    interval: u64,
    reserved: [u32; 8],
}

struct Mapping {
    handle: *mut std::ffi::c_void,
    view: *mut u8,
    cx: u32,
    cy: u32,
    offsets: [u32; 3],
    /// Set when this process owns the queue. A probe that only looks at an
    /// existing queue must not mark it STOPPING on drop.
    writer: bool,
}

// The video thread and the repeat thread share this only through the mutex.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                if self.writer {
                    let header = self.view as *mut QueueHeader;
                    (*header).state = STATE_STOPPING;
                }
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
                self.view = std::ptr::null_mut();
            }
            if !self.handle.is_null() {
                CloseHandle(self.handle);
                self.handle = std::ptr::null_mut();
            }
        }
    }
}

fn map_view(handle: *mut std::ffi::c_void) -> Result<*mut u8> {
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0) };
    if view.Value.is_null() {
        bail!("MapViewOfFile failed ({})", unsafe { GetLastError() });
    }
    Ok(view.Value.cast())
}

impl Mapping {
    fn create(cx: u32, cy: u32, interval: u64) -> Result<Self> {
        let (size, offsets) = queue_layout(cx, cy);
        let name = windows_sys::w!("OBSVirtualCamVideo");
        unsafe {
            let existing = OpenFileMappingW(FILE_MAP_READ, 0, name);
            if !existing.is_null() {
                CloseHandle(existing);
                bail!("OBS virtual camera queue is already open");
            }
            let handle = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, size, name);
            if handle.is_null() {
                bail!("CreateFileMappingW failed ({})", GetLastError());
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                bail!("OBS virtual camera queue is already open");
            }
            let view = map_view(handle).inspect_err(|_| {
                CloseHandle(handle);
            })?;
            let header = QueueHeader {
                write_idx: 0,
                read_idx: 0,
                state: STATE_STARTING,
                offsets,
                type_: 0,
                cx,
                cy,
                interval,
                reserved: [0; 8],
            };
            view.cast::<QueueHeader>().write(header);
            Ok(Mapping { handle, view, cx, cy, offsets, writer: true })
        }
    }

    fn attach() -> Result<Self> {
        let name = windows_sys::w!("OBSVirtualCamVideo");
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, name);
            if handle.is_null() {
                bail!("OBS virtual camera queue disappeared ({})", GetLastError());
            }
            let view = map_view(handle).inspect_err(|_| {
                CloseHandle(handle);
            })?;
            let (cx, cy, offsets) = {
                let header = &*view.cast::<QueueHeader>();
                if header.cx < 2 || header.cy < 2 || header.cx > 8192 || header.cy > 8192 {
                    let (cx, cy) = (header.cx, header.cy);
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.cast() });
                    CloseHandle(handle);
                    bail!("existing virtual camera has an unusable size {cx}x{cy}");
                }
                (header.cx, header.cy, header.offsets)
            };
            Ok(Mapping { handle, view, cx, cy, offsets, writer: false })
        }
    }

    /// Another producer (OBS's own virtual camera) is advancing the queue.
    fn other_producer(&self) -> bool {
        let before = unsafe { (*self.view.cast::<QueueHeader>()).read_idx };
        std::thread::sleep(Duration::from_millis(200));
        let after = unsafe { (*self.view.cast::<QueueHeader>()).read_idx };
        after != before
    }

    fn set_interval(&self, interval: u64) {
        unsafe {
            (*self.view.cast::<QueueHeader>()).interval = interval;
        }
    }
}

struct CamState {
    mapped: Option<Mapping>,
    nv12: Vec<u8>,
    fps: u32,
    pixel: PixelFormat,
    last_write: Instant,
}

impl Default for CamState {
    fn default() -> Self {
        Self { mapped: None, nv12: Vec::new(), fps: 30, pixel: PixelFormat::Yuyv, last_write: Instant::now() }
    }
}

pub struct ObsCamera {
    state: Arc<Mutex<CamState>>,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl ObsCamera {
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(CamState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (state_bg, stop_bg) = (state.clone(), stop.clone());
        let pump = std::thread::Builder::new()
            .name("vcam".into())
            .spawn(move || repeat_frames(state_bg, stop_bg))
            .expect("spawn virtual camera thread");
        Self { state, stop, pump: Some(pump) }
    }
}

impl Drop for ObsCamera {
    fn drop(&mut self) {
        self.close();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.pump.take() {
            let _ = thread.join();
        }
    }
}

impl VirtualCamera for ObsCamera {
    fn open(&mut self, format: OutputFormat, fps: u32) -> Result<OutputFormat> {
        if !is_registered() {
            bail!("OBS Virtual Camera is not registered. Install OBS Studio and run `smart-webcam setup`");
        }
        let fps = if fps == 0 { 30 } else { fps };
        let want_w = (format.width & !1) as u32;
        let want_h = (format.height & !1) as u32;
        if want_w < 2 || want_h < 2 {
            bail!("resolution too small");
        }
        let interval = 10_000_000u64 / u64::from(fps);
        let mut guard = lock(&self.state);
        let same = guard.mapped.as_ref().is_some_and(|m| m.cx == want_w && m.cy == want_h);
        if same {
            if let Some(mapped) = guard.mapped.as_ref() {
                mapped.set_interval(interval);
            }
        } else {
            // Dropping the old mapping marks it STOPPING so a reader lets the name go.
            guard.mapped = None;
            guard.nv12.clear();
            guard.mapped = Some(match Mapping::create(want_w, want_h, interval) {
                Ok(mapped) => mapped,
                Err(e) => {
                    let mut existing = Mapping::attach().context("opening the existing OBS virtual camera queue")?;
                    if existing.other_producer() {
                        bail!(
                            "OBS Virtual Camera is already in use by another program, usually OBS's Start Virtual Camera. Stop that and Smart Webcam will use it.\n{e:#}"
                        );
                    }
                    log::warn!("{e:#}; keeping the camera size already open");
                    existing.writer = true;
                    existing
                }
            });
        }
        let (cx, cy) = {
            let mapped = guard.mapped.as_ref().context("no virtual camera")?;
            (mapped.cx, mapped.cy)
        };
        guard.fps = fps;
        guard.pixel = format.pixel;
        drop(guard);
        write_hint(cx, cy, interval);
        if cx != want_w || cy != want_h {
            log::warn!("virtual camera is held at {cx}x{cy}; close the app using it to switch to {want_w}x{want_h}");
        } else {
            log::info!("virtual camera {FRIENDLY_NAME} {cx}x{cy} @ {fps} fps");
        }
        Ok(OutputFormat { width: cx as usize, height: cy as usize, pixel: format.pixel })
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let mut guard = lock(&self.state);
        let (view, offsets, cx, cy, pixel) = {
            let mapped = guard.mapped.as_ref().context("virtual camera is closed")?;
            (mapped.view, mapped.offsets, mapped.cx, mapped.cy, guard.pixel)
        };
        let need = cx as usize * cy as usize * 3 / 2;
        if guard.nv12.len() != need {
            guard.nv12.resize(need, 0);
        }
        to_nv12(frame, pixel, cx, cy, &mut guard.nv12)?;
        publish(view, &offsets, &guard.nv12);
        guard.last_write = Instant::now();
        Ok(())
    }

    fn close(&mut self) {
        let mut guard = lock(&self.state);
        guard.mapped = None;
        guard.nv12.clear();
    }

    fn describe(&self) -> String {
        FRIENDLY_NAME.into()
    }
}

/// The filter drops the queue after 10 unchanged frames, which at 60 fps is
/// only 160 ms. Placeholder frames arrive slower than that, so repeat the
/// last picture often enough to keep the camera alive.
fn repeat_frames(state: Arc<Mutex<CamState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let interval = {
            let mut guard = lock(&state);
            let fps = guard.fps.max(1);
            let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
            let due = guard.last_write.elapsed() >= interval.saturating_mul(3);
            if due && guard.mapped.is_some() && !guard.nv12.is_empty() {
                if let Some(mapped) = guard.mapped.as_ref() {
                    publish(mapped.view, &mapped.offsets, &guard.nv12);
                }
                guard.last_write = Instant::now();
            }
            interval
        };
        std::thread::sleep(interval / 2);
    }
}

fn lock(state: &Mutex<CamState>) -> MutexGuard<'_, CamState> {
    state.lock().unwrap_or_else(|err| err.into_inner())
}

fn publish(view: *mut u8, offsets: &[u32; 3], nv12: &[u8]) {
    unsafe {
        let header = view.cast::<QueueHeader>();
        let inc = {
            let next = (*header).write_idx.wrapping_add(1);
            std::ptr::write_volatile(&raw mut (*header).write_idx, next);
            next
        };
        let slot = view.add(offsets[(inc as usize) % 3] as usize);
        std::ptr::write_volatile(slot.cast::<u64>(), 0);
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), slot.add(FRAME_HEADER), nv12.len());
        fence(Ordering::Release);
        std::ptr::write_volatile(&raw mut (*header).read_idx, inc);
        std::ptr::write_volatile(&raw mut (*header).state, STATE_READY);
    }
}

fn queue_layout(cx: u32, cy: u32) -> (u32, [u32; 3]) {
    let frame = cx * cy * 3 / 2;
    let mut size = align32(std::mem::size_of::<QueueHeader>() as u32);
    let mut offsets = [0u32; 3];
    for slot in &mut offsets {
        *slot = size;
        size = align32(size + frame + FRAME_HEADER as u32);
    }
    (size, offsets)
}

fn align32(size: u32) -> u32 {
    (size + 31) & !31
}

/// `%APPDATA%\obs-virtualcam.txt`, which the filter reads when an app opens the camera.
fn write_hint(cx: u32, cy: u32, interval: u64) {
    let Some(dir) = dirs::config_dir() else { return };
    let path = dir.join("obs-virtualcam.txt");
    if let Err(e) = std::fs::write(&path, format!("{cx}x{cy}x{interval}")) {
        log::warn!("could not write {}: {e}", path.display());
    }
}

pub fn is_registered() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).open_subkey(CLSID).is_ok()
}

/// Registers `obs-virtualcam-module64.dll` (UAC prompt) when OBS is installed but the camera is not.
pub fn register() -> Result<()> {
    if is_registered() {
        return Ok(());
    }
    let dll = find_module().context(
        "OBS Virtual Camera is not installed. Install OBS Studio from https://obsproject.com/, then run `smart-webcam setup`",
    )?;
    log::info!("registering {}", dll.display());
    let quoted = dll.display().to_string().replace('\'', "''");
    let script = format!("Start-Process -FilePath regsvr32.exe -ArgumentList @('/i','/s','{quoted}') -Verb RunAs -Wait");
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .status()
        .context("starting the virtual camera installer")?;
    if !status.success() {
        bail!("virtual camera registration was cancelled or failed ({status})");
    }
    if !is_registered() {
        bail!("regsvr32 finished but OBS Virtual Camera is still not registered");
    }
    Ok(())
}

fn find_module() -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        let root = std::path::PathBuf::from(programs).join("obs-studio");
        candidates.push(root.join(r"core\win-dshow\data\obs-virtualcam-module64.dll"));
        candidates.push(root.join(r"data\obs-plugins\win-dshow\obs-virtualcam-module64.dll"));
    }
    candidates.push(r"C:\Program Files\obs-studio\core\win-dshow\data\obs-virtualcam-module64.dll".into());
    candidates.into_iter().find(|path| path.is_file())
}

fn to_nv12(src: &[u8], pixel: PixelFormat, w: u32, h: u32, dst: &mut [u8]) -> Result<()> {
    let (w, h) = (w as usize, h as usize);
    let nv12 = w * h * 3 / 2;
    if dst.len() != nv12 {
        bail!("internal nv12 buffer is {} bytes, expected {nv12}", dst.len());
    }
    match pixel {
        PixelFormat::Yuyv => {
            if src.len() < w * h * 2 {
                bail!("YUYV frame is too small");
            }
            yuyv_to_nv12(src, w, h, dst);
        }
        PixelFormat::I420 => {
            if src.len() < nv12 {
                bail!("I420 frame is too small");
            }
            i420_to_nv12(src, w, h, dst);
        }
    }
    Ok(())
}

fn yuyv_to_nv12(src: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let y_size = w * h;
    for row in 0..h {
        let s = &src[row * w * 2..];
        for x in 0..w {
            dst[row * w + x] = s[x * 2];
        }
        if row % 2 == 0 {
            let uv_at = y_size + (row / 2) * w;
            for x in (0..w).step_by(2) {
                dst[uv_at + x] = s[x * 2 + 1];
                dst[uv_at + x + 1] = s[x * 2 + 3];
            }
        }
    }
}

fn i420_to_nv12(src: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let y = w * h;
    let chroma = y / 4;
    dst[..y].copy_from_slice(&src[..y]);
    let u = &src[y..y + chroma];
    let v = &src[y + chroma..y + 2 * chroma];
    for i in 0..chroma {
        dst[y + 2 * i] = u[i];
        dst[y + 2 * i + 1] = v[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_matches_obs() {
        assert_eq!(std::mem::size_of::<QueueHeader>(), 80);
        assert_eq!(std::mem::offset_of!(QueueHeader, interval), 40);
        assert_eq!(std::mem::offset_of!(QueueHeader, reserved), 48);
        let (total, offsets) = queue_layout(1920, 1080);
        assert_eq!(offsets[0], 96);
        let frame = 1920 * 1080 * 3 / 2;
        let slot = align32(frame + FRAME_HEADER as u32);
        assert_eq!(offsets[1], 96 + slot);
        assert_eq!(offsets[2], 96 + 2 * slot);
        assert_eq!(total, 96 + 3 * slot);
    }

    #[test]
    fn yuyv_and_i420_become_nv12() {
        // 4x2, two pixels per macropixel: Y,U,Y,V.
        let yuyv = [10, 1, 20, 2, 30, 3, 40, 4, 50, 5, 60, 6, 70, 7, 80, 8];
        let mut nv12 = vec![0u8; 4 * 2 * 3 / 2];
        yuyv_to_nv12(&yuyv, 4, 2, &mut nv12);
        assert_eq!(&nv12[..8], &[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(&nv12[8..], &[1, 2, 3, 4]);

        let i420 = vec![10, 20, 30, 40, 50, 60, 70, 80, 1, 3, 2, 4];
        i420_to_nv12(&i420, 4, 2, &mut nv12);
        assert_eq!(nv12, vec![10, 20, 30, 40, 50, 60, 70, 80, 1, 2, 3, 4]);
    }

    #[test]
    fn publish_writes_a_frame_in_obs_layout() {
        let (size, offsets) = queue_layout(4, 2);
        let handle = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, size, std::ptr::null())
        };
        assert!(!handle.is_null(), "CreateFileMappingW");
        let view = map_view(handle).unwrap();
        let header = QueueHeader {
            write_idx: 0,
            read_idx: 0,
            state: STATE_STARTING,
            offsets,
            type_: 0,
            cx: 4,
            cy: 2,
            interval: 333_333,
            reserved: [0; 8],
        };
        unsafe { view.cast::<QueueHeader>().write(header) };
        let mut nv12 = vec![0u8; 4 * 2 * 3 / 2];
        nv12[0] = 42;
        nv12[8] = 7;
        publish(view, &offsets, &nv12);
        unsafe {
            let header = &*view.cast::<QueueHeader>();
            assert_eq!(header.state, STATE_READY);
            assert_eq!(header.read_idx, 1);
            let slot = view.add(header.offsets[1] as usize);
            assert_eq!(*slot.add(FRAME_HEADER), 42);
            assert_eq!(*slot.add(FRAME_HEADER + 8), 7);
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.cast() });
            CloseHandle(handle);
        }
    }

    #[test]
    fn queue_round_trip() {
        let name = windows_sys::w!("OBSVirtualCamVideo");
        let existing = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name) };
        if !existing.is_null() {
            unsafe { CloseHandle(existing) };
            return; // another producer (the app, or OBS) owns the camera
        }
        let mapped = Mapping::create(4, 2, 333_333).unwrap();
        let mut nv12 = vec![0u8; 4 * 2 * 3 / 2];
        nv12[0] = 42;
        nv12[8] = 7;
        publish(mapped.view, &mapped.offsets, &nv12);
        unsafe {
            let header = &*mapped.view.cast::<QueueHeader>();
            assert_eq!(header.state, STATE_READY);
            assert_eq!(header.cx, 4);
            assert_eq!(header.read_idx, 1);
            let slot = mapped.view.add(header.offsets[1] as usize);
            assert_eq!(*slot.add(FRAME_HEADER), 42);
            assert_eq!(*slot.add(FRAME_HEADER + 8), 7);
        }
    }
}
