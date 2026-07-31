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
//! source codec, and the effective detector kind. Credentials (RTSP
//! password, ONVIF secret) NEVER cross the tunnel — AGENTS.md Rule 6.
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
///
/// `default_model_kind` is `inference.model.kind` — the detector a
/// camera runs when it has no `model_override`.
async fn build_envelope(
    store: &Store,
    revision: u64,
    default_model_kind: &str,
) -> anyhow::Result<Envelope> {
    let cams = store.list_cameras().await?;
    let snapshot_ts = Utc::now().to_rfc3339();
    let entries: Vec<CameraRosterEntry> = cams
        .into_iter()
        .map(|c| {
            // CameraId is i64; the wire is u64. Cameras with negative
            // ids should be impossible (SQLite rowid alias is always
            // >=1) but guard anyway.
            let edge_camera_id = u64::try_from(c.id).unwrap_or(0);
            // The wire field is "active detector kind on this camera",
            // NOT "override, if any" — so resolve the same way
            // `Config::validate` does and always report something.
            // Sending the override alone left the cloud console's
            // Detector column blank for every camera running the
            // engine default, which is most of them.
            let model_kind = Some(
                c.detector
                    .model_override
                    .as_ref()
                    .map_or(default_model_kind, |m| m.kind.as_str())
                    .to_string(),
            );
            // `_plus` is a vendor SVC label; the wire enum is
            // base codecs only, so collapse via `.base()`.
            let codec = Some(
                c.ingest
                    .codec
                    .map(|k| k.base().to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            );
            CameraRosterEntry {
                edge_camera_id,
                name: c.name,
                kind: wire_kind_from_url(&c.ingest.url).to_string(),
                enabled: c.ingest.enabled,
                tags: None,
                resolution: None,
                codec,
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
async fn try_publish(
    store: &Store,
    outbox: &TunnelOutbox,
    revision_counter: &AtomicU64,
    default_model_kind: &str,
) -> bool {
    if !outbox.is_connected() {
        return false;
    }
    let revision = revision_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let env = match build_envelope(store, revision, default_model_kind).await {
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
///
/// `default_model_kind` is a boot-time snapshot of
/// `inference.model.kind`, used to resolve each entry's effective
/// detector. Consistent with `AppState::current_inference_model`,
/// which snapshots the same value — changing the global detector is
/// a restart-scoped operation.
pub fn spawn(
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    outbox: Arc<TunnelOutbox>,
    default_model_kind: String,
) -> JoinHandle<()> {
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
                                if try_publish(&store, &outbox, &revision_counter, &default_model_kind).await {
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
                        && try_publish(&store, &outbox, &revision_counter, &default_model_kind).await
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

    /// Phase 7.6.1 — ONVIF endpoint + credentials live edge-resident in
    /// the camera's `config_json` blob and MUST NEVER cross the tunnel
    /// (AGENTS.md Rule 6 / REPO_BOUNDARY R5b). The roster builder
    /// hand-picks metadata fields, so the credentials stay redacted —
    /// this asserts it end-to-end through `build_envelope`.
    #[tokio::test]
    async fn camera_roster_redacts_onvif_credentials() {
        use nexus_config::{
            CameraBehavior, CameraConfig, CameraDetector, CameraIngest, CameraOnvif,
            CameraTalkDown, StoreConfig,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Store::open(&StoreConfig {
            url: format!("sqlite://{}?mode=rwc", db_path.display()),
            ..StoreConfig::default()
        })
        .await
        .unwrap();

        let secret_user = "onvif-admin";
        let secret_pass = "sup3r-secret-onvif-pw";
        let secret_backchannel = "rtsp://127.0.0.1/talk-backchannel-secret";
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "ptz-cam".into(),
                ingest: CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: CameraBehavior::default(),
                onvif: CameraOnvif {
                    endpoint: Some("http://192.168.1.64/onvif/device_service".into()),
                    username: Some(secret_user.into()),
                    password: Some(secret_pass.into()),
                },
                talk_down: CameraTalkDown {
                    speaker_present: true,
                    backchannel_codec: Some("PCMU".into()),
                    backchannel_url: Some(secret_backchannel.into()),
                },
                zones: vec![],
            })
            .await
            .unwrap();

        let env = build_envelope(&store, 42, "yolo")
            .await
            .expect("build envelope");
        let json = serde_json::to_string(&env).expect("serialize envelope");

        // The camera URL is rtsp://, so "onvif" can only appear in the
        // serialized envelope if the credential blob leaked.
        assert!(
            !json.contains("onvif"),
            "camera_roster envelope must not mention onvif: {json}"
        );
        assert!(
            !json.contains(secret_user),
            "camera_roster envelope leaked the ONVIF username"
        );
        assert!(
            !json.contains(secret_pass),
            "camera_roster envelope leaked the ONVIF password"
        );
        assert!(
            !json.contains("talk_down") && !json.contains("backchannel"),
            "camera_roster envelope must not mention talk_down: {json}"
        );
        assert!(
            !json.contains(secret_backchannel),
            "camera_roster envelope leaked the talk-down backchannel URL"
        );
    }

    /// The wire field is "active detector kind on this camera", so a
    /// camera with no `model_override` must still report the engine
    /// default rather than omitting the field — otherwise the cloud
    /// console's Detector column is blank for every stock camera.
    #[tokio::test]
    async fn camera_roster_reports_effective_detector_kind() {
        use nexus_config::{
            CameraBehavior, CameraConfig, CameraDetector, CameraIngest, CameraOnvif,
            CameraTalkDown, ModelConfig, StoreConfig,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Store::open(&StoreConfig {
            url: format!("sqlite://{}?mode=rwc", db_path.display()),
            ..StoreConfig::default()
        })
        .await
        .unwrap();

        let cam = |id: i64, name: &str, model_override: Option<ModelConfig>| CameraConfig {
            id,
            name: name.into(),
            ingest: CameraIngest {
                url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                enabled: true,
                max_fps: 0,
                codec: None,
            },
            detector: CameraDetector {
                prompts: vec![],
                visual_prompts: vec![],
                model_override,
            },
            behavior: CameraBehavior::default(),
            onvif: CameraOnvif::default(),
            talk_down: CameraTalkDown::default(),
            zones: vec![],
        };

        let overridden = ModelConfig {
            kind: "yoloe".into(),
            ..Default::default()
        };

        store.upsert_camera(&cam(1, "stock", None)).await.unwrap();
        store
            .upsert_camera(&cam(2, "custom", Some(overridden)))
            .await
            .unwrap();

        let env = build_envelope(&store, 7, "yolo_world")
            .await
            .expect("build envelope");
        let EnvelopeBody::CameraRoster(payload) = env.body else {
            panic!("expected a camera_roster body");
        };

        let kind_of = |id: u64| {
            payload
                .cameras
                .iter()
                .find(|c| c.edge_camera_id == id)
                .unwrap_or_else(|| panic!("camera {id} missing from roster"))
                .model_kind
                .clone()
        };
        assert_eq!(kind_of(1).as_deref(), Some("yolo_world"));
        assert_eq!(kind_of(2).as_deref(), Some("yoloe"));
    }
}
