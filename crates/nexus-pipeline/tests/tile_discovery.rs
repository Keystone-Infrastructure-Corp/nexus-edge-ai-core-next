//! Discovery crops through the real supervisor (`behavior.tile_discovery_cells`).
//!
//! The planner and scheduler have unit tests in `tile.rs`; this drives the
//! `run_camera` branch that owns the wiring: the scheduler must persist
//! across frames, run with no stage-1 detection and no density trigger, and
//! its crop detections must reach the tracker and the rules.
//!
//! No ORT; `virtual://` + an in-memory bus + sqlite on a tempdir, mirroring
//! `end_to_end_alert.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraConfig, ClipsConfig, RuleConfig, RulesBackendKind, RulesConfig, StoreConfig,
    TrackerConfig,
};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{ClipRecorder, StubClipRecorder};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_types::{AlertEvent, BBox, Detection, Frame};
use url::Url;

/// Sees nothing in the full frame and one person in any crop: the distant
/// object stage-1 cannot resolve.
#[derive(Default)]
struct OnlyInCrops {
    full: AtomicUsize,
    crops: AtomicUsize,
}

#[async_trait]
impl Detector for OnlyInCrops {
    async fn detect(&self, _f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        self.full.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }

    async fn detect_crop(
        &self,
        _crop: &Frame,
        _p: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        self.crops.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 100.0,
                y1: 50.0,
                x2: 140.0,
                y2: 150.0,
            },
            attributes: Default::default(),
        }])
    }

    fn name(&self) -> &'static str {
        "only_in_crops"
    }
}

struct NoSinks;

impl nexus_pipeline::SinkRouter for NoSinks {
    fn sinks_for(&self, _rule_id: &str) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discovery_crop_alerts_with_an_empty_first_pass_and_respects_its_revisit_interval() {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let mut sub = bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .expect("subscribe alert.event");

    let detector = Arc::new(OnlyInCrops::default());
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn nexus_tracker::Tracker> =
        Arc::from(nexus_tracker::build_tracker(&tracker_cfg));
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
        sinks: Vec::new(),
        verify: false,
        talk_down: None,
    };
    let rules_cfg = RulesConfig {
        backend: RulesBackendKind::Cel,
        ..Default::default()
    };
    let evaluator = Arc::new(RuleEvaluator::new(&rules_cfg, &[rule]).expect("compile cel rule"));

    let dir = tempfile::tempdir().expect("tmpdir");
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .expect("open store"),
    );

    let cam = CameraConfig {
        id: 1,
        name: "virtual-discovery".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("virtual://local").unwrap(),
            analysis_url: None,
            enabled: true,
            max_fps: 10,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec!["person".into()],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: nexus_config::CameraBehavior {
            tile_enabled: Some(true),
            // No `tile_trigger`: only discovery can plan a tile.
            tile_discovery_cells: vec![0],
            // Far longer than the test, so the one cell is due exactly once.
            tile_discovery_revisit_secs: Some(600),
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    };
    let recorder: Arc<dyn ClipRecorder> = Arc::new(StubClipRecorder::new(
        store.clone(),
        dir.path().join("clips"),
    ));
    store.upsert_camera(&cam).await.expect("seed cameras row");
    let handle = spawn_camera(
        cam,
        detector.clone(),
        None,
        tracker,
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        ClipsConfig::default(),
        std::env::temp_dir(),
        evaluator,
        store.clone(),
        recorder,
        bus.clone(),
        Arc::new(LatestFrameCache::new()),
        Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        960,
        540,
        Arc::new(nexus_pipeline::NoopSightingHook),
        nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(NoSinks),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    );

    // Stage-1 never reports anything, so this alert can only come from a
    // discovery crop that was merged, tracked and evaluated.
    let event = match tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("no AlertEvent from a discovery crop within 5s")
    {
        Some(Ok(ev)) => ev,
        other => panic!("bus closed or errored: {:?}", other.map(|r| r.is_ok())),
    };
    assert_eq!(event.label, "person");

    // Let several more frames be analysed: the scheduler must persist
    // across them and not re-run the cell before its interval.
    let seen = detector.full.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while detector.full.load(Ordering::SeqCst) < seen + 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("supervisor stopped analysing frames");
    assert_eq!(
        detector.crops.load(Ordering::SeqCst),
        1,
        "the discovery cell ran once within its revisit interval"
    );

    handle.task.abort();
}
