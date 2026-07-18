//! M-Alert-Clip — short, burned-in "alert clip" builder.
//!
//! Produces a short MP4 covering only the alert timeframe
//! (`[alert − pre, alert + post]`) with the tracked object's bounding
//! box **burned into every frame**, delivered to alert sinks within
//! seconds — decoupled from the up-to-5-minute archival motion clip,
//! which keeps recording untouched. See
//! `../../../nexus-cloud-console/docs/edge-core/M_ALERT_CLIP.md`.
//!
//! **One source feeds both clips.** The alert clip is built from the
//! SAME per-camera [`crate::preroll_ingester::PreRollIngester`] the
//! motion recorder uses: the encoded H.264 pre-roll ring supplies the
//! PRE window and a second live-NAL subscriber supplies the POST
//! window. No second RTSP connection; the passthrough recorder is not
//! touched.
//!
//! This module splits into a **pure core** (this file's upper half —
//! box timeline, coordinate scaling, GOP-aligned pre-roll trim, path
//! layout; unit-tested on any host) and a **GStreamer encode path**
//! (gated behind `#[cfg(feature = "gstreamer")]`, validated by the
//! Linux CI integration job) that decodes the spliced window, overlays
//! the per-frame boxes, and re-encodes with `x264enc`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_types::CameraId;

use crate::preroll::{NalRingBuffer, NalSample};

/// A bounding box to burn into the alert clip, in **supervisor-frame**
/// (analysis) pixel coordinates. Sourced from the tracked object's
/// frame-aligned `detection_bbox` (M-Alert-Clip P1) so the burned-in
/// box matches where the object actually is on each frame, not the
/// EMA-smoothed live-view box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurnBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// One analysis frame's worth of boxes at a wall-clock instant, tagged
/// with the supervisor-frame dimensions they were computed against
/// (needed to scale the boxes onto the native-resolution decoded video).
#[derive(Debug, Clone)]
pub struct BoxFrame {
    pub ts: DateTime<Utc>,
    pub boxes: Vec<BurnBox>,
    pub sup_w: u32,
    pub sup_h: u32,
}

/// Rolling per-camera history of recent [`BoxFrame`]s. The supervisor
/// appends one entry per gated frame (only while alert clips are
/// enabled); the builder snapshots it to overlay boxes onto the decoded
/// window by wall-clock timestamp. Bounded by `retain` so the memory
/// cost is a few hundred small structs per camera — negligible next to
/// the encoded-NAL and RGB buffers.
#[derive(Debug)]
pub struct BoxTimeline {
    /// Ascending-`ts` (append-only) frames.
    frames: VecDeque<BoxFrame>,
    /// How far back to keep frames relative to the newest entry.
    retain: Duration,
}

impl BoxTimeline {
    #[must_use]
    pub fn new(retain: Duration) -> Self {
        Self {
            frames: VecDeque::new(),
            retain,
        }
    }

    /// Append a frame's boxes and trim anything older than `retain`
    /// before the newest entry. Cheap: one push + a bounded pop loop.
    pub fn push(&mut self, frame: BoxFrame) {
        let newest = frame.ts;
        self.frames.push_back(frame);
        let retain = chrono::Duration::from_std(self.retain)
            .unwrap_or_else(|_| chrono::Duration::seconds(10));
        let cutoff = newest - retain;
        while let Some(front) = self.frames.front() {
            if front.ts < cutoff {
                self.frames.pop_front();
            } else {
                break;
            }
        }
    }

    /// The boxes to draw for a decoded frame presented at wall-clock
    /// `ts`: the most recent entry at or before `ts` (**hold-last**), so
    /// boxes persist across the video frames between analysis frames.
    /// Returns `None` before the first entry at/before `ts` (the object
    /// hasn't been detected yet at that point in the window).
    #[must_use]
    pub fn boxes_at(&self, ts: DateTime<Utc>) -> Option<&BoxFrame> {
        // Ascending order, so the last entry with ts <= target is the
        // hold-last winner. Walk from the back for the common case
        // (queries advance monotonically through the window).
        self.frames.iter().rev().find(|f| f.ts <= ts)
    }

    /// Snapshot every retained frame (for the builder to move into the
    /// encode task without holding the supervisor's lock).
    #[must_use]
    pub fn snapshot(&self) -> Vec<BoxFrame> {
        self.frames.iter().cloned().collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

/// Scale a supervisor-frame box onto native-resolution pixels. The
/// supervisor computes boxes at the analysis resolution; the alert clip
/// decodes native-resolution H.264, so boxes must be scaled by the
/// per-axis ratio before being drawn. A zero supervisor dimension is a
/// no-op (returns the box unchanged) rather than dividing by zero.
#[must_use]
pub fn scale_box(b: &BurnBox, sup_w: u32, sup_h: u32, native_w: u32, native_h: u32) -> BurnBox {
    if sup_w == 0 || sup_h == 0 {
        return *b;
    }
    let sx = native_w as f32 / sup_w as f32;
    let sy = native_h as f32 / sup_h as f32;
    BurnBox {
        x1: b.x1 * sx,
        y1: b.y1 * sy,
        x2: b.x2 * sx,
        y2: b.y2 * sy,
    }
}

/// Trim a pre-roll ring snapshot to at most `pre` seconds of
/// **GOP-aligned** history, reusing the exact trim the live ring uses
/// (so the result still starts on a keyframe and decodes cleanly). The
/// ingester's ring is sized for `pre_roll_secs`, which may exceed the
/// alert clip's `pre_secs`; this bounds the PRE window to the configured
/// value without a bespoke GOP walker.
#[must_use]
pub fn trim_preroll(samples: Vec<NalSample>, pre: Duration) -> Vec<NalSample> {
    let mut ring = NalRingBuffer::new(pre);
    for s in samples {
        ring.push(s);
    }
    ring.snapshot()
}

/// Relative path (under `clips_dir`) for an alert clip, mirroring the
/// motion-clip layout but under an `alert/` subdir so it is easy to
/// find and sweep separately:
/// `alert/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}.mp4`.
///
/// Kept relative to the same `clips_dir` root the motion recorder uses
/// so the sink dispatcher resolves it with the existing
/// `clips_dir.join(rel)` logic — no second storage root.
#[must_use]
pub fn alert_clip_rel_path(camera_id: CameraId, started_at: DateTime<Utc>) -> PathBuf {
    let date = started_at.format("%Y-%m-%d").to_string();
    let start_ms = started_at.timestamp_millis();
    PathBuf::from("alert")
        .join(camera_id.to_string())
        .join(date)
        .join(format!("{start_ms}.mp4"))
}

/// Inflight (`.partial.mp4`) absolute path the builder writes to before
/// finalisation. The builder renames it to the final `.mp4` atomically
/// once `mp4mux` has written the moov atom, so a reader that sees the
/// final path can trust the file is complete (mirrors the motion
/// recorder's `.partial.mp4` → final rename).
#[must_use]
pub fn alert_clip_inflight_path(clips_dir: &Path, rel: &Path) -> PathBuf {
    clips_dir.join(rel).with_extension("partial.mp4")
}

/// Wall-clock instant a decoded frame maps to, given the window's start
/// wall-clock and the frame's PTS rebased to zero at window start.
///
/// The spliced NAL window is contiguous real-time footage, so a decoded
/// frame's zero-based PTS equals its elapsed offset from the window's
/// first frame. This lets the encoder look boxes up by
/// `boxes_at(window_start + rebased_pts)` without mapping RTP PTS to
/// wall-clock.
#[must_use]
pub fn frame_wall_clock(window_start: DateTime<Utc>, rebased_pts: Duration) -> DateTime<Utc> {
    window_start
        + chrono::Duration::from_std(rebased_pts).unwrap_or_else(|_| chrono::Duration::zero())
}

/// Bright-green stroke colour for burned-in alert-clip boxes (matches
/// the alert snapshot in `supervisor.rs` and the live-view overlay).
pub const BURN_BOX_RGB: [u8; 3] = [0x2e, 0xe6, 0x4a];

/// Draw `b` (in the SAME pixel space as the buffer) onto a packed RGB24
/// frame in place, with stroke half-width `half` (so the visible stroke
/// is `2*half + 1` px — the encoder scales this up for native
/// resolution). Coordinates are clamped to the frame; a degenerate box
/// is a no-op. Mirrors `supervisor::draw_bbox_rgb24` but takes a
/// [`BurnBox`] already scaled to native pixels via [`scale_box`].
pub fn draw_burnbox_rgb24(buf: &mut [u8], width: u32, height: u32, b: &BurnBox, half: i64) {
    let w = width as i64;
    let h = height as i64;
    if w <= 0 || h <= 0 {
        return;
    }
    let x1 = (b.x1.round() as i64).clamp(0, w - 1);
    let y1 = (b.y1.round() as i64).clamp(0, h - 1);
    let x2 = (b.x2.round() as i64).clamp(0, w - 1);
    let y2 = (b.y2.round() as i64).clamp(0, h - 1);
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    let mut put = |x: i64, y: i64| {
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let idx = ((y * w + x) * 3) as usize;
        if idx + 2 < buf.len() {
            buf[idx] = BURN_BOX_RGB[0];
            buf[idx + 1] = BURN_BOX_RGB[1];
            buf[idx + 2] = BURN_BOX_RGB[2];
        }
    };
    // Top + bottom edges, thickened by +/- half rows.
    for x in x1..=x2 {
        for d in -half..=half {
            put(x, y1 + d);
            put(x, y2 + d);
        }
    }
    // Left + right edges, thickened by +/- half columns.
    for y in y1..=y2 {
        for d in -half..=half {
            put(x1 + d, y);
            put(x2 + d, y);
        }
    }
}

/// Stroke half-width to use for a native frame of the given width, so
/// the burned-in box stays visible after H.264 compression regardless
/// of resolution (roughly 1px per 640px of width, min 1).
#[must_use]
pub fn burn_stroke_half(native_w: u32) -> i64 {
    ((native_w / 640).max(1)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(base: DateTime<Utc>, ms: i64) -> DateTime<Utc> {
        base + chrono::Duration::milliseconds(ms)
    }

    #[test]
    fn scale_box_maps_supervisor_to_native() {
        let b = BurnBox {
            x1: 100.0,
            y1: 50.0,
            x2: 200.0,
            y2: 150.0,
        };
        // 640x360 -> 1920x1080 is a uniform 3x.
        let s = scale_box(&b, 640, 360, 1920, 1080);
        assert_eq!(s.x1, 300.0);
        assert_eq!(s.y1, 150.0);
        assert_eq!(s.x2, 600.0);
        assert_eq!(s.y2, 450.0);
    }

    #[test]
    fn scale_box_zero_dim_is_noop() {
        let b = BurnBox {
            x1: 1.0,
            y1: 2.0,
            x2: 3.0,
            y2: 4.0,
        };
        assert_eq!(scale_box(&b, 0, 360, 1920, 1080), b);
        assert_eq!(scale_box(&b, 640, 0, 1920, 1080), b);
    }

    fn box_frame(base: DateTime<Utc>, ms: i64, n: usize) -> BoxFrame {
        BoxFrame {
            ts: ts(base, ms),
            boxes: vec![
                BurnBox {
                    x1: n as f32,
                    y1: 0.0,
                    x2: n as f32 + 10.0,
                    y2: 10.0,
                };
                n
            ],
            sup_w: 640,
            sup_h: 360,
        }
    }

    #[test]
    fn timeline_holds_last_box_between_analysis_frames() {
        let base = Utc::now();
        let mut tl = BoxTimeline::new(Duration::from_secs(10));
        tl.push(box_frame(base, 0, 1)); // one box at t=0
        tl.push(box_frame(base, 200, 2)); // two boxes at t=200ms

        // Before the first entry: no boxes.
        assert!(tl.boxes_at(ts(base, -50)).is_none());
        // Between t=0 and t=200: hold t=0's single box.
        assert_eq!(tl.boxes_at(ts(base, 100)).unwrap().boxes.len(), 1);
        // At/after t=200: two boxes.
        assert_eq!(tl.boxes_at(ts(base, 200)).unwrap().boxes.len(), 2);
        assert_eq!(tl.boxes_at(ts(base, 5000)).unwrap().boxes.len(), 2);
    }

    #[test]
    fn timeline_trims_older_than_retain() {
        let base = Utc::now();
        let mut tl = BoxTimeline::new(Duration::from_secs(3));
        tl.push(box_frame(base, 0, 1));
        tl.push(box_frame(base, 1000, 1));
        tl.push(box_frame(base, 2000, 1));
        // Pushing t=5s evicts everything older than t=5s-3s=2s, i.e. the
        // t=0 and t=1s frames (strictly < cutoff); t=2s is kept.
        tl.push(box_frame(base, 5000, 1));
        assert_eq!(tl.len(), 2);
        assert!(tl.boxes_at(ts(base, 0)).is_none());
        assert!(tl.boxes_at(ts(base, 2000)).is_some());
    }

    #[test]
    fn frame_wall_clock_offsets_from_window_start() {
        let start = Utc::now();
        let w = frame_wall_clock(start, Duration::from_millis(1500));
        assert_eq!(w, start + chrono::Duration::milliseconds(1500));
    }

    fn nal(pts_ms: u64, keyframe: bool) -> NalSample {
        NalSample {
            pts: Some(Duration::from_millis(pts_ms)),
            dts: Some(Duration::from_millis(pts_ms)),
            is_keyframe: keyframe,
            data: vec![0u8; 8],
        }
    }

    #[test]
    fn trim_preroll_bounds_to_pre_and_starts_on_keyframe() {
        // Five 1s GOPs (keyframe every second) spanning 0..4s.
        let samples = vec![
            nal(0, true),
            nal(500, false),
            nal(1000, true),
            nal(1500, false),
            nal(2000, true),
            nal(2500, false),
            nal(3000, true),
            nal(3500, false),
            nal(4000, true),
        ];
        // Trim to 2s: the ring keeps whole GOPs within ~2s of the newest
        // PTS and always starts on a keyframe.
        let trimmed = trim_preroll(samples, Duration::from_secs(2));
        assert!(!trimmed.is_empty());
        assert!(
            trimmed.first().unwrap().is_keyframe,
            "trimmed window must start on a keyframe"
        );
        let span = trimmed.last().unwrap().pts.unwrap() - trimmed.first().unwrap().pts.unwrap();
        assert!(
            span <= Duration::from_secs(2),
            "trimmed span {span:?} must not exceed the 2s pre window"
        );
    }

    #[test]
    fn alert_clip_paths_follow_layout() {
        let started = DateTime::parse_from_rfc3339("2026-07-17T12:34:56.789Z")
            .unwrap()
            .with_timezone(&Utc);
        let rel = alert_clip_rel_path(7, started);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        assert!(rel_str.starts_with("alert/7/2026-07-17/"));
        assert!(rel_str.ends_with(".mp4"));

        let inflight = alert_clip_inflight_path(Path::new("/var/lib/nexus/clips"), &rel);
        assert!(inflight.to_string_lossy().ends_with(".partial.mp4"));
    }

    #[test]
    fn draw_burnbox_paints_border_not_interior() {
        let (w, h) = (8u32, 8u32);
        let mut buf = vec![0u8; (w * h * 3) as usize];
        draw_burnbox_rgb24(
            &mut buf,
            w,
            h,
            &BurnBox {
                x1: 1.0,
                y1: 1.0,
                x2: 6.0,
                y2: 6.0,
            },
            0,
        );
        // A box corner is painted green...
        let corner = ((w + 1) * 3) as usize;
        assert_eq!(&buf[corner..corner + 3], &BURN_BOX_RGB);
        // ...but the interior is untouched (only the border is drawn).
        let center = ((3 * w + 3) * 3) as usize;
        assert_eq!(&buf[center..center + 3], &[0, 0, 0]);
    }

    #[test]
    fn draw_burnbox_degenerate_is_noop() {
        let mut buf = vec![5u8; 8 * 8 * 3];
        let before = buf.clone();
        draw_burnbox_rgb24(
            &mut buf,
            8,
            8,
            &BurnBox {
                x1: 4.0,
                y1: 4.0,
                x2: 4.0,
                y2: 4.0,
            },
            0,
        );
        assert_eq!(buf, before, "a zero-area box must paint nothing");
    }

    #[test]
    fn burn_stroke_half_scales_with_width() {
        assert_eq!(burn_stroke_half(320), 1); // min 1
        assert_eq!(burn_stroke_half(640), 1);
        assert_eq!(burn_stroke_half(1920), 3);
    }
}
