//! Phase A — push camera-roster snapshots to the cloud edge-gateway.
//!
//! The cloud control plane mirrors the per-core camera list so the
//! site dashboard can display cameras the operator configured locally,
//! BEFORE any alert has fired (the legacy auto-create-on-first-alert
//! path in `alert-ingest` is still the recovery floor).
//!
//! ### What crosses the tunnel
//!
//! Only camera metadata: id, name, scheme-derived kind, enabled flag,
//! optional model-override kind. Credentials (RTSP password, ONVIF
//! secret) NEVER cross the tunnel — AGENTS.md Rule 6.
//!
//! ### When we publish
//!
//! 1. Once on task startup (best-effort; if the tunnel is still down
//!    the next tick or bus event will retry).
//! 2. On every `topic::CONFIG_CHANGED` event whose `kind == "camera"`.
//! 3. On a 10-second tick if a previous send failed (dirty flag).
//!
//! Cloud-side dedup uses the monotonic `roster_revision` carried on
//! every envelope; we seed it from `Utc::now().timestamp_millis()` at
//! boot so revisions are monotonic across process restarts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{
    CameraRosterEntry, CameraRosterPayload, Envelope, EnvelopeBody, EnvelopeMeta,
};
use nexus_store::Store;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

/// Retry cadence when a publish failed (tunnel disconnected, etc).
const RETRY_TICK: Duration = Duration::from_secs(10);

/// Derive the wire-protocol `kind` enum value from a camera URL
/// scheme. Defaults to `"rtsp"` for unknown schemes — real cameras
/// dominate the install base; the cloud uses this only for
/// display-side iconography, not for routing decisions.
fn wire_kind_from_url(url: &Url) -> &'static str {
    match url.scheme() {
        "rtsp" | "rtsps" => "rtsp",
        "onvif" => "onvif",
        "youtube" => "youtube",
        "virtual" | "mock" => "virtual",
        "file" => "file",
        _ => "rtsp",
    }
}

fn seed_revision() -> u64 {
    // Wall-clock millis at boot — guaranteed greater than any
    // revision a prior process instance emitted (clock-monotonic
    // assumption; rollbacks are an operations problem).
    u64::try_from(Utc::now().timestamp_millis()).unwrap_or(1)
}

/// Build a `camera_roster` envelope from the current store snapshot.
async fn build_envelope(store: &Store, revision: u64) -> anyhow::Result<Envelope> {
    let cams = store.list_cameras().await?;
    let snapshot_ts = Utc::now().to_rfc3339();
    let entries: Vec<CameraRosterEntry> = cams
        .into_iter()
        .map(|c| {
            // CameraId is i64; the wire is u64. Cameras with negative
            // ids should be impossible (SQLite rowid alias is always
            // >=1) but guard anyway.
            let edge_camera_id = u64::try_from(c.id).unwrap_or(0);
            let model_kind = c.detector.model_override.as_ref().map(|m| m.kind.clone());
            CameraRosterEntry {
                edge_camera_id,
                name: c.name,
                kind: wire_kind_from_url(&c.ingest.url).to_string(),
                enabled: c.ingest.enabled,
                tags: None,
                resolution: None,
                codec: None,
                model_kind,
                online: None,
                // Phase A: per-camera revision == snapshot revision.
                // Phase D will introduce real per-row tracking when
                // cloud-side mutations need optimistic-concurrency.
                revision,
                updated_at: snapshot_ts.clone(),
            }
        })
        .collect();
    Ok(Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::CameraRoster(CameraRosterPayload {
            cameras: entries,
            roster_revision: revision,
            snapshot_at: snapshot_ts,
        }),
    })
}

/// Try one publish. Returns true on success, false otherwise.
async fn try_publish(store: &Store, outbox: &TunnelOutbox, revision_counter: &AtomicU64) -> bool {
    if !outbox.is_connected() {
        return false;
    }
    let revision = revision_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let env = match build_envelope(store, revision).await {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "roster publisher: snapshot build failed");
            return false;
        }
    };
    let count = match &env.body {
        EnvelopeBody::CameraRoster(p) => p.cameras.len(),
        _ => 0,
    };
    match outbox.send(env).await {
        Ok(()) => {
            debug!(
                camera_count = count,
                roster_revision = revision,
                "camera_roster published",
            );
            true
        }
        Err(e) => {
            // Disconnected mid-flight or writer closed — caller leaves
            // the dirty flag set and the retry tick handles it.
            debug!(error = %e, "roster publisher: send failed (will retry)");
            false
        }
    }
}

/// Spawn the long-running roster publisher task. Returns its join
/// handle so the engine shutdown path can abort it alongside the
/// other long-lived tasks.
pub fn spawn(store: Arc<Store>, bus: Arc<dyn Bus>, outbox: Arc<TunnelOutbox>) -> JoinHandle<()> {
    let revision_counter = Arc::new(AtomicU64::new(seed_revision()));
    let dirty = Arc::new(AtomicBool::new(true));
    tokio::spawn(async move {
        // Subscribe to config.changed BEFORE the initial publish so
        // we don't race a fast operator who creates a camera between
        // boot and subscribe.
        let mut stream = match bus
            .subscribe::<serde_json::Value>(topic::CONFIG_CHANGED)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                error!(
                    error = %e,
                    "roster publisher: failed to subscribe to config.changed; \
                     cameras will be invisible in cloud until restart"
                );
                return;
            }
        };
        info!("roster publisher: subscribed to config.changed");

        loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(v)) => {
                            // Schema matches the reconciler: only
                            // {"kind":"camera",...} events trigger
                            // a roster push. Older publishers that
                            // omit `kind` get a fresh push too (be
                            // conservative).
                            let is_camera_event = v
                                .get("kind")
                                .and_then(|k| k.as_str())
                                .is_none_or(|k| k == "camera");
                            if is_camera_event {
                                dirty.store(true, Ordering::Relaxed);
                                if try_publish(&store, &outbox, &revision_counter).await {
                                    dirty.store(false, Ordering::Relaxed);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "roster publisher: bus stream error");
                        }
                        None => {
                            warn!("roster publisher: bus stream closed; exiting");
                            return;
                        }
                    }
                }
                () = tokio::time::sleep(RETRY_TICK) => {
                    if dirty.load(Ordering::Relaxed)
                        && try_publish(&store, &outbox, &revision_counter).await
                    {
                        dirty.store(false, Ordering::Relaxed);
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
    fn wire_kind_maps_known_schemes() {
        let cases = [
            ("rtsp://cam.example/stream", "rtsp"),
            ("rtsps://cam.example/stream", "rtsp"),
            ("onvif://cam.example", "onvif"),
            ("youtube://watch?v=abc", "youtube"),
            ("virtual://local", "virtual"),
            ("mock://noop", "virtual"),
            ("file:///clips/sample.mp4", "file"),
        ];
        for (url, expected) in cases {
            let parsed = Url::parse(url).expect("parse");
            assert_eq!(wire_kind_from_url(&parsed), expected, "url={url}");
        }
    }

    #[test]
    fn wire_kind_unknown_scheme_falls_back_to_rtsp() {
        // Real cameras dominate the install base; an exotic scheme
        // should still get *some* icon in the cloud UI rather than
        // surfacing as a hard error.
        let parsed = Url::parse("ws://exotic.example/feed").expect("parse");
        assert_eq!(wire_kind_from_url(&parsed), "rtsp");
    }

    #[test]
    fn seed_revision_is_positive_and_recent() {
        let r = seed_revision();
        assert!(
            r > 1_700_000_000_000,
            "expected a recent millis seed, got {r}"
        );
    }
}
