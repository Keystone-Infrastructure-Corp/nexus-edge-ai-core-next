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

/// Fill a rounded rectangle (top-left `(x, y)`, size `rw`×`rh`, corner
/// radius `radius`) with `rgb`. `radius` is clamped to half the shorter
/// side; `radius == 0` is a plain filled rectangle. Used for the label
/// chip so it matches the console mock's rounded `.lbl`.
#[allow(clippy::too_many_arguments)]
fn fill_round_rect(
    buf: &mut [u8],
    w: i64,
    h: i64,
    x: i64,
    y: i64,
    rw: i64,
    rh: i64,
    radius: i64,
    rgb: [u8; 3],
) {
    let r = radius.clamp(0, rw.min(rh) / 2);
    for yy in 0..rh {
        for xx in 0..rw {
            // Snap to the nearest corner-arc centre; interior pixels
            // resolve to themselves (dx=dy=0) and are always filled.
            let cx = if xx < r {
                r
            } else if xx >= rw - r {
                rw - 1 - r
            } else {
                xx
            };
            let cy = if yy < r {
                r
            } else if yy >= rh - r {
                rh - 1 - r
            } else {
                yy
            };
            let (dx, dy) = (xx - cx, yy - cy);
            if dx * dx + dy * dy <= r * r {
                put_px(buf, w, h, x + xx, y + yy, rgb);
            }
        }
    }
}

/// Box stroke width + corner radius (px) proportional to a frame of the
/// given dimensions, so the burned box reads like the console mock's
/// `.bbox` (thin cyan stroke, rounded corners) across resolutions
/// (~2 px stroke / 8 px radius at 720p, scaling gently up).
#[must_use]
pub fn box_metrics(w: u32, h: u32) -> (i64, i64) {
    let base = w.min(h) as f32;
    let stroke = (base / 360.0).round().clamp(2.0, 6.0) as i64;
    let radius = (base / 80.0).round().clamp(4.0, 22.0) as i64;
    (stroke, radius)
}

/// Draw a rounded-rectangle box **outline** (`stroke` px thick, corner
/// `radius`) for the pixel box `(x1,y1)..(x2,y2)` in `rgb`, matching the
/// mock `.bbox`. Coordinates are clamped to the frame; a degenerate box
/// is a no-op.
#[allow(clippy::too_many_arguments)]
pub fn draw_box_rgb24(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    stroke: i64,
    radius: i64,
    rgb: [u8; 3],
) {
    let (wi, hi) = (w as i64, h as i64);
    if wi <= 0 || hi <= 0 {
        return;
    }
    let x1 = x1.clamp(0, wi - 1);
    let y1 = y1.clamp(0, hi - 1);
    let x2 = x2.clamp(0, wi - 1);
    let y2 = y2.clamp(0, hi - 1);
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    let stroke = stroke.max(1);
    let radius = radius.clamp(0, (x2 - x1).min(y2 - y1) / 2);

    // Straight edges (between the rounded corners), thickened inward.
    for x in (x1 + radius)..=(x2 - radius) {
        for d in 0..stroke {
            put_px(buf, wi, hi, x, y1 + d, rgb);
            put_px(buf, wi, hi, x, y2 - d, rgb);
        }
    }
    for y in (y1 + radius)..=(y2 - radius) {
        for d in 0..stroke {
            put_px(buf, wi, hi, x1 + d, y, rgb);
            put_px(buf, wi, hi, x2 - d, y, rgb);
        }
    }
    if radius == 0 {
        return;
    }
    // Rounded corners: a quarter-annulus of thickness `stroke` at each.
    let r_out2 = radius * radius;
    let r_in = (radius - stroke).max(0);
    let r_in2 = r_in * r_in;
    let corners = [
        (x1 + radius, y1 + radius, -1i64, -1i64),
        (x2 - radius, y1 + radius, 1, -1),
        (x1 + radius, y2 - radius, -1, 1),
        (x2 - radius, y2 - radius, 1, 1),
    ];
    for &(cx, cy, sx, sy) in &corners {
        for dy in 0..=radius {
            for dx in 0..=radius {
                let d2 = dx * dx + dy * dy;
                if d2 <= r_out2 && d2 >= r_in2 {
                    put_px(buf, wi, hi, cx + sx * dx, cy + sy * dy, rgb);
                }
            }
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
    // Mirror the mock `.lbl`: more horizontal than vertical padding,
    // rounded ~3 px corners.
    let pad_x = (px * 0.42).round().max(3.0) as i64;
    let pad_y = (px * 0.16).round().max(1.0) as i64;
    let radius = (px * 0.30).round().clamp(2.0, 10.0) as i64;
    let tw = text_width(text, px).ceil() as i64;
    let th = line_height(px).ceil() as i64;
    let chip_w = tw + pad_x * 2;
    let chip_h = th + pad_y * 2;

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

    fill_round_rect(
        buf, wi, hi, chip_x, chip_y, chip_w, chip_h, radius, ALERT_RGB,
    );
    // Baseline = chip top + top padding + ascent.
    let ascent = font().horizontal_line_metrics(px).map_or(px, |m| m.ascent);
    let baseline = chip_y as f32 + pad_y as f32 + ascent;
    draw_text_rgb24(
        buf,
        w,
        h,
        text,
        chip_x as f32 + pad_x as f32,
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
    fn draw_box_paints_rounded_outline_not_interior() {
        let (w, h) = (40u32, 40u32);
        let mut buf = vec![0u8; (w * h * 3) as usize];
        draw_box_rgb24(&mut buf, w, h, 5, 5, 34, 34, 2, 6, ALERT_RGB);
        // A mid-edge pixel on the top stroke is painted the alert colour.
        let mid_top = ((5 * w + 20) * 3) as usize;
        assert_eq!(&buf[mid_top..mid_top + 3], &ALERT_RGB);
        // The interior is untouched (outline only).
        let center = ((20 * w + 20) * 3) as usize;
        assert_eq!(&buf[center..center + 3], &[0, 0, 0]);
        // The extreme corner is rounded off (not painted).
        let corner = ((5 * w + 5) * 3) as usize;
        assert_eq!(&buf[corner..corner + 3], &[0, 0, 0]);
    }

    #[test]
    fn box_metrics_scale_and_clamp() {
        let (s720, r720) = box_metrics(1280, 720);
        assert_eq!(s720, 2);
        assert!(r720 >= 4);
        let (s_big, r_big) = box_metrics(4000, 4000);
        assert!(s_big <= 6 && r_big <= 22);
    }

    #[test]
    fn chip_paints_cyan_background_and_darkened_text() {
        // White frame, big enough for the chip above the anchor.
        let (w, h) = (160u32, 80u32);
        let mut buf = vec![255u8; (w * h * 3) as usize];
        draw_label_chip_rgb24(&mut buf, w, h, 6, 48, "hi 0.90", 18.0);
        // Some cyan (chip background) present.
        let cyan = buf.as_chunks::<3>().0.contains(&ALERT_RGB);
        assert!(cyan, "chip must paint its cyan background");
        // Anti-aliased dark text darkens some chip pixels below cyan's
        // green/blue channels (the exact colour varies with coverage).
        let text = buf
            .as_chunks::<3>()
            .0
            .iter()
            .any(|p| *p != ALERT_RGB && p[1] < ALERT_RGB[1] && p[2] < ALERT_RGB[2]);
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
