//! End-to-end smoke test: VirtualSource → MockDetector → IouNaiveTracker
//! → CelEngine rule → BroadcastBus.
//!
//! Closes M1's "first end-to-end CEL alert" item. The test wires the full
//! supervisor (`spawn_camera`) against an in-memory bus + an on-disk
//! sqlite Store under tempdir + the StubClipRecorder, runs a virtual
//! camera that emits a drifting "person" detection at 5fps, and asserts
//! an `AlertEvent` with rule_id = "any_person" lands on the bus within
//! a generous timeout.
//!
//! No GStreamer, no ORT. Pure CPU + sqlite under tempdir.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraConfig, ClipsConfig, RuleConfig, RulesBackendKind, RulesConfig, StoreConfig,
    TrackerConfig,
};
use nexus_inference::MockDetector;
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{ClipRecorder, StubClipRecorder};
use nexus_rules::RuleEvaluator;
use nexus_store::{EventStore, Store};
use nexus_types::AlertEvent;
use url::Url;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cel_rule_emits_alert_for_virtual_person() {
    // 1. In-memory bus + subscriber. Subscribe BEFORE spawning the camera
    //    so the supervisor's first publish lands on a live receiver.
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let mut sub = bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .expect("subscribe alert.event");

    // 2. Detector / tracker / rule wired by hand — no factory dance.
    let detector = Arc::new(MockDetector::new());
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn nexus_tracker::Tracker> =
        Arc::from(nexus_tracker::build_tracker(&tracker_cfg));

    // CEL rule: any object whose label is "person". Min track age = 0 and
    // consecutive_frames = 1 keep the latency low; the cooldown is
    // irrelevant because we exit after the first event.
    let rule = RuleConfig {
        id: "any_person".into(),
        name: "Any person".into(),
        predicate: nexus_config::RulePredicate {
            when: "object.label == 'person'".into(),
            severity: "low".into(),
        },
        gates: nexus_config::RuleGates {
            camera_filter: None,
            zones: None,
        },
        debounce: nexus_config::RuleDebounce {
            min_track_age_ms: 0,
            consecutive_frames: 1,
            cooldown_ms: 0,
        },
        enabled: true,
    };
    let rules_cfg = RulesConfig {
        backend: RulesBackendKind::Cel,
        ..Default::default()
    };
    let evaluator = Arc::new(RuleEvaluator::new(&rules_cfg, &[rule]).expect("compile cel rule"));

    // sqlite Store on tempdir. The supervisor needs the concrete Store
    // (not just the EventStore trait) so it can call insert_motion_event.
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .expect("open store"),
    );

    // 3. Spawn the camera. `virtual://` scheme dispatches to VirtualSource
    //    inside `build_source`; max_fps=5 → ~200ms per frame.
    let cam = CameraConfig {
        id: 1,
        name: "virtual-smoke".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("virtual://local").unwrap(),
            enabled: true,
            max_fps: 5,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec!["person".into()],
            model_override: None,
        },
        behavior: nexus_config::CameraBehavior {
            parking_lot_mode: false,
        },
        zones: vec![],
    };
    // Recorder + clips_dir under the same tempdir so artefacts are
    // cleaned up when the test ends.
    let clips_dir = dir.path().join("clips");
    let recorder: Arc<dyn ClipRecorder> =
        Arc::new(StubClipRecorder::new(store.clone(), clips_dir.clone()));
    // Pre-seed cameras row so motion_clips inserts don't FK-fail.
    store
        .upsert_camera(&cam)
        .await
        .expect("seed cameras row for the virtual camera");
    let handle = spawn_camera(
        cam,
        detector,
        tracker,
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        ClipsConfig::default(),
        std::env::temp_dir(),
        evaluator,
        store.clone(),
        recorder,
        bus.clone(),
        cache_arc(),
    );

    // 4. Wait for the first AlertEvent. 5s budget covers the gate warmup
    //    + the first decode→detect→track→rule trip; a green run typically
    //    takes <1s.
    let item = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("no AlertEvent on bus within 5s");
    let event = match item {
        Some(Ok(ev)) => ev,
        Some(Err(e)) => panic!("bus error: {e}"),
        None => panic!("bus subscription closed"),
    };

    assert_eq!(event.rule_id, "any_person");
    assert_eq!(event.label, "person");
    assert_eq!(event.camera_id, 1);
    // Track + frame ids should be populated by the supervisor.
    assert!(event.track_id.is_some(), "track_id must be populated");
    assert!(event.frame_id > 0, "frame_id must advance");

    // 5. Confirm the store also recorded the event (the supervisor writes
    //    to store BEFORE publishing, so this is guaranteed if we got the
    //    event off the bus).
    let stored = store.list_recent_events(10).await.expect("list ok");
    assert!(
        stored.iter().any(|e| e.event_id == event.event_id),
        "store must contain the same event"
    );

    // 5b. The supervisor opens a motion clip on the same frame the
    //     person track is born, then stamps events.clip_id for any
    //     alert raised on that frame. Give the supervisor a brief
    //     window to drain the deferred link write — the alert and
    //     the link land in the same loop iteration but the test
    //     observes through a separate sqlite connection.
    let mut linked: Option<i64> = None;
    for _ in 0..50 {
        match store
            .get_event_clip_id(&event.event_id.to_string())
            .await
            .expect("get_event_clip_id must succeed")
        {
            Some(cid) => {
                linked = Some(cid);
                break;
            }
            None => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let clip_id = linked.expect("events.clip_id must be stamped within 1s of the alert");
    let clip = store
        .get_clip(clip_id)
        .await
        .expect("get_clip must succeed")
        .expect("linked clip must exist");
    assert_eq!(
        clip.camera_id, event.camera_id,
        "clip must belong to the alert camera"
    );

    // 6. Tear down the supervisor task. abort() is best-effort; the test
    //    runtime is dropped immediately after this returns either way.
    handle.task.abort();
}

fn cache_arc() -> Arc<LatestFrameCache> {
    Arc::new(LatestFrameCache::new())
}
