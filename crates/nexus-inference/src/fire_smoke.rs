//! SPEC-036 — Fire and smoke detection head: persistence + cascade logic.
//!
//! The model itself (a separate YOLO26 head, one ONNX artifact per ladder
//! rung, ONNX-only per ADR-065) is a model-cut deliverable. What lives
//! here is the **edge-local, cloud-independent** logic that turns the
//! head's per-frame region/texture confidence into a
//! [`FireSmokeSignal`] only after a persistence window is satisfied, and
//! the **cascade** that keeps the expensive flicker check off the frame
//! path until the cheap full-frame scan nominates a candidate region:
//!
//! * A fire signal is emitted only after the detection **persists ≥ 2 s**.
//! * A smoke signal is emitted only after the detection **persists ≥ 5 s
//!   and the region grows**; a static grey region that never expands is
//!   not smoke.
//! * The full-rate flicker path is **never** taken for a frame that
//!   produced no nomination.
//!
//! None of this calls the cloud; the whole file is pure timer/geometry
//! logic verified against synthetic frame sequences, independent of model
//! quality.

use std::time::Duration;

use nexus_types::{BBox, CameraId, FireSmokeKind, FireSmokeSignal};

/// The persistence windows (ADR / HARM_TAXONOMY §4). Fire needs 2 s;
/// smoke needs 5 s **and** a growing region.
#[derive(Debug, Clone)]
pub struct FireSmokeConfig {
    pub fire_persistence: Duration,
    pub smoke_persistence: Duration,
    /// The fraction by which a smoke region must grow over the window to
    /// count as smoke (region expansion is the discriminator against a
    /// static grey patch). 0.15 = 15 % area growth.
    pub smoke_min_growth: f32,
}

impl Default for FireSmokeConfig {
    fn default() -> Self {
        Self {
            fire_persistence: Duration::from_secs(2),
            smoke_persistence: Duration::from_secs(5),
            smoke_min_growth: 0.15,
        }
    }
}

/// One frame's output from the cheap full-frame nomination scan. `None`
/// region means "no candidate this frame" — the cascade's off state.
#[derive(Debug, Clone, Copy)]
pub struct Nomination {
    pub kind: FireSmokeKind,
    pub region: BBox,
    pub confidence: f32,
}

/// Tracks one camera's fire/smoke candidate across frames and emits a
/// [`FireSmokeSignal`] when the persistence (and, for smoke, growth)
/// window is met. Edge-triggered: a satisfied window emits once, then the
/// candidate must clear and re-form to emit again.
#[derive(Debug)]
pub struct FireSmokeHead {
    camera_id: CameraId,
    cfg: FireSmokeConfig,
    candidate: Option<Candidate>,
    /// Counts frames the full-rate flicker check ran, so a test can assert
    /// it is never entered without a nomination.
    flicker_checks: u64,
}

#[derive(Debug, Clone)]
struct Candidate {
    kind: FireSmokeKind,
    first_seen_ms: i64,
    first_area: f32,
    last_area: f32,
    last_region: BBox,
    peak_confidence: f32,
    emitted: bool,
}

impl FireSmokeHead {
    #[must_use]
    pub fn new(camera_id: CameraId, cfg: FireSmokeConfig) -> Self {
        Self {
            camera_id,
            cfg,
            candidate: None,
            flicker_checks: 0,
        }
    }

    /// Number of frames the full-rate flicker check has been run. The
    /// cascade only runs it when a nomination is present.
    #[must_use]
    pub fn flicker_checks(&self) -> u64 {
        self.flicker_checks
    }

    /// Advance by one frame. `nomination` is the cheap scan's result for
    /// this frame (`None` = nothing nominated). Returns a signal when the
    /// persistence window is satisfied this frame.
    ///
    /// The flicker check — the expensive 5–12 Hz path — runs **only** when
    /// a nomination exists; a `None` frame short-circuits before it, which
    /// is the cascade invariant.
    pub fn observe(
        &mut self,
        now_ms: i64,
        nomination: Option<Nomination>,
    ) -> Option<FireSmokeSignal> {
        let Some(nom) = nomination else {
            // Cascade off state: no nomination → no flicker check, drop the
            // candidate so a transient never accumulates persistence.
            self.candidate = None;
            return None;
        };

        // A nomination exists: this is the only path that runs the
        // full-rate flicker check.
        self.flicker_checks += 1;
        let area = nom.region.area();

        let cand = match self.candidate.as_mut() {
            Some(c) if c.kind == nom.kind => {
                c.last_area = area;
                c.last_region = nom.region;
                c.peak_confidence = c.peak_confidence.max(nom.confidence);
                c
            }
            _ => {
                self.candidate = Some(Candidate {
                    kind: nom.kind,
                    first_seen_ms: now_ms,
                    first_area: area,
                    last_area: area,
                    last_region: nom.region,
                    peak_confidence: nom.confidence,
                    emitted: false,
                });
                self.candidate.as_mut().unwrap()
            }
        };

        if cand.emitted {
            return None;
        }

        let held = Duration::from_millis((now_ms - cand.first_seen_ms).max(0) as u64);
        let window = match cand.kind {
            FireSmokeKind::Fire => self.cfg.fire_persistence,
            FireSmokeKind::Smoke => self.cfg.smoke_persistence,
        };
        if held < window {
            return None;
        }

        // Smoke additionally requires the region to have grown.
        if cand.kind == FireSmokeKind::Smoke {
            let grew = cand.first_area > 0.0
                && (cand.last_area - cand.first_area) / cand.first_area
                    >= self.cfg.smoke_min_growth;
            if !grew {
                return None;
            }
        }

        cand.emitted = true;
        Some(FireSmokeSignal {
            camera_id: self.camera_id,
            kind: cand.kind,
            region: cand.last_region,
            confidence: cand.peak_confidence,
            first_seen_unix_ms: cand.first_seen_ms,
            emitted_unix_ms: now_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: f32, y: f32, w: f32, h: f32) -> BBox {
        BBox {
            x1: x,
            y1: y,
            x2: x + w,
            y2: y + h,
        }
    }

    fn fire_nom(area_side: f32) -> Nomination {
        Nomination {
            kind: FireSmokeKind::Fire,
            region: region(0.0, 0.0, area_side, area_side),
            confidence: 0.8,
        }
    }

    #[test]
    fn fire_emits_only_after_two_seconds_of_persistence() {
        let mut h = FireSmokeHead::new(1, FireSmokeConfig::default());
        assert!(h.observe(0, Some(fire_nom(0.2))).is_none(), "t=0 no emit");
        assert!(
            h.observe(1000, Some(fire_nom(0.2))).is_none(),
            "t=1s no emit"
        );
        let sig = h.observe(2000, Some(fire_nom(0.2))).expect("t=2s emits");
        assert_eq!(sig.kind, FireSmokeKind::Fire);
        // Held past the window: edge-triggered, no re-emit.
        assert!(
            h.observe(3000, Some(fire_nom(0.2))).is_none(),
            "no duplicate"
        );
    }

    #[test]
    fn a_single_frame_flicker_never_emits() {
        let mut h = FireSmokeHead::new(1, FireSmokeConfig::default());
        assert!(h.observe(0, Some(fire_nom(0.2))).is_none());
        // Nomination drops out before the 2 s window — candidate cleared.
        assert!(h.observe(500, None).is_none());
        assert!(
            h.observe(2001, Some(fire_nom(0.2))).is_none(),
            "restarted window"
        );
    }

    #[test]
    fn smoke_requires_five_seconds_and_region_growth() {
        let mut h = FireSmokeHead::new(1, FireSmokeConfig::default());
        let smoke = |side: f32| {
            Some(Nomination {
                kind: FireSmokeKind::Smoke,
                region: region(0.0, 0.0, side, side),
                confidence: 0.7,
            })
        };
        // Growing region across the window.
        assert!(h.observe(0, smoke(0.10)).is_none());
        assert!(h.observe(2000, smoke(0.14)).is_none());
        let sig = h.observe(5000, smoke(0.20)).expect("5s + growth emits");
        assert_eq!(sig.kind, FireSmokeKind::Smoke);
    }

    #[test]
    fn a_static_grey_region_that_never_expands_is_not_smoke() {
        let mut h = FireSmokeHead::new(1, FireSmokeConfig::default());
        let smoke = || {
            Some(Nomination {
                kind: FireSmokeKind::Smoke,
                region: region(0.0, 0.0, 0.15, 0.15),
                confidence: 0.7,
            })
        };
        assert!(h.observe(0, smoke()).is_none());
        assert!(h.observe(5000, smoke()).is_none(), "no growth → not smoke");
        assert!(h.observe(8000, smoke()).is_none());
    }

    #[test]
    fn the_full_rate_flicker_path_is_never_taken_without_a_nomination() {
        let mut h = FireSmokeHead::new(1, FireSmokeConfig::default());
        // A long run of empty frames must never enter the flicker check.
        for t in 0..100 {
            let _ = h.observe(t * 33, None);
        }
        assert_eq!(h.flicker_checks(), 0, "cascade never ran the flicker path");
        // One nomination → exactly one flicker check.
        let _ = h.observe(3300, Some(fire_nom(0.2)));
        assert_eq!(h.flicker_checks(), 1);
    }

    #[test]
    fn the_head_runs_edge_local_with_no_cloud_call() {
        // Structural: `observe` takes no cloud handle and this module has
        // no network dependency — a signal is produced from local frames
        // alone, which is the tunnel-disconnected case by construction.
        let mut h = FireSmokeHead::new(9, FireSmokeConfig::default());
        assert!(h.observe(0, Some(fire_nom(0.3))).is_none());
        assert!(h.observe(2500, Some(fire_nom(0.3))).is_some());
    }
}
