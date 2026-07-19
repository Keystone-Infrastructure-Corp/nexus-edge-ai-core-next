//! M7 per-rule sink routing — supervisor-facing abstraction.
//!
//! The per-camera supervisor enqueues every alert it records into the
//! `alert_sink_outbox` so the M7 dispatcher can deliver it. *Which*
//! sinks a given alert is enqueued to depends on the firing rule's
//! `sinks` allow-list (empty = all configured sinks) intersected with
//! the set of sinks that actually exist right now. Resolving that
//! needs both the live [`nexus_rules::RuleEvaluator`] (for the rule's
//! `sinks` field) and the engine's `SinkRegistry` (for the configured
//! set) — both of which live in `nexus-engine`.
//!
//! To keep `nexus-pipeline` free of a `nexus-sinks` dependency, the
//! supervisor depends only on this trait; `nexus-engine` supplies the
//! concrete implementation. Test harnesses that don't exercise
//! delivery use [`NoopSinkRouter`], which routes to nothing and so
//! makes `record_event_and_enqueue` behave exactly like the pre-M7
//! `record_event` (event row only, no outbox rows).

use chrono::{DateTime, Utc};

/// Resolves the alert-delivery sink ids an alert for a given rule
/// should be enqueued to. See the module docs for the resolution
/// contract; implementations are expected to be cheap (called once
/// per recorded alert event).
pub trait SinkRouter: Send + Sync {
    /// Sink ids (`"<kind>:<name>"`) to enqueue an alert for `rule_id`
    /// into. An empty result records the event with **no** outbox
    /// rows (no delivery).
    fn sinks_for(&self, rule_id: &str) -> Vec<String>;
}

/// A [`SinkRouter`] that routes to nothing. Used by tests / harnesses
/// that don't wire the M7 dispatcher, so the supervisor's enqueue path
/// degrades to a plain event-record with no outbox side effects.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSinkRouter;

impl SinkRouter for NoopSinkRouter {
    fn sinks_for(&self, _rule_id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Decides whether an alert clip should be built — and the event
/// marked `alerted` — for a rule firing at a given instant. `true`
/// iff the alert *would be delivered* under the active M7 delivery
/// cascade (global enabled AND rule enabled AND within schedule).
///
/// M-Event-Audit: the per-camera supervisor consults this at rule-fire,
/// BEFORE arming the expensive decode → burn-in → re-encode alert clip,
/// so an off-schedule match is still logged + linked to its motion clip
/// but never builds an alert clip and never enters the cloud alert
/// queue. `nexus-engine` supplies the concrete implementation over the
/// shared `CascadingPolicy` (the same hot-reloaded cache the dispatcher
/// reads, so arming and delivery can't disagree). Harnesses that don't
/// wire delivery use [`NoopAlertClipScheduleGate`].
pub trait AlertClipScheduleGate: Send + Sync {
    /// `true` when an alert from `rule_id` at `at` would be delivered
    /// (build the alert clip + mark the event `alerted`); `false` for
    /// an off-schedule / suppressed match (motion clip only).
    fn should_build(&self, rule_id: &str, at: DateTime<Utc>) -> bool;
}

/// An [`AlertClipScheduleGate`] that always arms — the
/// pre-M-Event-Audit behaviour where every rule-fire builds an alert
/// clip. Used by tests / harnesses that don't wire the M7 delivery
/// cascade.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAlertClipScheduleGate;

impl AlertClipScheduleGate for NoopAlertClipScheduleGate {
    fn should_build(&self, _rule_id: &str, _at: DateTime<Utc>) -> bool {
        true
    }
}
