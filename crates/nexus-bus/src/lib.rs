//! Typed in-process (and optionally NATS) message bus.
//!
//! The bus carries **metadata only** — never frame pixels. Frames flow on
//! the `LatestFrameCache` side-channel (see ARCHITECTURE.md L7).

#![forbid(unsafe_code)]

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use parking_lot::RwLock;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::StreamExt;

// ---------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------

pub mod topic {
    pub const FRAME_METADATA: &str = "frame.metadata";
    /// M_PERF_CROWD F1 — bandwidth-relief companion to
    /// [`FRAME_METADATA`]. Same per-frame cadence, payload is
    /// `FrameMetadataLite` (no per-object `attributes` map). Default
    /// SSE subscribers receive this topic; `?attributes=full` opts
    /// in to the full topic for the attributes panel.
    pub const FRAME_METADATA_LITE: &str = "frame.metadata.lite";
    pub const ALERT_EVENT: &str = "alert.event";
    pub const CONFIG_CHANGED: &str = "config.changed";
    pub const PIPELINE_STATUS: &str = "pipeline.status";
    /// Watermark sampler crossed `panic_watermark_pct`. Payload is
    /// `StoragePanicEvent` (free pct + reason). Emitted ONCE per
    /// Ok→Panic transition (debounced); the matching exit event is
    /// emitted on Panic→Ok.
    pub const STORAGE_PANIC: &str = "storage.panic";

    // ---- M2.2 cold-mirror topics ----
    //
    // These are metadata-only events; never carry frame bytes or the
    // clip blob itself. Subscribers fetch the actual payload from
    // `motion_clips` + the relevant backend.

    /// Recorder finished writing a clip to the hot backend. Payload
    /// is `ClipClosedEvent { clip_id, camera_id, hot_handle,
    /// hot_path, size_bytes, sha256 }`. Drives the cold replicator
    /// follower; the replicator also has a 5-min polling backstop
    /// in case the bus is lossy on a Lagged subscriber.
    pub const CLIP_CLOSED: &str = "clip.closed";

    /// Cold backend `health()` returned an error or an upload
    /// failed in a way that suggests the link is down. Emitted at
    /// most ONCE per outage (the replicator de-bounces follow-up
    /// failures so a 60 GiB backlog does not spam the bus).
    /// Payload: `StorageColdUnreachableEvent { handle, kind, error,
    /// pending_bytes }`.
    pub const STORAGE_COLD_UNREACHABLE: &str = "storage.cold.unreachable";

    /// Storage-safety soft-evict step ran: hot file was unlinked
    /// and `hot_handle`/`hot_path` cleared on a clip whose cold
    /// copy is intact. Payload: `ClipHotEvictedEvent { clip_id,
    /// camera_id, freed_bytes, cold_handle }`. UI uses this to
    /// flip the clip's badge from "local+cold" to "cold-only".
    pub const CLIP_HOT_EVICTED: &str = "clip.hot.evicted";

    /// Storage-safety hard-evict step ran: a clip with NO cold
    /// copy was metadata-cascade-deleted (M2.1 behavior).
    /// Payload: `ClipHardEvictedEvent { clip_id, camera_id,
    /// freed_bytes }`. UI uses this to remove the clip card.
    pub const CLIP_HARD_EVICTED: &str = "clip.hard.evicted";

    /// `storage_backends` table mutated (insert/update/delete) or
    /// `storage_cold_replica` policy row updated. Payload:
    /// `StorageBackendsChangedEvent { kind: "upsert"|"delete"|
    /// "policy", handle: Option<String> }`. The cold replicator
    /// re-loads its `Registry` on this signal.
    pub const STORAGE_BACKENDS_CHANGED: &str = "storage.backends.changed";

    // ---- M2.2 Phase 3 — USB hot-plug topics ----
    //
    // Emitted by the `usb_watch` task when a `NEXUS_*`-labeled
    // volume appears or disappears under the configured mount root
    // (`<clips_dir>/usb/` on Linux, `/Volumes/` on macOS dev).
    // Recorder subscribes indirectly via the shared `UsbRegistry`;
    // the bus event is for the UI + audit log so operators can see
    // the attach history.

    /// A USB volume mounted under the watch root and is now
    /// available as a hot-tier target. Payload:
    /// `UsbAttachedEvent { label, mount_path }`. Mount path is
    /// stored relative to the engine's `clips_dir`.
    pub const STORAGE_USB_ATTACHED: &str = "storage.usb.attached";

    /// A previously-attached USB volume disappeared. Payload:
    /// `UsbDetachedEvent { label }`. The recorder will fall back
    /// to the local hot tier on the next `open()` if this volume
    /// was the preferred target; in-flight clips finish at their
    /// original path (the data may become unreadable mid-write
    /// — that's a hardware-level failure, not something the
    /// recorder can recover from).
    pub const STORAGE_USB_DETACHED: &str = "storage.usb.detached";

    // ---- M7 Step 5 — delivery-policy hot-reload topics ----
    //
    // The dispatcher caches `DeliverySettings` and the per-rule
    // `RuleDeliveryPolicy` map in `ArcSwap`s and reloads on the
    // signals below. Payloads are metadata-only sentinels; the
    // dispatcher always re-reads the store on a signal rather
    // than trusting an inline payload, so a Lagged subscriber
    // catching up still converges on the right state.

    /// `delivery_settings` row mutated. Payload is the empty
    /// `DeliverySettingsChangedEvent` sentinel — the dispatcher
    /// re-reads from `nexus-store::delivery_settings_get` on every
    /// signal. Emitted ONCE per `PUT /api/v1/admin/delivery`.
    pub const DELIVERY_SETTINGS_CHANGED: &str = "delivery.settings.changed";

    /// `rules.delivery_policy_json` for some rule mutated. Payload
    /// is `RuleDeliveryPolicyChangedEvent { rule_id }` so the
    /// dispatcher can drop the cached entry for that rule and
    /// reload only it. Emitted ONCE per
    /// `PUT /api/v1/rules/:id/delivery`. The clear-override case
    /// uses the same topic.
    pub const RULE_DELIVERY_POLICY_CHANGED: &str = "rule.delivery_policy.changed";
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BusError {
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("publish: no subscribers")]
    NoSubscribers,
    #[error("publish: backend closed")]
    Closed,
    #[error("subscribe: lagged by {0} messages")]
    Lagged(u64),
    #[error("backend not available: {0}")]
    BackendUnavailable(&'static str),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub type DynStream =
    Pin<Box<dyn Stream<Item = Result<serde_json::Value, BusError>> + Send + 'static>>;

#[async_trait]
pub trait Bus: Send + Sync {
    async fn publish_raw(&self, topic: &str, payload: serde_json::Value) -> Result<(), BusError>;
    async fn subscribe_raw(&self, topic: &str) -> Result<DynStream, BusError>;
}

/// Typed convenience layer. Anything that implements [`Bus`] gets these for free.
#[async_trait]
pub trait BusExt: Bus {
    async fn publish<T: Serialize + Send + Sync>(
        &self,
        topic: &str,
        msg: &T,
    ) -> Result<(), BusError> {
        let v = serde_json::to_value(msg)?;
        self.publish_raw(topic, v).await
    }

    async fn subscribe<T>(
        &self,
        topic: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<T, BusError>> + Send>>, BusError>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let raw = self.subscribe_raw(topic).await?;
        let mapped = raw
            .map(|r| r.and_then(|v| serde_json::from_value::<T>(v).map_err(BusError::Serialize)));
        Ok(Box::pin(mapped))
    }
}

impl<T: Bus + ?Sized> BusExt for T {}

// ---------------------------------------------------------------------------
// BroadcastBus — default in-process implementation
// ---------------------------------------------------------------------------

pub struct BroadcastBus {
    capacity: usize,
    channels: RwLock<std::collections::HashMap<String, broadcast::Sender<serde_json::Value>>>,
}

impl BroadcastBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            channels: RwLock::new(Default::default()),
        }
    }

    fn sender_for(&self, topic: &str) -> broadcast::Sender<serde_json::Value> {
        if let Some(tx) = self.channels.read().get(topic) {
            return tx.clone();
        }
        let mut w = self.channels.write();
        w.entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }
}

#[async_trait]
impl Bus for BroadcastBus {
    async fn publish_raw(&self, topic: &str, payload: serde_json::Value) -> Result<(), BusError> {
        let tx = self.sender_for(topic);
        // Allow no-subscribers; ops bus events shouldn't fail because no one's listening.
        let _ = tx.send(payload);
        Ok(())
    }

    async fn subscribe_raw(&self, topic: &str) -> Result<DynStream, BusError> {
        let rx = self.sender_for(topic).subscribe();
        let s = BroadcastStream::new(rx).map(|r| match r {
            Ok(v) => Ok(v),
            Err(BroadcastStreamRecvError::Lagged(n)) => Err(BusError::Lagged(n)),
        });
        Ok(Box::pin(s))
    }
}

// ---------------------------------------------------------------------------
// NatsBus — feature-gated stub. Wired in M2.
// ---------------------------------------------------------------------------

#[cfg(feature = "nats")]
pub struct NatsBus {
    _url: String,
}

#[cfg(feature = "nats")]
#[async_trait]
impl Bus for NatsBus {
    async fn publish_raw(&self, _topic: &str, _payload: serde_json::Value) -> Result<(), BusError> {
        Err(BusError::BackendUnavailable(
            "nats backend not yet implemented (M2)",
        ))
    }
    async fn subscribe_raw(&self, _topic: &str) -> Result<DynStream, BusError> {
        Err(BusError::BackendUnavailable(
            "nats backend not yet implemented (M2)",
        ))
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub fn build_bus(cfg: &nexus_config::BusConfig) -> Arc<dyn Bus> {
    match cfg.backend {
        nexus_config::BusBackendKind::Broadcast => Arc::new(BroadcastBus::new(cfg.capacity)),
        nexus_config::BusBackendKind::Nats => {
            tracing::warn!(
                "nats backend selected but feature not enabled; falling back to broadcast"
            );
            Arc::new(BroadcastBus::new(cfg.capacity))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcast_pub_sub_roundtrip() {
        let bus = BroadcastBus::new(8);
        let mut rx = bus.subscribe::<serde_json::Value>("t").await.unwrap();
        bus.publish("t", &serde_json::json!({"hello": "world"}))
            .await
            .unwrap();
        let v = rx.next().await.unwrap().unwrap();
        assert_eq!(v["hello"], "world");
    }
}
