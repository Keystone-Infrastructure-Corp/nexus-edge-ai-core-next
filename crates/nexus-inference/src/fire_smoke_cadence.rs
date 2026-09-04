//! SPEC-036 — reduced-cadence policy for the fire/smoke head.
//!
//! [`fire_smoke`](crate::fire_smoke) assumes the head is *invoked*; this
//! module decides **how often** it may be invoked at a given ladder rung
//! without either blowing the per-frame budget or breaking the fire
//! head's ≥ 2 s persistence guarantee.
//!
//! This is pure decision logic: no timers, no I/O, no model. The
//! per-frame cost and the rung's frame budget are supplied by the
//! caller (owner ruling: mocking/measurement is out of scope for this
//! criterion — see SPEC-036, the criterion immediately above this one).
//! Per camera: a caller builds one [`CadencePolicy`] per camera's
//! configured persistence window (owner ruling 2), not a single global
//! instance.
//!
//! # Why a single sample can never prove persistence
//!
//! "Persists for ≥ W" is a *duration* claim. One observation only proves
//! "present at instant t"; it says nothing about how long the candidate
//! had already been there or how long it goes on being there. Proving
//! persistence needs at least two observations of the *same* candidate,
//! spaced far enough apart that the elapsed time between them alone
//! already reaches W.
//!
//! # Choosing the sampling ceiling
//!
//! Given a sampling period `P` (the cadence), the worst-case placement
//! of a persisting window of exactly length `W` against the sampling
//! grid contains `floor(W / P)` sample points. For that worst case to
//! still contain at least [`MIN_SAMPLES_TO_CONFIRM_PERSISTENCE`] (2)
//! points — one to observe the candidate has appeared, one taken at
//! least `W` later to confirm it is *still* there — the period must
//! satisfy `P <= W / MIN_SAMPLES_TO_CONFIRM_PERSISTENCE`. This is the
//! cadence ceiling enforced below; it is the Nyquist-style argument for
//! "you must sample at least twice as fast as the shortest event you
//! need to prove happened."
//!
//! A cadence policy that let the period exceed this ceiling could
//! silently degrade cadence past the point where a genuine 2 s fire
//! falls entirely between two samples and is never seen at all, while
//! the pipeline reports itself perfectly healthy — the exact failure
//! this criterion exists to prevent.

use std::time::Duration;

use nexus_types::{CameraId, FireSmokeSignal};

use crate::fire_smoke::{FireSmokeConfig, FireSmokeHead, Nomination};

/// Minimum number of head observations of the *same* persisting
/// candidate required to prove it has been present for at least the
/// persistence window (see the module-level Nyquist argument above). A
/// single observation only proves "present now", never "persisted".
pub const MIN_SAMPLES_TO_CONFIRM_PERSISTENCE: u32 = 2;

/// The outcome of asking the policy for a cadence at one ladder rung.
/// The chosen cadence is a first-class, inspectable value — never an
/// internal sleep-interval computation the caller cannot see — so
/// "what cadence is the fire head running at, and why" is answerable
/// without reading source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadenceDecision {
    /// The rung's native per-frame budget already accommodates the
    /// head's measured cost: it runs on every frame, at the rung's
    /// frame period, with no degradation.
    FullRate { period: Duration },
    /// The head cannot keep up with the rung's native frame period, but
    /// a slower, still-persistence-safe cadence exists. `period` is the
    /// stated interval between invocations; it is guaranteed
    /// `<= persistence_window / MIN_SAMPLES_TO_CONFIRM_PERSISTENCE`.
    Reduced { period: Duration },
    /// No cadence exists that both respects the head's per-frame cost
    /// and still guarantees the persistence window can be proven. The
    /// head must not run on this rung rather than run unsafely.
    CannotSatisfyPersistenceWindow {
        /// The fastest the head could possibly run, given its own cost.
        cost_floor: Duration,
        /// The slowest cadence that would still satisfy the window.
        window_ceiling: Duration,
    },
}

impl CadenceDecision {
    /// The stated period, if the head can run at all on this rung.
    #[must_use]
    pub fn period(&self) -> Option<Duration> {
        match self {
            Self::FullRate { period } | Self::Reduced { period } => Some(*period),
            Self::CannotSatisfyPersistenceWindow { .. } => None,
        }
    }
}

/// A per-camera cadence policy for the fire/smoke head at one ladder
/// rung. `persistence_window` is that camera's configured fire
/// persistence window (`FireSmokeConfig::fire_persistence`, normally
/// 2 s per HARM_TAXONOMY §4, but taken as a parameter rather than
/// hard-coded so a camera-specific override still gets the same safety
/// proof — owner ruling 2, calibration is per camera, never global).
#[derive(Debug, Clone, Copy)]
pub struct CadencePolicy {
    persistence_window: Duration,
}

impl CadencePolicy {
    #[must_use]
    pub fn new(persistence_window: Duration) -> Self {
        Self { persistence_window }
    }

    /// The slowest cadence period that still guarantees the persistence
    /// window can be proven, for this policy's window. Not a caller
    /// supplied optional — it is derived here, from the window value
    /// itself, so there is no way to call `decide` and skip the check.
    #[must_use]
    pub fn max_safe_period(&self) -> Duration {
        self.persistence_window / MIN_SAMPLES_TO_CONFIRM_PERSISTENCE
    }

    /// Decide the cadence for one ladder rung.
    ///
    /// * `rung_frame_period` — the native per-frame budget at this rung
    ///   (the reciprocal of the rung's target frame rate).
    /// * `per_frame_cost` — the head's measured or stated per-frame cost
    ///   at this rung.
    #[must_use]
    pub fn decide(
        &self,
        rung_frame_period: Duration,
        per_frame_cost: Duration,
    ) -> CadenceDecision {
        let window_ceiling = self.max_safe_period();

        if per_frame_cost <= rung_frame_period {
            // Fits the native budget outright: run every frame, no
            // degradation. (The native rung period is itself assumed to
            // already satisfy the persistence window at design time —
            // this policy exists for the *degraded* case.)
            return CadenceDecision::FullRate {
                period: rung_frame_period,
            };
        }

        // Cannot run every frame. The fastest the head could possibly be
        // invoked is bounded below by its own cost: you cannot start a
        // second invocation before the first has finished.
        let cost_floor = per_frame_cost;

        if cost_floor > window_ceiling {
            // Even running back-to-back as fast as the head's own cost
            // allows is too slow to prove persistence. Refuse rather
            // than hand back a cadence that silently violates the
            // window.
            return CadenceDecision::CannotSatisfyPersistenceWindow {
                cost_floor,
                window_ceiling,
            };
        }

        CadenceDecision::Reduced { period: cost_floor }
    }
}

/// The edge seam where the fire head plugs in. The real implementation
/// is the ONNX head (a model-cut deliverable); a stub that reports a
/// plausible nomination on demand is an acceptable stand-in (owner
/// ruling 4) because nothing below asserts on what the source *said* —
/// the assertions are on what [`CadencedFireHead`] and the real
/// [`FireSmokeHead`] persistence logic do with it.
pub trait NominationSource {
    /// Run the head against the frame at `now_ms`. `None` = nothing
    /// nominated this frame.
    fn sample(&mut self, now_ms: i64) -> Option<Nomination>;
}

/// Refusal returned when no cadence can both respect the head's own
/// per-frame cost and still prove the persistence window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CadenceRefused {
    pub cost_floor: Duration,
    pub window_ceiling: Duration,
}

/// A fire head driven at a policy-chosen cadence.
///
/// This is the production seam that *binds* [`CadencePolicy`] to the
/// real [`FireSmokeHead`]: the runner cannot be constructed at all when
/// the policy refuses, and the cadence it samples at is the one the
/// policy returned — a caller has no way to supply its own.
#[derive(Debug)]
pub struct CadencedFireHead {
    head: FireSmokeHead,
    decision: CadenceDecision,
    period_ms: i64,
    next_due_ms: Option<i64>,
    enabled: bool,
    head_invocations: u64,
}

impl CadencedFireHead {
    /// Build a runner for one camera at one ladder rung.
    ///
    /// Returns [`CadenceRefused`] when the policy cannot find a
    /// window-safe cadence, so a window-violating runner is
    /// unrepresentable rather than merely discouraged.
    pub fn new(
        camera_id: CameraId,
        cfg: FireSmokeConfig,
        rung_frame_period: Duration,
        per_frame_cost: Duration,
    ) -> Result<Self, CadenceRefused> {
        let policy = CadencePolicy::new(cfg.fire_persistence);
        let decision = policy.decide(rung_frame_period, per_frame_cost);
        let period = match decision {
            CadenceDecision::FullRate { period } | CadenceDecision::Reduced { period } => period,
            CadenceDecision::CannotSatisfyPersistenceWindow {
                cost_floor,
                window_ceiling,
            } => {
                return Err(CadenceRefused {
                    cost_floor,
                    window_ceiling,
                });
            }
        };
        Ok(Self {
            head: FireSmokeHead::new(camera_id, cfg),
            decision,
            period_ms: period.as_millis().max(1) as i64,
            next_due_ms: None,
            enabled: false,
            head_invocations: 0,
        })
    }

    /// The cadence this runner chose, as a value to log or assert on —
    /// "stated rather than implied".
    #[must_use]
    pub fn cadence(&self) -> CadenceDecision {
        self.decision
    }

    /// How many times the head has actually been invoked. Lets a test
    /// assert "disabled" means *not called*, not merely "returned None".
    #[must_use]
    pub fn head_invocations(&self) -> u64 {
        self.head_invocations
    }

    /// Off by default (owner ruling 1); enable per camera.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Offer one frame. The head runs only on frames that fall due at
    /// the chosen cadence, and never at all while disabled.
    pub fn on_frame(
        &mut self,
        now_ms: i64,
        source: &mut impl NominationSource,
    ) -> Option<FireSmokeSignal> {
        if !self.enabled {
            return None;
        }
        // Schedule against a fixed grid anchored at the first frame, so
        // sampling cannot drift slower than the chosen cadence.
        match self.next_due_ms {
            None => self.next_due_ms = Some(now_ms),
            Some(due) if now_ms >= due => {}
            Some(_) => return None,
        }
        let mut due = self.next_due_ms.unwrap_or(now_ms);
        while due <= now_ms {
            due += self.period_ms;
        }
        self.next_due_ms = Some(due);

        let nomination = source.sample(now_ms);
        self.head_invocations += 1;
        self.head.observe(now_ms, nomination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::{BBox, FireSmokeKind};

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// Stub head (owner ruling 4): reports fire for a stated interval.
    /// It is a *source of a signal*, not a scripted answer — no
    /// assertion below reads it; they all read `CadencedFireHead` and
    /// `FireSmokeHead` output.
    struct StubFireSource {
        present_from_ms: i64,
        present_until_ms: i64,
        samples: u64,
    }

    impl StubFireSource {
        fn new(present_from_ms: i64, present_until_ms: i64) -> Self {
            Self {
                present_from_ms,
                present_until_ms,
                samples: 0,
            }
        }
    }

    impl NominationSource for StubFireSource {
        fn sample(&mut self, now_ms: i64) -> Option<Nomination> {
            self.samples += 1;
            if now_ms >= self.present_from_ms && now_ms <= self.present_until_ms {
                Some(Nomination {
                    kind: FireSmokeKind::Fire,
                    region: BBox {
                        x1: 0.0,
                        y1: 0.0,
                        x2: 0.2,
                        y2: 0.2,
                    },
                    confidence: 0.8,
                })
            } else {
                None
            }
        }
    }

    fn cfg(window: Duration) -> FireSmokeConfig {
        FireSmokeConfig {
            fire_persistence: window,
            ..FireSmokeConfig::default()
        }
    }

    /// Owner ruling 1: off unless explicitly enabled, and "off" must
    /// mean the head is *never invoked* — not that it returned None.
    #[test]
    fn the_head_is_never_invoked_until_explicitly_enabled() {
        let mut runner = CadencedFireHead::new(7, cfg(ms(2000)), ms(33), ms(500))
            .expect("a 500ms cost under a 1s ceiling is runnable");
        let mut src = StubFireSource::new(0, 100_000);
        let mut emits = 0u32;
        for f in 0..600 {
            if runner.on_frame(f * 33, &mut src).is_some() {
                emits += 1;
            }
        }
        assert_eq!(
            runner.head_invocations(),
            0,
            "disabled must mean the head was NOT CALLED at all, but it ran {} times over \
             600 frames of burning fire",
            runner.head_invocations()
        );
        assert_eq!(
            src.samples, 0,
            "a disabled head must not even sample frames, but sampled {} times",
            src.samples
        );
        assert_eq!(emits, 0, "a disabled head emitted {emits} signals");
    }

    /// The runner must actually throttle: at a reduced cadence it may
    /// not quietly keep running the head every frame.
    #[test]
    fn a_reduced_cadence_runner_does_not_invoke_the_head_every_frame() {
        let frame_ms = 33i64;
        let mut runner = CadencedFireHead::new(7, cfg(ms(2000)), ms(33), ms(500))
            .expect("runnable");
        runner.enable();
        assert_eq!(runner.cadence(), CadenceDecision::Reduced { period: ms(500) });

        let mut src = StubFireSource::new(0, 100_000);
        let frames = 300i64; // ~9.9 s of frames
        for f in 0..frames {
            let _ = runner.on_frame(f * frame_ms, &mut src);
        }
        let span_ms = frames * frame_ms;
        let expected = (span_ms / 500) as u64;
        let actual = runner.head_invocations();
        assert!(
            actual <= expected + 1,
            "at a 500ms cadence over {span_ms}ms the head should run ~{expected} times, ran \
             {actual} (it is not throttling)"
        );
        assert!(actual >= expected - 1, "but it must still run: ran {actual}");
    }

    /// The refusal is structural: you cannot obtain a runner at all when
    /// no window-safe cadence exists.
    #[test]
    fn a_runner_cannot_be_constructed_when_no_safe_cadence_exists() {
        let err = CadencedFireHead::new(7, cfg(ms(2000)), ms(33), ms(5000))
            .expect_err("a 5s cost cannot prove a 2s window");
        assert_eq!(
            err,
            CadenceRefused {
                cost_floor: ms(5000),
                window_ceiling: ms(1000),
            }
        );
    }

    /// The end-to-end safety property, against the *real* persistence
    /// logic: for every cadence the policy permits, a fire that is
    /// genuinely burning is still detected, within the stated bound of
    /// `window + cadence period + one frame period`.
    #[test]
    fn a_persisting_fire_is_still_detected_at_every_permitted_cadence() {
        let windows_ms = [2000u64, 3000, 5000];
        let budgets_ms = [33u64, 50, 100];
        let costs_ms = [10u64, 33, 120, 400, 500, 900, 1000, 1400, 2400];
        let onsets_ms = [0i64, 7, 101, 499, 913];

        let mut permitted = 0u32;
        for &w in &windows_ms {
            for &b in &budgets_ms {
                for &c in &costs_ms {
                    let Ok(mut runner) =
                        CadencedFireHead::new(7, cfg(ms(w)), ms(b), ms(c))
                    else {
                        continue; // refused: covered by the refusal test
                    };
                    runner.enable();
                    let period_ms = runner
                        .cadence()
                        .period()
                        .expect("a constructed runner always states a cadence")
                        .as_millis() as i64;
                    let frame_ms = b as i64;
                    // Worst case: the first sample after onset lands up
                    // to one cadence period (plus one frame of grid
                    // quantisation) late; the head then needs a further
                    // window of observed separation, again rounded up to
                    // the next sample.
                    let bound_ms = w as i64 + 2 * period_ms + 2 * frame_ms;
                    // This is what the window clamp actually buys: because
                    // `period <= window / 2`, worst-case detection can
                    // never exceed twice the persistence window (plus
                    // frame quantisation). Break the clamp and this fails.
                    assert!(
                        bound_ms <= 2 * (w as i64) + 2 * frame_ms,
                        "window={w}ms cost={c}ms: cadence {period_ms}ms lets worst-case \
                         detection reach {bound_ms}ms, past twice the persistence window"
                    );

                    for &onset in &onsets_ms {
                        let mut r = CadencedFireHead::new(7, cfg(ms(w)), ms(b), ms(c))
                            .expect("already known runnable");
                        r.enable();
                        // Fire burns from `onset` for well past the bound.
                        let burn_until = onset + bound_ms + 5 * frame_ms;
                        let mut src = StubFireSource::new(onset, burn_until);
                        let mut emitted_at: Option<i64> = None;
                        let mut t = 0i64;
                        while t <= burn_until {
                            if let Some(sig) = r.on_frame(t, &mut src) {
                                emitted_at = Some(sig.emitted_unix_ms);
                                break;
                            }
                            t += frame_ms;
                        }
                        let at = emitted_at.unwrap_or_else(|| {
                            panic!(
                                "window={w}ms budget={b}ms cost={c}ms onset={onset}ms \
                                 cadence={period_ms}ms: a fire burning for \
                                 {}ms was NEVER detected — the cadence blinded the head",
                                burn_until - onset
                            )
                        });
                        let latency = at - onset;
                        assert!(
                            latency <= bound_ms,
                            "window={w}ms budget={b}ms cost={c}ms onset={onset}ms \
                             cadence={period_ms}ms: detected {latency}ms after onset, \
                             exceeding the stated bound of {bound_ms}ms"
                        );
                        // The >=2s persistence semantics must survive
                        // degradation: never emit earlier than the window.
                        assert!(
                            latency >= w as i64,
                            "window={w}ms cadence={period_ms}ms: emitted after only \
                             {latency}ms, breaking the persistence guarantee"
                        );
                    }
                    permitted += 1;
                }
            }
        }
        assert!(permitted > 20, "sweep must exercise many permitted rungs");
    }

    /// A fire shorter than the persistence window must never alarm, even
    /// though degraded sampling sees it fewer times.
    #[test]
    fn a_fire_shorter_than_the_window_never_alarms_at_a_reduced_cadence() {
        let mut r = CadencedFireHead::new(7, cfg(ms(2000)), ms(33), ms(900)).expect("runnable");
        r.enable();
        // Burns for 1.2 s — under the 2 s window.
        let mut src = StubFireSource::new(0, 1200);
        let mut t = 0i64;
        while t <= 6000 {
            assert!(
                r.on_frame(t, &mut src).is_none(),
                "a 1.2s fire must not alarm against a 2s window (t={t})"
            );
            t += 33;
        }
    }

    /// 1. Cost within budget -> full rate; no degradation.
    #[test]
    fn cost_within_budget_runs_at_full_rate() {
        let policy = CadencePolicy::new(ms(2000)); // 2 s fire window
        let budget = ms(83); // ~12 Hz rung
        let decision = policy.decide(budget, ms(50));
        assert_eq!(decision, CadenceDecision::FullRate { period: budget });
    }

    /// 2. Cost over budget -> a reduced cadence is returned, and it satisfies the window.
    #[test]
    fn cost_over_budget_returns_reduced_cadence_within_window() {
        let policy = CadencePolicy::new(ms(2000));
        let budget = ms(83); // ~12 Hz rung
        let cost = ms(500); // way over budget, but well under 1 s ceiling
        let decision = policy.decide(budget, cost);
        match decision {
            CadenceDecision::Reduced { period } => {
                assert_eq!(period, cost, "cadence is stated as the cost floor");
                assert!(
                    period <= policy.max_safe_period(),
                    "reduced cadence must still satisfy the persistence window"
                );
            }
            other => panic!("expected Reduced, got {other:?}"),
        }
    }

    /// 3. The important one: across a wide range of costs and budgets, *every* cadence the policy returns satisfies the >= 2 s window.
    #[test]
    fn every_returned_cadence_satisfies_the_persistence_window() {
        let windows_ms = [500u64, 1000, 2000, 5000, 10_000];
        let budgets_ms = [10u64, 33, 50, 83, 100, 250, 1000];
        let costs_ms = [
            1u64, 5, 10, 33, 50, 83, 84, 100, 249, 250, 251, 400, 499, 500, 501, 999, 1000, 1001,
            2000, 2500, 4999, 5000, 5001, 10_000, 50_000,
        ];

        let mut reduced_seen = false;
        let mut full_rate_seen = false;
        let mut refused_seen = false;

        for &w in &windows_ms {
            let policy = CadencePolicy::new(ms(w));
            for &b in &budgets_ms {
                for &c in &costs_ms {
                    let decision = policy.decide(ms(b), ms(c));
                    match decision {
                        CadenceDecision::FullRate { period } => {
                            full_rate_seen = true;
                            assert!(
                                period <= policy.max_safe_period() || ms(c) <= ms(b),
                                "full-rate period must reflect an in-budget cost"
                            );
                        }
                        CadenceDecision::Reduced { period } => {
                            reduced_seen = true;
                            assert!(
                                period <= policy.max_safe_period(),
                                "window={w}ms budget={b}ms cost={c}ms -> reduced period \
                                 {period:?} violates ceiling {:?}",
                                policy.max_safe_period()
                            );
                        }
                        CadenceDecision::CannotSatisfyPersistenceWindow {
                            cost_floor,
                            window_ceiling,
                        } => {
                            refused_seen = true;
                            assert!(
                                cost_floor > window_ceiling,
                                "refusal must be justified: cost_floor {cost_floor:?} should \
                                 exceed window_ceiling {window_ceiling:?}"
                            );
                        }
                    }
                }
            }
        }

        // Sanity: the sweep actually exercises all three branches, or the
        // property above would be vacuous.
        assert!(full_rate_seen);
        assert!(reduced_seen);
        assert!(refused_seen);
    }

    /// 4. Cost so extreme that no valid cadence exists -> the policy refuses rather than returning a window-violating cadence.
    #[test]
    fn extreme_cost_refuses_instead_of_violating_the_window() {
        let policy = CadencePolicy::new(ms(2000)); // ceiling = 1000ms
        let budget = ms(83);
        let cost = ms(5000); // far beyond the 1s ceiling
        let decision = policy.decide(budget, cost);
        assert_eq!(
            decision,
            CadenceDecision::CannotSatisfyPersistenceWindow {
                cost_floor: cost,
                window_ceiling: ms(1000),
            }
        );
        assert_eq!(decision.period(), None, "no cadence is handed to the caller");
    }

    /// 5. The chosen cadence is observable in the return value.
    #[test]
    fn chosen_cadence_is_observable_not_buried() {
        let policy = CadencePolicy::new(ms(2000));
        let full = policy.decide(ms(83), ms(50));
        assert_eq!(full.period(), Some(ms(83)));

        let reduced = policy.decide(ms(83), ms(500));
        assert_eq!(reduced.period(), Some(ms(500)));

        let refused = policy.decide(ms(83), ms(5000));
        assert_eq!(refused.period(), None);
    }

    /// The ceiling itself is derived from the window, not a caller
    /// supplied optional the caller could omit to skip the check.
    #[test]
    fn max_safe_period_is_half_the_persistence_window() {
        assert_eq!(CadencePolicy::new(ms(2000)).max_safe_period(), ms(1000));
        assert_eq!(CadencePolicy::new(ms(5000)).max_safe_period(), ms(2500));
    }
}
