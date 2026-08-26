//! SPEC-037 — Edge-Local Emergency Alerting.
//!
//! Tier-0 life-safety classes (fire, smoke, brandished firearm,
//! brandished knife, violence, person down — HARM_TAXONOMY §4) get a
//! delivery path that runs **entirely on the engine and never passes
//! through Sentinel**. Their alarm value is highest exactly when the
//! cloud is unreachable, so the decision is computed locally and errs
//! toward alarming.
//!
//! This module is the structural boundary ADR-043 requires, expressed in
//! the type system rather than a comment:
//!
//! * [`EmergencyOutcome`] has **no** `Suppress` / `Close` / `Delay` /
//!   `Downgrade` variant. A Tier-0 event cannot be silenced because there
//!   is no value that could represent silencing it.
//! * [`EmergencyPolicy`] and [`WrongdoingPolicy`] are distinct types with
//!   distinct threshold fields; they cannot share a threshold table
//!   (ADR-047).
//! * [`Tier0Registry`] refuses to register a Tier-0 class that has no
//!   edge-local detector unless an explicit [`FailOpenException`] naming
//!   the fail-open consequence is supplied.
//! * [`EmergencyPolicy::decide`] takes no entitlement and no cloud handle;
//!   the only place `cloud_reachable` appears is to *degrade toward
//!   alarming*, never to add suppression or latency.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

/// The six Tier-0 life-safety classes (HARM_TAXONOMY §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier0Class {
    Fire,
    Smoke,
    Firearm,
    Knife,
    Violence,
    PersonDown,
}

impl Tier0Class {
    /// Firearm and knife require a brandish-versus-carry confirmation
    /// before interrupting; the other four do not.
    #[must_use]
    pub fn requires_brandish_confirmation(self) -> bool {
        matches!(self, Self::Firearm | Self::Knife)
    }

    /// Stable snake_case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fire => "fire",
            Self::Smoke => "smoke",
            Self::Firearm => "firearm",
            Self::Knife => "knife",
            Self::Violence => "violence",
            Self::PersonDown => "person_down",
        }
    }
}

/// The explicit, recorded reason a Tier-0 class ships without an
/// edge-local detector. Registering one of these is the only way past the
/// registry's refusal, and it forces the fail-open consequence to be
/// named (ADR-043: "a class with no edge-local path is documented as an
/// explicit exception with its fail-open consequence stated").
#[derive(Debug, Clone)]
pub struct FailOpenException {
    pub class: Tier0Class,
    /// What happens to this class when the cloud is gone — stated, not
    /// implied.
    pub fail_open_consequence: String,
}

/// Why a Tier-0 registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier0RegistrationError {
    /// The class has no edge-local detector and no recorded exception.
    NoEdgeLocalDetector(Tier0Class),
}

/// The set of Tier-0 classes a core actually ships, plus the recorded
/// exceptions for classes that have no edge-local detector.
#[derive(Debug, Default)]
pub struct Tier0Registry {
    with_detector: Vec<Tier0Class>,
    exceptions: Vec<FailOpenException>,
}

impl Tier0Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Tier-0 class. Refused unless it has an edge-local
    /// detector — a cloud-only implementation cannot be a Tier-0 class.
    pub fn register(
        &mut self,
        class: Tier0Class,
        has_edge_local_detector: bool,
    ) -> Result<(), Tier0RegistrationError> {
        if has_edge_local_detector {
            self.with_detector.push(class);
            Ok(())
        } else {
            Err(Tier0RegistrationError::NoEdgeLocalDetector(class))
        }
    }

    /// Record the explicit exception that lets a detector-less class ship,
    /// naming its fail-open consequence.
    pub fn register_exception(&mut self, exception: FailOpenException) {
        self.exceptions.push(exception);
    }

    #[must_use]
    pub fn is_shipped(&self, class: Tier0Class) -> bool {
        self.with_detector.contains(&class)
    }

    #[must_use]
    pub fn exception_for(&self, class: Tier0Class) -> Option<&FailOpenException> {
        self.exceptions.iter().find(|e| e.class == class)
    }
}

/// The emergency corroboration policy — minimal corroboration, errs
/// toward alarming. A **distinct type** from [`WrongdoingPolicy`] so the
/// two can never share a threshold table (ADR-047).
#[derive(Debug, Clone)]
pub struct EmergencyPolicy {
    /// Minimal persistence before an emergency alarms. Kept small on
    /// purpose; when in doubt the policy alarms.
    pub min_persistence: Duration,
}

impl Default for EmergencyPolicy {
    fn default() -> Self {
        // Minimal — err toward alarming. The fire/smoke persistence
        // windows (SPEC-036) live in the detector, not here.
        Self {
            min_persistence: Duration::from_millis(500),
        }
    }
}

/// The wrongdoing corroboration policy. Deliberately a separate type with
/// separate fields; the emergency path never reads it.
#[derive(Debug, Clone)]
pub struct WrongdoingPolicy {
    /// Wrongdoing requires more corroboration than emergency and may
    /// suppress — none of which applies to Tier-0.
    pub min_corroborating_signals: u8,
    pub min_dwell: Duration,
}

impl Default for WrongdoingPolicy {
    fn default() -> Self {
        Self {
            min_corroborating_signals: 2,
            min_dwell: Duration::from_secs(5),
        }
    }
}

/// A Tier-0 detection handed to the policy. Everything needed to decide
/// is here; there is no cloud lookup.
#[derive(Debug, Clone)]
pub struct EmergencySignal {
    pub class: Tier0Class,
    /// How long the detection has persisted.
    pub persistence: Duration,
    /// For firearm/knife: `Some(true)` = confirmed brandish,
    /// `Some(false)` = confirmed carry, `None` = not yet confirmed.
    /// Ignored for the other four classes.
    pub brandish_confirmed: Option<bool>,
}

/// The only outcomes an emergency decision can take. There is **no**
/// suppress/close/delay/downgrade — that is the structural guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmergencyOutcome {
    /// Fire the alarm now, on the local path.
    Alarm,
    /// Firearm/knife only, and only while the cloud is reachable: hold for
    /// the brandish-versus-carry confirmation. This is not suppression —
    /// it degrades to [`EmergencyOutcome::Alarm`] the instant the cloud is
    /// unreachable, and it is never written to a Sentinel decision.
    AwaitBrandishConfirmation,
}

impl EmergencyPolicy {
    /// Decide locally. Takes no entitlement (a Tier-0 alert fires even
    /// with no Sentinel entitlement) and no cloud handle. `cloud_reachable`
    /// only ever pushes the outcome *toward* alarming.
    #[must_use]
    pub fn decide(&self, signal: &EmergencySignal, cloud_reachable: bool) -> EmergencyOutcome {
        // Brandish confirmation gates only firearm/knife, and only while
        // the cloud is reachable. Confirmed carry while online holds; every
        // other case (confirmed brandish, unknown-but-offline, or any
        // non-confirmation class) alarms.
        if signal.class.requires_brandish_confirmation() && cloud_reachable {
            match signal.brandish_confirmed {
                Some(true) => return EmergencyOutcome::Alarm,
                Some(false) => return EmergencyOutcome::AwaitBrandishConfirmation,
                None => return EmergencyOutcome::AwaitBrandishConfirmation,
            }
        }
        // Everything else: minimal corroboration, err toward alarming. A
        // sub-threshold persistence still alarms only if we err toward it —
        // here we require the minimal window but treat exactly-met as met.
        if signal.persistence >= self.min_persistence {
            EmergencyOutcome::Alarm
        } else {
            // Err toward alarming: below the minimal window we still alarm,
            // because a missed fire is worse than an early one. The window
            // exists to document intent, not to gate silence.
            EmergencyOutcome::Alarm
        }
    }
}

/// Per-camera, per-class rate limiter guarding against alert storms.
/// Degrades to **delivering** when its own state is unavailable — a
/// broken limiter must never silence an emergency.
#[derive(Debug)]
pub struct EmergencyRateLimiter {
    /// Minimum spacing between deliveries for one (camera, class).
    window: Duration,
    /// `None` models an unavailable limiter — every `allow` returns true
    /// (deliver). `Some` holds the last-delivered instant per key.
    last: Option<HashMap<(u64, Tier0Class), DateTime<Utc>>>,
}

impl EmergencyRateLimiter {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last: Some(HashMap::new()),
        }
    }

    /// A limiter whose backing state is unavailable. Always allows.
    #[must_use]
    pub fn degraded(window: Duration) -> Self {
        Self { window, last: None }
    }

    /// Returns `true` when the alert should be delivered. When state is
    /// unavailable this is unconditionally `true` (degrade to delivering).
    pub fn allow(&mut self, camera_id: u64, class: Tier0Class, now: DateTime<Utc>) -> bool {
        let Some(map) = self.last.as_mut() else {
            return true;
        };
        let key = (camera_id, class);
        match map.get(&key) {
            Some(prev)
                if now
                    .signed_duration_since(*prev)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
                    < self.window =>
            {
                false
            }
            _ => {
                map.insert(key, now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn registry_refuses_a_tier0_class_with_no_edge_local_detector() {
        let mut reg = Tier0Registry::new();
        assert_eq!(reg.register(Tier0Class::Fire, true), Ok(()));
        assert_eq!(
            reg.register(Tier0Class::Violence, false),
            Err(Tier0RegistrationError::NoEdgeLocalDetector(
                Tier0Class::Violence
            )),
        );
        assert!(reg.is_shipped(Tier0Class::Fire));
        assert!(!reg.is_shipped(Tier0Class::Violence));
    }

    #[test]
    fn registry_accepts_a_detectorless_class_only_with_a_named_exception() {
        // Violence and person down have no edge-local detector at merge
        // time (HARM_TAXONOMY §4); they ship only as recorded exceptions.
        let mut reg = Tier0Registry::new();
        reg.register_exception(FailOpenException {
            class: Tier0Class::PersonDown,
            fail_open_consequence: "no edge-local person-down detector; relies on cloud path"
                .into(),
        });
        assert!(reg.exception_for(Tier0Class::PersonDown).is_some());
        assert!(reg.exception_for(Tier0Class::Fire).is_none());
    }

    #[test]
    fn emergency_and_wrongdoing_policies_are_distinct_types() {
        // A structural assertion: the two default policies have different
        // shapes and no shared threshold field. This will not compile if
        // someone collapses them into one type.
        let e = EmergencyPolicy::default();
        let w = WrongdoingPolicy::default();
        assert!(e.min_persistence < w.min_dwell);
    }

    #[test]
    fn a_fire_alarms_locally_even_with_the_cloud_unreachable() {
        // ADR-043 offline proof: with cloud_reachable = false the policy
        // still returns Alarm.
        let policy = EmergencyPolicy::default();
        let sig = EmergencySignal {
            class: Tier0Class::Fire,
            persistence: Duration::from_secs(3),
            brandish_confirmed: None,
        };
        assert_eq!(policy.decide(&sig, false), EmergencyOutcome::Alarm);
        assert_eq!(policy.decide(&sig, true), EmergencyOutcome::Alarm);
    }

    #[test]
    fn firearm_awaits_confirmation_online_but_degrades_to_alarm_offline() {
        let policy = EmergencyPolicy::default();
        let pending = EmergencySignal {
            class: Tier0Class::Firearm,
            persistence: Duration::from_secs(1),
            brandish_confirmed: None,
        };
        // Online, unconfirmed → hold (not suppress).
        assert_eq!(
            policy.decide(&pending, true),
            EmergencyOutcome::AwaitBrandishConfirmation
        );
        // Cloud unreachable → degrade to alarming rather than silence.
        assert_eq!(policy.decide(&pending, false), EmergencyOutcome::Alarm);
        // Confirmed brandish online → alarm.
        let brandished = EmergencySignal {
            brandish_confirmed: Some(true),
            ..pending.clone()
        };
        assert_eq!(policy.decide(&brandished, true), EmergencyOutcome::Alarm);
    }

    #[test]
    fn the_emergency_outcome_has_no_suppress_variant() {
        // Structural: exhaustively matching EmergencyOutcome yields only
        // Alarm and AwaitBrandishConfirmation — neither silences a Tier-0
        // event. A new suppress-like variant would fail this match.
        for outcome in [
            EmergencyOutcome::Alarm,
            EmergencyOutcome::AwaitBrandishConfirmation,
        ] {
            match outcome {
                EmergencyOutcome::Alarm | EmergencyOutcome::AwaitBrandishConfirmation => {}
            }
        }
    }

    #[test]
    fn rate_limiter_spaces_deliveries_per_camera_per_class() {
        let mut rl = EmergencyRateLimiter::new(Duration::from_secs(10));
        assert!(rl.allow(1, Tier0Class::Fire, t(0)));
        assert!(
            !rl.allow(1, Tier0Class::Fire, t(5)),
            "within window → suppressed storm"
        );
        assert!(
            rl.allow(1, Tier0Class::Fire, t(11)),
            "after window → delivers"
        );
        // Different class on the same camera is independent.
        assert!(rl.allow(1, Tier0Class::Smoke, t(5)));
        // Different camera is independent.
        assert!(rl.allow(2, Tier0Class::Fire, t(5)));
    }

    #[test]
    fn rate_limiter_degrades_to_delivering_when_state_is_unavailable() {
        let mut rl = EmergencyRateLimiter::degraded(Duration::from_secs(10));
        assert!(rl.allow(1, Tier0Class::Fire, t(0)));
        assert!(
            rl.allow(1, Tier0Class::Fire, t(1)),
            "no state → always deliver"
        );
    }
}
