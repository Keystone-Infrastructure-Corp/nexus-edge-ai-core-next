//! BUG-136 — the cloud live wall must not inherit detector throughput.
//!
//! The supervisor's analysis loop `await`s `detect` inline, so before this
//! fix the only writer of [`LatestFrameCache`] ran once per inference. On a
//! saturated box that put the wall at 0.125 fps showing a frame over a
//! minute old, while the source was decoding at 15 fps and dropping those
//! fresh frames at the channel.
//!
//! The discriminator here is structural rather than a rate threshold: the
//! detector never returns at all. Pre-fix the cache is written zero times
//! and `get` stays `None` forever; post-fix the tap publishes at decode rate
//! regardless. 0-vs-N, so it cannot pass by luck on a slow CI box.
//!
//! No GStreamer, no ORT — `virtual://` + an in-memory bus + sqlite on a
//! tempdir.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nexus_bus::{Bus, BroadcastBus};
use nexus_config::{
    CameraConfig, ClipsConfig, RulesConfig, StoreConfig, TrackerConfig,
};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{ClipRecorder, StubClipRecorder};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_types::{Detection, Frame};
use url::Url;

/// Stands in for a wedged accelerator: the worker thread is alive, the
/// future simply never resolves. Also the shape of `supervisor.rs`'s
/// detector-error path, which returns before the post-inference cache write
/// and so produced zero cache updates for a camera whose detector was
/// failing.
struct NeverReturnsDetector;

#[async_trait]
impl Detector for NeverReturnsDetector {
    async fn detect(&self, _f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        std::future::pending().await
    }

    fn name(&self) -> &'static str {
        "never_returns"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wall_keeps_painting_while_inference_never_completes() {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn nexus_tracker::Tracker> =
        Arc::from(nexus_tracker::build_tracker(&tracker_cfg));
    let evaluator = Arc::new(
        RuleEvaluator::new(&RulesConfig::default(), &[]).expect("compile empty rule set"),
    );

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

    // `max_fps: 10` is 100 ms spacing, below the gate's MOTION_GAP_MS of
    // 125 ms, so the gate would pass on its 500 ms baseline. The tap is
    // upstream of the gate and publishes every decoded frame.
    let cam = CameraConfig {
        id: 1,
        name: "virtual-freshness".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("virtual://local").unwrap(),
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
            parking_lot_mode: false,
            anchor_ttl_secs: None,
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");

    let clips_dir = dir.path().join("clips");
    let recorder: Arc<dyn ClipRecorder> =
        Arc::new(StubClipRecorder::new(store.clone(), clips_dir.clone()));
    let cache = Arc::new(LatestFrameCache::new());

    let handle = spawn_camera(
        cam,
        Arc::new(NeverReturnsDetector),
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
        cache.clone(),
        Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        960,
        540,
        Arc::new(nexus_pipeline::NoopSightingHook),
        nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(nexus_pipeline::NoopSinkRouter),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    );

    // Sample the cache the way the LBR pump does.
    let mut seen: HashSet<u64> = HashSet::new();
    for _ in 0..40 {
        if let Some(entry) = cache.get(1) {
            seen.insert(entry.frame.frame_id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.task.abort();

    assert!(
        seen.len() >= 2,
        "the wall froze at detector rate: {} distinct frame_id(s) cached while inference \
         never completed. The cache is only being written after inference.",
        seen.len()
    );

    // The frame is live but nothing has been detected on it, and those are
    // different answers — a never-inferred camera must not report its empty
    // object list as belonging to the frame on screen.
    let entry = cache.get(1).expect("a frame was published");
    assert_eq!(
        entry.objects_frame_id, None,
        "objects claimed a frame the detector never ran on"
    );
}
