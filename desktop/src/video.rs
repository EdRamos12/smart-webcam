//! The video thread: decodes packets, converts them and writes to the virtual camera.
//! When no video is flowing it keeps the device alive with a placeholder frame.

use crate::config::PixelFormat;
use crate::convert::{Converter, OutputFormat, Transform};
use crate::decoder::H264Decoder;
use crate::placeholder;
use crate::protocol::{Started, VideoPacket};
use crate::vcam::VirtualCamera;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Packets buffered between the network and the decoder before we start dropping.
const QUEUE: usize = 20;
const PLACEHOLDER_INTERVAL: Duration = Duration::from_millis(200);
/// Show the placeholder if no picture arrived for this long.
const STALL: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub pixel: PixelFormat,
    pub mirror: bool,
    /// Extra clockwise rotation chosen by the user.
    pub rotation: u32,
}

pub enum VideoMsg {
    /// A packet and when it arrived from the network.
    Packet(VideoPacket, Instant),
    Started(Started),
    Stopped,
    Status(String),
    Settings(VideoSettings),
    Shutdown,
}

#[derive(Debug)]
pub enum VideoEvent {
    /// The decoder lost sync; ask the phone for an IDR.
    NeedKeyframe,
    DeviceReady(String),
    DeviceError(String),
}

/// Running totals; the app turns them into per-second rates.
#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
    /// Sum/count of network-arrival → device-write times.
    pub latency_us: AtomicU64,
    pub latency_n: AtomicU64,
}

/// A downscaled RGB copy of what the virtual camera shows.
pub struct Preview {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    /// When the frame was written to the virtual camera (µs since the Unix epoch).
    pub written_us: u64,
}

pub fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// Where previews go. They are only produced while someone is watching.
pub struct PreviewTap {
    pub viewers: Arc<AtomicUsize>,
    pub tx: tokio::sync::watch::Sender<Option<Arc<Preview>>>,
}

/// Full frame rate up to 60 fps; a throttled preview looks laggy even when the camera isn't.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(15);
const PREVIEW_WIDTH: usize = 640;

/// Sending side used by network sessions. Drops packets (until the next keyframe)
/// when the decoder falls behind instead of building up latency.
#[derive(Clone)]
pub struct VideoSink {
    tx: SyncSender<VideoMsg>,
    pub counters: Arc<Counters>,
}

impl VideoSink {
    /// A packet skipped by the network side while waiting for a keyframe.
    pub fn count_dropped(&self) {
        self.counters.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn send(&self, msg: VideoMsg) {
        let _ = self.tx.send(msg);
    }

    /// Returns false if the packet was dropped because the queue is full.
    pub fn push(&self, p: VideoPacket) -> bool {
        self.counters.bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
        match self.tx.try_send(VideoMsg::Packet(p, Instant::now())) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.count_dropped();
                false
            }
            Err(TrySendError::Disconnected(_)) => true,
        }
    }
}

pub fn spawn(
    cam: Box<dyn VirtualCamera>,
    settings: VideoSettings,
    preview: PreviewTap,
    events: impl Fn(VideoEvent) + Send + 'static,
) -> (VideoSink, std::thread::JoinHandle<()>) {
    let (tx, rx) = sync_channel(QUEUE);
    let counters = Arc::new(Counters::default());
    let c = counters.clone();
    let handle = std::thread::Builder::new()
        .name("video".into())
        .spawn(move || Pipeline::new(cam, settings, preview, c, Box::new(events)).run(rx))
        .expect("spawn video thread");
    (VideoSink { tx, counters }, handle)
}

struct Pipeline {
    cam: Box<dyn VirtualCamera>,
    settings: VideoSettings,
    events: Box<dyn Fn(VideoEvent) + Send>,
    preview: PreviewTap,
    last_preview: Option<Instant>,
    counters: Arc<Counters>,
    out: Option<OutputFormat>,
    decoder: Option<H264Decoder>,
    converter: Converter,
    frame: Vec<u8>,
    auto_rotation: u32,
    waiting_key: bool,
    streaming: bool,
    status: String,
    placeholder: Option<Vec<u8>>,
    last_picture: Option<Instant>,
    last_placeholder: Option<Instant>,
    last_open_attempt: Option<Instant>,
}

impl Pipeline {
    fn new(
        cam: Box<dyn VirtualCamera>,
        settings: VideoSettings,
        preview: PreviewTap,
        counters: Arc<Counters>,
        events: Box<dyn Fn(VideoEvent) + Send>,
    ) -> Self {
        Pipeline {
            cam,
            settings,
            events,
            preview,
            last_preview: None,
            counters,
            out: None,
            decoder: None,
            converter: Converter::new(),
            frame: Vec::new(),
            auto_rotation: 0,
            waiting_key: true,
            streaming: false,
            status: "Waiting for phone…".into(),
            placeholder: None,
            last_picture: None,
            last_open_attempt: None,
            last_placeholder: None,
        }
    }

    fn run(mut self, rx: Receiver<VideoMsg>) {
        self.ensure_open();
        loop {
            match rx.recv_timeout(PLACEHOLDER_INTERVAL) {
                Ok(VideoMsg::Packet(p, at)) => self.on_packet(p, at),
                Ok(VideoMsg::Started(s)) => {
                    // Sensor orientation 90 (typical back camera) is upright with the phone
                    // in landscape; correct the others (e.g. front at 270) to match.
                    self.auto_rotation = (s.orientation + 270) % 360;
                    self.decoder = None;
                    self.waiting_key = true;
                    self.streaming = true;
                }
                Ok(VideoMsg::Stopped) => {
                    self.streaming = false;
                    self.decoder = None;
                    self.last_picture = None;
                }
                Ok(VideoMsg::Status(s)) => {
                    if s != self.status {
                        self.status = s;
                        self.placeholder = None;
                    }
                }
                Ok(VideoMsg::Settings(s)) => {
                    if s != self.settings {
                        self.settings = s;
                        self.placeholder = None;
                        self.ensure_open();
                    }
                }
                Ok(VideoMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.maybe_placeholder();
        }
        self.cam.close();
    }

    fn wanted(&self) -> OutputFormat {
        OutputFormat { width: self.settings.width & !1, height: self.settings.height & !1, pixel: self.settings.pixel }
    }

    fn ensure_open(&mut self) {
        let want = self.wanted();
        if self.out.is_some_and(|o| o.width == want.width && o.height == want.height && o.pixel == want.pixel) {
            return;
        }
        if self.out.is_none() && self.last_open_attempt.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.last_open_attempt = Some(Instant::now());
        match self.cam.open(want, self.settings.fps) {
            Ok(actual) => {
                if self.out != Some(actual) {
                    (self.events)(VideoEvent::DeviceReady(format!("{} {}x{}", self.cam.describe(), actual.width, actual.height)));
                }
                self.out = Some(actual);
                self.frame = vec![0; actual.frame_size()];
                self.placeholder = None;
            }
            Err(e) => {
                if self.out.is_none() {
                    (self.events)(VideoEvent::DeviceError(format!("{e:#}")));
                }
                // Keep the old format if we had one (e.g. busy device), otherwise retry later.
                if self.out.is_some() {
                    log::warn!("reopen failed, keeping current format: {e:#}");
                } else {
                    log::warn!("cannot open virtual camera: {e:#}");
                }
            }
        }
    }

    fn write(&mut self, data_is_frame: bool) {
        let res = if data_is_frame {
            self.cam.write_frame(&self.frame)
        } else if let Some(p) = &self.placeholder {
            self.cam.write_frame(p)
        } else {
            Ok(())
        };
        if let Err(e) = res {
            log::warn!("write failed: {e:#}");
            self.cam.close();
            self.out = None;
            self.ensure_open();
            return;
        }
        if data_is_frame {
            self.counters.frames.fetch_add(1, Ordering::Relaxed);
        }
        self.maybe_preview(data_is_frame);
    }

    fn maybe_preview(&mut self, data_is_frame: bool) {
        if self.preview.viewers.load(Ordering::Relaxed) == 0 {
            return;
        }
        if self.last_preview.is_some_and(|t| t.elapsed() < PREVIEW_INTERVAL) {
            return;
        }
        let (Some(out), Some(data)) = (self.out, if data_is_frame { Some(&self.frame) } else { self.placeholder.as_ref() }) else {
            return;
        };
        self.last_preview = Some(Instant::now());
        let (width, height, rgb) = crate::convert::preview_rgb(data, out, PREVIEW_WIDTH);
        self.preview.tx.send_replace(Some(Arc::new(Preview { width, height, rgb, written_us: now_us() })));
    }

    fn on_packet(&mut self, p: VideoPacket, arrived: Instant) {
        let Some(out) = self.out else {
            self.ensure_open();
            return;
        };
        if self.waiting_key && !p.is_keyframe() && !p.is_config() {
            return;
        }
        if self.decoder.is_none() {
            match H264Decoder::new() {
                Ok(d) => self.decoder = Some(d),
                Err(e) => {
                    log::error!("{e:#}");
                    return;
                }
            }
        }
        if p.is_keyframe() {
            self.waiting_key = false;
        }
        let t = Transform {
            rotation: (self.auto_rotation + self.settings.rotation) % 360,
            mirror: self.settings.mirror,
        };
        let decoder = self.decoder.as_mut().unwrap();
        let (converter, frame) = (&mut self.converter, &mut self.frame);
        match decoder.decode(&p.data, |pic| converter.convert(pic, out, t, frame)) {
            Ok(Some(())) => {
                self.last_picture = Some(Instant::now());
                self.write(true);
                let us = arrived.elapsed().as_micros() as u64;
                self.counters.latency_us.fetch_add(us, Ordering::Relaxed);
                self.counters.latency_n.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(e) => {
                log::debug!("{e:#}; waiting for keyframe");
                self.waiting_key = true;
                self.decoder = None;
                (self.events)(VideoEvent::NeedKeyframe);
            }
        }
    }

    fn maybe_placeholder(&mut self) {
        if self.out.is_none() {
            self.ensure_open();
            return;
        }
        if self.last_picture.is_some_and(|t| t.elapsed() < STALL) {
            return;
        }
        if self.last_placeholder.is_some_and(|t| t.elapsed() < PLACEHOLDER_INTERVAL) {
            return;
        }
        if self.placeholder.is_none() {
            let out = self.out.unwrap();
            self.placeholder = Some(placeholder::render(out, "Smart Webcam", &self.status));
        }
        self.last_placeholder = Some(Instant::now());
        self.write(false);
    }
}
