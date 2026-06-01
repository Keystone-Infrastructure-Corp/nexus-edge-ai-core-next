//! M_PERF_CROWD Phase E1 — `DetectorSkipPolicy`.
//!
//! Co-located with [`crate::gate::MotionGate`] in the supervisor's
//! per-frame loop. Tracks per-camera EMA of `tracked.len()` and, when
//! the EMA exceeds an opt-in `crowded_threshold`, drops the detector
//! call on `(n - 1)` out of every `n` gate-allowed frames. The
//! supervisor still runs `tracker.update(empty)` on skip frames so
//! ByteTrack's `predict()` advances and existing tracks age normally.
//!
//! Knobs live on [`nexus_config::CameraBehavior`] as
//! `detector_skip_crowded_threshold` and `detector_skip_every_n_frames`.
//! Both default `None` → policy disabled, supervisor behaviour
//! unchanged.
//!
//! The policy is purely additive: when disabled, EMA isn't tracked and
//! `should_skip()` always returns `false`. When enabled, the EMA is
//! updated by `observe(tracked_len)` after each `tracker.update` call.

const EMA_ALPHA: f64 = 0.1;

pub struct DetectorSkipPolicy {
    enabled: bool,
    crowded_threshold: f64,
    skip_every_n_frames: u32,
    ema: f64,
    frame_counter: u32,
}

impl DetectorSkipPolicy {
    /// Build from per-camera knobs. Both `crowded_threshold` and
    /// `skip_every_n_frames` must be `Some(_)` to enable the policy.
    /// `skip_every_n_frames < 2` is coerced to "always run".
    pub fn new(crowded_threshold: Option<u32>, skip_every_n_frames: Option<u32>) -> Self {
        let (enabled, threshold, every_n) = match (crowded_threshold, skip_every_n_frames) {
            (Some(t), Some(n)) if n >= 2 => (true, t as f64, n),
            _ => (false, 0.0, 1),
        };
        Self {
            enabled,
            crowded_threshold: threshold,
            skip_every_n_frames: every_n,
            ema: 0.0,
            frame_counter: 0,
        }
    }

    /// Returns `true` when the detector should be skipped for the
    /// current frame. Increments an internal counter on every call.
    /// Caller must invoke exactly once per gate-allowed frame, before
    /// the detector decision.
    pub fn should_skip(&mut self) -> bool {
        if !self.enabled {
            return false;
        }
        self.frame_counter = self.frame_counter.wrapping_add(1);
        // Only skip when crowd EMA is at or above the threshold.
        if self.ema < self.crowded_threshold {
            return false;
        }
        // With `n = 2`: counter=1 → skip, counter=2 → run,
        // counter=3 → skip, counter=4 → run … i.e. detector runs every
        // n-th frame, skip otherwise.
        !self.frame_counter.is_multiple_of(self.skip_every_n_frames)
    }

    /// Updates the per-camera EMA after `tracker.update` has produced
    /// the new tracked-object slice. No-op when the policy is disabled.
    pub fn observe(&mut self, tracked_len: usize) {
        if !self.enabled {
            return;
        }
        self.ema = EMA_ALPHA * (tracked_len as f64) + (1.0 - EMA_ALPHA) * self.ema;
    }

    #[cfg(test)]
    pub(crate) fn ema(&self) -> f64 {
        self.ema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_either_knob_is_none() {
        let mut p = DetectorSkipPolicy::new(None, Some(2));
        for _ in 0..50 {
            p.observe(100);
            assert!(!p.should_skip());
        }
        let mut p = DetectorSkipPolicy::new(Some(5), None);
        for _ in 0..50 {
            p.observe(100);
            assert!(!p.should_skip());
        }
    }

    #[test]
    fn n_less_than_two_is_coerced_to_always_run() {
        let mut p = DetectorSkipPolicy::new(Some(5), Some(1));
        for _ in 0..50 {
            p.observe(100);
            assert!(!p.should_skip());
        }
    }

    #[test]
    fn no_skip_until_ema_clears_threshold() {
        let mut p = DetectorSkipPolicy::new(Some(20), Some(2));
        // Tracked = 0 sustained → EMA stays at 0 → never skip.
        for _ in 0..50 {
            assert!(!p.should_skip());
            p.observe(0);
        }
    }

    #[test]
    fn under_crowd_runs_every_nth_frame() {
        // Push EMA well past threshold first.
        let mut p = DetectorSkipPolicy::new(Some(20), Some(2));
        for _ in 0..200 {
            p.observe(100);
        }
        assert!(
            p.ema() >= 20.0,
            "EMA should have cleared threshold: {}",
            p.ema()
        );

        // From a fresh counter, with N=2 the pattern is skip, run, skip, run, ...
        let mut skips = 0;
        let mut runs = 0;
        for _ in 0..100 {
            if p.should_skip() {
                skips += 1;
            } else {
                runs += 1;
            }
            p.observe(100);
        }
        // Detector runs every 2nd frame → ~half skipped, half ran.
        assert_eq!(runs + skips, 100);
        assert!((49..=51).contains(&runs), "runs={runs}");
        assert!((49..=51).contains(&skips), "skips={skips}");
    }

    #[test]
    fn ema_decays_when_crowd_clears() {
        let mut p = DetectorSkipPolicy::new(Some(20), Some(2));
        for _ in 0..200 {
            p.observe(100);
        }
        assert!(p.ema() >= 20.0);

        // Crowd clears: tracked.len() drops to 0 sustained.
        for _ in 0..200 {
            p.observe(0);
        }
        // EMA must decay back well below threshold so skipping stops.
        assert!(p.ema() < 1.0, "EMA should have decayed: {}", p.ema());
        for _ in 0..20 {
            assert!(!p.should_skip());
            p.observe(0);
        }
    }

    #[test]
    fn n_three_runs_one_in_three() {
        let mut p = DetectorSkipPolicy::new(Some(5), Some(3));
        for _ in 0..200 {
            p.observe(50);
        }
        let mut runs = 0;
        for _ in 0..300 {
            if !p.should_skip() {
                runs += 1;
            }
            p.observe(50);
        }
        // 300 frames, run on every 3rd → exactly 100 runs.
        assert_eq!(runs, 100);
    }
}
