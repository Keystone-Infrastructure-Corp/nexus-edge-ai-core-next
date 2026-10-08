//! Every duration the camera loop measures between two frames is measured on
//! the frames' monotonic capture stamps (`Frame::captured_mono`), so a step of
//! the wall clock between them, such as the first NTP sync on a box without a
//! real-time clock, moves none of them. What a person or the cloud is shown
//! (an alert's time, a clip's start and end, a motion row) keeps the frame's
//! wall-clock `captured_at`.
//!
//! Each test drives the real supervisor with a scripted source. The source
//! stamps frames with synthetic clocks and sends a frame only once the one
//! before it has been tracked, so every expectation is in capture time and
//! none depends on how fast the machine runs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraBehavior, CameraConfig, ClipsConfig, RuleConfig, RuleDebounce, RuleGates, RulePredicate,
    RulesConfig, StoreConfig, TrackerBackendKind, TrackerConfig,
};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::{spawn_camera, SightingSchedulerConfig};
use nexus_pipeline::{
    ClipFinal, ClipHandle, ClipMeta, ClipRecorder, FrameSource, FrameSourceError, OpenClip,
    RecorderError, StubClipRecorder,
};
use nexus_rules::RuleEvaluator;
use nexus_store::{MotionEventKind, MotionEventRow, Store};
use nexus_tracker::build_tracker;
use nexus_types::{AlertEvent, BBox, CameraId, Detection, Frame, FrameMetadata, PixelFormat};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use url::Url;

use TrackerBackendKind::{Bytetrack, IouNaive};

const W: u32 = 512;
const H: u32 = 288;
const HOUR_MS: i64 = 3_600_000;

fn wall0() -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
}

/// Frame `i`'s capture stamps: both clocks advance by `gap_ms` per frame,
/// and the wall clock also jumps by `step_ms` at each frame in `steps_at`.
#[derive(Clone)]
struct Clock {
    mono0: Instant,
    gap_ms: u64,
    step_ms: i64,
    steps_at: Vec<usize>,
}

impl Clock {
    fn new(gap_ms: u64, step_ms: i64, steps_at: &[usize]) -> Self {
        Self {
            mono0: Instant::now(),
            gap_ms,
            step_ms,
            steps_at: steps_at.to_vec(),
        }
    }

    fn mono(&self, i: usize) -> Instant {
        self.mono0 + Duration::from_millis(self.gap_ms * i as u64)
    }

    fn wall(&self, i: usize) -> DateTime<Utc> {
        let steps = self.steps_at.iter().filter(|&&s| s <= i).count() as i64;
        wall0()
            + chrono::Duration::milliseconds(self.gap_ms as i64 * i as i64 + self.step_ms * steps)
    }
}

/// Sends its frames one per `go`, then stays open so the supervisor never
/// rebuilds its source.
struct Scripted {
    frames: Vec<Frame>,
    go: mpsc::UnboundedReceiver<()>,
}

#[async_trait]
impl FrameSource for Scripted {
    async fn run(mut self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        for f in std::mem::take(&mut self.frames) {
            if self.go.recv().await.is_none() || tx.send(f).await.is_err() {
                return Ok(());
            }
        }
        std::future::pending().await
    }
}

/// Hands the supervisor the scripted source and records what it asks of the
/// recorder. Clip rows come from the stub so motion rows can reference them.
struct Recording {
    stub: StubClipRecorder,
    source: Mutex<Option<Box<dyn FrameSource + Send>>>,
    opened: Mutex<Vec<DateTime<Utc>>>,
    closed: Mutex<Vec<DateTime<Utc>>>,
    resized: AtomicUsize,
}

#[async_trait]
impl ClipRecorder for Recording {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        self.opened.lock().push(args.started_at);
        self.stub.open(args).await
    }

    /// Not the stub's close: it would discard a clip whose wall-clock span a
    /// step made short, and the clip's motion rows with it.
    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        self.closed.lock().push(args.ended_at);
        Ok(ClipMeta {
            clip_id: handle.clip_id,
            camera_id: handle.camera_id,
            path: PathBuf::new(),
            duration_ms: 0,
            size_bytes: 0,
            codec: "stub".into(),
            container: "mp4".into(),
            discarded: false,
        })
    }

    fn set_panic(&self, _panic: bool) {}

    fn is_panic(&self) -> bool {
        false
    }

    fn kind(&self) -> &'static str {
        "recording"
    }

    fn resize_camera_rgb_tap(
        &self,
        _id: CameraId,
        _w: u32,
        _h: u32,
    ) -> Result<bool, RecorderError> {
        self.resized.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
        self.source.lock().take()
    }
}

/// Reports `people[i]` people, side by side, on frame `i`, and records which
/// frames it was asked about.
struct People {
    people: Vec<usize>,
    seen: Mutex<Vec<usize>>,
}

#[async_trait]
impl Detector for People {
    async fn detect(&self, f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        let i = f.frame_id as usize - 1;
        self.seen.lock().push(i);
        let n = self.people.get(i).copied().unwrap_or(0);
        Ok((0..n)
            .map(|k| {
                let x = 8.0 + 24.0 * k as f32;
                Detection {
                    label: "person".into(),
                    confidence: 0.9,
                    bbox: BBox {
                        x1: x,
                        y1: 40.0,
                        x2: x + 20.0,
                        y2: 120.0,
                    },
                    attributes: Default::default(),
                }
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "people"
    }
}

fn person_rule(min_track_age_ms: u64, cooldown_ms: u64) -> RuleConfig {
    RuleConfig {
        id: "person".into(),
        name: "person".into(),
        predicate: RulePredicate {
            when: "object.label == 'person'".into(),
            severity: "low".into(),
        },
        gates: RuleGates::default(),
        debounce: RuleDebounce {
            min_track_age_ms,
            consecutive_frames: 1,
            cooldown_ms,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    }
}

struct Scenario {
    clock: Clock,
    /// People on frame `i`; its length is the number of frames.
    people: Vec<usize>,
    tracker: TrackerBackendKind,
    rules: Vec<RuleConfig>,
    clips: ClipsConfig,
    behavior: CameraBehavior,
    low_res: bool,
}

impl Scenario {
    fn new(clock: &Clock, people: Vec<usize>, tracker: TrackerBackendKind) -> Self {
        Self {
            clock: clock.clone(),
            people,
            tracker,
            rules: Vec::new(),
            clips: ClipsConfig::default(),
            behavior: CameraBehavior::default(),
            low_res: false,
        }
    }
}

struct Outcome {
    /// `FRAME_METADATA` of frame `i`.
    metadata: Vec<FrameMetadata>,
    alerts: Vec<AlertEvent>,
    motion: Vec<MotionEventRow>,
    opened: Vec<DateTime<Utc>>,
    closed: Vec<DateTime<Utc>>,
    /// Supervisor-frame resize requests made by the time frame `i` was tracked.
    resized: Vec<usize>,
    /// Frames each detector was asked about.
    high_res: Vec<usize>,
    low_res: Vec<usize>,
}

/// Run the scenario's frames, plus one empty frame whose arrival shows the
/// last scripted frame has been fully handled.
async fn run(s: Scenario) -> Outcome {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(256));
    let mut metadata = bus
        .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
        .await
        .expect("subscribe frame.metadata");
    let mut alerts = bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .expect("subscribe alert.event");
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
        name: "scripted".into(),
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
        behavior: s.behavior,
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");

    let n = s.people.len();
    let pixels = Arc::new(vec![0u8; (W * H * 3) as usize]);
    let frames = (0..=n)
        .map(|i| Frame {
            camera_id: 1,
            frame_id: i as u64 + 1,
            captured_at: s.clock.wall(i),
            captured_mono: s.clock.mono(i),
            width: W,
            height: H,
            format: PixelFormat::Rgb24,
            data: pixels.clone(),
            trace_id: format!("frame-{i}"),
        })
        .collect();
    let (go, go_rx) = mpsc::unbounded_channel();
    let recorder = Arc::new(Recording {
        stub: StubClipRecorder::new(store.clone(), dir.path().join("clips")),
        source: Mutex::new(Some(Box::new(Scripted { frames, go: go_rx }))),
        opened: Mutex::new(Vec::new()),
        closed: Mutex::new(Vec::new()),
        resized: AtomicUsize::new(0),
    });
    let people = || {
        Arc::new(People {
            people: s.people.clone(),
            seen: Mutex::new(Vec::new()),
        })
    };
    let high = people();
    let low = s.low_res.then(people);
    let tracker_cfg = TrackerConfig {
        backend: s.tracker,
        ..Default::default()
    };
    let handle = spawn_camera(
        cam,
        high.clone(),
        low.clone().map(|d| d as Arc<dyn Detector>),
        Arc::from(build_tracker(&tracker_cfg)),
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        s.clips,
        dir.path().to_path_buf(),
        Arc::new(RuleEvaluator::new(&RulesConfig::default(), &s.rules).expect("compile rules")),
        store.clone(),
        recorder.clone(),
        bus.clone(),
        Arc::new(LatestFrameCache::new()),
        Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        W,
        H,
        Arc::new(nexus_pipeline::NoopSightingHook),
        SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(nexus_pipeline::NoopSinkRouter),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    );

    let mut tracked = Vec::new();
    let mut resized = Vec::new();
    for i in 0..=n {
        go.send(()).expect("source running");
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), metadata.next())
                .await
                .unwrap_or_else(|_| panic!("frame {i} tracked within 10 s"))
                .expect("bus open")
                .expect("frame.metadata decodes");
            if m.frame_id == i as u64 + 1 {
                tracked.push(m);
                break;
            }
        }
        resized.push(recorder.resized.load(Ordering::Relaxed));
    }
    handle.task.abort();
    tracked.truncate(n);
    resized.truncate(n);

    // Every scripted frame's alerts were published before the last frame
    // was tracked.
    let mut fired = Vec::new();
    while let Ok(Some(a)) = tokio::time::timeout(Duration::from_millis(100), alerts.next()).await {
        fired.push(a.expect("alert.event decodes"));
    }
    let hours = chrono::Duration::milliseconds(3 * HOUR_MS);
    let motion = store
        .list_motion_events_for_camera(1, wall0() - hours, wall0() + hours, 10_000)
        .await
        .expect("list motion rows");
    let opened = recorder.opened.lock().clone();
    let closed = recorder.closed.lock().clone();
    let high_res = high.seen.lock().clone();
    let low_res = low.map(|d| d.seen.lock().clone()).unwrap_or_default();
    Outcome {
        metadata: tracked,
        alerts: fired,
        motion,
        opened,
        closed,
        resized,
        high_res,
        low_res,
    }
}

/// Track age and TTL, the dwell the annotator derives, and the rule's age
/// gate and cooldown, across an hour's step of the wall clock either way.
/// Frames are 500 ms apart; the rule fires from 1000 ms of age with a
/// 1200 ms cooldown, so on frames 2 and 5, stamped with those frames' wall
/// time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_clock_step_moves_no_track_age_dwell_or_cooldown() {
    for tracker in [IouNaive, Bytetrack] {
        for step in [HOUR_MS, -HOUR_MS] {
            let clock = Clock::new(500, step, &[4]);
            let mut s = Scenario::new(&clock, vec![1; 8], tracker);
            s.rules = vec![person_rule(1000, 1200)];
            let o = run(s).await;
            let ctx = format!("{tracker:?}, wall clock stepped {step} ms at frame 4");

            let id = o.metadata[0].objects[0].track_id;
            for (i, m) in o.metadata.iter().enumerate() {
                let t = &m.objects[0];
                assert_eq!(t.track_id, id, "{ctx}: frame {i} kept the track");
                assert_eq!(t.age_ms, 500 * i as u64, "{ctx}: frame {i}'s track age");
                assert_eq!(
                    t.attributes["motion.dwell_seconds"],
                    serde_json::json!(i / 2),
                    "{ctx}: frame {i}'s dwell"
                );
            }
            let fired: Vec<_> = o
                .alerts
                .iter()
                .map(|a| (a.frame_id, a.captured_at))
                .collect();
            assert_eq!(
                fired,
                [(3, clock.wall(2)), (6, clock.wall(5))],
                "{ctx}: alerts as (frame id, captured_at)"
            );
        }
    }
}

/// Motion rows are sampled once a second of capture time, and the clip
/// closes `post_roll_secs` of capture time after the person leaves (frame 8),
/// across steps while the person is in view (frame 5) and inside the grace
/// window (frame 10).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_clock_step_moves_neither_motion_sampling_nor_post_roll() {
    for step in [HOUR_MS, -HOUR_MS] {
        let clock = Clock::new(500, step, &[5, 10]);
        let mut people = vec![1; 8];
        people.extend([0; 6]);
        let mut s = Scenario::new(&clock, people, IouNaive);
        s.clips = ClipsConfig {
            post_roll_secs: 2,
            ..Default::default()
        };
        let o = run(s).await;
        let ctx = format!("wall clock stepped {step} ms at frames 5 and 10");

        let mut updated: Vec<_> = o
            .motion
            .iter()
            .filter(|r| r.kind == MotionEventKind::Updated)
            .map(|r| r.captured_at)
            .collect();
        updated.sort();
        let mut want = vec![clock.wall(2), clock.wall(4), clock.wall(6)];
        want.sort();
        assert_eq!(updated, want, "{ctx}: Updated rows on frames 2, 4 and 6");
        assert_eq!(
            o.opened,
            [clock.wall(0)],
            "{ctx}: one clip, opened on frame 0"
        );
        assert_eq!(o.closed, [clock.wall(12)], "{ctx}: closed on frame 12");
    }
}

/// A clip is rotated after five minutes of capture time: frames a minute
/// apart rotate it on frame 5, however the wall clock stepped at frame 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_clock_step_does_not_move_clip_rotation() {
    for step in [HOUR_MS, -HOUR_MS] {
        let clock = Clock::new(60_000, step, &[3]);
        let o = run(Scenario::new(&clock, vec![1; 7], Bytetrack)).await;
        let ctx = format!("wall clock stepped {step} ms at frame 3");
        assert_eq!(o.closed, [clock.wall(5)], "{ctx}: rotated on frame 5");
        assert_eq!(
            o.opened,
            [clock.wall(0), clock.wall(5)],
            "{ctx}: reopened on the rotation frame"
        );
    }
}

/// Both crowd hysteresis windows (the E3 detector downscale and the E2
/// supervisor-frame downscale) are held in capture time. The crowd appears on
/// frame 0 and frames are 10 s apart, so a 30 s window is met on frame 3,
/// however quickly the frames were processed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crowd_downscale_windows_are_held_in_capture_time() {
    let clock = Clock::new(10_000, 0, &[]);
    let mut s = Scenario::new(&clock, vec![20; 6], Bytetrack);
    s.behavior = CameraBehavior {
        detector_downscale_crowded_threshold: Some(1),
        detector_downscale_sustained_secs: Some(30),
        supervisor_downscale_crowded_threshold: Some(1),
        supervisor_downscale_sustained_secs: Some(30),
        supervisor_downscale_to_width: Some(W),
        ..Default::default()
    };
    s.low_res = true;
    let o = run(s).await;
    assert_eq!(o.high_res, [0, 1, 2, 3], "full-size detector until frame 3");
    assert_eq!(o.low_res, [4, 5, 6], "downscaled detector from frame 4");
    assert_eq!(
        o.resized,
        [0, 0, 0, 1, 1, 1],
        "one supervisor-frame resize, on frame 3"
    );
}
