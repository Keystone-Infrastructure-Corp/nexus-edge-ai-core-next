//! BUG-218 — `frames_backpressure_dropped` has to be counted where the loss
//! actually happens.
//!
//! The arithmetic is unit-tested in `stats.rs`, but the arithmetic is not the
//! part that was wrong twice. The call site is. Frames reach the analysis loop
//! through two hops:
//!
//!   source --try_send--> bounded mpsc(8) --tap--> latest-wins watch --> loop
//!
//! The tap drains the mpsc unconditionally and cannot block, so that channel
//! only overflows under runtime starvation — counting there reports ~0 for a
//! slow consumer, which is the entire condition this counter exists to expose.
//! The loss that matters happens in the `watch`, which coalesces silently by
//! design, and only the analysis loop is downstream of it.
//!
//! The discriminator is structural, not a rate threshold: with the counter at
//! the tap this reads 0, with it at the analysis loop it reads tens. `calls`
//! keeps it honest about which of the requirement's two cases is under test —
//! a *wedged* loop would also report 0, so the detector here is slow and still
//! progressing.
//!
//! No GStreamer, no ORT — `virtual://` + an in-memory bus + sqlite on a
//! tempdir, mirroring `live_frame_freshness.rs`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nexus_bus::{BroadcastBus, Bus};
use nexus_config::{CameraConfig, ClipsConfig, RulesConfig, StoreConfig, TrackerConfig};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{ClipRecorder, StubClipRecorder};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_types::{Detection, Frame};
use url::Url;

/// A detector that is slow but alive — the box that cannot keep up, as
/// opposed to the wedged accelerator of BUG-217. The delay exceeds the
/// gate's `BASELINE_GAP_MS`, so every pass through the loop holds it open
/// long enough for the source to produce several frames the `watch` then
/// coalesces down to one.
struct SlowDetector {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Detector for SlowDetector {
    async fn detect(&self, _f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(vec![])
    }

    fn name(&self) -> &'static str {
        "slow"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_analysis_loop_is_counted_as_backpressure_not_reported_healthy() {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn nexus_tracker::Tracker> =
        Arc::from(nexus_tracker::build_tracker(&tracker_cfg));
    let evaluator =
        Arc::new(RuleEvaluator::new(&RulesConfig::default(), &[]).expect("compile empty rule set"));

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

    // 20 fps is 50 ms spacing — well inside the 600 ms the detector holds
    // the loop for, so each pass coalesces roughly a dozen frames away.
    let cam = CameraConfig {
        id: 1,
        name: "virtual-backpressure".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("virtual://local").unwrap(),
            analysis_url: None,
            enabled: true,
            max_fps: 20,
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
    let stats = Arc::new(nexus_pipeline::FrameStatsRegistry::new());
    let detect_calls = Arc::new(AtomicUsize::new(0));

    let handle = spawn_camera(
        cam,
        Arc::new(SlowDetector {
            calls: detect_calls.clone(),
        }),
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
        stats.clone(),
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

    tokio::time::sleep(Duration::from_secs(3)).await;
    handle.task.abort();

    let snap = stats.snapshot(1).expect("the camera produced frames");

    assert!(
        detect_calls.load(Ordering::Relaxed) >= 2,
        "the loop never progressed, so this measures a wedge rather than \
         backpressure: {} detect call(s)",
        detect_calls.load(Ordering::Relaxed)
    );
    assert!(
        snap.frames_backpressure_dropped >= 5,
        "the analysis loop ran far behind a 20 fps source and lost {} frame(s) \
         to the latest-wins watch. Counting anywhere upstream of that watch — \
         the live-view tap, which drains its channel unconditionally and so \
         never sees a gap — reports zero here, which is the console lying that \
         a starved camera is healthy.",
        snap.frames_backpressure_dropped
    );
    assert_eq!(
        snap.frames_dropped, 0,
        "every frame that reached the loop cleared the gate at this rate, so a \
         non-zero gate counter means the two are being conflated again"
    );
}
