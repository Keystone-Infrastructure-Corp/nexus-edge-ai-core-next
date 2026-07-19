//! M-Event-Audit — engine implementation of the pipeline's alert-clip
//! schedule gate.
//!
//! The per-camera supervisor consults [`nexus_pipeline::AlertClipScheduleGate`]
//! at rule-fire to decide whether to build a burned-in alert clip (and mark
//! the event `alerted`). This engine impl delegates to the shared
//! [`CascadingPolicy::would_deliver`], so the arming decision reads the SAME
//! hot-reloaded `ArcSwap` cache the M7 dispatcher reads at delivery time —
//! the two can't drift across a `delivery.settings.changed` reload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use nexus_pipeline::AlertClipScheduleGate;
use nexus_sinks::policy::CascadingPolicy;

/// Gates alert-clip arming on the live M7 delivery cascade (global +
/// per-rule enabled AND within schedule) by delegating to the shared
/// [`CascadingPolicy`].
pub struct EngineAlertClipGate {
    policy: Arc<CascadingPolicy>,
}

impl EngineAlertClipGate {
    /// Wrap the shared delivery policy. Pass the same `Arc` the
    /// dispatcher + `delivery_reload` hold so all three observe the
    /// same cache.
    #[must_use]
    pub fn new(policy: Arc<CascadingPolicy>) -> Self {
        Self { policy }
    }
}

impl AlertClipScheduleGate for EngineAlertClipGate {
    fn should_build(&self, rule_id: &str, at: DateTime<Utc>) -> bool {
        self.policy.would_deliver(rule_id, at)
    }
}
