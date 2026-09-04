//! SPEC-037 — supervisor-facing Tier-0 emergency dispatch abstraction.
//!
//! The per-camera supervisor observes every frame's post-tracking,
//! post-static-filter object set (the same `dynamic_tracked` slice the
//! rule evaluator sees) and hands it to this trait. *What* counts as a
//! Tier-0 signal, and how it is corroborated / rate-limited / delivered,
//! is entirely `nexus-sinks::emergency` policy — resolving that needs
//! the concrete `EmergencyPolicy` / `Tier0Registry` / `EmergencyRateLimiter`
//! / `EmergencyDelivery` types, plus the engine's tunnel-liveness signal
//! and its `Store` + `SinkRegistry`, all of which live in `nexus-engine`.
//!
//! To keep `nexus-pipeline` free of a `nexus-sinks` dependency (mirroring
//! [`crate::sink_router`]'s pattern precisely), the supervisor depends
//! only on this trait; `nexus-engine` supplies the concrete
//! implementation. Test harnesses that don't exercise the emergency path
//! use [`NoopEmergencyDispatch`], which observes every frame and does
//! nothing — the supervisor's per-frame behaviour is unchanged from
//! pre-SPEC-037.
//!
//! ## Non-blocking contract
//!
//! The supervisor calls [`EmergencyDispatch::observe`] from inside a
//! `tokio::spawn`ed task fed an owned snapshot (see `supervisor.rs`), so
//! **no** implementation of this trait — however slow, however much I/O
//! it performs — can stall the frame that produced the observation.
//! Implementations are still expected to be reasonably cheap since a
//! misbehaving one can still starve the async runtime's worker pool
//! under enough concurrent cameras, but correctness of "does not block a
//! frame" is structural (owned by the spawn point), not a property this
//! trait's implementations must individually uphold.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use nexus_types::{CameraId, TrackedObject};

/// Consulted once per frame with that frame's tracked objects. See the
/// module docs for the non-blocking contract the supervisor provides
/// around every call.
#[async_trait::async_trait]
pub trait EmergencyDispatch: Send + Sync {
    /// Observe one frame's tracked objects for `camera_id`, captured at
    /// `at`. Implementations decide internally whether anything in
    /// `tracked` constitutes a Tier-0 signal (SPEC-037) and, if so,
    /// drive the corroboration/rate-limit/delivery sequence themselves.
    async fn observe(&self, camera_id: CameraId, tracked: Arc<Vec<TrackedObject>>, at: DateTime<Utc>);
}

/// An [`EmergencyDispatch`] that observes nothing. Used by tests /
/// harnesses that don't wire the Tier-0 emergency path, so the
/// supervisor's per-frame call degrades to a no-op (pre-SPEC-037
/// behaviour).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEmergencyDispatch;

#[async_trait::async_trait]
impl EmergencyDispatch for NoopEmergencyDispatch {
    async fn observe(
        &self,
        _camera_id: CameraId,
        _tracked: Arc<Vec<TrackedObject>>,
        _at: DateTime<Utc>,
    ) {
    }
}
