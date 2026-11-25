//! Frame-rate-independent stillness window for the v2 parking-lot policy
//! (SPEC-077).
//!
//! Unlike the legacy per-frame filter in [`crate::static_object`], dwell is
//! credited only from observed (detector-matched) samples, in monotonic
//! capture milliseconds, and motion is measured in box diagonals: the same
//! scene classifies the same at any inference rate or image scale. Track
//! age, update counts, EMAs and receipt time never credit stillness.
//!
//! Speed is net motion over at least one capture second, never between
//! adjacent frames, so a parked box's per-frame jitter is not amplified by a
//! high frame rate. Drift from the window's first center is checked on every
//! sample, so an excursion between speed evaluations is never hidden.

use std::fmt;

/// The shortest capture interval a speed is measured over.
const SPEED_BASELINE_MS: u64 = 1000;

/// Validated v2 stillness thresholds. These come from explicit per-camera
/// configuration; there are no defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StillnessParams {
    dwell_secs: f64,
    max_speed_box_diagonals_per_sec: f64,
    max_displacement_box_diagonals: f64,
    max_observation_gap_secs: f64,
}

/// A [`StillnessParams`] field that was not finite and positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStillnessParam(pub &'static str);

impl fmt::Display for InvalidStillnessParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be finite and positive", self.0)
    }
}

impl std::error::Error for InvalidStillnessParam {}

impl StillnessParams {
    /// Every argument must be finite and positive.
    pub fn new(
        dwell_secs: f64,
        max_speed_box_diagonals_per_sec: f64,
        max_displacement_box_diagonals: f64,
        max_observation_gap_secs: f64,
    ) -> Result<Self, InvalidStillnessParam> {
        for (name, v) in [
            ("dwell_secs", dwell_secs),
            (
                "max_speed_box_diagonals_per_sec",
                max_speed_box_diagonals_per_sec,
            ),
            (
                "max_displacement_box_diagonals",
                max_displacement_box_diagonals,
            ),
            ("max_observation_gap_secs", max_observation_gap_secs),
        ] {
            if !(v.is_finite() && v > 0.0) {
                return Err(InvalidStillnessParam(name));
            }
        }
        Ok(Self {
            dwell_secs,
            max_speed_box_diagonals_per_sec,
            max_displacement_box_diagonals,
            max_observation_gap_secs,
        })
    }
}

/// The pending candidate window: opened by an observed sample, it holds the
/// diagonal frozen at that sample so later box-size jitter cannot rescale it.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    start_ms: u64,
    first_center: (f64, f64),
    diagonal: f64,
    last_ms: u64,
    /// Where the current speed interval began; it advances only when an
    /// interval of at least [`SPEED_BASELINE_MS`] passes the speed bound.
    ref_ms: u64,
    ref_center: (f64, f64),
}

/// Per-track stillness state for the v2 parking-lot policy.
#[derive(Debug, Clone)]
pub struct StillnessWindow {
    params: StillnessParams,
    candidate: Option<Candidate>,
}

impl StillnessWindow {
    pub fn new(params: StillnessParams) -> Self {
        Self {
            params,
            candidate: None,
        }
    }

    /// Feed one observed sample: capture time on the monotonic clock, and the
    /// box center and diagonal in the same (analysis) coordinates. Returns
    /// `true` while the current window has held still inside parking for at
    /// least the dwell and at least one speed interval has completed.
    ///
    /// Leaving parking, a degenerate box, nonpositive elapsed time, a gap
    /// over the limit, drift over its bound (checked every sample) or speed
    /// over its bound (checked once at least one second has elapsed since
    /// the speed reference) ends the window; any such sample still inside
    /// parking opens the next one.
    pub fn observe(
        &mut self,
        at_ms: u64,
        center: (f32, f32),
        box_diagonal: f32,
        inside_parking: bool,
    ) -> bool {
        let center = (f64::from(center.0), f64::from(center.1));
        let diagonal = f64::from(box_diagonal);
        if !(inside_parking
            && diagonal.is_finite()
            && diagonal > 0.0
            && center.0.is_finite()
            && center.1.is_finite())
        {
            self.candidate = None;
            return false;
        }
        let p = &self.params;
        if let Some(c) = &mut self.candidate {
            // checked_sub: a capture time before the last one is nonpositive.
            let in_gap = at_ms.checked_sub(c.last_ms).is_some_and(|dt_ms| {
                dt_ms > 0 && dt_ms as f64 / 1000.0 <= p.max_observation_gap_secs
            });
            // In gap, at_ms > last_ms >= ref_ms.
            let baseline_ms = at_ms.saturating_sub(c.ref_ms);
            let speed_due = baseline_ms >= SPEED_BASELINE_MS;
            let still = in_gap
                && distance(center, c.first_center) / c.diagonal
                    <= p.max_displacement_box_diagonals
                && (!speed_due
                    || distance(center, c.ref_center) / c.diagonal / (baseline_ms as f64 / 1000.0)
                        <= p.max_speed_box_diagonals_per_sec);
            if still {
                c.last_ms = at_ms;
                if speed_due {
                    c.ref_ms = at_ms;
                    c.ref_center = center;
                }
                // ref_ms moved past start_ms: a speed interval has completed.
                return c.ref_ms > c.start_ms
                    && (at_ms - c.start_ms) as f64 / 1000.0 >= p.dwell_secs;
            }
        }
        self.candidate = Some(Candidate {
            start_ms: at_ms,
            first_center: center,
            diagonal,
            last_ms: at_ms,
            ref_ms: at_ms,
            ref_center: center,
        });
        false
    }

    /// A missed or predicted-only frame: it credits no stillness and ends the
    /// pending window, even inside the observation gap.
    pub fn miss(&mut self) {
        self.candidate = None;
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIAGONAL: f32 = 100.0;
    const Y: f32 = 50.0;
    const FPS: [f64; 5] = [0.5, 1.45, 2.0, 8.0, 30.0];

    /// SPEC-077 test parameters, not customer defaults: dwell 5 s, speed
    /// <= 0.05 diagonals/s, displacement <= 0.10 diagonals, gap <= 3 s.
    fn params() -> StillnessParams {
        StillnessParams::new(5.0, 0.05, 0.10, 3.0).unwrap()
    }

    /// One analysed frame: a detector-observed box, or a predicted-only one.
    #[derive(Clone, Copy)]
    enum Sample {
        Observed { x: f32, inside: bool },
        Predicted,
    }

    /// A parked box's center jitter: +/-3 px on alternate frames.
    fn jitter(k: usize) -> f32 {
        if k.is_multiple_of(2) {
            3.0
        } else {
            -3.0
        }
    }

    struct Case {
        name: &'static str,
        diagonal: f32,
        /// Replay runs through the first frame at or after this second.
        end_secs: f64,
        /// The sample at capture second `t`, frame `k`.
        at: fn(f64, usize) -> Sample,
        /// First capture millisecond at which the dwell completes, given the
        /// replayed capture times; `None`: never.
        promote_from_ms: fn(&[u64]) -> Option<u64>,
    }

    fn cases() -> [Case; 8] {
        [
            // (0,100),(2,100),(4,100),(6,100) inside -> promote at t=6 (0.5 fps).
            Case {
                name: "stationary inside parking",
                diagonal: DIAGONAL,
                end_secs: 6.0,
                at: |_, _| Sample::Observed {
                    x: 100.0,
                    inside: true,
                },
                promote_from_ms: |_| Some(5000),
            },
            Case {
                name: "stationary outside parking",
                diagonal: DIAGONAL,
                end_secs: 6.0,
                at: |_, _| Sample::Observed {
                    x: 100.0,
                    inside: false,
                },
                promote_from_ms: |_| None,
            },
            // (0,100),(0.69,120),(1.38,140): 0.29 diagonals/s, held for 6 s.
            Case {
                name: "moving",
                diagonal: DIAGONAL,
                end_secs: 6.0,
                at: |t, _| Sample::Observed {
                    x: 100.0 + 20.0 * (t / 0.69) as f32,
                    inside: true,
                },
                promote_from_ms: |_| None,
            },
            // (0,100), predicted at 2/4/6, observed (8,100): a new window.
            Case {
                name: "predicted-only between observations",
                diagonal: DIAGONAL,
                end_secs: 8.0,
                at: |t, _| {
                    if t == 0.0 || t >= 8.0 {
                        Sample::Observed {
                            x: 100.0,
                            inside: true,
                        }
                    } else {
                        Sample::Predicted
                    }
                },
                promote_from_ms: |_| None,
            },
            // 0.04 diagonals/s passes the speed bound; drift breaks 0.10 first.
            Case {
                name: "creeping under the speed bound",
                diagonal: DIAGONAL,
                end_secs: 12.0,
                at: |t, _| Sample::Observed {
                    x: 100.0 + 4.0 * t as f32,
                    inside: true,
                },
                promote_from_ms: |_| None,
            },
            // 6 px peak to peak is 0.04 diagonals: parked at any frame rate.
            Case {
                name: "parked with +/-3 px jitter",
                diagonal: 150.0,
                end_secs: 8.0,
                at: |_, k| Sample::Observed {
                    x: 100.0 + jitter(k),
                    inside: true,
                },
                promote_from_ms: |_| Some(5000),
            },
            // 0.2 diagonals/s under the same jitter.
            Case {
                name: "moving at 30 px/s with +/-3 px jitter",
                diagonal: 150.0,
                end_secs: 8.0,
                at: |t, k| Sample::Observed {
                    x: 100.0 + 30.0 * t as f32 + jitter(k),
                    inside: true,
                },
                promote_from_ms: |_| None,
            },
            // Out 30 px (0.2 diagonals) for 0.9 s from t=3, then back: a
            // one-second speed baseline can span it, the drift bound cannot.
            Case {
                name: "excursion beyond the drift bound, back within 1 s",
                diagonal: 150.0,
                end_secs: 12.0,
                at: |t, k| Sample::Observed {
                    x: if (3.0..3.9).contains(&t) {
                        130.0
                    } else {
                        100.0
                    } + jitter(k),
                    inside: true,
                },
                promote_from_ms: |times| {
                    let out = |ms: &&u64| (3000..3900).contains(*ms);
                    match times.iter().position(|ms| out(&ms)) {
                        // No frame sampled it (0.5 and 1.45 fps can skip 0.9 s).
                        None => Some(5000),
                        // A sampled excursion restarts the dwell at the first frame back.
                        Some(i) => times[i..].iter().find(|ms| !out(ms)).map(|b| b + 5000),
                    }
                },
            },
        ]
    }

    /// Capture milliseconds from 0 through the first frame at or after
    /// `end_secs` at `fps`; `nonuniform` stretches or shrinks each interval
    /// to between 0.55 and 1.35 frames.
    fn capture_ms(fps: f64, nonuniform: bool, end_secs: f64) -> Vec<u64> {
        const OFFSETS: [f64; 5] = [0.0, 0.3, -0.15, 0.2, -0.1];
        let mut times = vec![0];
        while (*times.last().unwrap() as f64) < end_secs * 1000.0 {
            let k = times.len();
            let offset = if nonuniform { OFFSETS[k % 5] } else { 0.0 };
            times.push(((k as f64 + offset) / fps * 1000.0).round() as u64);
        }
        times
    }

    /// The `observe` result per frame of `case` at capture `times`, every
    /// position and dimension multiplied by `scale`.
    fn replay(case: &Case, times: &[u64], scale: f32) -> Vec<bool> {
        let mut w = StillnessWindow::new(params());
        times
            .iter()
            .enumerate()
            .map(|(k, &ms)| match (case.at)(ms as f64 / 1000.0, k) {
                Sample::Observed { x, inside } => {
                    w.observe(ms, (x * scale, Y * scale), case.diagonal * scale, inside)
                }
                Sample::Predicted => {
                    w.miss();
                    false
                }
            })
            .collect()
    }

    #[test]
    fn spec_vectors_classify_identically_at_every_frame_rate_spacing_and_scale() {
        let mut wrong = Vec::new();
        for case in &cases() {
            for fps in FPS {
                for nonuniform in [false, true] {
                    let times = capture_ms(fps, nonuniform, case.end_secs);
                    let from = (case.promote_from_ms)(&times);
                    for scale in [1.0, 2.0] {
                        let got = replay(case, &times, scale);
                        // The first wrong frame of each replay.
                        if let Some((ms, promoted)) = times
                            .iter()
                            .zip(got)
                            .find(|&(ms, promoted)| promoted != from.is_some_and(|f| *ms >= f))
                        {
                            wrong.push(format!(
                                "{} at {fps} fps, nonuniform {nonuniform}, scale {scale}: \
                                 t={ms} ms promoted={promoted}",
                                case.name
                            ));
                        }
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    fn still(w: &mut StillnessWindow, at_ms: u64) -> bool {
        w.observe(at_ms, (100.0, Y), DIAGONAL, true)
    }

    #[test]
    fn no_promotion_before_a_complete_speed_interval() {
        // A 0.5 s dwell is met at 0.5 s; the first 1 s speed interval ends at 1 s.
        let mut w = StillnessWindow::new(StillnessParams::new(0.5, 0.05, 0.10, 3.0).unwrap());
        let first = capture_ms(30.0, false, 2.0)
            .into_iter()
            .find(|&ms| still(&mut w, ms));
        assert_eq!(first, Some(1000));
    }

    #[test]
    fn the_speed_reference_advances_so_late_motion_is_not_averaged_away() {
        // Still for 4 s, then 8 px in one second: 0.08 diagonals/s over that
        // second, though only 0.016 averaged since the window opened.
        let mut w = StillnessWindow::new(params());
        for k in 0..=4 {
            assert!(!still(&mut w, k * 1000));
        }
        assert!(!w.observe(5000, (108.0, Y), DIAGONAL, true));
    }

    /// SPEC-077 keeps the conservative contract: a predicted-only frame (a
    /// detector dropout the tracker coasts through) restarts the dwell, and
    /// coasting earns no credit. So at 30 fps a parked car whose detection
    /// drops even one frame in every 5 s never promotes, and calibration may
    /// leave such a site on legacy. This asserts that outcome, not a goal.
    #[test]
    fn intermittent_detector_dropouts_at_30_fps_prevent_promotion() {
        /// Name, whether frame `k` is dropped, first promoted millisecond.
        type Dropouts = (&'static str, fn(usize) -> bool, Option<u64>);
        let patterns: [Dropouts; 4] = [
            ("one frame each second", |k| k % 30 == 29, None),
            ("a three-frame burst every 4 s", |k| k % 120 >= 117, None),
            ("one frame every 4.9 s", |k| k % 147 == 146, None),
            // The window reopens at 10 s and dwells 5 s.
            (
                "one frame each second for the first 10 s only",
                |k| k < 300 && k % 30 == 29,
                Some(15_000),
            ),
        ];
        let times = capture_ms(30.0, false, 60.0);
        for (name, dropped, want) in patterns {
            let mut w = StillnessWindow::new(params());
            let first = times.iter().enumerate().find_map(|(k, &ms)| {
                let promoted = if dropped(k) {
                    w.miss();
                    false
                } else {
                    w.observe(ms, (100.0 + jitter(k), Y), 150.0, true)
                };
                promoted.then_some(ms)
            });
            assert_eq!(first, want, "{name}");
        }
    }

    #[test]
    fn a_predicted_sample_inside_the_gap_restarts_the_dwell() {
        let mut w = StillnessWindow::new(params());
        assert!(!still(&mut w, 0));
        assert!(!still(&mut w, 2000));
        w.miss();
        assert!(!still(&mut w, 4000));
        assert!(!still(&mut w, 6000), "dwell must not span the miss");
        assert!(still(&mut w, 9000));
    }

    #[test]
    fn jitter_within_the_displacement_bound_is_still_too_fast() {
        // 0.08 diagonals each second: under the drift bound, over the speed one.
        let mut w = StillnessWindow::new(params());
        for k in 0..=8u64 {
            let x = if k % 2 == 0 { 100.0 } else { 108.0 };
            assert!(!w.observe(k * 1000, (x, Y), DIAGONAL, true), "t={k}s");
        }
    }

    #[test]
    fn the_diagonal_is_frozen_when_the_window_opens() {
        let mut w = StillnessWindow::new(params());
        assert!(!w.observe(0, (100.0, Y), 100.0, true));
        assert!(!w.observe(2000, (104.0, Y), 100.0, true));
        // 12 px is 0.12 of the frozen 100 px diagonal, not 0.06 of the new 200.
        assert!(!w.observe(4000, (112.0, Y), 200.0, true));
        assert!(!w.observe(6000, (112.0, Y), 200.0, true));
        assert!(w.observe(9000, (112.0, Y), 200.0, true));
    }

    #[test]
    fn the_observation_gap_bound_is_inclusive() {
        let every = |step_ms: u64| {
            let mut w = StillnessWindow::new(params());
            (0..4)
                .map(|k| still(&mut w, k * step_ms))
                .collect::<Vec<_>>()
        };
        assert_eq!(every(3000), [false, false, true, true]);
        assert_eq!(every(3001), [false; 4]);
    }

    #[test]
    fn nonpositive_elapsed_time_restarts_the_window() {
        let mut w = StillnessWindow::new(params());
        let got: Vec<bool> = [0, 2000, 2000, 4000, 6000, 7000, 6500, 9000, 11500]
            .into_iter()
            .map(|ms| still(&mut w, ms))
            .collect();
        // The duplicate 2000 opens a window there; the backwards 6500 another.
        let want = [false, false, false, false, false, true, false, false, true];
        assert_eq!(got, want);
    }

    #[test]
    fn leaving_parking_or_a_degenerate_box_restarts_the_window() {
        for (diagonal, inside) in [(DIAGONAL, false), (0.0, true), (f32::NAN, true)] {
            let mut w = StillnessWindow::new(params());
            assert!(!still(&mut w, 0));
            assert!(!still(&mut w, 2000));
            assert!(!w.observe(3000, (100.0, Y), diagonal, inside));
            assert!(!still(&mut w, 4000));
            assert!(!still(&mut w, 6000), "diagonal {diagonal}, inside {inside}");
            assert!(still(&mut w, 9000));
        }
    }

    /// Frame 512x288 analysis pixels, 100x100 px box: the frozen diagonal is
    /// in pixels, so no per-axis frame normalization can skew a direction.
    const BOX_DIAGONAL_PX: f32 = 141.421_36;

    /// Replay a box moving at `(vx, vy)` px/s from `(200, 100)` for 8 s at
    /// `fps`, with the trajectory rotated by `deg` and every length scaled.
    fn pixel_replay(v: (f32, f32), deg: f32, scale: f32, fps: f64) -> Vec<bool> {
        let (sin, cos) = deg.to_radians().sin_cos();
        let mut w = StillnessWindow::new(StillnessParams::new(5.0, 0.05, 0.20, 3.0).unwrap());
        capture_ms(fps, false, 8.0)
            .into_iter()
            .map(|ms| {
                let t = ms as f32 / 1000.0;
                let (x, y) = (200.0 + v.0 * t, 100.0 + v.1 * t);
                let c = ((x * cos - y * sin) * scale, (x * sin + y * cos) * scale);
                w.observe(ms, c, BOX_DIAGONAL_PX * scale, true)
            })
            .collect()
    }

    #[test]
    fn stillness_is_isotropic_in_analysis_frame_pixels() {
        let d = std::f32::consts::FRAC_1_SQRT_2;
        // (name, velocity px/s, promotes): 8 px/s is 0.0566 diagonals/s.
        let cases: [(&str, (f32, f32), bool); 6] = [
            ("8 px/s horizontal", (8.0, 0.0), false),
            ("8 px/s vertical", (0.0, 8.0), false),
            ("8 px/s diagonal", (8.0 * d, 8.0 * d), false),
            ("4 px/s horizontal", (4.0, 0.0), true),
            ("4 px/s vertical", (0.0, 4.0), true),
            ("4 px/s diagonal", (4.0 * d, 4.0 * d), true),
        ];
        for fps in [2.0, 30.0] {
            for (name, v, promotes) in cases {
                let base = pixel_replay(v, 0.0, 1.0, fps);
                assert_eq!(base.contains(&true), promotes, "{name} at {fps} fps");
                for (deg, scale) in [(90.0, 1.0), (45.0, 1.0), (0.0, 2.0), (45.0, 2.0)] {
                    assert_eq!(
                        pixel_replay(v, deg, scale, fps),
                        base,
                        "{name} at {fps} fps, rotated {deg}, scaled {scale}"
                    );
                }
            }
        }
        // Equal Euclidean length gives an identical decision in every direction.
        for fps in [2.0, 30.0] {
            let h = pixel_replay((8.0, 0.0), 0.0, 1.0, fps);
            assert_eq!(pixel_replay((0.0, 8.0), 0.0, 1.0, fps), h);
            assert_eq!(pixel_replay((8.0 * d, 8.0 * d), 0.0, 1.0, fps), h);
        }
    }

    #[test]
    fn non_finite_geometry_or_a_nonpositive_diagonal_never_promotes() {
        let bad: [((f32, f32), f32); 8] = [
            ((100.0, Y), 0.0),
            ((100.0, Y), -141.42),
            ((100.0, Y), f32::NAN),
            ((100.0, Y), f32::INFINITY),
            ((100.0, Y), f32::NEG_INFINITY),
            ((f32::NAN, Y), 141.42),
            ((100.0, f32::INFINITY), 141.42),
            ((f32::NEG_INFINITY, Y), 141.42),
        ];
        for fps in [2.0, 30.0] {
            for (center, diagonal) in bad {
                let mut w = StillnessWindow::new(params());
                let promoted = capture_ms(fps, false, 12.0)
                    .into_iter()
                    .any(|ms| w.observe(ms, center, diagonal, true));
                assert!(!promoted, "{center:?} {diagonal} at {fps} fps");
            }
        }
    }

    #[test]
    fn params_must_be_finite_and_positive() {
        let names = [
            "dwell_secs",
            "max_speed_box_diagonals_per_sec",
            "max_displacement_box_diagonals",
            "max_observation_gap_secs",
        ];
        let ok = [5.0, 0.05, 0.10, 3.0];
        for (i, name) in names.into_iter().enumerate() {
            for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
                let mut v = ok;
                v[i] = bad;
                assert_eq!(
                    StillnessParams::new(v[0], v[1], v[2], v[3]),
                    Err(InvalidStillnessParam(name)),
                    "{name} = {bad}"
                );
            }
        }
    }
}
