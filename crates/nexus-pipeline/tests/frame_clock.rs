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
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
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
use nexus_store::{ClipId, MotionEventKind, MotionEventRow, Store};
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
    sources: Mutex<VecDeque<Box<dyn FrameSource + Send>>>,
    rebuild_on_resize: bool,
    opened: Mutex<Vec<DateTime<Utc>>>,
    open_attempts: Mutex<Vec<DateTime<Utc>>>,
    refused_opens: Vec<usize>,
    io_error_opens: Vec<usize>,
    inflight_bytes: Option<u64>,
    closed: Mutex<Vec<DateTime<Utc>>>,
    resized: AtomicUsize,
    last_opened: Mutex<Option<ClipId>>,
    /// The clip whose media ended on a stream epoch change.
    epoch_ended: Mutex<Option<ClipId>>,
}

#[async_trait]
impl ClipRecorder for Recording {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        let started_at = args.started_at;
        let attempt = {
            let mut attempts = self.open_attempts.lock();
            let attempt = attempts.len();
            attempts.push(started_at);
            attempt
        };
        if self.io_error_opens.contains(&attempt) {
            return Err(std::io::Error::other("scripted open failure").into());
        }
        if self.refused_opens.contains(&attempt) {
            return Err(RecorderError::Refused);
        }
        let handle = self.stub.open(args).await?;
        self.opened.lock().push(started_at);
        *self.last_opened.lock() = Some(handle.clip_id);
        Ok(handle)
    }

    async fn inflight_size_bytes(&self, _handle: ClipHandle) -> Option<u64> {
        self.inflight_bytes
    }

    fn ended_on_epoch_change(&self, handle: ClipHandle) -> bool {
        *self.epoch_ended.lock() == Some(handle.clip_id)
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

    fn set_panic(&self, panic: bool) {
        self.stub.set_panic(panic);
    }

    fn is_panic(&self) -> bool {
        self.stub.is_panic()
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
        Ok(self.rebuild_on_resize)
    }

    fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
        self.sources.lock().pop_front()
    }
}

/// Reports `people[i]` people, side by side, on frame `i`, and records which
/// frames it was asked about.
struct People {
    people: Vec<usize>,
    seen: Mutex<Vec<usize>>,
    recorder: Arc<Recording>,
    panic_frames: std::ops::Range<usize>,
}

#[async_trait]
impl Detector for People {
    async fn detect(&self, f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        let i = f.frame_id as usize - 1;
        // Metadata precedes recorder work, so the driver must not apply the next frame's panic.
        self.recorder.set_panic(self.panic_frames.contains(&i));
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
    /// The recorder reports the open clip ended on an epoch change from
    /// this frame on.
    epoch_change_at: Option<usize>,
    graceful_stop: bool,
    refused_opens: Vec<usize>,
    io_error_opens: Vec<usize>,
    inflight_bytes: Option<u64>,
    rebuild_at: Option<usize>,
    panic_frames: std::ops::Range<usize>,
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
            epoch_change_at: None,
            graceful_stop: false,
            refused_opens: Vec::new(),
            io_error_opens: Vec::new(),
            inflight_bytes: None,
            rebuild_at: None,
            panic_frames: 0..0,
        }
    }
}

struct Outcome {
    /// `FRAME_METADATA` of frame `i`.
    metadata: Vec<FrameMetadata>,
    alerts: Vec<AlertEvent>,
    motion: Vec<MotionEventRow>,
    opened: Vec<DateTime<Utc>>,
    open_attempts: Vec<DateTime<Utc>>,
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
    let mut frames: Vec<Frame> = (0..=n)
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
    let (restart_go, restart_rx) = mpsc::unbounded_channel();
    let tail = s.rebuild_at.map(|i| frames.split_off(i + 1));
    let mut sources: VecDeque<Box<dyn FrameSource + Send>> = VecDeque::new();
    sources.push_back(Box::new(Scripted { frames, go: go_rx }));
    if let Some(frames) = tail {
        sources.push_back(Box::new(Scripted {
            frames,
            go: restart_rx,
        }));
    }
    let recorder = Arc::new(Recording {
        stub: StubClipRecorder::new(store.clone(), dir.path().join("clips")),
        sources: Mutex::new(sources),
        rebuild_on_resize: s.rebuild_at.is_some(),
        opened: Mutex::new(Vec::new()),
        open_attempts: Mutex::new(Vec::new()),
        refused_opens: s.refused_opens,
        io_error_opens: s.io_error_opens,
        inflight_bytes: s.inflight_bytes,
        closed: Mutex::new(Vec::new()),
        resized: AtomicUsize::new(0),
        last_opened: Mutex::new(None),
        epoch_ended: Mutex::new(None),
    });
    let people = || {
        Arc::new(People {
            people: s.people.clone(),
            seen: Mutex::new(Vec::new()),
            recorder: recorder.clone(),
            panic_frames: s.panic_frames.clone(),
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
        if s.epoch_change_at == Some(i) {
            *recorder.epoch_ended.lock() = *recorder.last_opened.lock();
        }
        if s.rebuild_at.is_some_and(|at| i > at) {
            restart_go.send(()).expect("rebuilt source running");
        } else {
            go.send(()).expect("source running");
        }
        // The resize frame breaks before publishing metadata.
        if s.rebuild_at == Some(i) {
            continue;
        }
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
    if s.graceful_stop {
        handle.stop.send(true).expect("supervisor running");
        tokio::time::timeout(Duration::from_secs(10), handle.task)
            .await
            .expect("supervisor stops")
            .expect("supervisor did not panic");
    } else {
        handle.task.abort();
    }
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
    let open_attempts = recorder.open_attempts.lock().clone();
    let closed = recorder.closed.lock().clone();
    let high_res = high.seen.lock().clone();
    let low_res = low.map(|d| d.seen.lock().clone()).unwrap_or_default();
    Outcome {
        metadata: tracked,
        alerts: fired,
        motion,
        opened,
        open_attempts,
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

/// A clip whose media ended on a stream epoch change (#375/#376) is rotated
/// on the next frame, and reopened there while motion is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clip_that_ended_on_an_epoch_change_is_rotated() {
    let clock = Clock::new(1_000, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 6], Bytetrack);
    s.epoch_change_at = Some(3);
    let o = run(s).await;
    assert_eq!(o.closed, [clock.wall(3)], "rotated on frame 3");
    assert_eq!(
        o.opened,
        [clock.wall(0), clock.wall(3)],
        "reopened on the rotation frame"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epoch_change_between_motion_samples_reopens_recording() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 12], Bytetrack);
    s.epoch_change_at = Some(3);
    let o = run(s).await;
    assert_eq!(o.closed, [clock.wall(3)], "rotated at the stream boundary");
    assert_eq!(
        o.opened,
        [clock.wall(0), clock.wall(3)],
        "continuous live motion must reopen after rotation even between sampled motion events"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epoch_reopen_with_every_frame_sampled() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 12], Bytetrack);
    s.clips.motion_events_sample_hz = 2.0;
    s.epoch_change_at = Some(3);
    let o = run(s).await;
    assert_eq!(o.opened, [clock.wall(0), clock.wall(3)]);
}

/// Rotation is recorder control, independent of persisted Updated events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_rotation_reopens_when_motion_updates_are_disabled() {
    for cause in ["epoch", "duration", "size"] {
        let (gap, boundary, count) = match cause {
            "duration" => (60_100, 5, 7),
            "size" => (500, 60, 63),
            _ => (500, 3, 8),
        };
        let clock = Clock::new(gap, 0, &[]);
        let mut s = Scenario::new(&clock, vec![1; count], Bytetrack);
        s.clips.motion_events_sample_hz = 0.0;
        match cause {
            "epoch" => s.epoch_change_at = Some(boundary),
            "size" => {
                s.clips.max_clip_bytes = 1;
                s.inflight_bytes = Some(1);
            }
            _ => {}
        }
        let o = run(s).await;
        assert_eq!(o.closed, [clock.wall(boundary)], "{cause}");
        assert_eq!(o.opened, [clock.wall(0), clock.wall(boundary)], "{cause}");
        assert_eq!(o.open_attempts, o.opened, "one open per rotation: {cause}");
        assert!(o.motion.iter().all(|m| m.kind != MotionEventKind::Updated));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initial_failed_open_preserves_motion_during_backoff() {
    for open_io_error in [false, true] {
        let clock = Clock::new(500, 0, &[]);
        let mut s = Scenario::new(&clock, vec![1, 2, 3, 3, 3, 3], IouNaive);
        s.clips.motion_events_sample_hz = 0.0;
        s.refused_opens = vec![0, 1];
        if open_io_error {
            s.io_error_opens = std::mem::take(&mut s.refused_opens);
        }
        let o = run(s).await;
        assert_eq!(o.open_attempts, [0, 1, 3].map(|i| clock.wall(i)));
        assert_eq!(o.opened, [clock.wall(3)]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initial_failed_open_retries_after_a_single_alert() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 6], IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.record_motion_clip_on_alert = true;
    s.rules = vec![person_rule(1_000, 10_000)];
    s.refused_opens = vec![0, 1];
    let o = run(s).await;
    assert_eq!(o.open_attempts, [0, 1, 3].map(|i| clock.wall(i)));
    assert_eq!(o.opened, [clock.wall(3)]);
    assert_eq!(o.alerts.len(), 1);
    assert_eq!(o.alerts[0].frame_id, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_rotation_retries_without_a_motion_event() {
    for open_io_error in [false, true] {
        let clock = Clock::new(500, 0, &[]);
        let mut s = Scenario::new(&clock, vec![1; 8], Bytetrack);
        s.clips.motion_events_sample_hz = 0.0;
        s.epoch_change_at = Some(3);
        s.refused_opens = vec![1, 2];
        if open_io_error {
            s.io_error_opens = std::mem::take(&mut s.refused_opens);
        }
        let o = run(s).await;
        assert_eq!(o.closed, [clock.wall(3)]);
        assert_eq!(o.opened, [clock.wall(0), clock.wall(6)]);
        assert_eq!(
            o.open_attempts,
            [clock.wall(0), clock.wall(3), clock.wall(4), clock.wall(6)]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_rotation_backs_off_and_recovers_with_post_roll() {
    for open_io_error in [false, true] {
        for step_ms in [0, HOUR_MS, -HOUR_MS] {
            let clock = Clock::new(500, step_ms, &[4, 80]);
            let mut people = vec![1; 186];
            people.extend([0; 6]);
            let mut s = Scenario::new(&clock, people, IouNaive);
            s.clips.motion_events_sample_hz = 0.0;
            s.clips.post_roll_secs = 2;
            s.epoch_change_at = Some(3);
            s.refused_opens = (1..=7).collect();
            if open_io_error {
                s.io_error_opens = std::mem::take(&mut s.refused_opens);
            }
            let o = run(s).await;
            assert_eq!(
                o.open_attempts,
                [0, 3, 4, 6, 10, 18, 34, 66, 126].map(|i| clock.wall(i)),
                "bounded retries with 30 s cap"
            );
            assert_eq!(o.opened, [clock.wall(0), clock.wall(126)]);
            assert_eq!(o.closed, [clock.wall(3), clock.wall(190)]);
        }
    }
}

/// Panic refuses once to publish recorder health, then resumes on the first
/// processed frame after recovery, even after a long pause or a wall-clock step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_recovery_does_not_wait_for_the_open_backoff() {
    for step_ms in [0, HOUR_MS, -HOUR_MS] {
        let clock = Clock::new(500, step_ms, &[4, 80]);
        let mut s = Scenario::new(&clock, vec![1; 90], IouNaive);
        s.clips.motion_events_sample_hz = 0.0;
        s.epoch_change_at = Some(3);
        s.panic_frames = 3..80;
        let o = run(s).await;
        assert_eq!(o.open_attempts, [0, 3, 80].map(|i| clock.wall(i)));
        assert_eq!(o.opened, [clock.wall(0), clock.wall(80)]);
        assert_eq!(o.closed, [clock.wall(3)]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_refusal_is_reset_when_post_roll_expires() {
    let clock = Clock::new(500, 0, &[]);
    let mut people = vec![1; 4];
    people.extend([0; 6]);
    people.extend([1; 4]);
    people.extend([0; 6]);
    people.extend([1; 4]);
    let mut s = Scenario::new(&clock, people, IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.post_roll_secs = 2;
    s.panic_frames = 0..18;
    let o = run(s).await;
    assert_eq!(
        o.open_attempts,
        [0, 10, 20].map(|i| clock.wall(i)),
        "each burst reports its first refusal; expired demand cannot reopen on recovery"
    );
    assert_eq!(o.opened, [clock.wall(20)]);
}

/// Captures supervisor diagnostics without a global subscriber or extra dependency.
struct OpenWarnings(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for OpenWarnings {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "nexus_pipeline::supervisor"
            && *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        event.record(
            &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                if field.name() == "message" {
                    let message = format!("{value:?}");
                    if message.starts_with("recorder.open failed:") {
                        self.0.lock().push(message);
                    }
                }
            },
        );
    }
}

// The current-thread runtime keeps the subscriber on the spawned supervisor's thread.
#[tokio::test]
async fn panic_then_io_failure_warns_once_and_retries_with_error_backoff() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let _subscriber = tracing::subscriber::set_default(OpenWarnings(logs.clone()));
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 100], IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.panic_frames = 0..80;
    s.io_error_opens = vec![1, 2, 4, 6];
    s.refused_opens = vec![3];
    // Attempt 5 succeeds on frame 95; the next rotation starts a new error streak.
    s.epoch_change_at = Some(97);
    let o = run(s).await;
    let logs = logs.lock();
    assert_eq!(
        logs.len(),
        2,
        "one warning per error streak, even after a refusal: {logs:?}"
    );
    assert_eq!(
        o.open_attempts,
        [0, 80, 81, 83, 87, 95, 97, 98].map(|i| clock.wall(i))
    );
    assert_eq!(o.opened, [clock.wall(95), clock.wall(98)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_retry_streak_ends_with_post_roll() {
    let clock = Clock::new(500, 0, &[]);
    let mut people = vec![1; 70];
    people.extend([0; 6]);
    people.extend([1; 6]);
    let mut s = Scenario::new(&clock, people, IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.post_roll_secs = 2;
    s.epoch_change_at = Some(3);
    s.refused_opens = (1..=7).collect();
    let o = run(s).await;
    assert_eq!(
        o.open_attempts,
        [0, 3, 4, 6, 10, 18, 34, 66, 76].map(|i| clock.wall(i))
    );
    assert_eq!(
        o.opened,
        [clock.wall(0), clock.wall(76)],
        "new motion does not inherit the 30 s backoff"
    );
    assert_eq!(o.closed, [clock.wall(3)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crowd_resize_reopens_without_sampled_motion() {
    for motion_ends in [false, true] {
        let clock = Clock::new(10_000, 0, &[]);
        let people = if motion_ends {
            vec![20, 20, 0, 0, 0, 0, 0, 0]
        } else {
            vec![20; 8]
        };
        let mut s = Scenario::new(&clock, people, IouNaive);
        s.clips.motion_events_sample_hz = 0.0;
        s.clips.post_roll_secs = 35;
        s.behavior.supervisor_downscale_crowded_threshold = Some(1);
        s.behavior.supervisor_downscale_sustained_secs = Some(30);
        s.behavior.supervisor_downscale_to_width = Some(512);
        s.rebuild_at = Some(3);
        let o = run(s).await;
        assert_eq!(o.opened, [clock.wall(0), clock.wall(4)]);
        if motion_ends {
            assert_eq!(o.closed, [clock.wall(3), clock.wall(6)]);
        } else {
            assert_eq!(o.closed, [clock.wall(3)]);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alerts_share_the_rotation_retry_budget() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 22], IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.record_motion_clip_on_alert = true;
    s.rules = vec![person_rule(0, 0)];
    s.epoch_change_at = Some(3);
    s.refused_opens = vec![1, 2];
    let o = run(s).await;
    assert_eq!(o.open_attempts, [0, 3, 4, 6].map(|i| clock.wall(i)));
    assert_eq!(o.opened, [clock.wall(0), clock.wall(6)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_does_not_reopen_on_a_died_only_frame() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1, 1, 1, 0, 0, 0], IouNaive);
    s.clips.post_roll_secs = 0;
    s.epoch_change_at = Some(3);
    let o = run(s).await;
    assert_eq!(o.closed, [clock.wall(3)]);
    assert_eq!(o.open_attempts, [clock.wall(0)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_stops_retrying_when_motion_ends() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1, 1, 1, 1, 0, 0, 0], IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.post_roll_secs = 0;
    s.epoch_change_at = Some(3);
    s.refused_opens = vec![1];
    let o = run(s).await;
    assert_eq!(o.closed, [clock.wall(3)]);
    assert_eq!(o.open_attempts, [clock.wall(0), clock.wall(3)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reopened_clip_keeps_its_post_roll() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0], IouNaive);
    s.clips.motion_events_sample_hz = 0.0;
    s.clips.post_roll_secs = 2;
    s.epoch_change_at = Some(3);
    let o = run(s).await;
    assert_eq!(o.opened, [clock.wall(0), clock.wall(3)]);
    assert_eq!(o.closed, [clock.wall(3), clock.wall(9)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_preserves_the_remaining_post_roll_deadline() {
    for boundary in [6, 9] {
        let clock = Clock::new(500, 0, &[]);
        let mut s = Scenario::new(&clock, vec![1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0], IouNaive);
        s.clips.motion_events_sample_hz = 0.0;
        s.clips.post_roll_secs = 2;
        s.epoch_change_at = Some(boundary);
        let o = run(s).await;
        if boundary == 6 {
            assert_eq!(o.opened, [clock.wall(0), clock.wall(6)]);
            assert_eq!(o.closed, [clock.wall(6), clock.wall(9)]);
        } else {
            assert_eq!(o.opened, [clock.wall(0)]);
            assert_eq!(o.closed, [clock.wall(9)]);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_finalizes_a_reopened_clip_once() {
    let clock = Clock::new(500, 0, &[]);
    let mut s = Scenario::new(&clock, vec![1; 8], Bytetrack);
    s.clips.motion_events_sample_hz = 0.0;
    s.epoch_change_at = Some(3);
    s.graceful_stop = true;
    let o = run(s).await;
    assert_eq!(o.opened, [clock.wall(0), clock.wall(3)]);
    assert_eq!(o.closed.len(), 2);
    assert_eq!(o.closed[0], clock.wall(3));
    assert!(o.closed[1] > clock.wall(8));
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
