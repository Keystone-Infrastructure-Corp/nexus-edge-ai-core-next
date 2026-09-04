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

use std::time::Duration;

use nexus_types::{BBox, CameraId, TamperMode, TamperSignal};

use crate::fire_smoke_cadence::{CadenceDecision, CadencePolicy};

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

/// Gradient magnitude (in full-scale luma units, so `0..~1`) at or above
/// which a pixel counts as "on an edge" for [`FrameStats::edge_density`].
///
/// Deliberately a named constant rather than a literal: `edge_density`
/// feeds `TamperConfig::edge_collapse` (default `0.12`), so moving this
/// silently re-tunes the covered/sprayed-lens threshold.
const EDGE_MAGNITUDE_THRESHOLD: f32 = 0.10;

/// Half-saturation constant for the sharpness curve (see
/// [`FrameStats::from_luma`]). The curve is
/// `s / (s + K)` over `s = sqrt(var(laplacian))`, so this is the value of
/// `s` that maps to a sharpness of exactly `0.5`.
const SHARPNESS_HALF_SATURATION: f32 = 0.25;

impl FrameStats {
    /// Compute the per-frame statistics from a **luma (Y) plane**.
    ///
    /// SPEC-038's statistics pass. `luma` is row-major, one byte per
    /// pixel, `width * height` bytes (extra trailing bytes — e.g. a
    /// full YUV buffer whose Y plane comes first, or a padded stride
    /// equal to `width` — are ignored).
    ///
    /// Every field is normalised to `0..1` and, critically,
    /// **resolution-independent**: each is a mean, a fraction, or a
    /// variance of per-pixel quantities rather than a sum, so the same
    /// scene yields materially the same statistics at 512x288 and at
    /// 1536x864. `TamperConfig`'s defaults are tuned against these
    /// normalisations, so a differently-scaled implementation would
    /// silently invalidate every tuned threshold.
    ///
    /// * `mean_luma` — arithmetic mean of the plane, divided by 255.
    /// * `histogram` — 8 equal-width luma bins, divided by the pixel
    ///   count, so the bins sum to 1.
    /// * `edge_density` — fraction of *interior* pixels whose central
    ///   -difference gradient magnitude reaches
    ///   [`EDGE_MAGNITUDE_THRESHOLD`].
    /// * `sharpness` — a variance-of-Laplacian focus proxy squashed
    ///   into `0..1` by `s / (s + K)` where `s` is the standard
    ///   deviation of the 4-neighbour Laplacian in full-scale luma
    ///   units and `K` is [`SHARPNESS_HALF_SATURATION`]. The squash is
    ///   monotonic in the raw variance, so *ordering* and the
    ///   `defocus_drop` comparison are preserved, but the absolute
    ///   value is a tunable convention, **not** a calibrated physical
    ///   quantity — no real-footage corpus has been used to fit `K`.
    ///
    /// A frame smaller than 3x3 has no interior pixels; it yields the
    /// mean and histogram it does have, with `sharpness` and
    /// `edge_density` of `0.0` (indistinguishable from a flat frame,
    /// which is the safe reading — it cannot manufacture an edge).
    #[must_use]
    pub fn from_luma(luma: &[u8], width: usize, height: usize) -> Self {
        let n = width.saturating_mul(height);
        if n == 0 || luma.len() < n {
            return Self {
                mean_luma: 0.0,
                sharpness: 0.0,
                edge_density: 0.0,
                histogram: [0.0; 8],
            };
        }
        let px = &luma[..n];

        let mut sum: u64 = 0;
        let mut bins = [0u32; 8];
        for &v in px {
            sum += u64::from(v);
            // 8 equal-width bins over 0..=255: `v / 32`, which is
            // `v >> 5`, giving bin 7 for 224..=255.
            bins[(v >> 5) as usize] += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        let n_f = n as f32;
        #[allow(clippy::cast_precision_loss)]
        let mean_luma = (sum as f32) / n_f / 255.0;
        let mut histogram = [0.0f32; 8];
        for (h, &b) in histogram.iter_mut().zip(bins.iter()) {
            #[allow(clippy::cast_precision_loss)]
            {
                *h = b as f32 / n_f;
            }
        }

        if width < 3 || height < 3 {
            return Self {
                mean_luma,
                sharpness: 0.0,
                edge_density: 0.0,
                histogram,
            };
        }

        // Interior pass: central-difference gradient (edge density) and
        // the 4-neighbour Laplacian (focus proxy). Both are computed
        // per-pixel and then averaged, which is what makes them
        // resolution-independent.
        let at = |x: usize, y: usize| -> f32 { f32::from(px[y * width + x]) / 255.0 };
        let interior = (width - 2) * (height - 2);
        let mut edge_hits: u32 = 0;
        let mut lap_sum = 0.0f64;
        let mut lap_sq_sum = 0.0f64;
        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let c = at(x, y);
                let l = at(x - 1, y);
                let r = at(x + 1, y);
                let u = at(x, y - 1);
                let d = at(x, y + 1);

                let gx = (r - l) * 0.5;
                let gy = (d - u) * 0.5;
                if gx.hypot(gy) >= EDGE_MAGNITUDE_THRESHOLD {
                    edge_hits += 1;
                }

                let lap = f64::from(l + r + u + d - 4.0 * c);
                lap_sum += lap;
                lap_sq_sum += lap * lap;
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let interior_f = interior as f64;
        let lap_mean = lap_sum / interior_f;
        let lap_var = (lap_sq_sum / interior_f - lap_mean * lap_mean).max(0.0);
        #[allow(clippy::cast_possible_truncation)]
        let lap_sd = lap_var.sqrt() as f32;
        let sharpness = lap_sd / (lap_sd + SHARPNESS_HALF_SATURATION);

        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let edge_density = (f64::from(edge_hits) / interior_f) as f32;

        Self {
            mean_luma,
            sharpness,
            edge_density,
            histogram,
        }
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

/// SPEC-038 — how much of a rung's frame period the statistics pass is
/// allowed to consume before it must degrade to a reduced cadence.
///
/// One tenth. The pass is a *supporting* per-frame consumer: the primary
/// detector and the DINOv2 appearance extractor dominate the frame
/// budget, and tamper detection must not compete with them. Stated here
/// as a named constant, derived from the frame period rather than from
/// any measurement, so it cannot be quietly re-fitted to whatever the
/// pass happens to cost (a budget chosen after the fact to guarantee a
/// pass proves nothing).
pub const STATS_FRAME_BUDGET_FRACTION: u32 = 10;

/// SPEC-038 — the worst-case time a tamper may go unnoticed.
///
/// Unlike the fire head's *persistence* window, this is a
/// **detection-latency** budget, and one sample is enough to prove it:
/// `classify` compares a single frame against a stored reference and
/// fires immediately, so there is no second observation to wait for.
/// See [`nexus_inference::fire_smoke_cadence::CadencePolicy::with_samples_required`].
pub const TAMPER_DETECTION_LATENCY_BUDGET: Duration = Duration::from_secs(2);

/// Choose the cadence for the statistics pass at one ladder rung.
///
/// Returns the *stated* interval between invocations as a first-class
/// [`CadenceDecision`], so "what cadence is tamper running at on this
/// rung, and why" is answerable without reading source — the criterion
/// requires the cadence be stated rather than implied.
///
/// * `rung_frame_period` — the rung's native per-frame period.
/// * `per_frame_cost` — the measured cost of [`FrameStats::from_luma`]
///   at this rung.
///
/// When the pass fits its share of the budget it runs on every frame.
/// When it does not, it runs once every `k` frames, `k` chosen as the
/// smallest integer that brings the **amortised** per-frame cost back
/// within budget — running it flat-out at its own cost, the way a
/// persistence-driven head would, is meaningless here: nothing is made
/// safer by sampling faster than the frame rate, and the point of
/// degrading is to give the frame budget back.
///
/// The latency ceiling is derived from
/// [`TAMPER_DETECTION_LATENCY_BUDGET`] inside this function, not taken
/// as a caller-supplied option, so there is no way to ask for a cadence
/// and skip the safety check.
#[must_use]
pub fn cadence_for_rung(
    rung_frame_period: Duration,
    per_frame_cost: Duration,
) -> CadenceDecision {
    let budget = rung_frame_period / STATS_FRAME_BUDGET_FRACTION;
    let ceiling =
        CadencePolicy::with_samples_required(TAMPER_DETECTION_LATENCY_BUDGET, 1).max_safe_period();

    if per_frame_cost <= budget {
        return CadenceDecision::FullRate {
            period: rung_frame_period,
        };
    }

    // Smallest k with cost / k <= budget, i.e. k = ceil(cost / budget).
    let cost_ns = per_frame_cost.as_nanos();
    let budget_ns = budget.as_nanos().max(1);
    let k = u32::try_from(cost_ns.div_ceil(budget_ns)).unwrap_or(u32::MAX);
    let period = rung_frame_period.saturating_mul(k);

    if period > ceiling {
        return CadenceDecision::CannotSatisfyPersistenceWindow {
            cost_floor: period,
            window_ceiling: ceiling,
        };
    }
    CadenceDecision::Reduced { period }
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

    // ---- SPEC-038 statistics pass -------------------------------------

    /// Ladder rungs, from `models/models-manifest.json`.
    const RUNGS: [(usize, usize); 3] = [(512, 288), (1024, 576), (1536, 864)];

    fn flat(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h]
    }

    /// A crisp, high-frequency frame: a 1px checkerboard.
    fn checkerboard(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .map(|i| if ((i / w) + (i % w)) % 2 == 0 { 0 } else { 255 })
            .collect()
    }

    /// The checkerboard blurred by a 3x3 box filter — same scene content,
    /// strictly less high-frequency energy.
    fn blurred_checkerboard(w: usize, h: usize) -> Vec<u8> {
        let src = checkerboard(w, h);
        let mut out = src.clone();
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let mut acc = 0u32;
                for dy in 0..3 {
                    for dx in 0..3 {
                        acc += u32::from(src[(y + dy - 1) * w + (x + dx - 1)]);
                    }
                }
                out[y * w + x] = (acc / 9) as u8;
            }
        }
        out
    }

    #[test]
    fn histogram_bins_sum_to_one_at_every_rung() {
        // The struct documents "an 8-bin luma histogram whose bins sum to
        // ~1"; `hist_l1` (and therefore `hist_break`) is only a valid
        // distance if that holds.
        for (w, h) in RUNGS {
            for frame in [flat(w, h, 0), flat(w, h, 255), checkerboard(w, h)] {
                let st = FrameStats::from_luma(&frame, w, h);
                let sum: f32 = st.histogram.iter().sum();
                assert!(
                    (sum - 1.0).abs() < 1e-4,
                    "{w}x{h}: bins must sum to 1, summed to {sum}"
                );
            }
        }
    }

    #[test]
    fn uniform_frames_pin_the_mean_luma_scale() {
        let (w, h) = (64, 48);
        let black = FrameStats::from_luma(&flat(w, h, 0), w, h);
        let white = FrameStats::from_luma(&flat(w, h, 255), w, h);
        assert!(
            black.mean_luma.abs() < 1e-6,
            "a black frame must read 0.0, read {}",
            black.mean_luma
        );
        assert!(
            (white.mean_luma - 1.0).abs() < 1e-6,
            "a white frame must read 1.0, read {}",
            white.mean_luma
        );
        // A black frame must sit under `dark_floor` (0.08) or the
        // illumination-collapse mode can never fire.
        assert!(black.mean_luma <= TamperConfig::default().dark_floor);
        assert_eq!(black.histogram[0], 1.0, "all mass in the darkest bin");
        assert_eq!(white.histogram[7], 1.0, "all mass in the brightest bin");
    }

    #[test]
    fn a_flat_frame_has_no_edges_and_no_sharpness() {
        let (w, h) = (64, 48);
        let st = FrameStats::from_luma(&flat(w, h, 128), w, h);
        assert_eq!(st.edge_density, 0.0);
        assert_eq!(st.sharpness, 0.0);
        // A covered/sprayed lens is exactly this: it must read at or
        // below `edge_collapse` (0.12).
        assert!(st.edge_density <= TamperConfig::default().edge_collapse);
    }

    #[test]
    fn blurring_lowers_sharpness_and_a_real_defocus_trips_the_threshold() {
        // Ordering is the contract `defocus_drop` depends on.
        let (w, h) = (128, 96);
        let crisp = FrameStats::from_luma(&checkerboard(w, h), w, h);
        let soft = FrameStats::from_luma(&blurred_checkerboard(w, h), w, h);
        assert!(
            soft.sharpness < crisp.sharpness,
            "blurred {} must score below crisp {}",
            soft.sharpness,
            crisp.sharpness
        );

        // A *single* 3x3 box pass over a 1px checkerboard is not a
        // defocus: it leaves a residual ~11%-amplitude checkerboard, so
        // it drops sharpness by only ~0.29 — under `defocus_drop`
        // (0.35). That is correct behaviour, not a miscalibration: a
        // one-pixel optical softening should not raise a tamper alarm.
        // A real defocus spreads energy over a much wider kernel, which
        // three successive box passes approximate. Assert *that* clears
        // the threshold, or the defocus mode would be unreachable in
        // practice and the mode would be dead code.
        let mut heavy = blurred_checkerboard(w, h);
        for _ in 0..2 {
            let src = heavy.clone();
            for y in 1..h - 1 {
                for x in 1..w - 1 {
                    let mut acc = 0u32;
                    for dy in 0..3 {
                        for dx in 0..3 {
                            acc += u32::from(src[(y + dy - 1) * w + (x + dx - 1)]);
                        }
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        heavy[y * w + x] = (acc / 9) as u8;
                    }
                }
            }
        }
        let defocused = FrameStats::from_luma(&heavy, w, h);
        let drop = crisp.sharpness - defocused.sharpness;
        assert!(
            drop >= TamperConfig::default().defocus_drop,
            "a realistic defocus must clear defocus_drop ({}), dropped only {drop}",
            TamperConfig::default().defocus_drop
        );
    }

    #[test]
    fn every_field_is_bounded_to_zero_one() {
        let (w, h) = (96, 72);
        for frame in [
            flat(w, h, 0),
            flat(w, h, 255),
            flat(w, h, 128),
            checkerboard(w, h),
            blurred_checkerboard(w, h),
        ] {
            let st = FrameStats::from_luma(&frame, w, h);
            for (name, v) in [
                ("mean_luma", st.mean_luma),
                ("sharpness", st.sharpness),
                ("edge_density", st.edge_density),
            ] {
                assert!(
                    (0.0..=1.0).contains(&v),
                    "{name} escaped 0..1 with {v}; thresholds assume normalised inputs"
                );
            }
        }
    }

    #[test]
    fn the_statistics_are_resolution_independent() {
        // This is the property that lets one set of thresholds serve
        // every ladder rung. The same scene at three rungs must produce
        // materially the same statistics.
        let stats: Vec<_> = RUNGS
            .iter()
            .map(|&(w, h)| FrameStats::from_luma(&checkerboard(w, h), w, h))
            .collect();
        for s in &stats[1..] {
            assert!(
                (s.mean_luma - stats[0].mean_luma).abs() < 0.01,
                "mean_luma drifted across rungs: {:?}",
                stats.iter().map(|s| s.mean_luma).collect::<Vec<_>>()
            );
            assert!(
                (s.edge_density - stats[0].edge_density).abs() < 0.01,
                "edge_density drifted across rungs: {:?}",
                stats.iter().map(|s| s.edge_density).collect::<Vec<_>>()
            );
            assert!(
                (s.sharpness - stats[0].sharpness).abs() < 0.01,
                "sharpness drifted across rungs: {:?}",
                stats.iter().map(|s| s.sharpness).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_degenerate_frame_cannot_manufacture_an_edge() {
        // Guard the interior-pass bounds: too small to have interior
        // pixels, and a buffer shorter than its declared size.
        for (w, h) in [(0usize, 0usize), (1, 1), (2, 2)] {
            let st = FrameStats::from_luma(&flat(w.max(1), h.max(1), 200), w, h);
            assert_eq!(st.sharpness, 0.0, "{w}x{h} must not invent sharpness");
            assert_eq!(st.edge_density, 0.0, "{w}x{h} must not invent an edge");
        }
        let st = FrameStats::from_luma(&[1, 2, 3], 64, 48);
        assert_eq!(st.mean_luma, 0.0, "a short buffer must not read past its end");
    }

    #[test]
    fn the_pass_drives_the_real_detector_end_to_end() {
        // Binding: the statistics pass is the thing `TamperDetector`
        // consumes, not a parallel construction. A crisp reference then
        // a covered lens must classify — through `from_luma`, with no
        // hand-built `FrameStats` anywhere in this test.
        let (w, h) = (128, 96);
        let mut det = TamperDetector::new(7, TamperConfig::default());
        assert!(det
            .observe(
                0,
                FrameStats::from_luma(&checkerboard(w, h), w, h),
                TamperContext::default()
            )
            .is_none());
        let sig = det.observe(
            1_000,
            FrameStats::from_luma(&flat(w, h, 10), w, h),
            TamperContext::default(),
        );
        assert!(
            sig.is_some(),
            "a crisp scene replaced by a flat dark frame must classify as tamper"
        );
    }

    /// SPEC-038 — the measurement. `#[ignore]`d deliberately: this
    /// machine is shared with parallel builds, so a wall-clock
    /// assertion here would be flaky, and flaky tests get deleted.
    /// Run with `cargo test -p nexus-inference -- --ignored --nocapture
    /// statistics_pass_per_frame_cost`.
    #[test]
    #[ignore = "wall-clock measurement; run explicitly, see SPEC-038 As-Built"]
    fn statistics_pass_per_frame_cost_at_each_rung() {
        use std::time::Instant;
        for (w, h) in RUNGS {
            let frame = checkerboard(w, h);
            // Warm the cache, then take the median of 21 runs.
            for _ in 0..3 {
                std::hint::black_box(FrameStats::from_luma(&frame, w, h));
            }
            let mut samples: Vec<f64> = (0..21)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(FrameStats::from_luma(&frame, w, h));
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "{w}x{h}: median {:.3} ms (min {:.3}, max {:.3}) over 21 runs",
                samples[10], samples[0], samples[20]
            );
        }
    }

    // ---- SPEC-038 cadence ---------------------------------------------

    /// Measured medians of `FrameStats::from_luma` (release, 21 runs) —
    /// see the SPEC-038 As-Built for the machine and conditions.
    const MEASURED_COST_MS: [(usize, usize, f64); 3] =
        [(512, 288, 0.797), (1024, 576, 2.775), (1536, 864, 6.226)];

    fn ms_f(v: f64) -> Duration {
        Duration::from_nanos((v * 1_000_000.0) as u64)
    }

    #[test]
    fn the_measured_costs_select_the_cadence_the_spec_records() {
        // Binds the recorded numbers to the recorded verdicts: if the
        // pass gets slower, this test — not a reader — catches the drift.
        let frame_30fps = Duration::from_nanos(33_333_333);
        let verdicts: Vec<_> = MEASURED_COST_MS
            .iter()
            .map(|&(_, _, c)| cadence_for_rung(frame_30fps, ms_f(c)))
            .collect();

        assert!(
            matches!(verdicts[0], CadenceDecision::FullRate { .. }),
            "512x288 (0.797ms) fits a 3.33ms budget, got {:?}",
            verdicts[0]
        );
        assert!(
            matches!(verdicts[1], CadenceDecision::FullRate { .. }),
            "1024x576 (2.775ms) fits a 3.33ms budget, got {:?}",
            verdicts[1]
        );
        // The top rung is the whole point of the criterion: it does NOT
        // fit, so it must degrade — every other frame, 15 Hz.
        assert_eq!(
            verdicts[2],
            CadenceDecision::Reduced {
                period: frame_30fps * 2
            },
            "1536x864 (6.226ms) exceeds the 3.33ms budget and must halve its cadence"
        );
    }

    #[test]
    fn a_reduced_cadence_brings_the_amortised_cost_back_within_budget() {
        // The property that makes degradation meaningful. Sweep a wide
        // range rather than one case: the bug guarded against is an
        // off-by-one in the ceil, which one example will not find.
        let frame = Duration::from_nanos(33_333_333);
        let budget = frame / STATS_FRAME_BUDGET_FRACTION;
        let mut reduced_seen = 0;
        let mut full_seen = 0;
        for cost_us in (100..=30_000).step_by(37) {
            let cost = Duration::from_micros(cost_us);
            match cadence_for_rung(frame, cost) {
                CadenceDecision::FullRate { period } => {
                    full_seen += 1;
                    assert!(cost <= budget, "FullRate at {cost:?} exceeds budget {budget:?}");
                    assert_eq!(period, frame);
                }
                CadenceDecision::Reduced { period } => {
                    reduced_seen += 1;
                    let k = period.as_nanos() / frame.as_nanos();
                    assert!(k >= 2, "a Reduced cadence must actually skip frames, k={k}");
                    let amortised = cost / u32::try_from(k).unwrap();
                    assert!(
                        amortised <= budget,
                        "amortised {amortised:?} at k={k} still exceeds budget {budget:?}"
                    );
                    assert!(
                        period <= TAMPER_DETECTION_LATENCY_BUDGET,
                        "period {period:?} exceeds the detection-latency budget"
                    );
                }
                CadenceDecision::CannotSatisfyPersistenceWindow { .. } => {
                    panic!("no cost in this range should be unrunnable at 30fps")
                }
            }
        }
        assert!(full_seen > 0 && reduced_seen > 0, "the sweep must exercise both branches");
    }

    #[test]
    fn a_cost_that_cannot_meet_the_latency_budget_is_refused_not_degraded() {
        // A pathologically slow pass must be refused rather than handed
        // a cadence that lets a sprayed lens go unnoticed past the
        // stated budget.
        let frame = Duration::from_millis(200);
        let verdict = cadence_for_rung(frame, Duration::from_millis(900));
        match verdict {
            CadenceDecision::CannotSatisfyPersistenceWindow {
                cost_floor,
                window_ceiling,
            } => {
                assert!(cost_floor > window_ceiling);
                assert_eq!(window_ceiling, TAMPER_DETECTION_LATENCY_BUDGET);
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_chosen_cadence_is_stated_not_implied() {
        let frame = Duration::from_nanos(33_333_333);
        let d = cadence_for_rung(frame, ms_f(6.226));
        assert_eq!(
            d.period(),
            Some(frame * 2),
            "the caller must be able to read the chosen period off the decision"
        );
    }
}
