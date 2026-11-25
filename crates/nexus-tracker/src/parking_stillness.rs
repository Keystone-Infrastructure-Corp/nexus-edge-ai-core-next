//! Frame-rate-independent stillness window for the v2 parking-lot policy
//! (SPEC-077).
//!
//! Unlike the legacy per-frame filter in [`crate::static_object`], dwell is
//! credited only from observed (detector-matched) samples, in monotonic
//! capture milliseconds, and motion is measured in box diagonals: the same
//! scene classifies the same at any inference rate or image scale. Track
//! age, update counts, EMAs and receipt time never credit stillness.

use std::fmt;

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
    last_center: (f64, f64),
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
    /// least the dwell.
    ///
    /// Leaving parking, a degenerate box, nonpositive elapsed time, a gap
    /// over the limit, or either motion bound exceeded ends the window; any
    /// such sample still inside parking opens the next one.
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
            let still = at_ms.checked_sub(c.last_ms).is_some_and(|dt_ms| {
                let dt = dt_ms as f64 / 1000.0;
                dt > 0.0
                    && dt <= p.max_observation_gap_secs
                    && distance(center, c.last_center) / c.diagonal / dt
                        <= p.max_speed_box_diagonals_per_sec
                    && distance(center, c.first_center) / c.diagonal
                        <= p.max_displacement_box_diagonals
            });
            if still {
                c.last_ms = at_ms;
                c.last_center = center;
                return (at_ms - c.start_ms) as f64 / 1000.0 >= p.dwell_secs;
            }
        }
        self.candidate = Some(Candidate {
            start_ms: at_ms,
            first_center: center,
            diagonal,
            last_ms: at_ms,
            last_center: center,
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

    struct Case {
        name: &'static str,
        /// Replay runs through the first frame at or after this second.
        end_secs: f64,
        at: fn(f64) -> Sample,
        /// First capture second at which the dwell completes; `None`: never.
        promote_from_secs: Option<f64>,
    }

    fn cases() -> [Case; 5] {
        [
            // (0,100),(2,100),(4,100),(6,100) inside -> promote at t=6 (0.5 fps).
            Case {
                name: "stationary inside parking",
                end_secs: 6.0,
                at: |_| Sample::Observed {
                    x: 100.0,
                    inside: true,
                },
                promote_from_secs: Some(5.0),
            },
            Case {
                name: "stationary outside parking",
                end_secs: 6.0,
                at: |_| Sample::Observed {
                    x: 100.0,
                    inside: false,
                },
                promote_from_secs: None,
            },
            // (0,100),(0.69,120),(1.38,140): 0.29 diagonals/s, held for 6 s.
            Case {
                name: "moving",
                end_secs: 6.0,
                at: |t| Sample::Observed {
                    x: 100.0 + 20.0 * (t / 0.69) as f32,
                    inside: true,
                },
                promote_from_secs: None,
            },
            // (0,100), predicted at 2/4/6, observed (8,100): a new window.
            Case {
                name: "predicted-only between observations",
                end_secs: 8.0,
                at: |t| {
                    if t == 0.0 || t >= 8.0 {
                        Sample::Observed {
                            x: 100.0,
                            inside: true,
                        }
                    } else {
                        Sample::Predicted
                    }
                },
                promote_from_secs: None,
            },
            // 0.04 diagonals/s passes the speed bound; drift breaks 0.10 first.
            Case {
                name: "creeping under the speed bound",
                end_secs: 12.0,
                at: |t| Sample::Observed {
                    x: 100.0 + 4.0 * t as f32,
                    inside: true,
                },
                promote_from_secs: None,
            },
        ]
    }

    /// `(capture second, observe result)` per frame of `case` at `fps`, every
    /// position and dimension multiplied by `scale`.
    fn replay(case: &Case, fps: f64, scale: f32) -> Vec<(f64, bool)> {
        let mut w = StillnessWindow::new(params());
        (0..=(case.end_secs * fps).ceil() as u32)
            .map(|k| {
                let t = f64::from(k) / fps;
                let promoted = match (case.at)(t) {
                    Sample::Observed { x, inside } => w.observe(
                        (t * 1000.0).round() as u64,
                        (x * scale, Y * scale),
                        DIAGONAL * scale,
                        inside,
                    ),
                    Sample::Predicted => {
                        w.miss();
                        false
                    }
                };
                (t, promoted)
            })
            .collect()
    }

    #[test]
    fn spec_vectors_classify_identically_at_every_frame_rate_and_scale() {
        for case in &cases() {
            for fps in FPS {
                for scale in [1.0, 2.0] {
                    for (t, promoted) in replay(case, fps, scale) {
                        let want = case.promote_from_secs.is_some_and(|s| t >= s);
                        assert_eq!(
                            promoted, want,
                            "{} at {fps} fps, scale {scale}, t={t:.3}s",
                            case.name
                        );
                    }
                }
            }
        }
    }

    fn still(w: &mut StillnessWindow, at_ms: u64) -> bool {
        w.observe(at_ms, (100.0, Y), DIAGONAL, true)
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
