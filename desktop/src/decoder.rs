//! H.264 → I420 with the bundled OpenH264 (no system ffmpeg needed, same code on Windows).

use crate::convert::I420;
use anyhow::{anyhow, Result};
use openh264::formats::YUVSource;

pub struct H264Decoder {
    inner: openh264::decoder::Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        Ok(H264Decoder { inner: openh264::decoder::Decoder::new().map_err(|e| anyhow!("openh264 init: {e}"))? })
    }

    /// Decodes one access unit (Annex-B). Calls `f` with the picture if one came out.
    pub fn decode<R>(&mut self, data: &[u8], f: impl FnOnce(&I420) -> R) -> Result<Option<R>> {
        let Some(yuv) = self.inner.decode(data).map_err(|e| anyhow!("decode: {e}"))? else { return Ok(None) };
        let (width, height) = yuv.dimensions();
        let (y_stride, uv_stride, _) = yuv.strides();
        let pic = I420 { width, height, y: yuv.y(), u: yuv.u(), v: yuv.v(), y_stride, uv_stride };
        Ok(Some(f(&pic)))
    }
}
