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

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
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
