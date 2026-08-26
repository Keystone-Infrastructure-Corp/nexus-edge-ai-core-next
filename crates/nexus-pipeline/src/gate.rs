//! Cheap motion gate — the low-bitrate (LBR) rate limiter.
//!
//! The gate is the cheapest layer that can drop work, and it is the single
//! place that decides a camera's *analysis* frame rate. Everything
//! downstream of it — detector, tracker, rules — runs at whatever rate this
//! gate passes. The `LatestFrameCache` frame, and therefore the cloud
//! live-view wall, does **not**: a tap upstream of the gate publishes every
//! decoded frame, so the wall stays current even when inference is starved
//! (BUG-136). Only the cache's *objects* run at the gate's rate.
//!
//! Two hard rate bounds bracket the motion decision:
//!
//! * [`BASELINE_GAP_MS`] — the floor. Even on a perfectly static scene the
//!   gate passes a frame this often, so the tracker's TTLs keep refreshing
//!   and a wall tile's boxes keep updating.
//! * [`MOTION_GAP_MS`] — the ceiling. Once motion is present the gate still
//!   refuses to pass faster than this. That cap is the whole point of LBR:
//!   a 16-tile wall cannot show 15 fps × 16 streams, and inference on frames
//!   nobody will ever see is pure waste.
//!
//! Both bounds are **time-based**, derived from `frame.captured_at`. An
//! earlier revision counted frames instead (`keyframe_every: 30`), which made
//! the baseline silently depend on the source rate — at the default 15 fps
//! ingest cap that yielded a 0.5 fps floor, 4× below the intended 2 fps, and
//! it was the direct cause of the live-view wall appearing to replay one
//! frame for seconds at a time on quiet cameras.
//!
//! Between the two bounds the motion test decides: downsample the Y plane
//! (or RGB→Y) by 8×, count per-pixel absolute deltas against the last
//! *passed* frame, and allow when the changed-pixel fraction clears a
//! threshold. Roughly 0.3 ms per 1080p frame on a recent CPU — and it is
//! skipped entirely for frames rejected by the ceiling.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use nexus_types::{Frame, PixelFormat};

/// Baseline floor: pass at least one frame every 500 ms (2 fps) regardless
/// of motion.
pub const BASELINE_GAP_MS: i64 = 500;

/// Motion ceiling: never pass more than one frame per 125 ms (8 fps), no
/// matter how much is moving.
pub const MOTION_GAP_MS: i64 = 125;

pub struct MotionGate {
    prev_y: Mutex<Option<Vec<u8>>>,
    delta_threshold: u8,
    pixel_pct_threshold: f32,
    baseline_gap_ms: i64,
    motion_gap_ms: i64,
    last_pass: Mutex<Option<DateTime<Utc>>>,
}

impl MotionGate {
    pub fn new() -> Self {
        Self {
            prev_y: Mutex::new(None),
            delta_threshold: 16,
            pixel_pct_threshold: 0.005,
            baseline_gap_ms: BASELINE_GAP_MS,
            motion_gap_ms: MOTION_GAP_MS,
            last_pass: Mutex::new(None),
        }
    }

    pub fn allow(&self, frame: &Frame) -> bool {
        let now = frame.captured_at;
        let gap_ms = match *self.last_pass.lock().unwrap() {
            Some(prev) => (now - prev).num_milliseconds(),
            None => i64::MAX,
        };

        // Source clock went backwards (camera reconnect, stream restart).
        // Treat it as a fresh start rather than stalling the camera until
        // the clock catches back up to the old high-water mark.
        if gap_ms < 0 {
            self.record_pass(frame, now);
            return true;
        }

        // Ceiling. The cheapest possible rejection: no downsample, no delta
        // scan, no lock on `prev_y`.
        if gap_ms < self.motion_gap_ms {
            return false;
        }

        // Floor. The scene may be perfectly static, but the wall still needs
        // a frame and the tracker still needs a tick.
        if gap_ms >= self.baseline_gap_ms {
            self.record_pass(frame, now);
            return true;
        }

        // Between the bounds: motion decides. Note the comparison is against
        // the last *passed* frame, not the last frame seen — that is the
        // question the wall actually asks ("has the picture changed since
        // what I'm showing?"), and it is the only baseline still available
        // now that ceiling-rejected frames are never downsampled.
        let y = downsample_y(frame);
        let moved = match self.prev_y.lock().unwrap().as_ref() {
            Some(prev_y) if prev_y.len() == y.len() => {
                let mut changed = 0usize;
                for (a, b) in prev_y.iter().zip(y.iter()) {
                    if a.abs_diff(*b) > self.delta_threshold {
                        changed += 1;
                    }
                }
                (changed as f32 / y.len() as f32) >= self.pixel_pct_threshold
            }
            // No baseline yet, or the frame geometry changed under us.
            _ => true,
        };

        if moved {
            *self.prev_y.lock().unwrap() = Some(y);
            *self.last_pass.lock().unwrap() = Some(now);
        }
        moved
    }

    fn record_pass(&self, frame: &Frame, now: DateTime<Utc>) {
        *self.prev_y.lock().unwrap() = Some(downsample_y(frame));
        *self.last_pass.lock().unwrap() = Some(now);
    }
}

impl Default for MotionGate {
    fn default() -> Self {
        Self::new()
    }
}

/// 8× downsample to a Y-only buffer. Cheap nearest-neighbour pick.
fn downsample_y(frame: &Frame) -> Vec<u8> {
    let scale = 8u32;
    let dw = (frame.width / scale).max(1) as usize;
    let dh = (frame.height / scale).max(1) as usize;
    let stride = frame.stride();
    let mut out = Vec::with_capacity(dw * dh);
    let data = frame.data.as_ref();

    match frame.format {
        PixelFormat::Nv12 | PixelFormat::I420 => {
            for j in 0..dh {
                let sy = (j * scale as usize).min(frame.height as usize - 1);
                for i in 0..dw {
                    let sx = (i * scale as usize).min(frame.width as usize - 1);
                    out.push(data[sy * stride + sx]);
                }
            }
        }
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => {
            for j in 0..dh {
                let sy = (j * scale as usize).min(frame.height as usize - 1);
                for i in 0..dw {
                    let sx = (i * scale as usize).min(frame.width as usize - 1);
                    let off = sy * stride + sx * 3;
                    if off + 2 < data.len() {
                        let r = data[off] as u32;
                        let g = data[off + 1] as u32;
                        let b = data[off + 2] as u32;
                        // Cheap luma approximation.
                        out.push(((r * 30 + g * 59 + b * 11) / 100) as u8);
                    } else {
                        out.push(0);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The ingest default (`CameraIngest::max_fps` falls back to 15), i.e.
    /// the rate the gate actually sees in the field.
    const SOURCE_FPS: i64 = 15;
    const FRAME_INTERVAL_US: i64 = 1_000_000 / SOURCE_FPS;

    fn frame_at(i: i64, luma: u8) -> Frame {
        let base = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        Frame {
            camera_id: 1,
            frame_id: i as u64,
            captured_at: base + chrono::Duration::microseconds(i * FRAME_INTERVAL_US),
            width: 64,
            height: 64,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![luma; 64 * 64 * 3]),
            trace_id: String::new(),
        }
    }

    /// Static scene: every frame is byte-identical.
    fn static_frame(i: i64) -> Frame {
        frame_at(i, 128)
    }

    /// Busy scene. The luma step between any two frames within four of each
    /// other is at least 47 in circular distance, comfortably clear of the
    /// gate's delta_threshold of 16, so *every* sampled pixel counts as
    /// changed regardless of which pair the gate happens to compare.
    fn moving_frame(i: i64) -> Frame {
        frame_at(i, (i * 101 % 256) as u8)
    }

    fn passes(gate: &MotionGate, frames: impl Iterator<Item = Frame>) -> usize {
        frames.filter(|f| gate.allow(f)).count()
    }

    #[test]
    fn baseline_floor_passes_a_static_scene_at_about_two_fps() {
        let gate = MotionGate::new();
        // 10 s of a perfectly motionless camera at the 15 fps ingest cap.
        let n = passes(&gate, (0..SOURCE_FPS * 10).map(static_frame));

        // The floor is 500 ms, but passes can only land on the 66.7 ms
        // source grid, so the realised gap is 8 frames = 533 ms = 1.875 fps.
        assert!(
            (18..=20).contains(&n),
            "static scene should pass ~19 frames in 10 s (~1.9 fps), got {n}"
        );
    }

    /// Regression lock on the defect this replaced: the gate used to force a
    /// pass every 30th *frame*, which at the 15 fps ingest default is 0.5 fps
    /// — 4× under the intended 2 fps LBR baseline. Downstream that starved
    /// `LatestFrameCache`, so the cloud video wall re-served one frame for
    /// two seconds at a time and looked like it was replaying.
    #[test]
    fn baseline_floor_is_not_the_old_frame_counted_half_fps() {
        let gate = MotionGate::new();
        let n = passes(&gate, (0..SOURCE_FPS * 10).map(static_frame));

        let old_behaviour = (SOURCE_FPS * 10) / 30; // keyframe_every = 30
        assert_eq!(old_behaviour, 5, "old rule passed 5 frames in 10 s");
        assert!(
            n >= old_behaviour as usize * 3,
            "new floor must be several times the old 0.5 fps, got {n}"
        );
    }

    #[test]
    fn motion_ceiling_caps_a_busy_scene_below_the_source_rate() {
        let gate = MotionGate::new();
        // 10 s of a camera where every single frame differs.
        let n = passes(&gate, (0..SOURCE_FPS * 10).map(moving_frame));

        // Ceiling is 125 ms; on the 66.7 ms grid that realises as every
        // 2nd frame = 7.5 fps, i.e. 75 of the 150 frames.
        assert!(
            (74..=76).contains(&n),
            "busy scene should be capped near 7.5 fps (75 frames), got {n}"
        );
        assert!(
            n < (SOURCE_FPS * 10) as usize,
            "the ceiling must actually drop frames"
        );
    }

    #[test]
    fn a_backwards_source_clock_does_not_stall_the_camera() {
        let gate = MotionGate::new();
        assert!(gate.allow(&static_frame(1000)), "first frame always passes");

        // Camera reconnects and its clock restarts well behind the old
        // high-water mark. Without the negative-gap escape the gate would
        // reject everything until the clock climbed back past it.
        assert!(
            gate.allow(&static_frame(0)),
            "a backwards clock must re-baseline, not block"
        );
        // ...and the new baseline is honoured from there.
        assert!(!gate.allow(&static_frame(1)), "ceiling applies after reset");
    }
}
