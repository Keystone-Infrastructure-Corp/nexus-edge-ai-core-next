//! M_PERF_CROWD Phase E3 — `CrowdHysteresis`.
//!
//! Sibling of [`crate::skip_policy::DetectorSkipPolicy`]. Tracks the
//! same per-camera EMA of `tracked.len()` but instead of dropping
//! frames it decides *which detector* the supervisor should call —
//! the camera's normal (high-res) detector or a pre-built low-res
//! variant.
//!
//! The hysteresis is symmetric: the EMA must sit at or above the
//! threshold continuously for `sustained_secs` before the state flips
//! to "downscaled", and below the threshold continuously for the same
//! window before it flips back. This avoids per-frame thrashing when
//! the EMA brushes the threshold.
//!
//! Knobs live on [`nexus_config::CameraBehavior`] as
//! `detector_downscale_crowded_threshold`,
//! `detector_downscale_sustained_secs`,
//! `detector_downscale_to_width`, and `detector_downscale_to_height`.
//! All four default `None` → policy disabled, supervisor uses the
//! high-res detector for every frame.
//!
//! Note: this module owns only the *decision*. Pre-building the
//! low-res inference layer is the router's job
//! ([`nexus_inference::InferenceRouter::detector_for_camera_low_res`]);
//! resolving the active detector per frame is the supervisor's job.

use std::time::{Duration, Instant};

const EMA_ALPHA: f64 = 0.1;

pub struct CrowdHysteresis {
    enabled: bool,
    crowded_threshold: f64,
    sustained: Duration,
    ema: f64,
    crowded_since: Option<Instant>,
    clear_since: Option<Instant>,
    downscaled: bool,
}

impl CrowdHysteresis {
    /// Build from per-camera knobs. Both `crowded_threshold` and
    /// `sustained_secs` must be `Some(_)` (and the size knobs must be
    /// set at the caller) for the policy to be enabled. A
    /// `sustained_secs` of `0` is accepted and means "flip on the
    /// first crossing", but the typical value is 60.
    pub fn new(crowded_threshold: Option<u32>, sustained_secs: Option<u32>) -> Self {
        let (enabled, threshold, sustained) = match (crowded_threshold, sustained_secs) {
            (Some(t), Some(s)) => (true, t as f64, Duration::from_secs(s as u64)),
            _ => (false, 0.0, Duration::ZERO),
        };
        Self {
            enabled,
            crowded_threshold: threshold,
            sustained,
            ema: 0.0,
            crowded_since: None,
            clear_since: None,
            downscaled: false,
        }
    }

    /// Feed the new tracked-object count and current monotonic time.
    /// Returns the desired downscale state for the *next* detector
    /// call. No-op (always returns `false`) when the policy is
    /// disabled.
    pub fn observe(&mut self, tracked_len: usize, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        self.ema = EMA_ALPHA * (tracked_len as f64) + (1.0 - EMA_ALPHA) * self.ema;
        if self.ema >= self.crowded_threshold {
            self.clear_since = None;
            let since = *self.crowded_since.get_or_insert(now);
            if !self.downscaled && now.saturating_duration_since(since) >= self.sustained {
                self.downscaled = true;
            }
        } else {
            self.crowded_since = None;
            let since = *self.clear_since.get_or_insert(now);
            if self.downscaled && now.saturating_duration_since(since) >= self.sustained {
                self.downscaled = false;
            }
        }
        self.downscaled
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
        let mut h = CrowdHysteresis::new(None, Some(10));
        let start = Instant::now();
        for i in 0..200 {
            assert!(!h.observe(100, start + Duration::from_secs(i)));
        }
        let mut h = CrowdHysteresis::new(Some(5), None);
        for i in 0..200 {
            assert!(!h.observe(100, start + Duration::from_secs(i)));
        }
    }

    #[test]
    fn ema_must_sit_above_threshold_before_flip() {
        let mut h = CrowdHysteresis::new(Some(20), Some(10));
        let start = Instant::now();
        // The first crowd sample alone is not enough: EMA jumps from 0
        // to only 10.0 after one update (α=0.1), still below threshold.
        assert!(!h.observe(100, start));
        assert!(!h.observe(100, start + Duration::from_millis(100)));
        // After many seconds of sustained crowd the EMA is well above
        // threshold AND the sustained window has elapsed → must flip.
        let mut flipped = false;
        for i in 1..=200 {
            if h.observe(100, start + Duration::from_secs(i)) {
                flipped = true;
                break;
            }
        }
        assert!(flipped, "hysteresis never flipped under sustained crowd");
        assert!(h.ema() >= 20.0, "ema={}", h.ema());
    }

    #[test]
    fn brief_crowd_spike_does_not_flip() {
        let mut h = CrowdHysteresis::new(Some(20), Some(10));
        let start = Instant::now();
        // Spike for 5s then drop back to 0. EMA briefly rises but
        // never sustains; should never downscale.
        for i in 0..5 {
            h.observe(100, start + Duration::from_secs(i));
        }
        for i in 5..200 {
            assert!(
                !h.observe(0, start + Duration::from_secs(i)),
                "downscaled on a transient spike"
            );
        }
    }

    #[test]
    fn flips_back_when_crowd_clears_sustained() {
        let mut h = CrowdHysteresis::new(Some(20), Some(10));
        let start = Instant::now();
        // Drive into downscale mode.
        for i in 0..120 {
            h.observe(100, start + Duration::from_secs(i));
        }
        let mut last = false;
        for i in 0..30 {
            last = h.observe(100, start + Duration::from_secs(120 + i));
        }
        assert!(last, "expected downscaled state by t=150s");

        // Crowd clears. EMA decays; once below threshold for sustained
        // window the state flips back.
        let mut still_down = true;
        let base = 150u64;
        for i in 0..200 {
            still_down = h.observe(0, start + Duration::from_secs(base + i));
            if !still_down {
                break;
            }
        }
        assert!(!still_down, "expected re-upscale once crowd cleared");
    }

    #[test]
    fn idempotent_once_downscaled() {
        // Once downscaled, observing more crowded frames keeps the
        // state at `true` (does not re-trigger or oscillate).
        let mut h = CrowdHysteresis::new(Some(20), Some(10));
        let start = Instant::now();
        for i in 0..200 {
            h.observe(100, start + Duration::from_secs(i));
        }
        let s1 = h.observe(100, start + Duration::from_secs(201));
        let s2 = h.observe(100, start + Duration::from_secs(202));
        let s3 = h.observe(100, start + Duration::from_secs(300));
        assert!(s1 && s2 && s3, "downscaled state should stay sticky");
    }
}
