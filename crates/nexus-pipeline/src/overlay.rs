//! Shared bounding-box + label overlay rendering onto packed RGB24
//! buffers, used by BOTH the alert snapshot (`supervisor::write_alert_snapshot`)
//! and the burned-in alert clip (`alert_clip::encode_alert_clip`), so the
//! box and the `"person 0.96"` label chip read identically in the console
//! snapshot, the sink email / SureView copies, and the clip.
//!
//! Text is rendered with the public-domain `font8x8` 8×8 bitmap font
//! (no system font / no heavy dep), integer-scaled per caller so the
//! label stays legible at both supervisor (~720p analysis) and native
//! (up to 1080p) clip resolutions.

use font8x8::{UnicodeFonts, BASIC_FONTS};

/// The single alert overlay colour — cyan `#22d3ee`, matching the
/// console's alert-detail styling. Used for the box stroke AND the
/// label-chip background.
pub const ALERT_RGB: [u8; 3] = [0x22, 0xd3, 0xee];

/// Dark text drawn on the cyan chip (mirrors the mock `.lbl`
/// `color:#04161c`), for legible contrast against the cyan background.
const CHIP_TEXT_RGB: [u8; 3] = [0x04, 0x16, 0x1c];

/// One glyph cell is 8×8 before scaling.
const GLYPH: i64 = 8;

/// Set one pixel (bounds-checked) in a packed RGB24 buffer.
#[inline]
fn put_px(buf: &mut [u8], w: i64, h: i64, x: i64, y: i64, rgb: [u8; 3]) {
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let idx = ((y * w + x) * 3) as usize;
    if idx + 2 < buf.len() {
        buf[idx] = rgb[0];
        buf[idx + 1] = rgb[1];
        buf[idx + 2] = rgb[2];
    }
}

/// Fill an axis-aligned rectangle `[x, x+rw) × [y, y+rh)` with `rgb`.
#[allow(clippy::too_many_arguments)]
fn fill_rect(buf: &mut [u8], w: i64, h: i64, x: i64, y: i64, rw: i64, rh: i64, rgb: [u8; 3]) {
    for yy in y..y + rh {
        for xx in x..x + rw {
            put_px(buf, w, h, xx, yy, rgb);
        }
    }
}

/// Pixel width of `text` rendered at `scale` (8 px per glyph cell).
#[must_use]
pub fn text_width(text: &str, scale: i64) -> i64 {
    text.chars().count() as i64 * GLYPH * scale.max(1)
}

/// Draw `text` (ASCII; unsupported glyphs are skipped as blanks) at
/// top-left `(x, y)`, integer-scaled by `scale`, in `rgb`. The
/// `font8x8` glyph rows are bytes whose bit `c` (LSB = leftmost column)
/// is the pixel at column `c`.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_rgb24(
    buf: &mut [u8],
    w: u32,
    h: u32,
    text: &str,
    x: i64,
    y: i64,
    scale: i64,
    rgb: [u8; 3],
) {
    let (w, h) = (w as i64, h as i64);
    let scale = scale.max(1);
    let mut pen_x = x;
    for ch in text.chars() {
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8i64 {
                    if (bits >> col) & 1 == 1 {
                        // Scale each lit cell into a `scale`×`scale` block.
                        let bx = pen_x + col * scale;
                        let by = y + row as i64 * scale;
                        fill_rect(buf, w, h, bx, by, scale, scale, rgb);
                    }
                }
            }
        }
        pen_x += GLYPH * scale;
    }
}

/// Format the chip text: `"label 0.96"` (or just `"label"` when no
/// confidence). The label is lower-cased/trimmed as-is from the
/// detector; confidence is clamped to `0.00..=1.00`.
#[must_use]
pub fn label_text(label: &str, confidence: Option<f32>) -> String {
    let label = label.trim();
    match confidence {
        Some(c) => format!("{label} {:.2}", c.clamp(0.0, 1.0)),
        None => label.to_string(),
    }
}

/// Draw a filled cyan chip with dark text for `text`, anchored to the
/// box's top-left `(box_x1, box_y1)`. The chip sits just ABOVE the box
/// when there's room, otherwise just inside the top edge; it is clamped
/// horizontally so it never runs off-frame. `scale` sizes the font.
/// A blank `text` draws nothing.
pub fn draw_label_chip_rgb24(
    buf: &mut [u8],
    w: u32,
    h: u32,
    box_x1: i64,
    box_y1: i64,
    text: &str,
    scale: i64,
) {
    if text.is_empty() {
        return;
    }
    let (wi, hi) = (w as i64, h as i64);
    let scale = scale.max(1);
    let pad = scale; // 1 logical px of padding, scaled.
    let tw = text_width(text, scale);
    let th = GLYPH * scale;
    let chip_w = tw + pad * 2;
    let chip_h = th + pad * 2;

    // Prefer sitting the chip directly above the box; if it would clip
    // off the top, tuck it just inside the box's top edge instead.
    let mut chip_x = box_x1;
    let mut chip_y = box_y1 - chip_h;
    if chip_y < 0 {
        chip_y = box_y1.max(0);
    }
    // Clamp horizontally so the whole chip stays on-frame.
    if chip_x + chip_w > wi {
        chip_x = (wi - chip_w).max(0);
    }
    chip_x = chip_x.max(0);

    fill_rect(buf, wi, hi, chip_x, chip_y, chip_w, chip_h, ALERT_RGB);
    draw_text_rgb24(
        buf,
        w,
        h,
        text,
        chip_x + pad,
        chip_y + pad,
        scale,
        CHIP_TEXT_RGB,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_text_formats_confidence() {
        assert_eq!(label_text("person", Some(0.9648)), "person 0.96");
        assert_eq!(label_text("  person  ", Some(1.5)), "person 1.00");
        assert_eq!(label_text("vehicle", None), "vehicle");
    }

    #[test]
    fn text_width_scales() {
        assert_eq!(text_width("ab", 1), 16);
        assert_eq!(text_width("ab", 2), 32);
        assert_eq!(text_width("", 3), 0);
    }

    #[test]
    fn draw_text_paints_glyph_pixels_and_skips_space() {
        // A glyph ('A') paints at least one pixel; a space paints none.
        let (w, h) = (16u32, 8u32);
        let mut glyph_buf = vec![0u8; (w * h * 3) as usize];
        draw_text_rgb24(&mut glyph_buf, w, h, "A", 0, 0, 1, [255, 255, 255]);
        assert!(
            glyph_buf.iter().any(|&b| b != 0),
            "a printable glyph must paint pixels"
        );

        let mut space_buf = vec![0u8; (w * h * 3) as usize];
        draw_text_rgb24(&mut space_buf, w, h, " ", 0, 0, 1, [255, 255, 255]);
        assert!(
            space_buf.iter().all(|&b| b == 0),
            "a space must paint nothing"
        );
    }

    #[test]
    fn chip_paints_cyan_background_and_dark_text() {
        // Frame big enough for the chip below the (0,0) anchor.
        let (w, h) = (80u32, 40u32);
        let mut buf = vec![0u8; (w * h * 3) as usize];
        // Anchor at y=20 so the chip sits above it (room at top).
        draw_label_chip_rgb24(&mut buf, w, h, 4, 20, "hi 0.90", 1);
        // Some cyan (chip background) present.
        let cyan = buf.chunks_exact(3).any(|p| p == ALERT_RGB);
        assert!(cyan, "chip must paint its cyan background");
        // Some dark text pixels present.
        let text = buf.chunks_exact(3).any(|p| p == CHIP_TEXT_RGB);
        assert!(text, "chip must paint dark text glyphs");
    }

    #[test]
    fn chip_empty_text_is_noop() {
        let (w, h) = (40u32, 20u32);
        let mut buf = vec![7u8; (w * h * 3) as usize];
        let before = buf.clone();
        draw_label_chip_rgb24(&mut buf, w, h, 4, 10, "", 1);
        assert_eq!(buf, before, "empty label draws nothing");
    }
}
