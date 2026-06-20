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
