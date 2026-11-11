//! v4l2loopback output: set the format with VIDIOC_S_FMT on the OUTPUT queue, then `write()` frames.

use super::VirtualCamera;
use crate::config::PixelFormat;
use crate::convert::OutputFormat;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const CARD_LABEL: &str = "Smart Webcam";

const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SMPTE170M: u32 = 1;

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
const PIX_YUYV: u32 = fourcc(b"YUYV");
const PIX_YU12: u32 = fourcc(b"YU12");

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: the union is 8-byte aligned on 64-bit, hence the padding.
#[repr(C)]
struct Format {
    type_: u32,
    _pad: u32,
    pix: PixFormat,
    _rest: [u8; 200 - std::mem::size_of::<PixFormat>()],
}

/// `struct v4l2_streamparm` with the `output` member of the union.
#[repr(C)]
struct StreamParm {
    type_: u32,
    capability: u32,
    outputmode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    writebuffers: u32,
    _rest: [u8; 200 - 6 * 4],
}

const fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const VIDIOC_G_FMT: u64 = iowr(4, std::mem::size_of::<Format>());
const VIDIOC_S_FMT: u64 = iowr(5, std::mem::size_of::<Format>());
const VIDIOC_S_PARM: u64 = iowr(22, std::mem::size_of::<StreamParm>());

const _: () = assert!(std::mem::size_of::<Format>() == 208);
const _: () = assert!(std::mem::size_of::<StreamParm>() == 204);
const _: () = assert!(VIDIOC_S_FMT == 0xC0D0_5605);

pub struct V4l2Loopback {
    path: PathBuf,
    file: Option<File>,
    format: Option<OutputFormat>,
}

impl V4l2Loopback {
    pub fn new(path: PathBuf) -> Self {
        V4l2Loopback { path, file: None, format: None }
    }
}

fn pix_for(f: OutputFormat) -> PixFormat {
    let (pixelformat, bytesperline) = match f.pixel {
        PixelFormat::Yuyv => (PIX_YUYV, f.width * 2),
        PixelFormat::I420 => (PIX_YU12, f.width),
    };
    PixFormat {
        width: f.width as u32,
        height: f.height as u32,
        pixelformat,
        field: V4L2_FIELD_NONE,
        bytesperline: bytesperline as u32,
        sizeimage: f.frame_size() as u32,
        colorspace: V4L2_COLORSPACE_SMPTE170M,
        ..Default::default()
    }
}

fn ioctl<T>(file: &File, req: u64, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: `arg` is a properly sized #[repr(C)] struct matching the request code.
    let r = unsafe { libc::ioctl(file.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn new_format(pix: PixFormat) -> Format {
    Format { type_: V4L2_BUF_TYPE_VIDEO_OUTPUT, _pad: 0, pix, _rest: [0; 200 - std::mem::size_of::<PixFormat>()] }
}

impl VirtualCamera for V4l2Loopback {
    fn open(&mut self, want: OutputFormat, fps: u32) -> Result<OutputFormat> {
        if self.file.is_some() && self.format == Some(want) {
            return Ok(want);
        }
        self.close();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;

        let mut fmt = new_format(pix_for(want));
        let actual = match ioctl(&file, VIDIOC_S_FMT, &mut fmt) {
            Ok(()) => OutputFormat { width: fmt.pix.width as usize, height: fmt.pix.height as usize, pixel: want.pixel },
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                // A consumer has the device open with another format: keep theirs and scale into it.
                let mut cur = new_format(PixFormat::default());
                ioctl(&file, VIDIOC_G_FMT, &mut cur).context("VIDIOC_G_FMT")?;
                let pixel = match cur.pix.pixelformat {
                    PIX_YUYV => PixelFormat::Yuyv,
                    PIX_YU12 => PixelFormat::I420,
                    other => bail!("device busy with unsupported pixel format {other:#x}"),
                };
                log::warn!("{} is busy at {}x{}; scaling into that", self.path.display(), cur.pix.width, cur.pix.height);
                OutputFormat { width: cur.pix.width as usize, height: cur.pix.height as usize, pixel }
            }
            Err(e) => return Err(e).context("VIDIOC_S_FMT"),
        };

        // Advertise the frame rate to consumers (best effort).
        let mut parm = StreamParm {
            type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
            capability: 0,
            outputmode: 0,
            numerator: 1,
            denominator: fps.max(1),
            extendedmode: 0,
            writebuffers: 0,
            _rest: [0; 200 - 6 * 4],
        };
        if let Err(e) = ioctl(&file, VIDIOC_S_PARM, &mut parm) {
            log::debug!("VIDIOC_S_PARM: {e}");
        }

        log::info!("{}: {}x{} {:?}", self.path.display(), actual.width, actual.height, actual.pixel);
        self.file = Some(file);
        self.format = Some(actual);
        Ok(actual)
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let file = self.file.as_mut().context("device not open")?;
        file.write_all(frame).context("writing frame")?;
        Ok(())
    }

    fn close(&mut self) {
        self.file = None;
        self.format = None;
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// `/dev/videoN` whose sysfs name is [`CARD_LABEL`].
pub fn find_device() -> Option<PathBuf> {
    devices().into_iter().find(|(_, name)| name == CARD_LABEL).map(|(p, _)| p)
}

/// All video4linux devices as (path, card name), sorted by number.
pub fn devices() -> Vec<(PathBuf, String)> {
    let mut out: Vec<(u32, PathBuf, String)> = std::fs::read_dir("/sys/class/video4linux")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let fname = e.file_name().to_string_lossy().to_string();
            let n: u32 = fname.strip_prefix("video")?.parse().ok()?;
            let name = std::fs::read_to_string(e.path().join("name")).ok()?.trim().to_string();
            Some((n, Path::new("/dev").join(&fname), name))
        })
        .collect();
    out.sort_by_key(|(n, _, _)| *n);
    out.into_iter().map(|(_, p, n)| (p, n)).collect()
}
