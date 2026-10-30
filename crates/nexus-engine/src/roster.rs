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
//! 3. On a 10-second tick if a previous send failed (dirty flag), if any
//!    camera's frame-source liveness differs from the last sent roster,
//!    or if the tunnel reconnected since the last tick. The cloud derives
//!    `camera.offline` only from these rosters (BUG-315), so a camera
//!    that stops streaming must ship one on its own.
//!
//! Cloud-side dedup uses the monotonic `roster_revision` carried on
//! every envelope; we seed it from `Utc::now().timestamp_millis()` at
//! boot so revisions are monotonic across process restarts.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{
    CameraRosterEntry, CameraRosterPayload, Envelope, EnvelopeBody, EnvelopeMeta,
};
use nexus_pipeline::FrameStatsRegistry;
use nexus_store::Store;
use nexus_types::CameraId;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

/// Cadence of the retry, liveness and reconnect checks.
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
/// camera runs when it has no `model_override`. `online` is one
/// [`online_cameras`] read, so the roster and the set the publisher
/// records as sent cannot disagree.
async fn build_envelope(
    store: &Store,
    revision: u64,
    default_model_kind: &str,
    online: &BTreeSet<CameraId>,
) -> anyhow::Result<Envelope> {
    let cams = store.list_cameras().await?;
    let now = Utc::now();
    let snapshot_ts = now.to_rfc3339();
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
                // Edge-observed liveness in the last frame-source pass —
                // derived from the same `FrameStatsRegistry` the pipeline
                // supervisor writes on every decoded frame, so a camera
                // nobody is watching in the live-view grid is still
                // reported correctly (unlike `live_view::stalled_cameras`,
                // which only tracks cameras with a live-view subscriber).
                online: Some(online.contains(&c.id)),
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

/// The cameras whose last frame is inside the offline window at
/// `now_mono`.
fn online_cameras(frame_stats: &FrameStatsRegistry, now_mono: Instant) -> BTreeSet<CameraId> {
    frame_stats
        .snapshot_all()
        .into_iter()
        .filter_map(|(id, s)| s.is_online(now_mono).then_some(id))
        .collect()
}

/// Try one publish. Returns the cameras it reported online on success,
/// `None` otherwise.
async fn try_publish(
    store: &Store,
    outbox: &TunnelOutbox,
    revision_counter: &AtomicU64,
    default_model_kind: &str,
    frame_stats: &FrameStatsRegistry,
    now_mono: Instant,
) -> Option<BTreeSet<CameraId>> {
    if !outbox.is_connected() {
        return None;
    }
    let revision = revision_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let online = online_cameras(frame_stats, now_mono);
    let env = match build_envelope(store, revision, default_model_kind, &online).await {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "roster publisher: snapshot build failed");
            return None;
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
            Some(online)
        }
        Err(e) => {
            // Disconnected mid-flight or writer closed — caller leaves
            // the dirty flag set and the retry tick handles it.
            debug!(error = %e, "roster publisher: send failed (will retry)");
            None
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
    frame_stats: Arc<FrameStatsRegistry>,
) -> JoinHandle<()> {
    tokio::spawn(run(
        store,
        bus,
        outbox,
        default_model_kind,
        frame_stats,
        RETRY_TICK,
        Instant::now,
    ))
}

/// The publisher loop behind [`spawn`]. `tick` and `now_mono` are
/// injected so a test can age a camera's last frame past
/// `CAMERA_OFFLINE_AFTER_MS` without waiting 90 s.
async fn run(
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    outbox: Arc<TunnelOutbox>,
    default_model_kind: String,
    frame_stats: Arc<FrameStatsRegistry>,
    tick: Duration,
    now_mono: impl Fn() -> Instant + Send + 'static,
) {
    let revision_counter = Arc::new(AtomicU64::new(seed_revision()));
    let dirty = Arc::new(AtomicBool::new(true));
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

    // The cloud derives `camera.offline` only from rosters (BUG-315), so a
    // liveness flip since the last sent roster, or a reconnect that may
    // have dropped it, is a roster the cloud still needs.
    let mut sent_online = BTreeSet::new();
    let mut seen_connects = 0;
    // An interval, not a sleep re-armed per loop: any `config.changed`
    // would restart a sleep and push the liveness check back.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + tick, tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                            if let Some(online) = try_publish(&store, &outbox, &revision_counter, &default_model_kind, &frame_stats, now_mono()).await {
                                dirty.store(false, Ordering::Relaxed);
                                sent_online = online;
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
            _ = ticker.tick() => {
                let connects = outbox.connects();
                if connects != seen_connects || online_cameras(&frame_stats, now_mono()) != sent_online {
                    seen_connects = connects;
                    dirty.store(true, Ordering::Relaxed);
                }
                if dirty.load(Ordering::Relaxed) {
                    if let Some(online) = try_publish(&store, &outbox, &revision_counter, &default_model_kind, &frame_stats, now_mono()).await {
                        dirty.store(false, Ordering::Relaxed);
                        sent_online = online;
                    }
                }
            }
        }
    }
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
        let secret_analysis_pw = "sup3r-secret-substream-pw";
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "ptz-cam".into(),
                ingest: CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    // SPEC-069 — the analysis stream carries credentials
                    // exactly like `url`, so it is redacted exactly like it.
                    analysis_url: Some(
                        Url::parse(&format!(
                            "rtsp://admin:{secret_analysis_pw}@127.0.0.1/substream"
                        ))
                        .unwrap(),
                    ),
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

        let env = build_envelope(&store, 42, "yolo", &BTreeSet::new())
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
        assert!(
            !json.contains(secret_analysis_pw) && !json.contains("analysis_url"),
            "camera_roster envelope leaked the analysis stream URL: {json}"
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
                analysis_url: None,
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

        let env = build_envelope(&store, 7, "yolo_world", &BTreeSet::new())
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

    /// `CameraRosterEntry.online` is "Edge-observed liveness in the last
    /// frame-source pass" — derived from `FrameStatsRegistry`, which every
    /// spawned camera writes to regardless of live-view subscribers. This
    /// is the regression that would catch a mistaken switch to
    /// `live_view::stalled_cameras()`, which only tracks watched cameras
    /// and would misreport an unwatched-but-live camera as not online.
    #[tokio::test]
    async fn camera_roster_online_reflects_frame_stats_not_live_view() {
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

        let cam = |id: i64, name: &str| CameraConfig {
            id,
            name: name.into(),
            ingest: CameraIngest {
                url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                analysis_url: None,
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
            onvif: CameraOnvif::default(),
            talk_down: CameraTalkDown::default(),
            zones: vec![],
        };

        // Camera 1 produces frames but has no live-view subscriber at all
        // (nothing ever calls into `live_view` here). Camera 2 has never
        // produced a frame (e.g. still connecting).
        store
            .upsert_camera(&cam(1, "unwatched-live"))
            .await
            .unwrap();
        store.upsert_camera(&cam(2, "never-started")).await.unwrap();

        let frame_stats = FrameStatsRegistry::new();
        frame_stats.observe_frame(1, frame_stats.begin_session(1), Utc::now(), 960, 540);

        let env = build_envelope(
            &store,
            1,
            "yolo",
            &online_cameras(&frame_stats, Instant::now()),
        )
        .await
        .expect("build envelope");
        let EnvelopeBody::CameraRoster(payload) = env.body else {
            panic!("expected a camera_roster body");
        };

        let online_of = |id: u64| {
            payload
                .cameras
                .iter()
                .find(|c| c.edge_camera_id == id)
                .unwrap_or_else(|| panic!("camera {id} missing from roster"))
                .online
        };
        assert_eq!(
            online_of(1),
            Some(true),
            "camera producing frames with no live-view subscriber must still report online"
        );
        assert_eq!(
            online_of(2),
            Some(false),
            "camera with no observed frame must report offline, not unknown"
        );
    }

    struct Capture(tokio::sync::mpsc::UnboundedSender<Envelope>);

    #[async_trait::async_trait]
    impl nexus_cloud_client::TunnelHandle for Capture {
        async fn send(&self, envelope: Envelope) -> Result<(), nexus_cloud_client::TunnelError> {
            let _ = self.0.send(envelope);
            Ok(())
        }
    }

    /// BUG-315 — the cloud derives `camera.offline` only from roster
    /// diffs, so the roster must go out when a camera's liveness flips
    /// and on every reconnect, with no `config.changed` involved. Before
    /// the fix the only triggers were startup, a camera edit and a
    /// failed-send retry, so a dead camera waited on an unrelated edit.
    #[tokio::test]
    async fn liveness_change_and_reconnect_publish_without_a_config_change() {
        use nexus_bus::BroadcastBus;
        use nexus_config::{
            CameraBehavior, CameraConfig, CameraDetector, CameraIngest, CameraOnvif,
            CameraTalkDown, StoreConfig,
        };
        use nexus_pipeline::stats::CAMERA_OFFLINE_AFTER_MS;
        use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
        use tokio::time::timeout;

        async fn next_online(rx: &mut UnboundedReceiver<Envelope>, step: &str) -> Option<bool> {
            let env = timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no camera_roster within 2 s: {step}"))
                .expect("capture open");
            let EnvelopeBody::CameraRoster(p) = env.body else {
                panic!("expected a camera_roster body");
            };
            p.cameras[0].online
        }
        async fn quiet(rx: &mut UnboundedReceiver<Envelope>) -> bool {
            timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err()
        }

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
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "cam".into(),
                ingest: CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    analysis_url: None,
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
                onvif: CameraOnvif::default(),
                talk_down: CameraTalkDown::default(),
                zones: vec![],
            })
            .await
            .unwrap();

        let frame_stats = Arc::new(FrameStatsRegistry::new());
        let epoch = frame_stats.begin_session(1);
        frame_stats.observe_frame(1, epoch, Utc::now(), 960, 540);

        let outbox = Arc::new(TunnelOutbox::new());
        let (tx, mut rx) = unbounded_channel();
        outbox.set_handle(Some(Arc::new(Capture(tx))));

        // Shifting this clock ages the last frame without a 90 s wait.
        let skew_ms = Arc::new(AtomicU64::new(0));
        let clock = {
            let skew_ms = skew_ms.clone();
            move || Instant::now() + Duration::from_millis(skew_ms.load(Ordering::Relaxed))
        };
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
        let task = tokio::spawn(run(
            store,
            bus.clone(),
            outbox.clone(),
            "yolo".into(),
            frame_stats.clone(),
            Duration::from_millis(20),
            clock,
        ));

        assert_eq!(
            next_online(&mut rx, "startup roster").await,
            Some(true),
            "startup roster"
        );
        assert!(quiet(&mut rx).await, "nothing changed, nothing sent");

        // The camera goes dark: no frame for longer than the offline window.
        // Config churn of other kinds, faster than the tick, must not push
        // the check back.
        let churn = tokio::spawn(async move {
            loop {
                let _ = bus
                    .publish(topic::CONFIG_CHANGED, &serde_json::json!({"kind": "rules"}))
                    .await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        skew_ms.store(CAMERA_OFFLINE_AFTER_MS as u64 + 1_000, Ordering::Relaxed);
        assert_eq!(
            next_online(&mut rx, "went offline").await,
            Some(false),
            "went offline"
        );
        assert!(quiet(&mut rx).await, "offline is sent once, not every tick");
        churn.abort();

        // Frames resume.
        skew_ms.store(0, Ordering::Relaxed);
        frame_stats.observe_frame(1, epoch, Utc::now(), 960, 540);
        assert_eq!(
            next_online(&mut rx, "came back").await,
            Some(true),
            "came back"
        );

        // A reconnect quicker than one tick still re-sends: a roster queued
        // on the old socket may never have reached the cloud.
        let (tx2, mut rx2) = unbounded_channel();
        outbox.set_handle(None);
        outbox.set_handle(Some(Arc::new(Capture(tx2))));
        assert_eq!(
            next_online(&mut rx2, "re-sent on reconnect").await,
            Some(true),
            "re-sent on reconnect"
        );
        assert!(quiet(&mut rx2).await, "once per reconnect");

        task.abort();
    }
}
