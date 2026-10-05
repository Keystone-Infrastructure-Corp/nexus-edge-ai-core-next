//! Per-camera inference-rate estimate that turns the wall-clock tracker
//! thresholds (`*_secs`) into the frame counts the state machines count
//! in (#335). Frame-count defaults written against 30 fps were ~16x too
//! long on a Hailo-8 core running ~1.9 inferences/s/camera.

/// Rate assumed until two calls have been observed.
const DEFAULT_INTERVAL_SECS: f64 = 1.0 / 30.0;
/// Clamp on a single observed interval: 60 fps .. 0.2 fps. Keeps a stall
/// (camera reconnect, paused stream) from swinging the estimate.
const MIN_INTERVAL_SECS: f64 = 1.0 / 60.0;
const MAX_INTERVAL_SECS: f64 = 5.0;
/// EWMA weight of the newest interval.
const ALPHA: f64 = 0.1;

/// EWMA of the interval between consecutive inferences on one camera.
#[derive(Debug, Clone, Default)]
pub(crate) struct InferenceRate {
    last_secs: Option<f64>,
    interval_secs: Option<f64>,
}

impl InferenceRate {
    /// Record an inference at `t_secs` (any monotonic origin). Non-positive
    /// deltas (duplicate or out-of-order timestamps) are ignored.
    pub(crate) fn observe(&mut self, t_secs: f64) {
        if let Some(prev) = self.last_secs {
            let dt = t_secs - prev;
            if dt > 0.0 {
                let dt = dt.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
                self.interval_secs = Some(match self.interval_secs {
                    Some(ewma) => ALPHA * dt + (1.0 - ALPHA) * ewma,
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
    fn stall_is_clamped_and_duplicate_timestamps_ignored() {
        let mut r = at_fps(2.0, 10);
        r.observe(4.5); // duplicate of the last timestamp
        r.observe(3600.0); // an hour-long stall counts as 5 s
                           // EWMA 0.1 * 5 s + 0.9 * 0.5 s = 0.95 s per inference.
        assert_eq!(r.frames(10.0), 11);
    }
}
