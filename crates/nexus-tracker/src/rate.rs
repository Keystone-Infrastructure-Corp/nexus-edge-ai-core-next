//! Per-camera inference-rate estimate that turns the wall-clock tracker
//! thresholds (`*_secs`) into the frame counts the state machines count
//! in (#335). Frame-count defaults written against 30 fps were ~16x too
//! long on a Hailo-8 core running ~1.9 inferences/s/camera.

use std::time::Instant;

/// Rate assumed until two calls have been observed.
const DEFAULT_INTERVAL_SECS: f64 = 1.0 / 30.0;
/// Clamp on a single observed interval: 60 fps .. 0.2 fps.
const MIN_INTERVAL_SECS: f64 = 1.0 / 60.0;
const MAX_INTERVAL_SECS: f64 = 5.0;
/// EWMA weight of the newest interval.
const ALPHA: f64 = 0.1;
/// Each gap is winsorized to at most this multiple of the estimate before it
/// is blended in, so one stall (reconnect, paused stream) can stretch the
/// estimate by at most `1 + ALPHA * (WINSOR_FACTOR - 1)` = 1.3x per frame
/// instead of collapsing a 30 fps estimate ~16x. Unlike rejecting such gaps,
/// clamping still samples them: alternating short/long gaps (bursty delivery)
/// converge on the true mean, and a sustained rate drop grows the estimate
/// 1.3x per frame until the gaps fit under the clamp.
const WINSOR_FACTOR: f64 = 4.0;

/// EWMA of the interval between consecutive inferences on one camera.
#[derive(Debug, Clone, Default)]
pub(crate) struct InferenceRate {
    /// First stamp `observe_at` saw; the origin its seconds count from.
    origin: Option<Instant>,
    last_secs: Option<f64>,
    interval_secs: Option<f64>,
}

impl InferenceRate {
    /// Record an inference whose frame was captured at `captured_mono`
    /// (`Frame::captured_mono`). The interval is a duration between frames,
    /// so it reads the monotonic stamp: a wall-clock step moves it not at all.
    pub(crate) fn observe_at(&mut self, captured_mono: Instant) {
        let origin = *self.origin.get_or_insert(captured_mono);
        self.observe(
            captured_mono
                .saturating_duration_since(origin)
                .as_secs_f64(),
        );
    }

    /// Record an inference at `t_secs` (any monotonic origin). Non-positive
    /// deltas (duplicate or out-of-order timestamps) are ignored.
    pub(crate) fn observe(&mut self, t_secs: f64) {
        if let Some(prev) = self.last_secs {
            let dt = t_secs - prev;
            if dt > 0.0 {
                let dt = dt.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
                self.interval_secs = Some(match self.interval_secs {
                    Some(ewma) => ALPHA * dt.min(WINSOR_FACTOR * ewma) + (1.0 - ALPHA) * ewma,
                    None => dt,
                });
            }
        }
        self.last_secs = Some(t_secs);
    }

    /// Frames spanning `secs` at the current rate, rounded (at least 1).
    pub(crate) fn frames(&self, secs: f32) -> u32 {
        let interval = self.interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS);
        (secs.max(0.0) as f64 / interval).round().max(1.0) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_fps(fps: f64, n: u32) -> InferenceRate {
        let mut r = InferenceRate::default();
        for i in 0..n {
            r.observe(i as f64 / fps);
        }
        r
    }

    #[test]
    fn unmeasured_rate_assumes_30_fps() {
        assert_eq!(InferenceRate::default().frames(5.0), 150);
    }

    #[test]
    fn converts_seconds_at_the_measured_rate() {
        assert_eq!(at_fps(30.0, 10).frames(5.0), 150);
        assert_eq!(at_fps(1.9, 10).frames(5.0), 10);
    }

    #[test]
    fn duplicates_are_ignored_and_an_hour_gap_is_winsorized() {
        let mut r = at_fps(2.0, 10);
        r.observe(4.5); // duplicate of the last timestamp: ignored
        assert_eq!(r.frames(10.0), 20);
        // An hour-long gap clamps to 5 s, then winsorizes to 4 x 0.5 s = 2 s,
        // so the estimate moves to 0.1 * 2 + 0.9 * 0.5 = 0.65 s: frames(10)
        // drops to 15, never below 20 / 1.3 for a single gap.
        r.observe(3600.0);
        assert_eq!(r.frames(10.0), 15);
    }

    #[test]
    fn a_stall_barely_moves_a_30_fps_estimate() {
        let mut r = at_fps(30.0, 100);
        let t = 99.0 / 30.0 + 3.0; // one 3 s stall
        r.observe(t);
        // 3 s winsorizes to 4/30 s: 0.1 * 4/30 + 0.9/30 = 1.3/30 -> ~115.
        assert!(r.frames(5.0) >= 110, "got {}", r.frames(5.0));
        for i in 1..=30 {
            r.observe(t + i as f64 / 30.0);
        }
        assert!(r.frames(5.0) >= 145, "got {}", r.frames(5.0));
    }

    #[test]
    fn a_sustained_drop_to_hailo_rate_converges() {
        let mut r = at_fps(30.0, 100);
        let t0 = 99.0 / 30.0;
        let mut converged_at = None;
        for i in 1..=60u32 {
            r.observe(t0 + i as f64 / 1.9);
            match converged_at {
                None if r.frames(10.0) == 19 => converged_at = Some(i),
                None => {}
                Some(_) => assert_eq!(r.frames(10.0), 19, "slow frame {i}"),
            }
        }
        let n = converged_at.expect("never converged");
        assert!(n <= 40, "converged after {n} slow frames");
    }

    #[test]
    fn alternating_short_and_long_gaps_track_the_mean_interval() {
        // Bursty delivery: 1/60 s then 1 s, repeated. True mean ~0.508 s.
        let mut r = InferenceRate::default();
        let mut t = 0.0;
        r.observe(t);
        for i in 0..400 {
            t += if i % 2 == 0 { 1.0 / 60.0 } else { 1.0 };
            r.observe(t);
        }
        let mean = (1.0 / 60.0 + 1.0) / 2.0;
        let est = r.interval_secs.unwrap();
        assert!(
            (est - mean).abs() <= 0.3 * mean,
            "estimate {est} not within 30% of {mean}"
        );
    }
}
