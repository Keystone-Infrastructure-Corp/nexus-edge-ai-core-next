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
#[derive(Debug, Clone, PartialEq)]
pub struct BurnBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    /// Detector class label for the object in this box (e.g. "person").
    /// Rendered as the box's label chip; empty draws no chip.
    pub label: String,
    /// Detector confidence `0..1` for `label`, shown as "label 0.96".
    pub confidence: f32,
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
        return b.clone();
    }
    let sx = native_w as f32 / sup_w as f32;
    let sy = native_h as f32 / sup_h as f32;
    BurnBox {
        x1: b.x1 * sx,
        y1: b.y1 * sy,
        x2: b.x2 * sx,
        y2: b.y2 * sy,
        label: b.label.clone(),
        confidence: b.confidence,
    }
}

/// IoU above which [`dedupe_burn_boxes`] treats two boxes as the same
/// physical object. Deliberately moderate: tracker fragments / coasting
/// ghosts of one object overlap heavily, while two genuinely distinct
/// objects standing close together stay below it.
const BURN_DEDUPE_IOU: f32 = 0.45;

/// Intersection-over-union of two boxes in the same coordinate space.
/// `0.0` when they don't overlap or either is degenerate.
fn burn_box_iou(a: &BurnBox, b: &BurnBox) -> f32 {
    let ix1 = a.x1.max(b.x1);
    let iy1 = a.y1.max(b.y1);
    let ix2 = a.x2.min(b.x2);
    let iy2 = a.y2.min(b.y2);
    let inter = (ix2 - ix1).max(0.0) * (iy2 - iy1).max(0.0);
    if inter <= 0.0 {
        return 0.0;
    }
    let area_a = (a.x2 - a.x1).max(0.0) * (a.y2 - a.y1).max(0.0);
    let area_b = (b.x2 - b.x1).max(0.0) * (b.y2 - b.y1).max(0.0);
    let union = area_a + area_b - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Collapse overlapping duplicate boxes so one physical object yields
/// exactly one burned box. Greedy NMS: keep the highest-confidence box,
/// then drop any remaining box whose IoU with an already-kept box
/// exceeds [`BURN_DEDUPE_IOU`]. This removes the tracker-fragment /
/// coasting-ghost "trail" (several stale boxes for one object) that the
/// operator otherwise sees stacked on the alert clip.
pub fn dedupe_burn_boxes(boxes: &mut Vec<BurnBox>) {
    if boxes.len() < 2 {
        return;
    }
    boxes.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<BurnBox> = Vec::with_capacity(boxes.len());
    for b in boxes.drain(..) {
        if kept.iter().any(|k| burn_box_iou(k, &b) > BURN_DEDUPE_IOU) {
            continue;
        }
        kept.push(b);
    }
    *boxes = kept;
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

/// Font pixel-height for the burned-in label chip at the given native
/// width, so "person 0.96" stays legible after H.264 compression across
/// resolutions while reading skinny. ~1.7% of width, clamped so a small
/// frame stays readable and a huge one can't produce an absurd chip.
#[must_use]
pub fn label_px(native_w: u32) -> f32 {
    (native_w as f32 * 0.0172).clamp(13.0, 34.0)
}

/// Re-pack a tightly-packed RGB24 frame (`row_bytes` = `width*3` per
/// row, no padding) into GStreamer's default RGB stride, which rounds
/// each row UP to a multiple of 4 bytes. Returns the input unchanged
/// when `row_bytes` is already 4-aligned (the common case).
///
/// Without this the appsrc buffer we feed the alert-clip encoder is
/// tighter than the stride the caps imply for any native width not
/// divisible by 4, so every row shears — rotating the R/G/B channels
/// (rainbow/blue discoloration) and ghosting the burned box + label.
#[must_use]
pub fn align_rgb_stride(tight: Vec<u8>, row_bytes: usize, h: usize) -> Vec<u8> {
    let out_stride = (row_bytes + 3) & !3;
    if out_stride == row_bytes {
        return tight;
    }
    let mut padded = vec![0u8; out_stride * h];
    for y in 0..h {
        let s = y * row_bytes;
        let d = y * out_stride;
        padded[d..d + row_bytes].copy_from_slice(&tight[s..s + row_bytes]);
    }
    padded
}

// ---------------------------------------------------------------------------
// GStreamer encode path (decode -> burn-in overlay -> re-encode).
//
// Gated behind `feature = "gstreamer"`. Two coupled pipelines run in
// lock-step: a decode pipeline (appsrc -> parse -> avdec -> RGB appsink)
// feeds one frame at a time to a lazily-built encode pipeline (appsrc ->
// scale -> H.264 encoder -> mp4mux -> filesink). The pull loop draws the
// per-frame boxes onto each decoded RGB frame before re-pushing it, so
// memory stays bounded (a few frames in flight) regardless of clip
// length. Validated by the Linux CI integration job and the local
// `gstreamer`-feature test below.
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
pub use encode::{encode_alert_clip, AlertClipError, AlertClipStats};

#[cfg(feature = "gstreamer")]
mod encode {
    use std::path::Path;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app::{AppSink, AppSrc};
    use gstreamer_video::prelude::*;
    use gstreamer_video::{VideoFormat, VideoFrameRef, VideoInfo};
    use nexus_types::CodecKind;
    use tracing::warn;

    use super::{align_rgb_stride, frame_wall_clock, label_px, scale_box, BoxFrame};
    use crate::gst_clip_recorder::push_sample;
    use crate::preroll::NalSample;

    /// Inter-frame interval [`push_sample`] synthesises for a NAL with no
    /// PTS/DTS (30 fps). Only ever used for the rare PTS-less startup
    /// sample; real footage carries PTS.
    const FALLBACK_INTERVAL_NS: u64 = 33_333_333;
    /// How long to wait for the encode pipeline to finalise the moov atom
    /// after EOS before giving up (the file is still usable if partially
    /// muxed, but we prefer a clean close).
    const EOS_TIMEOUT: Duration = Duration::from_secs(10);

    /// Final stats from a successful [`encode_alert_clip`].
    #[derive(Debug, Clone, Copy)]
    pub struct AlertClipStats {
        pub duration_ms: i64,
        pub size_bytes: i64,
    }

    #[derive(Debug, thiserror::Error)]
    pub enum AlertClipError {
        #[error("gstreamer: {0}")]
        Gst(String),
        #[error("io: {0}")]
        Io(#[from] std::io::Error),
        #[error("no frames decoded from the alert-clip window")]
        NoFrames,
        #[error("no H.264 encoder available on this host")]
        NoEncoder,
    }

    /// First available H.264 encoder, hardware-first (matching the
    /// live-view transcode preference in `webrtc.rs`) then software.
    /// `None` on a box with no usable encoder — the caller marks the
    /// clip failed and the dispatcher delivers the alarm clip-less.
    fn pick_h264_encoder() -> Option<&'static str> {
        [
            "vah264enc",
            "vaapih264enc",
            "nvh264enc",
            "x264enc",
            "openh264enc",
        ]
        .into_iter()
        .find(|n| gst::ElementFactory::find(n).is_some())
    }

    /// Even-round down — H.264 requires even width/height.
    fn even(v: u32) -> u32 {
        v & !1
    }

    /// Downscale target preserving aspect ratio, capped at `max_w`
    /// (`0` disables the cap). Never upscales; floors at 2px.
    fn capped_dims(w: u32, h: u32, max_w: u32) -> (u32, u32) {
        if max_w == 0 || w <= max_w {
            return (even(w).max(2), even(h).max(2));
        }
        let scale = f64::from(max_w) / f64::from(w);
        (
            even(max_w).max(2),
            even((f64::from(h) * scale).round() as u32).max(2),
        )
    }

    fn by_name_appsrc(p: &gst::Pipeline, name: &str) -> Result<AppSrc, AlertClipError> {
        p.by_name(name)
            .ok_or_else(|| AlertClipError::Gst(format!("element {name} missing")))?
            .downcast::<AppSrc>()
            .map_err(|_| AlertClipError::Gst(format!("{name} is not an appsrc")))
    }

    fn drain_eos(pipeline: &gst::Pipeline) -> Result<(), AlertClipError> {
        let Some(bus) = pipeline.bus() else {
            return Ok(());
        };
        let deadline = std::time::Instant::now() + EOS_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                warn!("alert-clip encode EOS timed out; file may be truncated");
                return Ok(());
            }
            let timeout = gst::ClockTime::from_nseconds(remaining.as_nanos() as u64);
            match bus.timed_pop(Some(timeout)) {
                None => return Ok(()),
                Some(msg) => match msg.view() {
                    gst::MessageView::Eos(..) => return Ok(()),
                    gst::MessageView::Error(e) => {
                        return Err(AlertClipError::Gst(format!(
                            "encode bus error: {} ({})",
                            e.error(),
                            e.debug().unwrap_or_default()
                        )));
                    }
                    _ => {}
                },
            }
        }
    }

    /// Decode `window` (H.264/H.265 byte-stream, keyframe-first), burn the
    /// per-frame boxes from `box_frames` in (looked up by wall-clock,
    /// hold-last), and re-encode to an MP4 at `out_partial`. The caller
    /// renames `out_partial` to the final path once this returns `Ok`
    /// (existence of the final path = "ready").
    ///
    /// `window_start` is the wall-clock of the window's first frame
    /// (`alert_ts - pre`); box lookup uses `window_start + rebased_pts`.
    pub fn encode_alert_clip(
        window: &[NalSample],
        codec: CodecKind,
        box_frames: &[BoxFrame],
        window_start: DateTime<Utc>,
        out_partial: &Path,
        max_encode_width: u32,
    ) -> Result<AlertClipStats, AlertClipError> {
        gst::init().map_err(|e| AlertClipError::Gst(format!("gst init: {e}")))?;
        if window.is_empty() {
            return Err(AlertClipError::NoFrames);
        }
        let encoder = pick_h264_encoder().ok_or(AlertClipError::NoEncoder)?;
        let (parse, dec, in_media) = match codec.base() {
            "h265" => ("h265parse", "avdec_h265", "video/x-h265"),
            _ => ("h264parse", "avdec_h264", "video/x-h264"),
        };

        // --- Decode pipeline: appsrc -> parse -> avdec -> RGB appsink. ---
        let dec_desc = format!(
            "appsrc name=dsrc is-live=false format=time do-timestamp=false block=true \
                 max-bytes=67108864 \
             ! {parse} config-interval=0 \
             ! {dec} \
             ! videoconvert \
             ! video/x-raw,format=RGB \
             ! appsink name=dsink sync=false max-buffers=4 drop=false"
        );
        let dec_pipeline = gst::parse::launch(&dec_desc)
            .map_err(|e| AlertClipError::Gst(format!("decode launch: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| AlertClipError::Gst("decode graph is not a pipeline".into()))?;
        let dsrc = by_name_appsrc(&dec_pipeline, "dsrc")?;
        dsrc.set_caps(Some(
            &gst::Caps::builder(in_media)
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));
        let dsink = dec_pipeline
            .by_name("dsink")
            .ok_or_else(|| AlertClipError::Gst("appsink dsink missing".into()))?
            .downcast::<AppSink>()
            .map_err(|_| AlertClipError::Gst("dsink is not an appsink".into()))?;
        dec_pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| AlertClipError::Gst(format!("decode set Playing: {e}")))?;

        // Push NALs from a helper thread so the pull loop drains
        // concurrently — otherwise the decode appsink (max-buffers=4)
        // fills and back-pressures the push, deadlocking a single-thread
        // push-then-pull.
        let base_pts = window.iter().find_map(|s| s.pts).unwrap_or(Duration::ZERO);
        let window_owned: Vec<NalSample> = window.to_vec();
        let dsrc_push = dsrc.clone();
        let pusher = std::thread::spawn(move || {
            let mut last: Option<u64> = None;
            for s in &window_owned {
                match push_sample(&dsrc_push, s, base_pts, last, FALLBACK_INTERVAL_NS) {
                    Ok(w) => last = Some(w),
                    Err(e) => {
                        warn!("alert-clip decode push failed: {e}");
                        break;
                    }
                }
            }
            let _ = dsrc_push.end_of_stream();
        });

        // --- Encode pipeline: built lazily once we know the native dims. ---
        let mut enc: Option<(gst::Pipeline, AppSrc)> = None;
        let mut first_pts_ns: Option<u64> = None;
        let mut last_pts_ns: u64 = 0;

        loop {
            let sample = match dsink.pull_sample() {
                Ok(s) => s,
                Err(_) => break, // EOS (or error) ends the drain.
            };
            let buffer = sample
                .buffer()
                .ok_or_else(|| AlertClipError::Gst("decoded sample without buffer".into()))?;
            let caps = sample
                .caps()
                .ok_or_else(|| AlertClipError::Gst("decoded sample without caps".into()))?;
            let info = VideoInfo::from_caps(caps)
                .map_err(|e| AlertClipError::Gst(format!("VideoInfo::from_caps: {e}")))?;
            if info.format() != VideoFormat::Rgb {
                return Err(AlertClipError::Gst(format!(
                    "decode produced {:?}, expected RGB",
                    info.format()
                )));
            }
            let native_w = info.width();
            let native_h = info.height();
            let frame_ref = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                .map_err(|_| AlertClipError::Gst("map decoded RGB frame".into()))?;
            let plane = frame_ref
                .plane_data(0)
                .map_err(|_| AlertClipError::Gst("decoded plane_data".into()))?;
            let stride = frame_ref.plane_stride().first().copied().unwrap_or(0) as usize;
            let (w, h) = (native_w as usize, native_h as usize);
            let row_bytes = w * 3;
            if stride < row_bytes || plane.len() < stride * h {
                return Err(AlertClipError::Gst("decoded RGB geometry mismatch".into()));
            }
            // Tight-pack into width*height*3 (drop row padding).
            let mut data = Vec::with_capacity(row_bytes * h);
            if stride == row_bytes {
                data.extend_from_slice(&plane[..row_bytes * h]);
            } else {
                for y in 0..h {
                    let s = y * stride;
                    data.extend_from_slice(&plane[s..s + row_bytes]);
                }
            }

            // Input PTS was rebased to the window base, so the first
            // decoded frame anchors 0 for box lookup.
            let pts_ns = buffer.pts().map(|t| t.nseconds()).unwrap_or(last_pts_ns);
            if first_pts_ns.is_none() {
                first_pts_ns = Some(pts_ns);
            }
            last_pts_ns = pts_ns;

            // Burn the per-frame boxes in (hold-last by wall-clock).
            let rebased = Duration::from_nanos(pts_ns.saturating_sub(first_pts_ns.unwrap_or(0)));
            let wall = frame_wall_clock(window_start, rebased);
            if let Some(bf) = box_frames.iter().rev().find(|f| f.ts <= wall) {
                // Draw exactly ONE box — the highest-confidence detection
                // on this frame — so the clip shows a single clean box
                // (no tracker-fragment / coasting duplicates) that tracks
                // the alert's object, matching the console mock.
                if let Some(b) = bf
                    .boxes
                    .iter()
                    .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
                {
                    let nb = scale_box(b, bf.sup_w, bf.sup_h, native_w, native_h);
                    let (stroke, radius) = crate::overlay::box_metrics(native_w, native_h);
                    crate::overlay::draw_box_rgb24(
                        &mut data,
                        native_w,
                        native_h,
                        nb.x1.round() as i64,
                        nb.y1.round() as i64,
                        nb.x2.round() as i64,
                        nb.y2.round() as i64,
                        stroke,
                        radius,
                        crate::overlay::ALERT_RGB,
                    );
                    // Label chip anchored to the box top-left, matching
                    // the alert snapshot so the box + "person 0.96" read
                    // identically across snapshot and clip.
                    let chip = crate::overlay::label_text(&nb.label, Some(nb.confidence));
                    crate::overlay::draw_label_chip_rgb24(
                        &mut data,
                        native_w,
                        native_h,
                        nb.x1.round() as i64,
                        nb.y1.round() as i64,
                        &chip,
                        label_px(native_w),
                    );
                }
            }

            if enc.is_none() {
                let (w2, h2) = capped_dims(native_w, native_h, max_encode_width);
                let loc = out_partial.to_string_lossy().replace('"', "");
                let enc_desc = format!(
                    "appsrc name=esrc is-live=false format=time do-timestamp=false block=true \
                     ! videoconvert ! videoscale \
                     ! capsfilter name=enccaps \
                     ! {encoder} \
                     ! h264parse \
                     ! mp4mux faststart=true \
                     ! filesink location=\"{loc}\" sync=false"
                );
                let ep = gst::parse::launch(&enc_desc)
                    .map_err(|e| AlertClipError::Gst(format!("encode launch: {e}")))?
                    .downcast::<gst::Pipeline>()
                    .map_err(|_| AlertClipError::Gst("encode graph is not a pipeline".into()))?;
                // Pin the encoder input to FULL-RANGE (pc) BT.709 I420 so
                // the re-encode preserves the source camera's colorimetry.
                // Without this, videoconvert defaults to LIMITED-range (tv)
                // BT.601 / SMPTE170M — squeezing 0..255 into 16..235 and
                // swapping the matrix, which crushes levels + shifts colour
                // (the washed-out alert clip vs the full-range source).
                // The overlay RGB is full-range 0..255, so a full-range
                // BT.709 output round-trips it losslessly.
                let colorimetry = gstreamer_video::VideoColorimetry::new(
                    gstreamer_video::VideoColorRange::Range0_255,
                    gstreamer_video::VideoColorMatrix::Bt709,
                    gstreamer_video::VideoTransferFunction::Bt709,
                    gstreamer_video::VideoColorPrimaries::Bt709,
                );
                let enc_caps = gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .field("width", w2 as i32)
                    .field("height", h2 as i32)
                    .field("colorimetry", colorimetry.to_string())
                    .build();
                ep.by_name("enccaps")
                    .ok_or_else(|| AlertClipError::Gst("capsfilter enccaps missing".into()))?
                    .set_property("caps", &enc_caps);
                let esrc = by_name_appsrc(&ep, "esrc")?;
                esrc.set_caps(Some(
                    &gst::Caps::builder("video/x-raw")
                        .field("format", "RGB")
                        .field("width", native_w as i32)
                        .field("height", native_h as i32)
                        .field("framerate", gst::Fraction::new(30, 1))
                        .build(),
                ));
                ep.set_state(gst::State::Playing)
                    .map_err(|e| AlertClipError::Gst(format!("encode set Playing: {e}")))?;
                enc = Some((ep, esrc));
            }
            let (_, esrc) = enc.as_ref().expect("encode pipeline built above");
            // GStreamer's RGB stride is rounded UP to a multiple of 4
            // bytes; our overlay buffer is tightly packed (`width*3` per
            // row). For a native width whose `width*3` isn't 4-aligned the
            // two disagree and every row reads a byte or two into the next
            // — shearing the frame, rotating the R/G/B channels
            // (rainbow/blue discoloration) and ghosting the burned box +
            // label. Re-pack into the aligned stride the encoder expects.
            let bytes = align_rgb_stride(data, row_bytes, h);
            let mut buf = gst::Buffer::from_mut_slice(bytes);
            if let Some(bref) = buf.get_mut() {
                bref.set_pts(gst::ClockTime::from_nseconds(pts_ns));
            }
            esrc.push_buffer(buf)
                .map_err(|e| AlertClipError::Gst(format!("encode push_buffer: {e:?}")))?;
        }

        let _ = pusher.join();
        let _ = dec_pipeline.set_state(gst::State::Null);

        let (ep, esrc) = enc.ok_or(AlertClipError::NoFrames)?;
        let _ = esrc.end_of_stream();
        let drain = drain_eos(&ep);
        let _ = ep.set_state(gst::State::Null);
        drain?;

        let size_bytes = std::fs::metadata(out_partial)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let duration_ms =
            i64::try_from(last_pts_ns.saturating_sub(first_pts_ns.unwrap_or(0)) / 1_000_000)
                .unwrap_or(0);
        Ok(AlertClipStats {
            duration_ms,
            size_bytes,
        })
    }
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
            label: "person".into(),
            confidence: 0.9,
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
            label: "person".into(),
            confidence: 0.9,
        };
        assert_eq!(scale_box(&b, 0, 360, 1920, 1080), b);
        assert_eq!(scale_box(&b, 640, 0, 1920, 1080), b);
    }

    #[test]
    fn dedupe_burn_boxes_collapses_overlaps_keeps_distinct() {
        let mk = |x1: f32, y1: f32, x2: f32, y2: f32, c: f32| BurnBox {
            x1,
            y1,
            x2,
            y2,
            label: "person".into(),
            confidence: c,
        };
        // Three heavily-overlapping ghosts of ONE object (the tracker
        // fragment "trail") plus one far-away distinct object.
        let mut boxes = vec![
            mk(100.0, 100.0, 200.0, 300.0, 0.31), // coasting ghost
            mk(104.0, 108.0, 205.0, 305.0, 0.80), // the real detection
            mk(96.0, 94.0, 196.0, 296.0, 0.32),   // coasting ghost
            mk(600.0, 100.0, 700.0, 300.0, 0.75), // a different person
        ];
        dedupe_burn_boxes(&mut boxes);
        // The overlapping trio collapses to one box; the distant object
        // survives → one box per physical object.
        assert_eq!(boxes.len(), 2, "one box per physical object");
        // The highest-confidence member of the cluster is the survivor.
        assert!(boxes.iter().any(|b| (b.confidence - 0.80).abs() < 1e-6));
        assert!(boxes.iter().any(|b| (b.confidence - 0.75).abs() < 1e-6));
        // No two survivors overlap beyond the dedupe threshold.
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(burn_box_iou(a, b) <= BURN_DEDUPE_IOU);
            }
        }
    }

    #[test]
    fn dedupe_burn_boxes_small_inputs_are_noops() {
        let mut empty: Vec<BurnBox> = vec![];
        dedupe_burn_boxes(&mut empty);
        assert!(empty.is_empty());

        let one = BurnBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
            label: "car".into(),
            confidence: 0.5,
        };
        let mut single = vec![one.clone()];
        dedupe_burn_boxes(&mut single);
        assert_eq!(single, vec![one]);
    }

    #[test]
    fn align_rgb_stride_pads_unaligned_and_noops_aligned() {
        // row_bytes 12 (mult of 4): returned unchanged.
        let aligned = vec![1u8; 12 * 2];
        assert_eq!(
            align_rgb_stride(aligned.clone(), 12, 2),
            aligned,
            "already-aligned stride is a no-op"
        );

        // row_bytes 9 (NOT a mult of 4) → padded to 12 bytes/row so each
        // row lands on the encoder's expected 4-aligned offset.
        let tight: Vec<u8> = (0..18).collect(); // row0: 0..9, row1: 9..18
        let out = align_rgb_stride(tight, 9, 2);
        assert_eq!(out.len(), 12 * 2, "padded to 12-byte rows");
        assert_eq!(&out[0..9], &[0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&out[9..12], &[0, 0, 0], "row 0 zero-padded");
        assert_eq!(&out[12..21], &[9, 10, 11, 12, 13, 14, 15, 16, 17]);
        assert_eq!(&out[21..24], &[0, 0, 0], "row 1 zero-padded");
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
                    label: "person".into(),
                    confidence: 0.9,
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
}

// End-to-end encode test. Runs wherever GStreamer + an H.264 encoder are
// present (the Linux CI `gstreamer` job, and local dev with `x264enc`).
#[cfg(all(test, feature = "gstreamer"))]
mod gst_tests {
    use std::time::Duration as StdDuration;

    use chrono::Utc;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app::AppSink;
    use nexus_types::CodecKind;

    use super::{encode_alert_clip, BoxFrame, BurnBox};
    use crate::preroll::NalSample;

    /// Generate a short synthetic H.264 byte-stream (AU-aligned, SPS/PPS
    /// before every IDR) as a NAL sample vector — stand-in for what the
    /// pre-roll ring + live tap hand the builder.
    fn gen_h264_nals(num: i32, w: i32, h: i32) -> Vec<NalSample> {
        gst::init().unwrap();
        let desc = format!(
            "videotestsrc num-buffers={num} is-live=false \
             ! video/x-raw,width={w},height={h},framerate=30/1 \
             ! x264enc key-int-max=15 tune=zerolatency \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=out sync=false"
        );
        let pipeline = gst::parse::launch(&desc)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let buffer = sample.buffer().unwrap();
            let map = buffer.map_readable().unwrap();
            let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let pts = buffer.pts().map(|t| StdDuration::from_nanos(t.nseconds()));
            out.push(NalSample {
                pts,
                dts: pts,
                is_keyframe,
                data: map.to_vec(),
            });
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out
    }

    /// Decode `path` to EOS to prove the muxed MP4 is well-formed.
    fn decode_ok(path: &std::path::Path) -> bool {
        let loc = path.to_string_lossy().replace('"', "");
        let desc = format!(
            "filesrc location=\"{loc}\" ! qtdemux ! h264parse ! avdec_h264 ! fakesink sync=false"
        );
        let Ok(el) = gst::parse::launch(&desc) else {
            return false;
        };
        let pipeline = el.downcast::<gst::Pipeline>().unwrap();
        if pipeline.set_state(gst::State::Playing).is_err() {
            return false;
        }
        let bus = pipeline.bus().unwrap();
        let mut ok = false;
        let deadline = std::time::Instant::now() + StdDuration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match bus.timed_pop(Some(gst::ClockTime::from_seconds(1))) {
                Some(msg) => match msg.view() {
                    gst::MessageView::Eos(..) => {
                        ok = true;
                        break;
                    }
                    gst::MessageView::Error(..) => break,
                    _ => {}
                },
                None => break,
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        ok
    }

    #[test]
    fn encode_alert_clip_produces_playable_mp4_with_boxes() {
        let nals = gen_h264_nals(20, 320, 240);
        assert!(!nals.is_empty(), "generator produced no NALs");
        assert!(nals[0].is_keyframe, "window must start on a keyframe");

        let start = Utc::now();
        // One box covering the object region for the whole clip.
        let boxes = vec![BoxFrame {
            ts: start,
            boxes: vec![BurnBox {
                x1: 40.0,
                y1: 40.0,
                x2: 220.0,
                y2: 190.0,
                label: "person".into(),
                confidence: 0.94,
            }],
            sup_w: 320,
            sup_h: 240,
        }];

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("alert.partial.mp4");
        let stats = encode_alert_clip(&nals, CodecKind::H264, &boxes, start, &out, 0)
            .expect("encode_alert_clip should succeed with x264enc present");

        assert!(out.exists(), "output MP4 must exist");
        assert!(stats.size_bytes > 0, "MP4 must be non-empty");
        // ISO-BMFF: bytes 4..8 are the 'ftyp' box type.
        let bytes = std::fs::read(&out).unwrap();
        assert!(
            bytes.len() > 12 && &bytes[4..8] == b"ftyp",
            "not an MP4 container"
        );
        assert!(decode_ok(&out), "muxed MP4 must decode to EOS cleanly");
    }

    #[test]
    fn encode_with_downscale_cap_still_valid() {
        let nals = gen_h264_nals(16, 640, 360);
        let start = Utc::now();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("alert.partial.mp4");
        // Cap encode width to 320 — exercises the videoscale path.
        let stats = encode_alert_clip(&nals, CodecKind::H264, &[], start, &out, 320)
            .expect("encode with downscale cap should succeed");
        assert!(stats.size_bytes > 0);
        assert!(decode_ok(&out));
    }
}
