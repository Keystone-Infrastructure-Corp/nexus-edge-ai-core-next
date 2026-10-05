//! Per-camera inference-rate estimate that turns the wall-clock tracker
//! thresholds (`*_secs`) into the frame counts the state machines count
//! in (#335). Frame-count defaults written against 30 fps were ~16x too
//! long on a Hailo-8 core running ~1.9 inferences/s/camera.

/// Rate assumed until two calls have been observed.
const DEFAULT_INTERVAL_SECS: f64 = 1.0 / 30.0;
/// Clamp on a single observed interval: 60 fps .. 0.2 fps.
const MIN_INTERVAL_SECS: f64 = 1.0 / 60.0;
const MAX_INTERVAL_SECS: f64 = 5.0;
/// EWMA weight of the newest interval.
const ALPHA: f64 = 0.1;
/// A gap longer than this multiple of the estimate is a discontinuity (stall,
/// reconnect, paused stream), not a sample: blending even one 5 s gap into a
/// 30 fps estimate would cut every `*_secs` threshold ~16x for about a second,
/// long enough to promote a briefly stopped vehicle to a static anchor. 4x
/// still blends in ordinary jitter and up to three dropped frames in a row.
const DISCONTINUITY_FACTOR: f64 = 4.0;
/// This many consecutive discontinuities mean the rate genuinely dropped
/// (e.g. 30 -> 1.9 fps), so the estimate is re-seeded from the latest gap.
const RESEED_AFTER: u32 = 3;

/// EWMA of the interval between consecutive inferences on one camera.
#[derive(Debug, Clone, Default)]
pub(crate) struct InferenceRate {
    last_secs: Option<f64>,
    interval_secs: Option<f64>,
    /// Consecutive gaps rejected as discontinuities.
    rejected: u32,
}

impl InferenceRate {
    /// Record an inference at `t_secs` (any monotonic origin). Non-positive
    /// deltas (duplicate or out-of-order timestamps) are ignored.
    pub(crate) fn observe(&mut self, t_secs: f64) {
        if let Some(prev) = self.last_secs {
            let dt = t_secs - prev;
            if dt > 0.0 {
                let dt = dt.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
                self.interval_secs = match self.interval_secs {
                    Some(ewma) if dt > DISCONTINUITY_FACTOR * ewma => {
                        self.rejected += 1;
                        if self.rejected < RESEED_AFTER {
                            Some(ewma)
                        } else {
                            self.rejected = 0;
                            Some(dt)
                        }
                    }
                    Some(ewma) => {
                        self.rejected = 0;
                        Some(ALPHA * dt + (1.0 - ALPHA) * ewma)
                    }
                    None => Some(dt),
                };
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
    fn stall_is_skipped_and_duplicate_timestamps_ignored() {
        let mut r = at_fps(2.0, 10);
        r.observe(4.5); // duplicate of the last timestamp
        r.observe(3600.0); // an hour-long stall is a discontinuity, not a sample
        assert_eq!(r.frames(10.0), 20);
    }

    #[test]
    fn a_stall_does_not_collapse_a_30_fps_estimate() {
        let mut r = at_fps(30.0, 100);
        let t = 99.0 / 30.0 + 3.0; // one 3 s stall
        r.observe(t);
        assert!(r.frames(5.0) >= 140, "got {}", r.frames(5.0));
        // Normal frames after the stall keep the estimate at 30 fps.
        for i in 1..=30 {
            r.observe(t + i as f64 / 30.0);
            assert!(r.frames(5.0) >= 140, "frame {i}: got {}", r.frames(5.0));
        }
    }

    #[test]
    fn a_sustained_drop_to_hailo_rate_converges() {
        let mut r = at_fps(30.0, 100);
        let t0 = 99.0 / 30.0;
        for i in 1..=RESEED_AFTER {
            r.observe(t0 + i as f64 / 1.9);
        }
        assert_eq!(r.frames(10.0), 19);
        for i in RESEED_AFTER + 1..=RESEED_AFTER + 20 {
            r.observe(t0 + i as f64 / 1.9);
            assert_eq!(r.frames(10.0), 19);
        }
    }
}
