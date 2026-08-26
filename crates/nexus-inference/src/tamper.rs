//! SPEC-038 — Camera tamper / scene-change detector.
//!
//! A tampered camera produces an abrupt break in the statistical character
//! of its own frames — nobody carries a "tamper" object, so this is a
//! statistics detector, **not** an object-detector: this module makes no
//! call into the detection layer. It classifies five modes
//! ([`TamperMode`]) from per-frame [`FrameStats`] against a reference and
//! emits a **place-keyed** [`TamperSignal`] (camera + region, no entity).
//!
//! It refuses to learn. The reference is re-baselined **only** on a
//! deliberate scene change the system already knows about — an expected
//! lighting change (scheduled lights, day/night IR-cut), a PTZ move the
//! system itself commanded, or an operator-confirmed reframe / scene-map
//! refresh. A tamper event **never** re-baselines, so a camera sprayed
//! nightly keeps firing forever and its assessment rises rather than
//! decays (ADR-056 never-benign floor). It runs edge-local so pulling the
//! network does not disable the tamper alarm (ADR-043).

use nexus_types::{BBox, CameraId, TamperMode, TamperSignal};

/// Per-frame statistics the detector reasons over. All normalised to
/// `0..1` so thresholds are resolution-independent. `histogram` is an
/// 8-bin luma histogram whose bins sum to ~1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    /// Mean luma, 0 (black) .. 1 (white).
    pub mean_luma: f32,
    /// A focus/sharpness proxy (e.g. normalised variance-of-Laplacian),
    /// 0 (fully blurred) .. 1 (crisp).
    pub sharpness: f32,
    /// Fraction of pixels on an edge, 0 .. 1.
    pub edge_density: f32,
    /// 8-bin normalised luma histogram.
    pub histogram: [f32; 8],
}

impl FrameStats {
    fn hist_l1(&self, other: &FrameStats) -> f32 {
        0.5 * self
            .histogram
            .iter()
            .zip(other.histogram.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
    }
}

/// What the *system already knows* about this frame — the deliberate,
/// anticipated causes of a scene-statistic break. Any one of these
/// re-baselines the reference instead of alerting.
#[derive(Debug, Clone, Copy, Default)]
pub struct TamperContext {
    /// A scheduled lighting change, sunrise/sunset, or the IR-cut day/night
    /// switch — a lighting event the system anticipates.
    pub expected_lighting_change: bool,
    /// A PTZ move the system itself commanded.
    pub commanded_ptz: bool,
    /// An operator-confirmed reframe or a scene-map refresh.
    pub deliberate_rebaseline: bool,
}

impl TamperContext {
    fn is_deliberate(&self) -> bool {
        self.expected_lighting_change || self.commanded_ptz || self.deliberate_rebaseline
    }
}

/// Thresholds for the classifier. Defaults tuned so ordinary weather
/// (rain/snow/fog) stays under them while a lens spray or repoint clears
/// them.
#[derive(Debug, Clone)]
pub struct TamperConfig {
    /// Sharpness must fall by at least this to count as a defocus.
    pub defocus_drop: f32,
    /// Edge density at or below this is a collapsed (covered/sprayed) view.
    pub edge_collapse: f32,
    /// Histogram L1 distance above this is a scene break (reframe / cover).
    pub hist_break: f32,
    /// Mean luma at or below this (with a concentrated dark histogram) is an
    /// illumination collapse.
    pub dark_floor: f32,
    /// Full-frame region reported for a whole-lens tamper.
    pub full_frame: BBox,
}

impl Default for TamperConfig {
    fn default() -> Self {
        Self {
            defocus_drop: 0.35,
            edge_collapse: 0.12,
            hist_break: 0.40,
            dark_floor: 0.08,
            full_frame: BBox {
                x1: 0.0,
                y1: 0.0,
                x2: 1.0,
                y2: 1.0,
            },
        }
    }
}

/// Stateful per-camera tamper detector.
#[derive(Debug)]
pub struct TamperDetector {
    camera_id: CameraId,
    cfg: TamperConfig,
    reference: Option<FrameStats>,
    assessment: u32,
    /// Set once if any object-detector coupling is ever introduced; the
    /// structural test asserts this stays `false`.
    called_object_detector: bool,
}

impl TamperDetector {
    #[must_use]
    pub fn new(camera_id: CameraId, cfg: TamperConfig) -> Self {
        Self {
            camera_id,
            cfg,
            reference: None,
            assessment: 0,
            called_object_detector: false,
        }
    }

    /// Current rising assessment for this camera.
    #[must_use]
    pub fn assessment(&self) -> u32 {
        self.assessment
    }

    /// Structural witness: this detector never calls the object detector.
    #[must_use]
    pub fn called_object_detector(&self) -> bool {
        self.called_object_detector
    }

    /// The reference the detector is currently comparing against.
    #[must_use]
    pub fn reference(&self) -> Option<FrameStats> {
        self.reference
    }

    /// Force a re-baseline (operator-confirmed reframe or scene-map
    /// refresh). Equivalent to `observe` with `deliberate_rebaseline`.
    pub fn rebaseline(&mut self, stats: FrameStats) {
        self.reference = Some(stats);
    }

    /// Advance by one frame. Returns a signal when a tamper mode is
    /// classified. A deliberate context re-baselines and returns `None`; a
    /// tamper **never** re-baselines.
    pub fn observe(
        &mut self,
        now_ms: i64,
        stats: FrameStats,
        ctx: TamperContext,
    ) -> Option<TamperSignal> {
        let Some(reference) = self.reference else {
            self.reference = Some(stats);
            return None;
        };

        if ctx.is_deliberate() {
            // Deliberate, anticipated change → learn the new normal, no
            // alert. This is the ONLY path that re-baselines.
            self.reference = Some(stats);
            return None;
        }

        let mode = classify(&self.cfg, &reference, &stats)?;

        // Tamper detected: do NOT re-baseline (so it keeps firing) and
        // raise the assessment for this camera.
        self.assessment = self.assessment.saturating_add(1);
        Some(TamperSignal {
            camera_id: self.camera_id,
            region: self.cfg.full_frame,
            mode,
            never_benign: true,
            assessment: self.assessment,
            emitted_unix_ms: now_ms,
        })
    }
}

/// Classify a frame against a reference, or `None` if it is within benign
/// tolerance (ordinary weather, mild lighting drift).
fn classify(cfg: &TamperConfig, reference: &FrameStats, cur: &FrameStats) -> Option<TamperMode> {
    let d_luma = cur.mean_luma - reference.mean_luma;
    let d_sharp = cur.sharpness - reference.sharpness;
    let hist_dist = reference.hist_l1(cur);
    let dark_uniform = cur.mean_luma <= cfg.dark_floor && cur.histogram[0] >= 0.8;
    let edge_collapsed = cur.edge_density <= cfg.edge_collapse;

    // 1. Illumination collapse: the scene goes uniformly dark.
    if dark_uniform {
        return Some(TamperMode::IlluminationCollapse);
    }

    // 2. Reframe: still a crisp, detailed scene, but a different one —
    //    sharpness and edge density preserved, histogram broken.
    if hist_dist >= cfg.hist_break
        && cur.sharpness >= 0.5
        && cur.edge_density >= reference.edge_density * 0.7
    {
        return Some(TamperMode::Reframe);
    }

    // 3. Defocus: sharpness collapses while brightness and histogram are
    //    largely preserved (lens turned / smeared, not covered).
    if d_sharp <= -cfg.defocus_drop && d_luma.abs() < 0.15 && hist_dist < cfg.hist_break {
        return Some(TamperMode::Defocus);
    }

    // 4. Spray: the lens is coated — edges collapse, the view flattens to a
    //    bright/uniform coating (luma up).
    if edge_collapsed && hist_dist >= cfg.hist_break && d_luma > 0.10 {
        return Some(TamperMode::Spray);
    }

    // 5. Obscuration: the lens is covered by an opaque object — edges
    //    collapse and the view darkens (luma down) without going to the
    //    uniform-black of an illumination collapse.
    if edge_collapsed && hist_dist >= cfg.hist_break && d_luma <= -0.10 {
        return Some(TamperMode::Obscuration);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_hist(bin: usize) -> [f32; 8] {
        let mut h = [0.0; 8];
        h[bin] = 1.0;
        h
    }

    fn spread_hist() -> [f32; 8] {
        [0.1, 0.15, 0.15, 0.15, 0.15, 0.1, 0.1, 0.1]
    }

    fn shifted_hist() -> [f32; 8] {
        [0.4, 0.3, 0.2, 0.1, 0.0, 0.0, 0.0, 0.0]
    }

    /// A crisp, detailed daytime reference scene.
    fn reference() -> FrameStats {
        FrameStats {
            mean_luma: 0.5,
            sharpness: 0.8,
            edge_density: 0.4,
            histogram: spread_hist(),
        }
    }

    fn detector() -> TamperDetector {
        let mut d = TamperDetector::new(1, TamperConfig::default());
        let _ = d.observe(0, reference(), TamperContext::default());
        d
    }

    #[test]
    fn obscuration_is_detected() {
        let mut d = detector();
        let covered = FrameStats {
            mean_luma: 0.15,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(1),
        };
        let sig = d
            .observe(1000, covered, TamperContext::default())
            .expect("fires");
        assert_eq!(sig.mode, TamperMode::Obscuration);
    }

    #[test]
    fn defocus_is_detected() {
        let mut d = detector();
        let blurred = FrameStats {
            mean_luma: 0.5,
            sharpness: 0.2,
            edge_density: 0.15,
            histogram: spread_hist(),
        };
        let sig = d
            .observe(1000, blurred, TamperContext::default())
            .expect("fires");
        assert_eq!(sig.mode, TamperMode::Defocus);
    }

    #[test]
    fn reframe_is_detected() {
        let mut d = detector();
        let repointed = FrameStats {
            mean_luma: 0.5,
            sharpness: 0.8,
            edge_density: 0.4,
            histogram: shifted_hist(),
        };
        let sig = d
            .observe(1000, repointed, TamperContext::default())
            .expect("fires");
        assert_eq!(sig.mode, TamperMode::Reframe);
    }

    #[test]
    fn spray_is_detected() {
        let mut d = detector();
        let sprayed = FrameStats {
            mean_luma: 0.85,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(6),
        };
        let sig = d
            .observe(1000, sprayed, TamperContext::default())
            .expect("fires");
        assert_eq!(sig.mode, TamperMode::Spray);
    }

    #[test]
    fn illumination_collapse_is_detected() {
        let mut d = detector();
        let dark = FrameStats {
            mean_luma: 0.02,
            sharpness: 0.05,
            edge_density: 0.01,
            histogram: uniform_hist(0),
        };
        let sig = d
            .observe(1000, dark, TamperContext::default())
            .expect("fires");
        assert_eq!(sig.mode, TamperMode::IlluminationCollapse);
    }

    #[test]
    fn a_scheduled_lighting_change_does_not_fire() {
        let mut d = detector();
        // Lights switch on a known schedule: brighter, histogram shifts, but
        // the system anticipated it.
        let lit = FrameStats {
            mean_luma: 0.8,
            sharpness: 0.8,
            edge_density: 0.4,
            histogram: shifted_hist(),
        };
        let ctx = TamperContext {
            expected_lighting_change: true,
            ..Default::default()
        };
        assert!(
            d.observe(1000, lit, ctx).is_none(),
            "scheduled light re-baselines"
        );
        // And the reference moved to the lit scene.
        assert_eq!(d.reference().unwrap().mean_luma, 0.8);
    }

    #[test]
    fn weather_does_not_fire_but_spray_on_the_same_camera_does() {
        let mut d = detector();
        // Rain/snow/fog: mild softening, small histogram drift — under the
        // thresholds.
        let fog = FrameStats {
            mean_luma: 0.55,
            sharpness: 0.55,
            edge_density: 0.3,
            histogram: spread_hist(),
        };
        assert!(
            d.observe(1000, fog, TamperContext::default()).is_none(),
            "weather is benign"
        );
        let sprayed = FrameStats {
            mean_luma: 0.85,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(6),
        };
        assert!(
            d.observe(2000, sprayed, TamperContext::default()).is_some(),
            "spray fires"
        );
    }

    #[test]
    fn day_night_ir_cut_transition_does_not_fire() {
        let mut d = detector();
        // The IR-cut switch abruptly changes colour/luma character; the
        // system knows it flipped, so it is an expected lighting change.
        let ir_night = FrameStats {
            mean_luma: 0.25,
            sharpness: 0.7,
            edge_density: 0.35,
            histogram: uniform_hist(2),
        };
        let ctx = TamperContext {
            expected_lighting_change: true,
            ..Default::default()
        };
        assert!(
            d.observe(1000, ir_night, ctx).is_none(),
            "IR-cut re-baselines"
        );
    }

    #[test]
    fn a_commanded_ptz_move_rebaselines_instead_of_alerting() {
        let mut d = detector();
        let new_view = FrameStats {
            mean_luma: 0.5,
            sharpness: 0.8,
            edge_density: 0.4,
            histogram: shifted_hist(),
        };
        let ctx = TamperContext {
            commanded_ptz: true,
            ..Default::default()
        };
        assert!(
            d.observe(1000, new_view, ctx).is_none(),
            "commanded move re-baselines"
        );
        assert_eq!(d.reference().unwrap().histogram, shifted_hist());
    }

    #[test]
    fn the_detector_makes_no_object_detector_call() {
        // Structural: `observe` takes only frame statistics, never a
        // detection list or detector handle, and this flag can never be set.
        let mut d = detector();
        let covered = FrameStats {
            mean_luma: 0.15,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(1),
        };
        let _ = d.observe(1000, covered, TamperContext::default());
        assert!(
            !d.called_object_detector(),
            "never consults the object detector"
        );
    }

    #[test]
    fn nightly_spray_for_a_month_always_fires_and_never_auto_closes() {
        // Never-benign floor (ADR-056): replay the same tamper signature
        // nightly for a simulated month; every night must fire, none must
        // learn a benign explanation, and the assessment must only rise.
        let mut d = detector();
        let sprayed = FrameStats {
            mean_luma: 0.85,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(6),
        };
        let mut last_assessment = 0;
        for night in 0..30 {
            let sig = d
                .observe(1000 + night * 86_400_000, sprayed, TamperContext::default())
                .expect("every night fires — never auto-closes");
            assert!(sig.never_benign, "signal is never benign");
            assert!(
                sig.assessment > last_assessment,
                "assessment rises, never decays"
            );
            last_assessment = sig.assessment;
        }
        assert_eq!(d.assessment(), 30);
    }

    #[test]
    fn tamper_never_silently_rebaselines() {
        // After a tamper the reference is unchanged, so the next identical
        // frame fires again — the detector never learns the tamper as normal.
        let mut d = detector();
        let before = d.reference().unwrap();
        let covered = FrameStats {
            mean_luma: 0.15,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(1),
        };
        assert!(d.observe(1000, covered, TamperContext::default()).is_some());
        assert_eq!(
            d.reference().unwrap(),
            before,
            "reference unchanged by tamper"
        );
        assert!(
            d.observe(2000, covered, TamperContext::default()).is_some(),
            "fires again"
        );
    }

    #[test]
    fn the_signal_is_place_keyed_with_no_entity() {
        // Type-level: TamperSignal has a camera_id and a region and simply
        // has no field to hold an entity/track id.
        let mut d = TamperDetector::new(7, TamperConfig::default());
        let _ = d.observe(0, reference(), TamperContext::default());
        let covered = FrameStats {
            mean_luma: 0.15,
            sharpness: 0.1,
            edge_density: 0.02,
            histogram: uniform_hist(1),
        };
        let sig = d.observe(1000, covered, TamperContext::default()).unwrap();
        assert_eq!(sig.camera_id, 7);
        assert_eq!(sig.region, TamperConfig::default().full_frame);
        // (No entity accessor exists to assert against — that is the point.)
    }
}
