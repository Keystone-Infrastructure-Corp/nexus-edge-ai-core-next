//! Fire-and-forget tunnel outbox.
//!
//! Phase 2 · Step 2.8 introduces a single shared handle slot
//! ([`TunnelOutbox`]) that the engine creates once at boot. The
//! [`crate::tunnel::Connection`] reconnect loop publishes its current
//! handle into the slot on connect and clears it on disconnect.
//! Engine subsystems that need to *push* an envelope (cold replicator
//! emitting `clip_replicated`, future per-event sinks) call
//! [`TunnelOutbox::send`] without caring whether the tunnel is up,
//! whether they came in before the tunnel attached, or which
//! reconnect generation they're publishing into.
//!
//! ### Why not just hand out `Arc<dyn TunnelHandle>` directly?
//!
//! The active [`crate::tunnel::Connection`] is rebuilt every time the
//! reconnect loop dials a new WSS session. An `Arc<dyn TunnelHandle>`
//! handed to a long-lived consumer at boot would either (a) become
//! stale on the first disconnect, or (b) need the consumer to track
//! the reconnect generation itself. The outbox folds that
//! handle-swap into one `RwLock` whose write side is only touched
//! by the reconnect loop.
//!
//! ### Semantics
//!
//! * `send` returns `Ok(())` only when the active handle accepted the
//!   envelope. `TunnelError::Disconnected` means "no handle is
//!   currently installed" — callers log and move on; the next
//!   polling backstop pass (cold replicator) or the Phase 6.17
//!   reconciler sweep will re-publish anything dropped.
//! * The outbox does NOT persist envelopes. If an envelope was sent
//!   while disconnected, it is lost from the engine's perspective —
//!   the cloud-side reconciler is the authoritative recovery path.
//! * Cloning the outbox handle is cheap (one `Arc::clone`).

use std::sync::Arc;

use nexus_cloud_protocol::v1::Envelope;
use parking_lot::RwLock;

use crate::tunnel::{TunnelError, TunnelHandle};

/// Capability tag advertised by the cloud in
/// `HeartbeatAckPayload.cloud_capabilities`. Wire-defined string the
/// cloud guarantees in `proto/v1.json`; the engine treats unknown tags
/// as inert (forward-compatible).
pub mod caps {
    /// Cloud routes batched `entity_sighting_batch` envelopes.
    pub const ENTITY_SIGHTING_BATCH: &str = "entity_sighting_batch";
    /// Cloud decodes IEEE-754 binary16 (`f16`) embedding payloads.
    pub const EMBEDDING_DTYPE_F16: &str = "embedding_dtype_f16";
}

/// Shared, swappable tunnel handle slot. See module docs.
pub struct TunnelOutbox {
    inner: RwLock<Option<Arc<dyn TunnelHandle>>>,
    caps: RwLock<std::collections::HashSet<String>>,
}

impl TunnelOutbox {
    /// Construct an empty outbox. Engine creates one of these at boot
    /// and `Arc::clone`s it into both the tunnel reconnect loop and
    /// every publisher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
            caps: RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// Install or clear the active tunnel handle. Called by the
    /// reconnect loop on every connect (`Some(handle)`) and on every
    /// disconnect (`None`). Disconnect also clears advertised
    /// capabilities — the next session's heartbeat_ack re-advertises.
    pub fn set_handle(&self, handle: Option<Arc<dyn TunnelHandle>>) {
        let connected = handle.is_some();
        *self.inner.write() = handle;
        if !connected {
            self.caps.write().clear();
        }
    }

    /// Replace the advertised cloud capability set. Called from the
    /// inbound pump when a `heartbeat_ack` arrives. `None` or empty
    /// slice means "pre-Phase-A cloud, no opt-in features".
    pub fn update_caps(&self, advertised: Option<&[String]>) {
        let mut w = self.caps.write();
        w.clear();
        if let Some(list) = advertised {
            for tag in list {
                w.insert(tag.clone());
            }
        }
    }

    /// `true` if the cloud most recently advertised support for `tag`.
    #[must_use]
    pub fn supports(&self, tag: &str) -> bool {
        self.caps.read().contains(tag)
    }

    /// `true` while a handle is installed. Cheap.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.read().is_some()
    }

    /// Send an envelope through the current handle. If no handle is
    /// installed, returns [`TunnelError::Disconnected`] immediately
    /// without blocking — callers log and move on.
    ///
    /// # Errors
    ///
    /// Propagates whatever the inner [`TunnelHandle`] returns. Most
    /// commonly `TunnelError::Disconnected` (no handle installed) or
    /// `TunnelError::SendChannelClosed` (writer task exited between
    /// `set_handle(Some(_))` and `send`).
    pub async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
        // Clone the Arc out under the read lock, then drop the guard
        // before awaiting — `send().await` may yield, and holding a
        // parking_lot guard across an await is a deadlock waiting to
        // happen.
        let handle = self.inner.read().clone();
        match handle {
            Some(h) => h.send(envelope).await,
            None => Err(TunnelError::Disconnected),
        }
    }
}

impl Default for TunnelOutbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use chrono::Utc;
    use nexus_cloud_protocol::v1::{AlertPayload, EnvelopeBody, EnvelopeMeta};

    struct CountingTunnel {
        count: parking_lot::Mutex<u32>,
    }

    #[async_trait]
    impl TunnelHandle for CountingTunnel {
        async fn send(&self, _envelope: Envelope) -> Result<(), TunnelError> {
            *self.count.lock() += 1;
            Ok(())
        }
    }

    fn envelope() -> Envelope {
        Envelope {
            meta: EnvelopeMeta {
                v: 1,
                id: "00000000-0000-7000-8000-000000000000".into(),
                ts: Utc::now().to_rfc3339(),
                in_reply_to: None,
                seq: None,
                trace: None,
            },
            body: EnvelopeBody::Alert(AlertPayload {
                edge_event_id: "evt".into(),
                ts: Utc::now().to_rfc3339(),
                camera_id: 1,
                severity: 1,
                edge_rule_id: None,
                matched_label: None,
                confidence: None,
                bbox: None,
                snapshot_blob_url: None,
                clip_blob_url: None,
                attached_history: None,
                verification_state: None,
                verdict_description: None,
                verdict_evidence: None,
                verdict_confidence: None,
                evidence_clip_ref: None,
            }),
        }
    }

    #[tokio::test]
    async fn send_without_handle_returns_disconnected() {
        let outbox = TunnelOutbox::new();
        assert!(!outbox.is_connected());
        let err = outbox.send(envelope()).await.unwrap_err();
        assert!(matches!(err, TunnelError::Disconnected));
    }

    #[tokio::test]
    async fn send_routes_through_installed_handle() {
        let outbox = TunnelOutbox::new();
        let tunnel = Arc::new(CountingTunnel {
            count: parking_lot::Mutex::new(0),
        });
        outbox.set_handle(Some(tunnel.clone()));
        assert!(outbox.is_connected());
        outbox.send(envelope()).await.expect("send");
        outbox.send(envelope()).await.expect("send");
        assert_eq!(*tunnel.count.lock(), 2);
    }

    #[tokio::test]
    async fn clearing_handle_disconnects() {
        let outbox = TunnelOutbox::new();
        let tunnel = Arc::new(CountingTunnel {
            count: parking_lot::Mutex::new(0),
        });
        outbox.set_handle(Some(tunnel.clone()));
        outbox.send(envelope()).await.expect("first send");
        outbox.set_handle(None);
        assert!(!outbox.is_connected());
        let err = outbox.send(envelope()).await.unwrap_err();
        assert!(matches!(err, TunnelError::Disconnected));
        assert_eq!(*tunnel.count.lock(), 1);
    }
}
