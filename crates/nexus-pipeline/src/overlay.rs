//! Shared bounding-box + label overlay rendering onto packed RGB24
//! buffers, used by BOTH the alert snapshot (`supervisor::write_alert_snapshot`)
//! and the burned-in alert clip (`alert_clip::encode_alert_clip`), so the
//! box and the `"person 0.96"` label chip read identically in the console
//! snapshot, the sink email / SureView copies, and the clip.
//!
//! Text is rendered with the bundled **Barlow Semi Condensed** typeface
//! (SIL Open Font License — `assets/BarlowSemiCondensed-OFL.txt`) via the
//! pure-Rust `fontdue` rasteriser, anti-aliased and sized in pixels per
//! caller so the label reads skinny + legible at both supervisor (~720p
//! analysis) and native (up to 1080p) clip resolutions.

use std::sync::OnceLock;

use fontdue::{Font, FontSettings};

/// Bundled condensed UI font (OFL — see `assets/BarlowSemiCondensed-OFL.txt`).
/// A condensed, medium-weight sans that stays readable after H.264
/// compression while reading far thinner than a blocky bitmap font.
static FONT_BYTES: &[u8] = include_bytes!("../assets/BarlowSemiCondensed-Medium.ttf");

/// Parsed font, built once on first overlay draw.
fn font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, FontSettings::default())
            .expect("bundled BarlowSemiCondensed-Medium.ttf must parse")
    })
}

/// The single alert overlay colour — cyan `#22d3ee`, matching the
/// console's alert-detail styling. Used for the box stroke AND the
/// label-chip background.
pub const ALERT_RGB: [u8; 3] = [0x22, 0xd3, 0xee];

/// Dark text drawn on the cyan chip (mirrors the mock `.lbl`
/// `color:#04161c`), for legible contrast against the cyan background.
const CHIP_TEXT_RGB: [u8; 3] = [0x04, 0x16, 0x1c];

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

/// Advance width in pixels of `text` at font pixel-height `px`.
#[must_use]
pub fn text_width(text: &str, px: f32) -> f32 {
    let f = font();
    text.chars().map(|c| f.metrics(c, px).advance_width).sum()
}

/// Total line-box height (ascent + descent) in pixels at `px`.
fn line_height(px: f32) -> f32 {
    font()
        .horizontal_line_metrics(px)
        .map_or(px, |m| m.ascent - m.descent)
}

/// Alpha-blend `rgb` at coverage `a` (0..=255) over the existing pixel.
#[inline]
fn blend_px(buf: &mut [u8], w: i64, h: i64, x: i64, y: i64, rgb: [u8; 3], a: u8) {
    if a == 0 || x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let idx = ((y * w + x) * 3) as usize;
    if idx + 2 >= buf.len() {
        return;
    }
    let a = a as u32;
    let ia = 255 - a;
    buf[idx] = ((rgb[0] as u32 * a + buf[idx] as u32 * ia) / 255) as u8;
    buf[idx + 1] = ((rgb[1] as u32 * a + buf[idx + 1] as u32 * ia) / 255) as u8;
    buf[idx + 2] = ((rgb[2] as u32 * a + buf[idx + 2] as u32 * ia) / 255) as u8;
}

/// Draw `text` in `rgb`, anti-aliased, with the text baseline at screen
/// row `baseline` and the left pen at `x`, sized to font pixel-height
/// `px`. Glyph coverage is alpha-blended so edges stay smooth after
/// H.264 compression.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_rgb24(
    buf: &mut [u8],
    w: u32,
    h: u32,
    text: &str,
    x: f32,
    baseline: f32,
    px: f32,
    rgb: [u8; 3],
) {
    let (wi, hi) = (w as i64, h as i64);
    let f = font();
    let mut pen_x = x;
    for ch in text.chars() {
        let (m, bitmap) = f.rasterize(ch, px);
        if m.width > 0 && m.height > 0 {
            let gx = (pen_x + m.xmin as f32).round() as i64;
            // Glyph top in screen (y-down) space, measured from baseline.
            let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i64;
            for (row, line) in bitmap.chunks_exact(m.width).enumerate() {
                for (col, &a) in line.iter().enumerate() {
                    blend_px(buf, wi, hi, gx + col as i64, gy + row as i64, rgb, a);
                }
            }
        }
        pen_x += m.advance_width;
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
    px: f32,
) {
    if text.is_empty() {
        return;
    }
    let (wi, hi) = (w as i64, h as i64);
    let px = px.max(6.0);
    let pad = (px * 0.28).round().max(2.0) as i64;
    let tw = text_width(text, px).ceil() as i64;
    let th = line_height(px).ceil() as i64;
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
    // Baseline = chip top + top padding + ascent.
    let ascent = font().horizontal_line_metrics(px).map_or(px, |m| m.ascent);
    let baseline = chip_y as f32 + pad as f32 + ascent;
    draw_text_rgb24(
        buf,
        w,
        h,
        text,
        chip_x as f32 + pad as f32,
        baseline,
        px,
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
    fn text_width_grows_with_size_and_length() {
        assert_eq!(text_width("", 20.0), 0.0);
        assert!(text_width("ab", 20.0) > 0.0);
        // More characters and a larger px both widen the advance.
        assert!(text_width("abcd", 20.0) > text_width("ab", 20.0));
        assert!(text_width("ab", 30.0) > text_width("ab", 15.0));
    }

    #[test]
    fn chip_paints_cyan_background_and_darkened_text() {
        // White frame, big enough for the chip above the anchor.
        let (w, h) = (160u32, 80u32);
        let mut buf = vec![255u8; (w * h * 3) as usize];
        draw_label_chip_rgb24(&mut buf, w, h, 6, 48, "hi 0.90", 18.0);
        // Some cyan (chip background) present.
        let cyan = buf.chunks_exact(3).any(|p| p == ALERT_RGB);
        assert!(cyan, "chip must paint its cyan background");
        // Anti-aliased dark text darkens some chip pixels below cyan's
        // green/blue channels (the exact colour varies with coverage).
        let text = buf
            .chunks_exact(3)
            .any(|p| p != ALERT_RGB && p[1] < ALERT_RGB[1] && p[2] < ALERT_RGB[2]);
        assert!(text, "chip must paint darkened (blended) text pixels");
    }

    #[test]
    fn draw_text_paints_glyph_pixels_and_skips_space() {
        // A glyph ('A') paints at least one pixel; a space paints none.
        let (w, h) = (32u32, 32u32);
        let mut glyph_buf = vec![0u8; (w * h * 3) as usize];
        draw_text_rgb24(&mut glyph_buf, w, h, "A", 2.0, 24.0, 22.0, [255, 255, 255]);
        assert!(
            glyph_buf.iter().any(|&b| b != 0),
            "a printable glyph must paint pixels"
        );

        let mut space_buf = vec![0u8; (w * h * 3) as usize];
        draw_text_rgb24(&mut space_buf, w, h, " ", 2.0, 24.0, 22.0, [255, 255, 255]);
        assert!(
            space_buf.iter().all(|&b| b == 0),
            "a space must paint nothing"
        );
    }

    #[test]
    fn chip_empty_text_is_noop() {
        let (w, h) = (40u32, 20u32);
        let mut buf = vec![7u8; (w * h * 3) as usize];
        let before = buf.clone();
        draw_label_chip_rgb24(&mut buf, w, h, 4, 10, "", 14.0);
        assert_eq!(buf, before, "empty label draws nothing");
    }
}
