//! End-to-end smoke test: VirtualSource → MockDetector → IouNaiveTracker
//! → CelEngine rule → BroadcastBus.
//!
//! Closes M1's "first end-to-end CEL alert" item. The test wires the full
//! supervisor (`spawn_camera`) against an in-memory bus + an in-memory
//! event store, runs a virtual camera that emits a drifting "person"
//! detection at 5fps, and asserts an `AlertEvent` with rule_id =
//! "any_person" lands on the bus within a generous timeout.
//!
//! No filesystem, no GStreamer, no ORT. Pure CPU.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{CameraConfig, RuleConfig, RulesBackendKind, RulesConfig, TrackerConfig};
use nexus_inference::MockDetector;
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_rules::RuleEvaluator;
use nexus_store::{EventStore, StoreError};
use nexus_types::AlertEvent;
use parking_lot::Mutex;
use url::Url;

/// In-memory `EventStore` so the test never touches sqlite. The supervisor
/// only needs `record_event` to succeed for the alert path; query-side
/// methods can stay unimplemented for the smoke run.
#[derive(Default)]
struct MemoryEventStore {
    events: Mutex<Vec<AlertEvent>>,
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn record_event(&self, event: &AlertEvent) -> Result<(), StoreError> {
        self.events.lock().push(event.clone());
        Ok(())
    }

    async fn list_recent_events(&self, _limit: i64) -> Result<Vec<AlertEvent>, StoreError> {
        Ok(self.events.lock().clone())
    }
}

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
        camera_filter: None,
        when: "object.label == 'person'".into(),
        severity: "low".into(),
        min_track_age_ms: 0,
        consecutive_frames: 1,
        cooldown_ms: 0,
        enabled: true,
    };
    let rules_cfg = RulesConfig {
        backend: RulesBackendKind::Cel,
        ..Default::default()
    };
    let evaluator = Arc::new(RuleEvaluator::new(&rules_cfg, &[rule]).expect("compile cel rule"));

    let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
    let cache = Arc::new(LatestFrameCache::new());

    // 3. Spawn the camera. `virtual://` scheme dispatches to VirtualSource
    //    inside `build_source`; max_fps=5 → ~200ms per frame.
    let cam = CameraConfig {
        id: 1,
        name: "virtual-smoke".into(),
        url: Url::parse("virtual://local").unwrap(),
        enabled: true,
        prompts: vec!["person".into()],
        model_override: None,
        zones: vec![],
        max_fps: 5,
        parking_lot_mode: false,
    };
    let handle = spawn_camera(
        cam,
        detector,
        tracker,
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        std::env::temp_dir(),
        evaluator,
        store.clone(),
        bus.clone(),
        cache,
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

    // 6. Tear down the supervisor task. abort() is best-effort; the test
    //    runtime is dropped immediately after this returns either way.
    handle.task.abort();
}
