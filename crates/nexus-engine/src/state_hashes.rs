//! Phase 7.5 · Step 7.5.5 — push fleet-settings drift hashes to the cloud.
//!
//! The console renders configuration drift (local edits diverging from
//! the fleet baseline) without round-tripping the full config. To do
//! that the edge periodically reports one canonical SHA-256 per
//! fleet-settings category in a `core_state_hashes` envelope; the cloud
//! compares each against its own projection of the effective fleet
//! payload (`core_runtime_hashes`).
//!
//! ### When we publish
//!
//! 1. Once on task startup (best-effort; retried on the dirty tick if
//!    the tunnel is still down).
//! 2. After a short debounce following any `topic::CONFIG_CHANGED` or
//!    `topic::DELIVERY_SETTINGS_CHANGED` event (rules, prompts,
//!    detector overrides, and delivery settings all live behind these
//!    two topics). The debounce coalesces a burst of edits into one
//!    envelope and keeps end-to-end latency well under the 6 s
//!    acceptance budget.
//! 3. After every tunnel reconnect, so the cloud can re-push any fleet
//!    change made while the tunnel was down.
//! 4. The hashes are recomputed from the live store each time, so the
//!    report always reflects committed state.
//!
//! Hashing is delegated to [`crate::fleet_hash`], which mirrors the
//! cloud's canonical-JSON algorithm byte-for-byte.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{CoreStateHashesPayload, Envelope, EnvelopeBody, EnvelopeMeta};
use nexus_store::Store;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::fleet_hash::{self, CategoryHashes};

/// Coalesce a burst of config edits into a single report.
const DEBOUNCE: Duration = Duration::from_secs(2);

/// Retry cadence when a publish failed (tunnel disconnected, etc).
const RETRY_TICK: Duration = Duration::from_secs(10);

/// Build a `core_state_hashes` envelope from a computed snapshot.
fn build_envelope(hashes: &CategoryHashes) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::CoreStateHashes(CoreStateHashesPayload {
            computed_at: Utc::now().to_rfc3339(),
            rules_sha256: hashes.rules.clone(),
            text_prompts_sha256: hashes.text_prompts.clone(),
            visual_prompts_sha256: hashes.visual_prompts.clone(),
            detector_config_sha256: hashes.detector_config.clone(),
            delivery_settings_sha256: hashes.delivery_settings.clone(),
            talkdown_audio_sha256: hashes.talkdown_audio.clone(),
        }),
    }
}

/// Compute + publish one snapshot. Returns the published hashes on
/// success so the caller can suppress an unchanged re-send.
async fn try_publish(store: &Store, outbox: &TunnelOutbox) -> Option<CategoryHashes> {
    if !outbox.is_connected() {
        return None;
    }
    let hashes = match fleet_hash::compute(store).await {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "state-hash publisher: compute failed");
            return None;
        }
    };
    let env = build_envelope(&hashes);
    match outbox.send(env).await {
        Ok(()) => {
            debug!("core_state_hashes published");
            Some(hashes)
        }
        Err(e) => {
            debug!(error = %e, "state-hash publisher: send failed (will retry)");
            None
        }
    }
}

/// Spawn the long-running fleet-state-hash publisher. Returns its join
/// handle so the engine shutdown path can abort it alongside the other
/// long-lived tasks.
pub fn spawn(store: Arc<Store>, bus: Arc<dyn Bus>, outbox: Arc<TunnelOutbox>) -> JoinHandle<()> {
    spawn_with_tick(store, bus, outbox, RETRY_TICK)
}

fn spawn_with_tick(
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    outbox: Arc<TunnelOutbox>,
    retry_tick: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Subscribe BEFORE the initial publish so an edit between boot
        // and subscribe still triggers a fresh report.
        let mut config_stream = match bus
            .subscribe::<serde_json::Value>(topic::CONFIG_CHANGED)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                error!(
                    error = %e,
                    "state-hash publisher: failed to subscribe to config.changed; \
                     drift hashes will not update until restart"
                );
                return;
            }
        };
        let mut delivery_stream = match bus
            .subscribe::<serde_json::Value>(topic::DELIVERY_SETTINGS_CHANGED)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                error!(
                    error = %e,
                    "state-hash publisher: failed to subscribe to delivery.settings.changed"
                );
                return;
            }
        };
        info!("state-hash publisher: subscribed to config + delivery changes");

        // `dirty` means a report is owed; `last` suppresses an
        // unchanged re-send. We owe an initial report at boot.
        let mut dirty = true;
        let mut last: Option<CategoryHashes> = None;
        // The gateway reconciles a reconnected core only against hashes sent
        // on the new session (BUG-337), so every reconnect owes a report.
        // `connects()`, not `is_connected()`, so a fast drop is not missed.
        let mut seen_connects = outbox.connects();

        loop {
            // If something is owed, wait out the debounce window while
            // still draining further events; otherwise idle on the
            // retry tick.
            let wait = if dirty { DEBOUNCE } else { retry_tick };
            tokio::select! {
                msg = config_stream.next() => {
                    match msg {
                        Some(Ok(_)) => dirty = true,
                        Some(Err(e)) => warn!(error = %e, "state-hash publisher: config bus error"),
                        None => {
                            warn!("state-hash publisher: config bus closed; exiting");
                            return;
                        }
                    }
                }
                msg = delivery_stream.next() => {
                    match msg {
                        Some(Ok(_)) => dirty = true,
                        Some(Err(e)) => warn!(error = %e, "state-hash publisher: delivery bus error"),
                        None => {
                            warn!("state-hash publisher: delivery bus closed; exiting");
                            return;
                        }
                    }
                }
                () = tokio::time::sleep(wait) => {
                    let connects = outbox.connects();
                    if connects != seen_connects {
                        seen_connects = connects;
                        dirty = true;
                    }
                    if dirty {
                        if let Some(hashes) = try_publish(&store, &outbox).await {
                            // Only clear the owe-flag once a fresh
                            // report actually left the box.
                            if last.as_ref() != Some(&hashes) {
                                last = Some(hashes);
                            }
                            dirty = false;
                        }
                        // If the tunnel was down, leave `dirty` set so
                        // the retry tick re-attempts.
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_carries_the_talkdown_audio_hash() {
        let hashes = CategoryHashes {
            talkdown_audio: Some("ab".into()),
            ..CategoryHashes::default()
        };
        let EnvelopeBody::CoreStateHashes(p) = build_envelope(&hashes).body else {
            panic!("not a core_state_hashes envelope");
        };
        assert_eq!(p.talkdown_audio_sha256.as_deref(), Some("ab"));
        let EnvelopeBody::CoreStateHashes(p) = build_envelope(&CategoryHashes::default()).body
        else {
            panic!("not a core_state_hashes envelope");
        };
        assert_eq!(p.talkdown_audio_sha256, None);
    }

    struct Capture(tokio::sync::mpsc::UnboundedSender<Envelope>);

    #[async_trait::async_trait]
    impl nexus_cloud_client::TunnelHandle for Capture {
        async fn send(&self, envelope: Envelope) -> Result<(), nexus_cloud_client::TunnelError> {
            let _ = self.0.send(envelope);
            Ok(())
        }
    }

    /// BUG-337 follow-up: the gateway reconciles a core only against hashes
    /// sent on the new session, so a reconnect with no engine restart and no
    /// config edit must still publish `core_state_hashes`.
    #[tokio::test]
    async fn a_reconnect_republishes_state_hashes_without_a_config_change() {
        use nexus_bus::BroadcastBus;
        use nexus_config::StoreConfig;
        use tokio::sync::mpsc::unbounded_channel;
        use tokio::time::timeout;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!(
                    "sqlite://{}?mode=rwc",
                    dir.path().join("nexus.db").display()
                ),
                ..StoreConfig::default()
            })
            .await
            .unwrap(),
        );
        let outbox = Arc::new(TunnelOutbox::new());
        let (tx, mut rx) = unbounded_channel();
        outbox.set_handle(Some(Arc::new(Capture(tx))));
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
        let task = spawn_with_tick(store, bus, outbox.clone(), Duration::from_millis(20));

        let first = timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(first.is_ok(), "startup core_state_hashes");
        assert!(
            timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "nothing changed, nothing sent"
        );

        // A reconnect quicker than one tick still counts.
        let (tx2, mut rx2) = unbounded_channel();
        outbox.set_handle(None);
        outbox.set_handle(Some(Arc::new(Capture(tx2))));
        let env = timeout(Duration::from_secs(5), rx2.recv())
            .await
            .expect("core_state_hashes after reconnect")
            .expect("capture open");
        assert!(matches!(env.body, EnvelopeBody::CoreStateHashes(_)));

        task.abort();
    }
}
