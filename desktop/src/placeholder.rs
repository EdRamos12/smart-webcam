//! The "Waiting for phone…" frame shown while no video is coming in.

use crate::convert::{gray_to_output, OutputFormat};
use font8x8::{UnicodeFonts, BASIC_FONTS};

const BG: u8 = 40;
const FG: u8 = 235;
const DIM: u8 = 150;

/// Renders a dark frame with a camera glyph, a title and a status line.
pub fn render(out: OutputFormat, title: &str, status: &str) -> Vec<u8> {
    let (w, h) = (out.width, out.height);
    let mut gray = vec![BG; w * h];

    let scale_title = (h / 180).max(1);
    let scale_status = (h / 360).max(1);
    let glyph = h / 6;

    let title_h = 8 * scale_title;
    let status_h = 8 * scale_status;
    let gap = h / 30;
    let total = glyph + gap + title_h + gap + status_h;
    let mut y = h.saturating_sub(total) / 2;

    draw_camera(&mut gray, w, h, w / 2, y + glyph / 2, glyph);
    y += glyph + gap;
    draw_text_centered(&mut gray, w, h, y, title, scale_title, FG);
    y += title_h + gap;
    draw_text_centered(&mut gray, w, h, y, status, scale_status, DIM);

    let mut buf = vec![0u8; out.frame_size()];
    gray_to_output(&gray, out, &mut buf);
    buf
}

fn draw_text_centered(gray: &mut [u8], w: usize, h: usize, y: usize, text: &str, scale: usize, color: u8) {
    let chars: Vec<char> = text.chars().map(|c| if c == '…' { '.' } else { c }).collect();
    let text_w = chars.len() * 8 * scale;
    let x0 = w.saturating_sub(text_w) / 2;
    for (i, ch) in chars.iter().enumerate() {
        let Some(rows) = BASIC_FONTS.get(*ch).or_else(|| BASIC_FONTS.get('?')) else { continue };
        for (ry, bits) in rows.iter().enumerate() {
            for rx in 0..8 {
                if bits & (1 << rx) == 0 {
                    continue;
                }
                let px = x0 + (i * 8 + rx) * scale;
                let py = y + ry * scale;
                fill_rect(gray, w, h, px, py, scale, scale, color);
            }
        }
    }
}

/// A simple video-camera glyph: rounded body plus a triangular lens housing.
fn draw_camera(gray: &mut [u8], w: usize, h: usize, cx: usize, cy: usize, size: usize) {
    let body_w = size * 9 / 10;
    let body_h = size * 6 / 10;
    let x0 = cx.saturating_sub(size * 6 / 10);
    let y0 = cy.saturating_sub(body_h / 2);
    fill_rect(gray, w, h, x0, y0, body_w, body_h, DIM);
    // Lens triangle to the right of the body.
    let tx = x0 + body_w + size / 20;
    let tw = size * 3 / 10;
    for i in 0..tw {
        let half = body_h / 2 * (i + 1) / tw;
        fill_rect(gray, w, h, tx + i, cy.saturating_sub(half / 2 + body_h / 6), 1, half + body_h / 3, DIM);
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(gray: &mut [u8], w: usize, h: usize, x: usize, y: usize, rw: usize, rh: usize, color: u8) {
    for yy in y.min(h)..(y + rh).min(h) {
        let row = &mut gray[yy * w..(yy + 1) * w];
        row[x.min(w)..(x + rw).min(w)].fill(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PixelFormat;

    #[test]
    fn renders_text_in_both_formats() {
        for pixel in [PixelFormat::Yuyv, PixelFormat::I420] {
            let out = OutputFormat { width: 1280, height: 720, pixel };
            let frame = render(out, "Smart Webcam", "Waiting for phone…");
            assert_eq!(frame.len(), out.frame_size());
            let luma: Vec<u8> = match pixel {
                PixelFormat::Yuyv => frame.iter().step_by(2).copied().collect(),
                PixelFormat::I420 => frame[..1280 * 720].to_vec(),
            };
            assert!(luma.iter().any(|&p| p == FG), "title drawn");
            assert!(luma.iter().any(|&p| p == DIM), "status drawn");
            assert_eq!(luma[0], BG);
        }
    }
}
