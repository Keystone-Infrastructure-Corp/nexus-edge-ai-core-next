//! M7 Phase 1 Step 5 — real `DeliveryPolicy` cascade.
//!
//! Replaces the Step 3 `AllowAllPolicy` stub with the full
//! per-rule + global cascade described in `docs/M7_DELIVERY.md`.
//! Decision shape, in evaluation order:
//!
//! 1. **Global `enabled = false`** → suppress
//!    (`SuppressionReason::GlobalDisabled`). Master kill switch
//!    wins over everything below.
//! 2. **Per-rule `enabled = false`** → suppress
//!    (`SuppressionReason::RuleDisabled`). Schedules are
//!    irrelevant for a rule that's been turned off entirely.
//! 3. **Schedule check.** The rule's schedule, if set, **fully
//!    replaces** (does not intersect) the global schedule. Off-
//!    slot → suppress (`OffScheduleRule` or `OffScheduleGlobal`
//!    depending on which schedule was consulted). If neither
//!    schedule is set, this step is a no-op and we deliver.
//!
//! Off-schedule alerts are TERMINALLY suppressed — the dispatcher
//! never backfills when a schedule re-opens. That's the
//! surveillance methodology spelled out in the doc: an alert is a
//! decision point at the moment of the event, not a queued
//! notification.
//!
//! ## Hot-reload
//!
//! The dispatcher's cached `DeliverySettings` + per-rule policy
//! map are wrapped in [`arc_swap::ArcSwap`]s. A small reload task
//! in `nexus-engine/src/main.rs` subscribes to the
//! `delivery.settings.changed` and `rule.delivery_policy.changed`
//! bus topics and calls [`CascadingPolicy::reload_settings`] /
//! [`CascadingPolicy::reload_all_rules`] on every signal. The
//! re-read is unconditional (the bus payload is a sentinel) so a
//! Lagged subscriber that misses an intermediate update still
//! converges on the right state on the next signal.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Timelike, Utc};
use chrono_tz::Tz;
use nexus_store::{OutboxRow, Store, SuppressionReason};
use nexus_types::{AlertEvent, DeliverySchedule, DeliverySettings, RuleDeliveryPolicy, RuleId};
use tracing::warn;

use crate::dispatcher::{DeliveryPolicy, DeliveryVerdict};
use crate::SinkError;

/// Cached `DeliverySettings` with the resolved `chrono-tz::Tz`
/// alongside, so the hot path doesn't re-parse the IANA string on
/// every outbox row.
#[derive(Debug, Clone)]
struct CachedSettings {
    settings: DeliverySettings,
    tz: Tz,
}

impl CachedSettings {
    fn from_db(settings: DeliverySettings) -> Self {
        let tz = parse_tz_or_warn(&settings.timezone);
        Self { settings, tz }
    }
}

/// Resolve an IANA timezone name, falling back to UTC with a
/// `warn!` if the name isn't recognised. The fallback means a typo
/// in the admin UI can't brick delivery — the dispatcher keeps
/// running with UTC semantics until the operator fixes it.
fn parse_tz_or_warn(name: &str) -> Tz {
    match name.parse::<Tz>() {
        Ok(tz) => tz,
        Err(e) => {
            warn!(
                timezone = name,
                error = %e,
                "delivery policy: unknown IANA timezone, falling back to UTC"
            );
            Tz::UTC
        }
    }
}

/// The production [`DeliveryPolicy`]. Holds `ArcSwap`-wrapped
/// caches for the global `DeliverySettings` and the per-rule
/// override map, so reloads from bus signals are lock-free for
/// readers (the dispatcher's hot path) and a swap is atomic for
/// writers (the reload task).
pub struct CascadingPolicy {
    settings: Arc<ArcSwap<CachedSettings>>,
    rule_policies: Arc<ArcSwap<HashMap<RuleId, RuleDeliveryPolicy>>>,
}

impl CascadingPolicy {
    /// Hydrate from the store. Called once at engine boot before
    /// the dispatcher loop starts. Both reads are best-effort — a
    /// failure here is a fatal boot error rather than a runtime
    /// fallback, because a dispatcher with empty caches would
    /// deliver everything (which is exactly the wrong direction
    /// for a privacy-sensitive feature).
    pub async fn hydrate(store: &Store) -> Result<Self, SinkError> {
        let settings = store
            .delivery_settings_get()
            .await
            .map_err(|e| SinkError::Permanent(format!("hydrate delivery_settings: {e}")))?;
        let policies = store
            .rule_delivery_policies_all()
            .await
            .map_err(|e| SinkError::Permanent(format!("hydrate rule_delivery_policies: {e}")))?;
        Ok(Self {
            settings: Arc::new(ArcSwap::from_pointee(CachedSettings::from_db(settings))),
            rule_policies: Arc::new(ArcSwap::from_pointee(policies)),
        })
    }

    /// Re-read `delivery_settings` from the store and swap the
    /// cache. Invoked by the reload task on every
    /// `delivery.settings.changed` bus signal. Logs and returns
    /// `Err` on store failure — the caller (the reload task)
    /// keeps running with the previous cached value, which is the
    /// only safe behaviour: stale-deny would over-deliver during
    /// the outage.
    pub async fn reload_settings(&self, store: &Store) -> Result<(), SinkError> {
        let settings = store
            .delivery_settings_get()
            .await
            .map_err(|e| SinkError::Transient(format!("reload delivery_settings: {e}")))?;
        self.settings
            .store(Arc::new(CachedSettings::from_db(settings)));
        Ok(())
    }

    /// Re-read every rule's policy and swap the cache. Invoked on
    /// `rule.delivery_policy.changed`. We reload the whole map
    /// rather than the single mutated rule because (a) the bus
    /// payload is a sentinel by design, (b) the table is tiny
    /// (operators have ≪ 1000 rules), and (c) a full reload also
    /// handles rule deletion correctly without a separate event.
    pub async fn reload_all_rules(&self, store: &Store) -> Result<(), SinkError> {
        let policies = store
            .rule_delivery_policies_all()
            .await
            .map_err(|e| SinkError::Transient(format!("reload rule_delivery_policies: {e}")))?;
        self.rule_policies.store(Arc::new(policies));
        Ok(())
    }

    /// Synchronous "would an alert from `rule_id` be delivered at
    /// `now`?" — the exact cascade the dispatcher applies at delivery
    /// time (global enabled AND rule enabled AND within schedule),
    /// collapsed to a bool.
    ///
    /// M-Event-Audit exposes this so callers that must decide *before*
    /// enqueue — the supervisor's alert-clip arming and the
    /// `events.alerted` stamp — read the SAME hot-reloaded `ArcSwap`
    /// caches the dispatcher reads. Sharing the cache is what keeps the
    /// arming decision and the later delivery decision from ever
    /// disagreeing across a `delivery.settings.changed` reload.
    #[must_use]
    pub fn would_deliver(&self, rule_id: &str, now: DateTime<Utc>) -> bool {
        let settings_arc = self.settings.load_full();
        let cached = &*settings_arc;
        let rule_policies_arc = self.rule_policies.load_full();
        let rule_policy = rule_policies_arc.get(rule_id);
        matches!(
            evaluate_cascade(&cached.settings, cached.tz, rule_policy, now),
            DeliveryVerdict::Deliver
        )
    }
}

#[async_trait]
impl DeliveryPolicy for CascadingPolicy {
    async fn evaluate(
        &self,
        _row: &OutboxRow,
        event: &AlertEvent,
        now: DateTime<Utc>,
    ) -> DeliveryVerdict {
        let settings_arc = self.settings.load_full();
        let cached = &*settings_arc;
        let rule_policies_arc = self.rule_policies.load_full();
        let rule_policy = rule_policies_arc.get(&event.rule_id);
        evaluate_cascade(&cached.settings, cached.tz, rule_policy, now)
    }
}

/// Pure cascade function — no I/O, no store, no Arc/Atomic. The
/// unit tests below drive the whole decision matrix through this
/// without booting the policy struct.
///
/// `tz` is the timezone the schedule grid is interpreted in; pass
/// `Tz::UTC` when the operator hasn't set one.
pub fn evaluate_cascade(
    settings: &DeliverySettings,
    tz: Tz,
    rule_policy: Option<&RuleDeliveryPolicy>,
    now: DateTime<Utc>,
) -> DeliveryVerdict {
    // (1) Global kill switch.
    if !settings.enabled {
        return DeliveryVerdict::Suppressed(SuppressionReason::GlobalDisabled);
    }

    // (2) Per-rule kill switch.
    if let Some(p) = rule_policy {
        if !p.enabled {
            return DeliveryVerdict::Suppressed(SuppressionReason::RuleDisabled);
        }
    }

    // (3) Schedule check — rule schedule REPLACES global if present.
    let (schedule, off_reason) = match rule_policy.and_then(|p| p.schedule.as_ref()) {
        Some(s) => (Some(s), SuppressionReason::OffScheduleRule),
        None => (
            settings.schedule.as_ref(),
            SuppressionReason::OffScheduleGlobal,
        ),
    };
    if let Some(s) = schedule {
        if !schedule_allows(s, tz, now) {
            return DeliveryVerdict::Suppressed(off_reason);
        }
    }

    DeliveryVerdict::Deliver
}

/// Resolve `now` into local time in `tz`, then look up the
/// half-hour slot for the resulting weekday. Out-of-range indices
/// (shouldn't happen given the slot math) resolve to `false` per
/// [`DeliverySchedule::is_allowed`]'s contract.
fn schedule_allows(schedule: &DeliverySchedule, tz: Tz, now: DateTime<Utc>) -> bool {
    let local = now.with_timezone(&tz);
    let day = local.weekday().num_days_from_monday() as usize;
    let slot = (local.hour() * 2 + if local.minute() >= 30 { 1 } else { 0 }) as usize;
    schedule.is_allowed(day, slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn settings(enabled: bool, schedule: Option<DeliverySchedule>) -> DeliverySettings {
        DeliverySettings {
            enabled,
            schedule,
            timezone: "UTC".into(),
            attach_alert_clip: true,
            updated_at: Utc::now(),
        }
    }

    /// Build a one-cell-true schedule: every slot is `false`
    /// except (day, slot). Lets tests pin a single point on the
    /// grid without manually filling 336 booleans.
    fn one_slot_on(day: usize, slot: usize) -> DeliverySchedule {
        let mut s = DeliverySchedule::never();
        s.grid[day][slot] = true;
        s
    }

    /// A UTC instant whose local-time slot is known: 2026-01-05
    /// 12:15Z is a Monday at 12:15 → day=0, slot=24.
    fn monday_noon_utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 5, 12, 15, 0).unwrap()
    }

    #[test]
    fn allow_when_everything_open() {
        let verdict = evaluate_cascade(&settings(true, None), Tz::UTC, None, monday_noon_utc());
        assert_eq!(verdict, DeliveryVerdict::Deliver);
    }

    #[test]
    fn global_disabled_short_circuits_everything() {
        // Even with a permissive rule policy + always-on
        // schedule, global disabled wins.
        let rule_policy = RuleDeliveryPolicy {
            enabled: true,
            schedule: Some(DeliverySchedule::always()),
        };
        let verdict = evaluate_cascade(
            &settings(false, Some(DeliverySchedule::always())),
            Tz::UTC,
            Some(&rule_policy),
            monday_noon_utc(),
        );
        assert_eq!(
            verdict,
            DeliveryVerdict::Suppressed(SuppressionReason::GlobalDisabled)
        );
    }

    #[test]
    fn rule_disabled_wins_over_global_schedule() {
        let rule_policy = RuleDeliveryPolicy {
            enabled: false,
            schedule: None,
        };
        let verdict = evaluate_cascade(
            &settings(true, Some(DeliverySchedule::always())),
            Tz::UTC,
            Some(&rule_policy),
            monday_noon_utc(),
        );
        assert_eq!(
            verdict,
            DeliveryVerdict::Suppressed(SuppressionReason::RuleDisabled)
        );
    }

    #[test]
    fn off_global_schedule_suppresses() {
        // Global says only Friday 8a–10a; we're on Monday noon.
        let mut sched = DeliverySchedule::never();
        sched.grid[4][16] = true;
        let verdict = evaluate_cascade(
            &settings(true, Some(sched)),
            Tz::UTC,
            None,
            monday_noon_utc(),
        );
        assert_eq!(
            verdict,
            DeliveryVerdict::Suppressed(SuppressionReason::OffScheduleGlobal)
        );
    }

    #[test]
    fn rule_schedule_replaces_global_open() {
        // Global schedule says nope at this slot, but the rule
        // override says yes — rule wins.
        let global_never = DeliverySchedule::never();
        let rule_policy = RuleDeliveryPolicy {
            enabled: true,
            schedule: Some(one_slot_on(0, 24)), // Mon, 12:00–12:30
        };
        let verdict = evaluate_cascade(
            &settings(true, Some(global_never)),
            Tz::UTC,
            Some(&rule_policy),
            monday_noon_utc(),
        );
        assert_eq!(verdict, DeliveryVerdict::Deliver);
    }

    #[test]
    fn rule_schedule_replaces_global_closed() {
        // Global always-on, but the rule says only Friday — and
        // we're on Monday. Rule wins → suppressed by RULE schedule.
        let rule_policy = RuleDeliveryPolicy {
            enabled: true,
            schedule: Some(one_slot_on(4, 16)),
        };
        let verdict = evaluate_cascade(
            &settings(true, Some(DeliverySchedule::always())),
            Tz::UTC,
            Some(&rule_policy),
            monday_noon_utc(),
        );
        assert_eq!(
            verdict,
            DeliveryVerdict::Suppressed(SuppressionReason::OffScheduleRule)
        );
    }

    #[test]
    fn rule_inherits_global_when_no_override() {
        // No rule policy at all → global schedule applies.
        let verdict = evaluate_cascade(
            &settings(true, Some(DeliverySchedule::always())),
            Tz::UTC,
            None,
            monday_noon_utc(),
        );
        assert_eq!(verdict, DeliveryVerdict::Deliver);

        let verdict2 = evaluate_cascade(
            &settings(true, Some(DeliverySchedule::never())),
            Tz::UTC,
            None,
            monday_noon_utc(),
        );
        assert_eq!(
            verdict2,
            DeliveryVerdict::Suppressed(SuppressionReason::OffScheduleGlobal)
        );
    }

    #[test]
    fn no_schedule_means_always_on() {
        // Both global and rule schedule are None — enabled-only
        // wins. No accidental suppression.
        let rule_policy = RuleDeliveryPolicy {
            enabled: true,
            schedule: None,
        };
        let verdict = evaluate_cascade(
            &settings(true, None),
            Tz::UTC,
            Some(&rule_policy),
            monday_noon_utc(),
        );
        assert_eq!(verdict, DeliveryVerdict::Deliver);
    }

    #[test]
    fn slot_math_handles_half_hours_correctly() {
        // 12:29 → slot 24; 12:30 → slot 25. Pin the boundary.
        let only_25 = one_slot_on(0, 25);
        let twelve_29 = Utc.with_ymd_and_hms(2026, 1, 5, 12, 29, 59).unwrap();
        let twelve_30 = Utc.with_ymd_and_hms(2026, 1, 5, 12, 30, 0).unwrap();

        let v1 = evaluate_cascade(
            &settings(true, Some(only_25.clone())),
            Tz::UTC,
            None,
            twelve_29,
        );
        assert_eq!(
            v1,
            DeliveryVerdict::Suppressed(SuppressionReason::OffScheduleGlobal)
        );

        let v2 = evaluate_cascade(&settings(true, Some(only_25)), Tz::UTC, None, twelve_30);
        assert_eq!(v2, DeliveryVerdict::Deliver);
    }

    #[test]
    fn timezone_shifts_grid_evaluation() {
        // 12:00 UTC on Monday is 04:00 in Los Angeles (PST UTC-8
        // in January — no DST yet). A schedule that opens only
        // LA-local Monday 04:00–04:30 should deliver; an LA-local
        // Monday noon should not.
        let la = "America/Los_Angeles".parse::<Tz>().unwrap();

        // slot 8 = 04:00–04:30
        let only_la_4am_mon = one_slot_on(0, 8);
        let now_utc = Utc.with_ymd_and_hms(2026, 1, 5, 12, 15, 0).unwrap();
        assert_eq!(
            evaluate_cascade(&settings(true, Some(only_la_4am_mon)), la, None, now_utc,),
            DeliveryVerdict::Deliver
        );

        // slot 24 = noon. Same UTC instant, but in LA it's 04:15
        // → grid says off.
        let only_la_noon_mon = one_slot_on(0, 24);
        assert_eq!(
            evaluate_cascade(&settings(true, Some(only_la_noon_mon)), la, None, now_utc,),
            DeliveryVerdict::Suppressed(SuppressionReason::OffScheduleGlobal)
        );
    }

    #[test]
    fn parse_tz_falls_back_to_utc_on_garbage() {
        // Sanity check the warn-and-fallback behaviour — a typo
        // in the admin UI shouldn't crash the dispatcher.
        let tz = parse_tz_or_warn("Mars/Olympus_Mons");
        assert_eq!(tz, Tz::UTC);
    }

    /// Build a `CascadingPolicy` from a settings snapshot + rule map so
    /// the `would_deliver` tests exercise the real `ArcSwap`-backed
    /// read path (not just the pure `evaluate_cascade`).
    fn policy_with(
        settings: DeliverySettings,
        rules: HashMap<RuleId, RuleDeliveryPolicy>,
    ) -> CascadingPolicy {
        CascadingPolicy {
            settings: Arc::new(ArcSwap::from_pointee(CachedSettings::from_db(settings))),
            rule_policies: Arc::new(ArcSwap::from_pointee(rules)),
        }
    }

    #[test]
    fn would_deliver_true_when_schedule_open() {
        let policy = policy_with(settings(true, None), HashMap::new());
        let rid: RuleId = "any-rule".into();
        assert!(policy.would_deliver(&rid, monday_noon_utc()));
    }

    #[test]
    fn would_deliver_false_off_global_schedule() {
        // Global opens only Friday 08:00–08:30; we ask on Monday noon.
        let mut sched = DeliverySchedule::never();
        sched.grid[4][16] = true;
        let policy = policy_with(settings(true, Some(sched)), HashMap::new());
        let rid: RuleId = "any-rule".into();
        assert!(!policy.would_deliver(&rid, monday_noon_utc()));
    }

    #[test]
    fn would_deliver_honours_per_rule_override() {
        // Global never; the rule opens Monday 12:00–12:30 → rule wins.
        let rid: RuleId = "open-rule".into();
        let mut rules = HashMap::new();
        rules.insert(
            rid.clone(),
            RuleDeliveryPolicy {
                enabled: true,
                schedule: Some(one_slot_on(0, 24)),
            },
        );
        let policy = policy_with(settings(true, Some(DeliverySchedule::never())), rules);
        assert!(policy.would_deliver(&rid, monday_noon_utc()));

        // A rule with no override falls back to the (never) global.
        let other: RuleId = "inherits-global".into();
        assert!(!policy.would_deliver(&other, monday_noon_utc()));
    }

    #[test]
    fn would_deliver_false_when_globally_disabled() {
        let policy = policy_with(
            settings(false, Some(DeliverySchedule::always())),
            HashMap::new(),
        );
        let rid: RuleId = "any-rule".into();
        assert!(!policy.would_deliver(&rid, monday_noon_utc()));
    }
}
