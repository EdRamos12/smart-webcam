//! I420 → output frame (YUYV or I420) with letterboxed nearest-neighbour scaling,
//! rotation and mirroring, using per-row/per-column lookup tables.

use crate::config::PixelFormat;

/// A borrowed I420 picture.
pub struct I420<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    pub width: usize,
    pub height: usize,
    pub pixel: PixelFormat,
}

impl OutputFormat {
    pub fn frame_size(&self) -> usize {
        match self.pixel {
            PixelFormat::Yuyv => self.width * self.height * 2,
            PixelFormat::I420 => self.width * self.height + 2 * (self.width / 2) * (self.height / 2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    /// Clockwise rotation in degrees (0, 90, 180, 270).
    pub rotation: u32,
    /// Horizontal flip, applied after rotation.
    pub mirror: bool,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct Key {
    src_w: usize,
    src_h: usize,
    y_stride: usize,
    uv_stride: usize,
    out: OutputFormat,
    t: Transform,
}

/// Index of a source sample for output pixel (x, y) is `row[y] + col[x]`, or
/// `NONE` in either table for letterbox bars.
const NONE: usize = usize::MAX;

pub struct Converter {
    key: Option<Key>,
    y_row: Vec<usize>,
    y_col: Vec<usize>,
    c_row: Vec<usize>,
    c_col: Vec<usize>,
}

impl Default for Converter {
    fn default() -> Self {
        Self::new()
    }
}

impl Converter {
    pub fn new() -> Self {
        Converter { key: None, y_row: vec![], y_col: vec![], c_row: vec![], c_col: vec![] }
    }

    /// Converts `src` into `dst` (which must be `out.frame_size()` bytes).
    pub fn convert(&mut self, src: &I420, out: OutputFormat, t: Transform, dst: &mut [u8]) {
        assert_eq!(dst.len(), out.frame_size());
        let key = Key { src_w: src.width, src_h: src.height, y_stride: src.y_stride, uv_stride: src.uv_stride, out, t };
        if self.key != Some(key) {
            self.build(key);
        }
        match out.pixel {
            PixelFormat::Yuyv => self.to_yuyv(src, out, dst),
            PixelFormat::I420 => self.to_i420(src, out, dst),
        }
    }

    fn build(&mut self, k: Key) {
        let rot = k.t.rotation % 360;
        let swapped = rot == 90 || rot == 270;
        // Size of the rotated source image.
        let (rw, rh) = if swapped { (k.src_h, k.src_w) } else { (k.src_w, k.src_h) };
        // Letterbox: fit the rotated image into the output keeping aspect ratio.
        let (ow, oh) = (k.out.width, k.out.height);
        let (cw, ch) = if rw * oh > rh * ow { (ow, (ow * rh / rw).max(1)) } else { ((oh * rw / rh).max(1), oh) };
        let (ox, oy) = ((ow - cw) / 2, (oh - ch) / 2);

        // Rotated-space coordinate for each output column / row.
        let rx_of = |x: usize| -> Option<usize> {
            if x < ox || x >= ox + cw {
                return None;
            }
            let rx = (x - ox) * rw / cw;
            Some(if k.t.mirror { rw - 1 - rx } else { rx })
        };
        let ry_of = |y: usize| -> Option<usize> {
            if y < oy || y >= oy + ch {
                return None;
            }
            Some((y - oy) * rh / ch)
        };

        // Source (sx, sy) from rotated (rx, ry):
        //   0: (rx, ry)   90: (ry, h-1-rx)   180: (w-1-rx, h-1-ry)   270: (w-1-ry, rx)
        // For 0/180 sx depends on columns and sy on rows; for 90/270 it's the other way round.
        let (sw, sh) = (k.src_w, k.src_h);
        let sx_from = |r: usize| -> usize {
            match rot {
                0 | 90 => r,         // r = rx for 0, ry for 90
                _ => sw - 1 - r,     // 180: rx, 270: ry
            }
            .min(sw - 1)
        };
        let sy_from = |r: usize| -> usize {
            match rot {
                0 | 270 => r,        // r = ry for 0, rx for 270
                _ => sh - 1 - r,     // 180: ry, 90: rx
            }
            .min(sh - 1)
        };
        // Luma/chroma offsets contributed by an sx or an sy.
        let from_sx = |sx: usize| (sx, sx / 2);
        let from_sy = |sy: usize| (sy * k.y_stride, (sy / 2) * k.uv_stride);

        let (mut y_col, mut c_col) = (Vec::with_capacity(ow), Vec::with_capacity(ow));
        for x in 0..ow {
            let (yl, cl) = match rx_of(x) {
                None => (NONE, NONE),
                Some(rx) if swapped => from_sy(sy_from(rx)),
                Some(rx) => from_sx(sx_from(rx)),
            };
            y_col.push(yl);
            c_col.push(cl);
        }
        let (mut y_row, mut c_row) = (Vec::with_capacity(oh), Vec::with_capacity(oh));
        for y in 0..oh {
            let (yl, cl) = match ry_of(y) {
                None => (NONE, NONE),
                Some(ry) if swapped => from_sx(sx_from(ry)),
                Some(ry) => from_sy(sy_from(ry)),
            };
            y_row.push(yl);
            c_row.push(cl);
        }
        self.y_row = y_row;
        self.y_col = y_col;
        self.c_row = c_row;
        self.c_col = c_col;
        self.key = Some(k);
    }

    fn to_yuyv(&self, src: &I420, out: OutputFormat, dst: &mut [u8]) {
        let w = out.width;
        for (y, line) in dst.chunks_exact_mut(w * 2).enumerate() {
            let (yr, cr) = (self.y_row[y], self.c_row[y]);
            if yr == NONE {
                for px in line.chunks_exact_mut(4) {
                    px.copy_from_slice(&[16, 128, 16, 128]);
                }
                continue;
            }
            for (i, px) in line.chunks_exact_mut(4).enumerate() {
                let x = i * 2;
                let (c0, c1) = (self.y_col[x], self.y_col[x + 1]);
                if c0 == NONE || c1 == NONE {
                    let y0 = if c0 == NONE { 16 } else { src.y[yr + c0] };
                    let y1 = if c1 == NONE { 16 } else { src.y[yr + c1] };
                    px.copy_from_slice(&[y0, 128, y1, 128]);
                    continue;
                }
                let cc = cr + self.c_col[x];
                px[0] = src.y[yr + c0];
                px[1] = src.u[cc];
                px[2] = src.y[yr + c1];
                px[3] = src.v[cc];
            }
        }
    }

    fn to_i420(&self, src: &I420, out: OutputFormat, dst: &mut [u8]) {
        let (w, h) = (out.width, out.height);
        let (luma, chroma) = dst.split_at_mut(w * h);
        for (y, line) in luma.chunks_exact_mut(w).enumerate() {
            let yr = self.y_row[y];
            for (x, p) in line.iter_mut().enumerate() {
                let c = self.y_col[x];
                *p = if yr == NONE || c == NONE { 16 } else { src.y[yr + c] };
            }
        }
        let (cw, chh) = (w / 2, h / 2);
        let (u_out, v_out) = chroma.split_at_mut(cw * chh);
        for cy in 0..chh {
            let cr = self.c_row[cy * 2];
            for cx in 0..cw {
                let c = self.c_col[cx * 2];
                let i = cy * cw + cx;
                if cr == NONE || c == NONE {
                    u_out[i] = 128;
                    v_out[i] = 128;
                } else {
                    u_out[i] = src.u[cr + c];
                    v_out[i] = src.v[cr + c];
                }
            }
        }
    }
}

/// Packs a luma-only image (neutral chroma) into the output format.
pub fn gray_to_output(gray: &[u8], out: OutputFormat, dst: &mut [u8]) {
    match out.pixel {
        PixelFormat::Yuyv => {
            for (px, g) in dst.chunks_exact_mut(4).zip(gray.chunks_exact(2)) {
                px.copy_from_slice(&[g[0], 128, g[1], 128]);
            }
        }
        PixelFormat::I420 => {
            let luma = out.width * out.height;
            dst[..luma].copy_from_slice(&gray[..luma]);
            dst[luma..].fill(128);
        }
    }
}

/// Downscaled RGB copy of an output frame, for the preview window.
pub fn preview_rgb(frame: &[u8], out: OutputFormat, max_width: usize) -> (usize, usize, Vec<u8>) {
    let (w, h) = (out.width, out.height);
    let pw = w.min(max_width).max(1);
    let ph = (h * pw / w).max(1);
    let mut rgb = Vec::with_capacity(pw * ph * 3);
    for py in 0..ph {
        let y = py * h / ph;
        for px in 0..pw {
            let x = px * w / pw;
            let (yy, u, v) = match out.pixel {
                PixelFormat::Yuyv => {
                    let pair = (y * w + (x & !1)) * 2;
                    (frame[(y * w + x) * 2], frame[pair + 1], frame[pair + 3])
                }
                PixelFormat::I420 => {
                    let luma = w * h;
                    let ci = (y / 2) * (w / 2) + x / 2;
                    (frame[y * w + x], frame[luma + ci], frame[luma + luma / 4 + ci])
                }
            };
            rgb.extend_from_slice(&yuv_to_rgb(yy, u, v));
        }
    }
    (pw, ph, rgb)
}

/// BT.601 limited range.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (y as i32 - 16);
    let (d, e) = (u as i32 - 128, v as i32 - 128);
    let clamp = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [clamp(c + 409 * e), clamp(c - 100 * d - 208 * e), clamp(c + 516 * d)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x2 source where each luma sample encodes its coordinates: 10*y + x.
    fn sample() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let y: Vec<u8> = (0..2).flat_map(|r| (0..4).map(move |c| (10 * r + c) as u8)).collect();
        (y, vec![200, 201], vec![100, 101])
    }

    fn src<'a>(y: &'a [u8], u: &'a [u8], v: &'a [u8]) -> I420<'a> {
        I420 { width: 4, height: 2, y, u, v, y_stride: 4, uv_stride: 2 }
    }

    fn luma_i420(out: &[u8], w: usize, h: usize) -> Vec<Vec<u8>> {
        out[..w * h].chunks(w).map(|r| r.to_vec()).collect()
    }

    #[test]
    fn identity_yuyv() {
        let (y, u, v) = sample();
        let out = OutputFormat { width: 4, height: 2, pixel: PixelFormat::Yuyv };
        let mut dst = vec![0; out.frame_size()];
        Converter::new().convert(&src(&y, &u, &v), out, Transform { rotation: 0, mirror: false }, &mut dst);
        assert_eq!(dst, vec![0, 200, 1, 100, 2, 201, 3, 101, 10, 200, 11, 100, 12, 201, 13, 101]);
    }

    #[test]
    fn mirror_and_180() {
        let (y, u, v) = sample();
        let out = OutputFormat { width: 4, height: 2, pixel: PixelFormat::I420 };
        let mut dst = vec![0; out.frame_size()];
        let mut c = Converter::new();
        c.convert(&src(&y, &u, &v), out, Transform { rotation: 0, mirror: true }, &mut dst);
        assert_eq!(luma_i420(&dst, 4, 2), vec![vec![3, 2, 1, 0], vec![13, 12, 11, 10]]);
        c.convert(&src(&y, &u, &v), out, Transform { rotation: 180, mirror: false }, &mut dst);
        assert_eq!(luma_i420(&dst, 4, 2), vec![vec![13, 12, 11, 10], vec![3, 2, 1, 0]]);
    }

    #[test]
    fn rotate_90_and_270() {
        let (y, u, v) = sample();
        // Rotated image is 2 wide, 4 tall.
        let out = OutputFormat { width: 2, height: 4, pixel: PixelFormat::I420 };
        let mut dst = vec![0; out.frame_size()];
        let mut c = Converter::new();
        c.convert(&src(&y, &u, &v), out, Transform { rotation: 90, mirror: false }, &mut dst);
        // Clockwise: the left column (bottom→top) becomes the top row.
        assert_eq!(luma_i420(&dst, 2, 4), vec![vec![10, 0], vec![11, 1], vec![12, 2], vec![13, 3]]);
        c.convert(&src(&y, &u, &v), out, Transform { rotation: 270, mirror: false }, &mut dst);
        assert_eq!(luma_i420(&dst, 2, 4), vec![vec![3, 13], vec![2, 12], vec![1, 11], vec![0, 10]]);
    }

    #[test]
    fn letterbox_and_scale() {
        let (y, u, v) = sample();
        // 4x2 into 8x8: content is 8x4 centred with 2 black rows above and below.
        let out = OutputFormat { width: 8, height: 8, pixel: PixelFormat::I420 };
        let mut dst = vec![0; out.frame_size()];
        Converter::new().convert(&src(&y, &u, &v), out, Transform { rotation: 0, mirror: false }, &mut dst);
        let l = luma_i420(&dst, 8, 8);
        assert_eq!(l[0], vec![16; 8]);
        assert_eq!(l[2], vec![0, 0, 1, 1, 2, 2, 3, 3]);
        assert_eq!(l[5], vec![10, 10, 11, 11, 12, 12, 13, 13]);
        assert_eq!(l[7], vec![16; 8]);
    }

    #[test]
    fn preview_downscales_and_converts() {
        // 4x2 YUYV: white left half, black right half.
        let frame = [235, 128, 235, 128, 16, 128, 16, 128, 235, 128, 235, 128, 16, 128, 16, 128];
        let out = OutputFormat { width: 4, height: 2, pixel: PixelFormat::Yuyv };
        let (w, h, rgb) = preview_rgb(&frame, out, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb, vec![255, 255, 255, 0, 0, 0]);
    }

    #[test]
    fn gray_frame() {
        let out = OutputFormat { width: 4, height: 2, pixel: PixelFormat::Yuyv };
        let mut dst = vec![0; out.frame_size()];
        gray_to_output(&[1, 2, 3, 4, 5, 6, 7, 8], out, &mut dst);
        assert_eq!(&dst[..8], &[1, 128, 2, 128, 3, 128, 4, 128]);
    }
}
