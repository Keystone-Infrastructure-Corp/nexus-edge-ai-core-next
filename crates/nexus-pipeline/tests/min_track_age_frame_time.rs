//! `min_track_age_ms` gates a rule on how long the camera has watched the
//! object, so the age it compares against must be measured in frame time
//! (`Frame::captured_mono`), not in the time the engine happened to process
//! the frames in.
//!
//! Every production source stamps `captured_mono` (and the wall-clock
//! `captured_at` beside it) as the frame leaves the decoder, and the
//! analysis loop takes the latest frame, so
//! the two clocks differ by a frame's capture-to-tracker latency: its wait for
//! the loop plus its inference, including any wait for a detector shared with
//! other cameras. An age read at tracking time is off by however much that
//! latency has changed since the track's first frame. The last test shows it
//! through the real supervisor.
//! The others drive the real tracker backends into the real rule evaluator
//! with synthetic capture times, so the contract does not depend on how fast
//! the machine is.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraConfig, ClipsConfig, RuleConfig, RuleDebounce, RuleGates, RulePredicate, RulesConfig,
    StoreConfig, TrackerBackendKind, TrackerConfig,
};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{ClipRecorder, StubClipRecorder};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::{build_tracker, Tracker};
use nexus_types::{BBox, Detection, Frame, FrameMetadata};
use url::Url;

const W: u32 = 1024;
const H: u32 = 576;
const BACKENDS: [TrackerBackendKind; 2] =
    [TrackerBackendKind::IouNaive, TrackerBackendKind::Bytetrack];

fn tracker(backend: TrackerBackendKind) -> Box<dyn Tracker> {
    build_tracker(&TrackerConfig {
        backend,
        ..Default::default()
    })
}

/// Fires on every frame a person is at least 500 ms old: no streak, no cooldown.
fn evaluator() -> RuleEvaluator {
    let rule = RuleConfig {
        id: "person".into(),
        name: "person".into(),
        predicate: RulePredicate {
            when: "object.label == 'person'".into(),
            severity: "low".into(),
        },
        gates: RuleGates::default(),
        debounce: RuleDebounce {
            min_track_age_ms: 500,
            consecutive_frames: 1,
            cooldown_ms: 0,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    };
    RuleEvaluator::new(&RulesConfig::default(), &[rule]).unwrap()
}

fn at(ms: u64) -> Instant {
    static T0: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    *T0 + Duration::from_millis(ms)
}

fn person(i: u64) -> Vec<Detection> {
    let x = 100.0 + i as f32;
    vec![Detection {
        label: "person".into(),
        confidence: 0.9,
        bbox: BBox {
            x1: x,
            y1: 100.0,
            x2: x + 60.0,
            y2: 260.0,
        },
        attributes: Default::default(),
    }]
}

/// Track the frame and return how many alerts it fired.
fn fire(tracker: &dyn Tracker, eval: &RuleEvaluator, i: u64, captured_mono: Instant) -> usize {
    let tracked = tracker.update(person(i), captured_mono);
    eval.evaluate(
        1,
        i,
        Utc::now(),
        captured_mono,
        &String::from("t"),
        W,
        H,
        &[],
        &tracked,
    )
    .len()
}

/// 30 frames captured at 30 fps, tracked as fast as this loop runs. The
/// person becomes 500 ms old on frame 16 (528 ms of capture time), however
/// little time the loop took.
#[test]
fn the_gate_opens_at_500_ms_of_frame_time_however_fast_the_frames_are_tracked() {
    for backend in BACKENDS {
        let t = tracker(backend);
        let eval = evaluator();
        let first = (0..30u64).find(|&i| fire(t.as_ref(), &eval, i, at(i * 33)) > 0);
        assert_eq!(
            first,
            Some(16),
            "{backend:?}: first frame to pass the age gate"
        );
    }
}

/// Two frames captured 33 ms apart, the second tracked 600 ms after the
/// first because its inference was slow. The camera has watched the person
/// for 33 ms: the 500 ms gate must hold.
#[test]
fn a_processing_delay_does_not_age_the_track() {
    for backend in BACKENDS {
        let t = tracker(backend);
        let eval = evaluator();
        assert_eq!(fire(t.as_ref(), &eval, 0, at(0)), 0);
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert_eq!(
            fire(t.as_ref(), &eval, 1, at(33)),
            0,
            "{backend:?}: a 33 ms-old track passed the 500 ms age gate"
        );
    }
}

/// Answers the first frame at once and every later one after 400 ms: the
/// detector became contended after the person appeared.
struct SlowingDetector {
    calls: AtomicUsize,
}

#[async_trait]
impl Detector for SlowingDetector {
    async fn detect(&self, _f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        if self.calls.fetch_add(1, Ordering::Relaxed) > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        Ok(vec![Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 200.0,
                y1: 100.0,
                x2: 320.0,
                y2: 400.0,
            },
            attributes: Default::default(),
        }])
    }

    fn name(&self) -> &'static str {
        "slowing"
    }
}

/// The supervisor must hand the tracker each frame's capture stamp. A clock
/// read at tracking time ages the person by the 400 ms the detector slowed
/// down, while the camera watched it for only the capture-time difference.
/// The bus carries only the wall-clock `captured_at`, read beside the
/// monotonic stamp the age is measured on, so the two differences may round
/// 1 ms apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_supervisor_ages_a_track_in_capture_time_when_inference_slows() {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let mut metadata = bus
        .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
        .await
        .expect("subscribe frame.metadata");
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn Tracker> = Arc::from(build_tracker(&tracker_cfg));
    let evaluator =
        Arc::new(RuleEvaluator::new(&RulesConfig::default(), &[]).expect("compile empty rule set"));

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
        name: "virtual-slowing-detector".into(),
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
        behavior: Default::default(),
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");
    let recorder: Arc<dyn ClipRecorder> = Arc::new(StubClipRecorder::new(
        store.clone(),
        dir.path().join("clips"),
    ));

    let handle = spawn_camera(
        cam,
        Arc::new(SlowingDetector {
            calls: AtomicUsize::new(0),
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
        Arc::new(LatestFrameCache::new()),
        Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        640,
        480,
        Arc::new(nexus_pipeline::NoopSightingHook),
        nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(nexus_pipeline::NoopSinkRouter),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    );

    // (captured_at, track_id, age_ms) of the person on its first 4 tracked frames.
    let mut seen = Vec::new();
    while seen.len() < 4 {
        let m = tokio::time::timeout(std::time::Duration::from_secs(10), metadata.next())
            .await
            .expect("a tracked frame within 10 s")
            .expect("bus open")
            .expect("frame.metadata decodes");
        if let Some(o) = m.objects.first() {
            seen.push((m.captured_at, o.track_id, o.age_ms));
        }
    }
    handle.task.abort();

    let (born, id, age) = seen[0];
    assert_eq!(age, 0, "the first frame the person was tracked on");
    for &(captured_at, track_id, age_ms) in &seen[1..] {
        assert_eq!(track_id, id, "one person, one track");
        let watched = (captured_at - born).num_milliseconds() as u64;
        assert!(
            age_ms.abs_diff(watched) <= 1,
            "the camera watched the person for {watched} ms of capture time, but the track \
             is {age_ms} ms old"
        );
    }
}
