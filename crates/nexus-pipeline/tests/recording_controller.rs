//! SPEC-089 Task 3: each camera's recording controller alone opens, closes,
//! rotates and retries its motion clips, on timers of its own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraBehavior, CameraConfig, ClipsConfig, RecordingPolicyConfig, RecordingPolicyMode,
    RuleConfig, RulesBackendKind, RulesConfig, StoreConfig, TrackerBackendKind, TrackerConfig,
};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::supervisor::{spawn_camera, CameraHandle, SightingSchedulerConfig};
use nexus_pipeline::{
    ClipFinal, ClipHandle, ClipMeta, ClipRecorder, FrameSource, FrameSourceError,
    FrameStatsRegistry, OpenClip, RecorderError, StubClipRecorder,
};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::build_tracker;
use nexus_types::{AlertEvent, BBox, CameraId, Detection, Frame, FrameMetadata, PixelFormat};
use parking_lot::Mutex;
use tokio::sync::mpsc;

const W: u32 = 512;
const H: u32 = 288;
const GAP_MS: u64 = 500;

fn wall_at(ms: u64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000 + ms as i64)
        .unwrap()
}

/// What the detector does on one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    /// `n` people and `m` cars (a car fires the scenario's rule).
    See(usize, usize),
    /// The detector errors.
    Fail,
    /// The detector never answers.
    Hang,
    /// Storage panic on or off before this frame is analysed, nobody seen.
    Panic(bool),
}

/// Sends its frames one per `go`, then stays open.
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

/// The stub recorder, logging every call made of it. Its open clip grows
/// by `growth` bytes per size check, and the clip ids in `epoch_ended`
/// report that a stream epoch change ended their media.
struct Logged {
    stub: StubClipRecorder,
    source: Mutex<Option<Box<dyn FrameSource + Send>>>,
    calls: Mutex<Vec<String>>,
    growth: u64,
    sizes: Mutex<std::collections::HashMap<i64, u64>>,
    epoch_ended: Vec<i64>,
}

#[async_trait]
impl ClipRecorder for Logged {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        let at = args.started_at;
        let opened = self.stub.open(args).await;
        let id = opened
            .as_ref()
            .map(|h| h.clip_id)
            .map_err(|e| e.to_string());
        self.calls.lock().push(format!("open {at} -> {id:?}"));
        opened
    }

    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        let at = args.ended_at;
        let closed = self.stub.close(handle, args).await;
        let discarded = closed
            .as_ref()
            .map(|m| m.discarded)
            .map_err(|e| e.to_string());
        self.calls
            .lock()
            .push(format!("close {} at {at} -> {discarded:?}", handle.clip_id));
        closed
    }

    async fn inflight_size_bytes(&self, handle: ClipHandle) -> Option<u64> {
        self.calls.lock().push(format!("size {}", handle.clip_id));
        let mut sizes = self.sizes.lock();
        let size = sizes.entry(handle.clip_id).or_default();
        *size += self.growth;
        Some(*size)
    }

    fn ended_on_epoch_change(&self, handle: ClipHandle) -> bool {
        self.epoch_ended.contains(&handle.clip_id)
    }

    fn set_panic(&self, panic: bool) {
        self.stub.set_panic(panic);
    }

    fn is_panic(&self) -> bool {
        self.stub.is_panic()
    }

    fn kind(&self) -> &'static str {
        "logged"
    }

    fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
        self.source.lock().take()
    }
}

/// Plays a [`Step`] per frame.
struct Script {
    steps: Vec<Step>,
    recorder: Mutex<Option<Arc<Logged>>>,
    seen: Mutex<Vec<usize>>,
}

fn boxes(label: &str, n: usize, x0: f32) -> impl Iterator<Item = Detection> + '_ {
    (0..n).map(move |k| {
        let x = x0 + 24.0 * k as f32;
        Detection {
            label: label.into(),
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
}

#[async_trait]
impl Detector for Script {
    async fn detect(&self, f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        let i = f.frame_id as usize - 1;
        let step = self.steps[i];
        if let Step::Panic(on) = step {
            if let Some(r) = self.recorder.lock().as_ref() {
                r.set_panic(on);
            }
        }
        self.seen.lock().push(i);
        match step {
            Step::Fail => Err(InferenceError::Failed("scripted".into())),
            Step::Hang => std::future::pending().await,
            Step::Panic(_) => Ok(Vec::new()),
            Step::See(people, cars) => Ok(boxes("person", people, 8.0)
                .chain(boxes("car", cars, 300.0))
                .collect()),
        }
    }

    fn name(&self) -> &'static str {
        "script"
    }
}

struct Rig {
    handle: CameraHandle,
    recorder: Arc<Logged>,
    store: Arc<Store>,
    stats: Arc<FrameStatsRegistry>,
    detector: Arc<Script>,
    go: mpsc::UnboundedSender<()>,
    metadata: futures::stream::BoxStream<'static, FrameMetadata>,
    alerts: futures::stream::BoxStream<'static, AlertEvent>,
    _dir: tempfile::TempDir,
}

struct Setup {
    steps: Vec<Step>,
    /// Capture offset of each frame from the first, in ms.
    at_ms: Vec<u64>,
    policy: Option<RecordingPolicyConfig>,
    clips: ClipsConfig,
    growth: u64,
    epoch_ended: Vec<i64>,
}

impl Setup {
    fn new(steps: Vec<Step>) -> Self {
        let at_ms = (0..steps.len() as u64).map(|i| i * GAP_MS).collect();
        Self {
            steps,
            at_ms,
            policy: None,
            clips: ClipsConfig {
                post_roll_secs: 2,
                ..Default::default()
            },
            growth: 0,
            epoch_ended: Vec::new(),
        }
    }
}

/// A rule firing on every car, so the scenario controls its alerts.
fn car_rule() -> RuleConfig {
    RuleConfig {
        id: "car".into(),
        name: "Car".into(),
        predicate: nexus_config::RulePredicate {
            when: "object.label == 'car'".into(),
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
    }
}

fn camera(recording_policy: Option<RecordingPolicyConfig>) -> CameraConfig {
    CameraConfig {
        id: 1,
        name: "controller".into(),
        ingest: nexus_config::CameraIngest {
            url: url::Url::parse("virtual://local").unwrap(),
            analysis_url: None,
            enabled: true,
            max_fps: 20,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec!["person".into(), "car".into()],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: CameraBehavior {
            recording_policy,
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    }
}

async fn rig(setup: Setup) -> Rig {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(1024));
    let metadata = bus
        .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
        .await
        .unwrap()
        .filter_map(|m| async move { m.ok() })
        .boxed();
    let alerts = bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .unwrap()
        .filter_map(|m| async move { m.ok() })
        .boxed();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .unwrap(),
    );
    let cam = camera(setup.policy);
    store.upsert_camera(&cam).await.unwrap();

    let mono0 = Instant::now();
    let pixels = Arc::new(vec![0u8; (W * H * 3) as usize]);
    let frames = setup
        .at_ms
        .iter()
        .enumerate()
        .map(|(i, &ms)| Frame {
            camera_id: 1,
            frame_id: i as u64 + 1,
            captured_at: wall_at(ms),
            captured_mono: mono0 + Duration::from_millis(ms),
            width: W,
            height: H,
            format: PixelFormat::Rgb24,
            data: pixels.clone(),
            trace_id: format!("frame-{i}"),
        })
        .collect();
    let (go, go_rx) = mpsc::unbounded_channel();
    let recorder = Arc::new(Logged {
        stub: StubClipRecorder::new(store.clone(), dir.path().join("clips")),
        source: Mutex::new(Some(Box::new(Scripted { frames, go: go_rx }))),
        calls: Mutex::new(Vec::new()),
        growth: setup.growth,
        sizes: Mutex::new(Default::default()),
        epoch_ended: setup.epoch_ended,
    });
    let detector = Arc::new(Script {
        steps: setup.steps,
        recorder: Mutex::new(Some(recorder.clone())),
        seen: Mutex::new(Vec::new()),
    });
    let tracker_cfg = TrackerConfig {
        backend: TrackerBackendKind::Bytetrack,
        ..Default::default()
    };
    let rules = RulesConfig {
        backend: RulesBackendKind::Cel,
        ..Default::default()
    };
    let stats = Arc::new(FrameStatsRegistry::new());
    let handle = spawn_camera(
        cam,
        detector.clone(),
        None,
        Arc::from(build_tracker(&tracker_cfg)),
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        setup.clips,
        dir.path().to_path_buf(),
        Arc::new(RuleEvaluator::new(&rules, &[car_rule()]).unwrap()),
        store.clone(),
        recorder.clone(),
        bus.clone(),
        Arc::new(LatestFrameCache::new()),
        stats.clone(),
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
    Rig {
        handle,
        recorder,
        store,
        stats,
        detector,
        go,
        metadata,
        alerts,
        _dir: dir,
    }
}

impl Rig {
    /// Release frame `i` and wait until the supervisor has analysed it: its
    /// metadata is published, or, for a failed frame, its detect ran.
    async fn feed(&mut self, i: usize) {
        self.go.send(()).unwrap();
        let fails = matches!(self.detector.steps[i], Step::Fail | Step::Hang);
        tokio::time::timeout(Duration::from_secs(10), async {
            if fails {
                while !self.detector.seen.lock().contains(&i) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            } else {
                while let Some(m) = self.metadata.next().await {
                    if m.frame_id == i as u64 + 1 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("frame {i} analysed within 10 s"));
    }

    fn calls(&self) -> Vec<String> {
        self.recorder.calls.lock().clone()
    }

    /// Ask the supervisor to stop and wait until it, and whatever it left
    /// recording, has ended.
    async fn stop(&self) {
        self.handle.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.handle.stop.closed())
            .await
            .expect("the camera stops and finalizes within 10 s");
    }
}

fn opens(calls: &[String]) -> usize {
    calls.iter().filter(|c| c.starts_with("open")).count()
}

fn closes(calls: &[String]) -> usize {
    calls.iter().filter(|c| c.starts_with("close")).count()
}

/// Wait up to `secs` for `cond` on the recorder's calls.
async fn calls_until(rig: &Rig, secs: u64, cond: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let calls = rig.calls();
        if cond(&calls) || Instant::now() > deadline {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Detector hang: the file deadline and stop do not wait for inference.
// ---------------------------------------------------------------------------

/// R-4: a person opens a clip, then the detector never answers again. The
/// encoded stream keeps flowing, so the five-minute file deadline must still
/// rotate the clip, and a stop must still finalize it, while the analysis
/// loop is parked in `detect`. Frames 1-2 are captured just before the
/// deadline; the hang starts on frame 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_detector_cannot_stop_rotation_or_stop() {
    let mut setup = Setup::new(vec![
        Step::See(1, 0),
        Step::See(1, 0),
        Step::See(1, 0),
        Step::Hang,
    ]);
    // The clip opens at 0; frames 1-3 land 4:58.5 to 4:59.5 into it, the gate's 500 ms apart.
    setup.at_ms = vec![0, 298_500, 299_000, 299_500];
    let mut rig = rig(setup).await;
    for i in 0..4 {
        rig.feed(i).await;
    }
    let calls = writes(&calls_until(&rig, 5, |c| opens(c) >= 2).await);
    assert_eq!(
        (opens(&calls), closes(&calls)),
        (2, 1),
        "the clip rotated at its deadline while inference hung: {calls:#?}"
    );
    // Rotated on the capture clock the last frame left running: 5 min after the open.
    assert!(
        calls[1].starts_with("close 1 at 2023-11-14 22:18:2"),
        "{calls:#?}"
    );
    assert!(
        calls[2].starts_with("open 2023-11-14 22:18:2"),
        "{calls:#?}"
    );

    rig.stop().await;
    let calls = writes(&rig.calls());
    assert_eq!(
        (opens(&calls), closes(&calls)),
        (2, 2),
        "the stop finalized the open clip: {calls:#?}"
    );
    assert!(calls.last().unwrap().starts_with("close 2 "), "{calls:#?}");
}

/// R-4/R-6: the five-minute file deadline is the recorder's own. Inference
/// hangs 299 s into a clip: the clip rotates at five minutes, at the same
/// instant in legacy and in shadow, however long the shadow policy waits
/// before calling analysis stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hang_late_in_a_clip_rotates_at_the_file_deadline_in_legacy_and_shadow() {
    let shadow = Some(RecordingPolicyConfig {
        mode: RecordingPolicyMode::Shadow,
        analysis_stale_after_ms: 60_000,
    });
    for policy in [None, shadow] {
        let mut setup = Setup::new(vec![
            Step::See(1, 0),
            Step::See(1, 0),
            Step::See(1, 0),
            Step::Hang,
        ]);
        setup.at_ms = vec![0, 298_000, 298_500, 299_000];
        setup.policy = policy;
        let mut rig = rig(setup).await;
        for i in 0..4 {
            rig.feed(i).await;
        }
        let calls = writes(&calls_until(&rig, 5, |c| opens(c) >= 2).await);
        assert_eq!(
            (opens(&calls), closes(&calls)),
            (2, 1),
            "{policy:?}: no rotation while inference hung: {calls:#?}"
        );
        // Five minutes after the open at 22:13:20, and the next file at once.
        assert!(
            calls[1].starts_with("close 1 at 2023-11-14 22:18:20"),
            "{policy:?}: {calls:#?}"
        );
        assert!(
            calls[2].starts_with("open 2023-11-14 22:18:20"),
            "{policy:?}: {calls:#?}"
        );
        rig.stop().await;
    }
}

fn observed_policy() -> Option<RecordingPolicyConfig> {
    Some(RecordingPolicyConfig {
        mode: RecordingPolicyMode::Observed,
        analysis_stale_after_ms: 10_000,
    })
}

/// R-4 through the real supervisor: a camera saved as observed records
/// degraded coverage from startup while its first analysis never returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_records_degraded_coverage_while_the_first_analysis_hangs() {
    let mut setup = Setup::new(vec![Step::Hang]);
    setup.policy = observed_policy();
    let mut rig = rig(setup).await;
    rig.feed(0).await;
    let calls = writes(&calls_until(&rig, 5, |c| opens(c) >= 1).await);
    assert_eq!((opens(&calls), closes(&calls)), (1, 0), "{calls:#?}");
    let runtime = rig.stats.recording_activity().snapshot(1, Instant::now());
    assert_eq!(
        runtime.map(|r| (r.mode, r.diagnostics.reason)),
        Some((
            RecordingPolicyMode::Observed,
            Some(nexus_pipeline::recording_activity::RecordingReason::DegradedCoverage)
        ))
    );
    rig.stop().await;
    assert_eq!(closes(&rig.calls()), 1, "the stop finalized it");
}

/// Observed through the real supervisor: a healthy, empty scene ends the
/// startup's degraded coverage through the recovery gate and opens no
/// motion clip after it; cars then open one, linked to their alert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_records_what_its_policy_wants_and_links_alerts() {
    let mut steps = vec![Step::See(0, 0); 16];
    steps.extend([Step::See(0, 1); 3]);
    steps.extend([Step::See(0, 0); 2]);
    let n = steps.len();
    let mut setup = Setup::new(steps);
    setup.policy = observed_policy();
    let mut rig = rig(setup).await;
    for i in 0..16 {
        rig.feed(i).await;
    }
    let quiet = writes(&rig.calls());
    assert_eq!(
        (opens(&quiet), closes(&quiet)),
        (1, 1),
        "only the startup's degraded coverage: {quiet:#?}"
    );
    for i in 16..n {
        rig.feed(i).await;
    }
    rig.stop().await;
    let calls = writes(&rig.calls());
    assert_eq!((opens(&calls), closes(&calls)), (2, 2), "{calls:#?}");
    let alert = tokio::time::timeout(Duration::from_secs(1), rig.alerts.next())
        .await
        .expect("a car alerted")
        .unwrap();
    assert_eq!(
        rig.store
            .get_event_clip_id(&alert.event_id.to_string())
            .await
            .unwrap(),
        Some(2),
        "linked to the clip the cars opened"
    );
}

/// The opens and closes, without the size checks between them.
fn writes(calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .filter(|c| !c.starts_with("size"))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Legacy equivalence: the controller makes the calls the supervisor made.
// ---------------------------------------------------------------------------

/// Every branch legacy has: an open on a born track, motion rows, the
/// sampled byte cap rotating, a stream epoch ending a clip, a failed
/// detect, cars firing alerts that link and keep the clip alive, a
/// post-roll close after ByteTrack lets its lost tracks go, a storage
/// panic refusing an open until it clears, and a final post-roll close.
fn equivalence_steps() -> Vec<Step> {
    let mut s = vec![Step::See(0, 0); 3];
    s.extend([Step::See(1, 0); 72]);
    s.push(Step::Fail);
    s.extend([Step::See(1, 1); 5]);
    s.extend([Step::See(1, 0); 10]);
    s.extend([Step::See(0, 0); 45]);
    s.push(Step::Panic(true));
    s.extend([Step::See(1, 0); 4]);
    s.push(Step::Panic(false));
    s.extend([Step::See(1, 1); 2]);
    s.extend([Step::See(1, 0); 4]);
    s.extend([Step::See(0, 0); 45]);
    s
}

/// The scenario's recorder calls, then one line per clip, motion and alert row.
async fn equivalence_run(policy: Option<RecordingPolicyConfig>) -> (Vec<String>, Vec<String>) {
    let steps = equivalence_steps();
    let n = steps.len();
    let mut setup = Setup::new(steps);
    setup.policy = policy;
    setup.clips.max_clip_bytes = 25;
    setup.clips.record_motion_clip_on_alert = true;
    setup.growth = 30;
    setup.epoch_ended = vec![2];
    let mut rig = rig(setup).await;
    for i in 0..n {
        rig.feed(i).await;
    }
    rig.stop().await;
    let calls = rig.calls();
    let mut rows = Vec::new();
    for id in 1..=opens(&calls) as i64 {
        if let Some(c) = rig.store.get_clip(id).await.unwrap() {
            rows.push(format!(
                "clip {} {} {:?} {} {}x{}",
                c.id, c.started_at, c.ended_at, c.duration_ms, c.frame_width, c.frame_height
            ));
        }
    }
    let day = chrono::Duration::days(1);
    for m in rig
        .store
        .list_motion_events_for_camera(1, wall_at(0) - day, wall_at(0) + day, 100_000)
        .await
        .unwrap()
    {
        rows.push(format!(
            "motion clip={} track={} {:?} {}",
            m.clip_id, m.track_id, m.kind, m.captured_at
        ));
    }
    let mut alerts = Vec::new();
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_millis(50), rig.alerts.next()).await
    {
        alerts.push(ev);
    }
    for ev in alerts {
        let clip = rig
            .store
            .get_event_clip_id(&ev.event_id.to_string())
            .await
            .unwrap();
        rows.push(format!(
            "alert {} {} clip={clip:?}",
            ev.rule_id, ev.captured_at
        ));
    }
    (calls, rows)
}

fn digest(rows: &[String]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(rows.join("\n")))
}

/// Recorded at base a3f646d, before the controller existed: the supervisor
/// made these calls and left these rows on [`equivalence_steps`].
const SUPERVISOR_CALLS: &[&str] = &[
    "open 2023-11-14 22:13:21.500 UTC -> Ok(1)",
    "size 1",
    "close 1 at 2023-11-14 22:13:51.500 UTC -> Ok(false)",
    "open 2023-11-14 22:13:51.500 UTC -> Ok(2)",
    "close 2 at 2023-11-14 22:13:52 UTC -> Ok(true)",
    "open 2023-11-14 22:13:52 UTC -> Ok(3)",
    "size 3",
    "close 3 at 2023-11-14 22:14:22 UTC -> Ok(false)",
    "open 2023-11-14 22:14:22 UTC -> Ok(4)",
    "close 4 at 2023-11-14 22:14:22.500 UTC -> Ok(true)",
    "open 2023-11-14 22:14:28.500 UTC -> Err(\"recorder is panic-paused; new clips refused\")",
    "open 2023-11-14 22:14:30.500 UTC -> Ok(5)",
    "close 5 at 2023-11-14 22:14:51 UTC -> Ok(false)",
];
const SUPERVISOR_ROWS: (usize, &str) = (
    125,
    "d6b210e6f1f363a1b461a328044979456c11433f3aea4016156b0ebdda5e13b7",
);

/// Controller-driven legacy, the default, makes the calls the supervisor's
/// loop made, at the same capture times, and leaves the same clip, motion
/// and alert-link rows. Shadow does too: only diagnostics differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_driven_legacy_matches_the_supervisor_it_replaced() {
    for policy in [
        None,
        Some(RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 10_000,
        }),
    ] {
        let (calls, rows) = equivalence_run(policy).await;
        assert_eq!(calls, SUPERVISOR_CALLS, "{policy:?}");
        assert_eq!(
            (rows.len(), digest(&rows).as_str()),
            SUPERVISOR_ROWS,
            "{policy:?}: {rows:#?}"
        );
    }
}

/// R-5: the reconciler's forced stop aborts a supervisor that cannot honour
/// a stop, here one parked in a detect while its clip is open. The
/// controller owns that clip: the bare abort makes no further decision and
/// leaves it open (process shutdown drains it), and the stop request the
/// reconciler makes finalizes it before the camera's `closed()` resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_stop_finalizes_the_clip_an_aborted_supervisor_held() {
    let mut rig = rig(Setup::new(vec![Step::See(1, 0), Step::Hang])).await;
    rig.feed(0).await;
    rig.feed(1).await;
    rig.handle.task.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        writes(&rig.calls()).len(),
        1,
        "an abort alone closes nothing: {:#?}",
        rig.calls()
    );
    rig.stop().await;
    let calls = writes(&rig.calls());
    assert_eq!((opens(&calls), closes(&calls)), (1, 1), "{calls:#?}");
    assert!(calls[1].starts_with("close 1 "), "{calls:#?}");
}

// ---------------------------------------------------------------------------
// The controller on its own, against a recorder with barriers.
// ---------------------------------------------------------------------------

mod controller {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use nexus_pipeline::recording_activity::{AnalysisOutcome, RecordingReason};
    use nexus_pipeline::recording_controller::{
        self as rc, Alert, ControllerConfig, ControllerGuard, EpochChange, EvidenceProducer,
        Motion, RecordingCommand, RecordingControl, RecordingError, RecordingReceipt,
    };
    use nexus_pipeline::recording_diagnostics::EvidenceCounts;
    use tokio::sync::{watch, Semaphore};

    use super::*;

    const FREE: usize = 1 << 20;
    const SHORT: Duration = Duration::from_millis(200);
    const LONG: Duration = Duration::from_secs(5);

    /// Stub-backed clips whose opens and closes each wait for a permit the
    /// test hands out, logging every step. A clip has `preroll_ms` of media
    /// when it opens and gains media in real time unless the source is lost.
    struct Fake {
        stub: StubClipRecorder,
        calls: Mutex<Vec<String>>,
        opens: Semaphore,
        closes: Semaphore,
        panic: AtomicBool,
        preroll_ms: u64,
        source_lost: AtomicBool,
        opened: Mutex<HashMap<i64, tokio::time::Instant>>,
        epoch_ended: Mutex<Vec<i64>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }

        fn log(&self, call: String) {
            self.calls.lock().push(call);
        }
    }

    #[async_trait]
    impl ClipRecorder for Fake {
        async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
            self.log("opening".into());
            self.opens.acquire().await.unwrap().forget();
            if self.panic.load(Ordering::SeqCst) {
                self.log("refused".into());
                return Err(RecorderError::Refused);
            }
            let dims = format!("{}x{}", args.frame_width, args.frame_height);
            let handle = self.stub.open(args).await?;
            self.opened
                .lock()
                .insert(handle.clip_id, tokio::time::Instant::now());
            self.log(format!("opened {} {dims}", handle.clip_id));
            Ok(handle)
        }

        async fn close(
            &self,
            handle: ClipHandle,
            args: ClipFinal,
        ) -> Result<ClipMeta, RecorderError> {
            self.log(format!("closing {}", handle.clip_id));
            self.closes.acquire().await.unwrap().forget();
            let media = self.inflight_media_ms(handle).unwrap_or(0);
            self.opened.lock().remove(&handle.clip_id);
            let meta = self.stub.close(handle, args).await;
            self.log(format!("closed {} media={}", handle.clip_id, media / 1_000));
            meta
        }

        fn inflight_media_ms(&self, handle: ClipHandle) -> Option<u64> {
            let opened = *self.opened.lock().get(&handle.clip_id)?;
            let live = if self.source_lost.load(Ordering::SeqCst) {
                0
            } else {
                opened.elapsed().as_millis() as u64
            };
            Some(self.preroll_ms + live)
        }

        fn ended_on_epoch_change(&self, handle: ClipHandle) -> bool {
            self.epoch_ended.lock().contains(&handle.clip_id)
        }

        fn set_panic(&self, panic: bool) {
            self.panic.store(panic, Ordering::SeqCst);
        }

        fn is_panic(&self) -> bool {
            self.panic.load(Ordering::SeqCst)
        }

        fn kind(&self) -> &'static str {
            "fake"
        }
    }

    struct Cam {
        control: RecordingControl,
        guard: Option<ControllerGuard>,
        stop: watch::Sender<bool>,
        rec: Arc<Fake>,
        store: Arc<Store>,
        stats: Arc<FrameStatsRegistry>,
        producer: EvidenceProducer,
        t0: Instant,
        w0: DateTime<Utc>,
        _dir: tempfile::TempDir,
    }

    fn config(policy: Option<RecordingPolicyConfig>) -> ControllerConfig {
        ControllerConfig {
            camera_id: 1,
            generation: 6,
            policy,
            policy_revision: "rev-start".into(),
            tracker: "bytetrack".into(),
            post_roll_secs: 1,
            pre_roll_secs: 0,
            max_clip_bytes: 0,
            record_motion_clip_on_alert: true,
            frame_width: W,
            frame_height: H,
        }
    }

    fn shadow(stale_after_ms: u64) -> ControllerConfig {
        config(Some(RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: stale_after_ms,
        }))
    }

    fn observed(post_roll_secs: u32) -> ControllerConfig {
        ControllerConfig {
            post_roll_secs,
            ..config(Some(RecordingPolicyConfig {
                mode: RecordingPolicyMode::Observed,
                analysis_stale_after_ms: 1_000,
            }))
        }
    }

    /// `gate_opens` / `gate_closes`: those calls wait for permits.
    async fn cam(
        cfg: ControllerConfig,
        preroll_ms: u64,
        gate_opens: bool,
        gate_closes: bool,
    ) -> Cam {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        store
            .upsert_camera(&super::camera(cfg.policy))
            .await
            .unwrap();
        let rec = Arc::new(Fake {
            stub: StubClipRecorder::new(store.clone(), dir.path().join("clips")),
            calls: Mutex::new(Vec::new()),
            opens: Semaphore::new(if gate_opens { 0 } else { FREE }),
            closes: Semaphore::new(if gate_closes { 0 } else { FREE }),
            panic: AtomicBool::new(false),
            preroll_ms,
            source_lost: AtomicBool::new(false),
            opened: Mutex::new(HashMap::new()),
            epoch_ended: Mutex::new(Vec::new()),
        });
        let stats = Arc::new(FrameStatsRegistry::new());
        let (stop, stop_rx) = watch::channel(false);
        let producer = EvidenceProducer::new(cfg.generation, cfg.policy);
        let (control, guard) = rc::spawn(cfg, rec.clone(), store.clone(), stats.clone(), stop_rx);
        Cam {
            control,
            guard: Some(guard),
            stop,
            rec,
            store,
            stats,
            producer,
            t0: Instant::now(),
            w0: Utc::now(),
            _dir: dir,
        }
    }

    /// A live track, `born` on this frame or not.
    fn live(born: bool) -> Motion {
        Motion {
            live: true,
            born,
            ..Motion::default()
        }
    }

    fn quiet() -> Motion {
        Motion::default()
    }

    impl Cam {
        fn clock(&self, ms: u64) -> (Instant, DateTime<Utc>) {
            (
                self.t0 + Duration::from_millis(ms),
                self.w0 + chrono::Duration::milliseconds(ms as i64),
            )
        }

        fn elapsed_ms(&self) -> u64 {
            self.t0.elapsed().as_millis() as u64
        }

        async fn send(
            &self,
            command: RecordingCommand,
            timeout: Duration,
        ) -> Result<RecordingReceipt, RecordingError> {
            self.control.request(command, timeout).await
        }

        /// Frame `ms` arrives, then its analysis: `observed` qualifying
        /// objects and legacy's view of its motion.
        async fn frame_within(
            &mut self,
            ms: u64,
            outcome: AnalysisOutcome,
            observed: u32,
            motion: Motion,
            timeout: Duration,
        ) -> Result<RecordingReceipt, RecordingError> {
            let (mono, wall) = self.clock(ms);
            let arrived = self.producer.arrived(mono, wall);
            self.send(RecordingCommand::Evidence(arrived), timeout)
                .await?;
            let counts = EvidenceCounts {
                observed,
                ..EvidenceCounts::default()
            };
            let analysed = self
                .producer
                .analysed(mono, wall, outcome, counts, Some(motion));
            self.send(RecordingCommand::Evidence(analysed), timeout)
                .await
        }

        async fn frame(&mut self, ms: u64, motion: Motion) -> RecordingReceipt {
            self.frame_within(ms, AnalysisOutcome::Success, 0, motion, LONG)
                .await
                .unwrap()
        }

        /// A successful analysis captured now, with `observed` qualifying objects.
        async fn now(&mut self, outcome: AnalysisOutcome, observed: u32) -> RecordingReceipt {
            let ms = self.elapsed_ms();
            self.frame_within(ms, outcome, observed, quiet(), LONG)
                .await
                .unwrap()
        }

        async fn alert(&self, ms: u64, event_id: &str) -> RecordingReceipt {
            let (captured_mono, captured_at) = self.clock(ms);
            let alert = Alert {
                event_ids: vec![event_id.into()],
                captured_mono,
                captured_at,
            };
            self.send(RecordingCommand::Alert(alert), LONG)
                .await
                .unwrap()
        }

        async fn event(&self, ms: u64) -> String {
            let ev = AlertEvent {
                event_id: uuid::Uuid::from_u128(u128::from(ms) + 1),
                camera_id: 1,
                rule_id: "car".into(),
                track_id: Some(1),
                label: "car".into(),
                severity: nexus_types::Severity::Low,
                bbox: None,
                frame_id: 1,
                captured_at: self.clock(ms).1,
                trace_id: "t".into(),
                frame_w: W,
                frame_h: H,
                artifacts: Default::default(),
                context: serde_json::Map::new(),
            };
            self.store
                .record_event_and_enqueue_classified(&ev, &[], true)
                .await
                .unwrap();
            ev.event_id.to_string()
        }

        async fn linked(&self, event_id: &str) -> Option<i64> {
            self.store.get_event_clip_id(event_id).await.unwrap()
        }

        /// Wait up to `secs` for the recorder to log a call starting `call`.
        async fn until(&self, secs: u64, call: &str) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                let calls = self.rec.calls();
                if calls.iter().any(|c| c.starts_with(call)) {
                    return calls;
                }
                assert!(Instant::now() < deadline, "no {call:?} in {calls:#?}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Healthy empty analyses every 200 ms until the recorder logs a
        /// call starting `call`.
        async fn analysing_until(&mut self, secs: u64, call: &str) {
            let deadline = Instant::now() + Duration::from_secs(secs);
            while !self.rec.calls().iter().any(|c| c.starts_with(call)) {
                assert!(
                    Instant::now() < deadline,
                    "no {call:?} in {:#?}",
                    self.rec.calls()
                );
                self.now(AnalysisOutcome::Success, 0).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        /// Successes every 200 ms until the recovery gate (three passes
        /// over two seconds) has passed.
        async fn healthy(&mut self) {
            for _ in 0..12 {
                self.now(AnalysisOutcome::Success, 0).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        fn runtime(
            &self,
        ) -> Option<nexus_pipeline::recording_diagnostics::RecordingActivityRuntime> {
            self.stats.recording_activity().snapshot(1, Instant::now())
        }

        async fn stop(&self) -> RecordingReceipt {
            let receipt = self.send(RecordingCommand::Stop, LONG).await.unwrap();
            tokio::time::timeout(LONG, self.stop.closed())
                .await
                .expect("the controller ends after its stop");
            receipt
        }
    }

    /// The shadow policy's reason, whether it would record, and whether its
    /// trace has a gap.
    fn proposed(c: &Cam) -> Option<(Option<RecordingReason>, bool, bool)> {
        c.runtime().map(|r| {
            let d = r.diagnostics;
            (d.reason, d.shadow_open, d.trace_incomplete)
        })
    }

    /// R-4/R-6: shadow proposes what observed would do, on its own clock. A
    /// first analysis that never returns is degraded coverage from the frame
    /// that arrived; a healthy, quiet camera whose detector then hangs is
    /// proposed degraded once analysis goes stale. Legacy writes nothing and
    /// the trace has no gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shadow_proposes_degraded_coverage_when_analysis_hangs() {
        let mut c = cam(shadow(1_000), 0, false, false).await;
        let (mono, wall) = c.clock(0);
        let arrived = c.producer.arrived(mono, wall);
        c.send(RecordingCommand::Evidence(arrived), LONG)
            .await
            .unwrap();
        assert_eq!(
            proposed(&c),
            Some((Some(RecordingReason::DegradedCoverage), true, false)),
            "the first analysis is still pending"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while proposed(&c) != Some((Some(RecordingReason::Quiet), false, false)) {
            assert!(Instant::now() < deadline, "{:#?}", c.runtime());
            c.now(AnalysisOutcome::Success, 0).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // The detector hangs: no evidence at all from here.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(
            proposed(&c),
            Some((Some(RecordingReason::DegradedCoverage), true, false)),
            "stale analysis is proposed as degraded coverage"
        );
        assert!(c.rec.calls().is_empty(), "legacy recorded nothing");
    }

    /// R-4: freshness is judged when the controller uses the evidence, not
    /// when its frame was captured. A frame whose analysis takes 1.5 s under
    /// a 1 s stale deadline cannot end degraded coverage, though the success
    /// run it extends met the recovery gate long before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evidence_consumed_after_the_stale_deadline_is_stale() {
        let mut c = cam(shadow(1_000), 0, false, false).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while proposed(&c) != Some((Some(RecordingReason::Quiet), false, false)) {
            assert!(Instant::now() < deadline, "{:#?}", c.runtime());
            c.now(AnalysisOutcome::Success, 0).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let (mono, wall) = c.clock(c.elapsed_ms());
        let arrived = c.producer.arrived(mono, wall);
        c.send(RecordingCommand::Evidence(arrived), LONG)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let analysed = c.producer.analysed(
            mono,
            wall,
            AnalysisOutcome::Success,
            EvidenceCounts::default(),
            Some(quiet()),
        );
        c.send(RecordingCommand::Evidence(analysed), LONG)
            .await
            .unwrap();
        assert_eq!(
            proposed(&c),
            Some((Some(RecordingReason::DegradedCoverage), true, false)),
            "1.5 s old when used"
        );
    }

    /// R-6: a live policy change mid-minute keeps what the earlier policy
    /// counted in that minute: each revision's minute is a row of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_policy_change_keeps_the_earlier_policys_minute() {
        let mut c = cam(shadow(1_000), 0, false, false).await;
        c.frame(0, quiet()).await;
        c.frame(500, quiet()).await;
        c.send(
            RecordingCommand::PolicyChanged(shadow(2_000).policy, "rev-2".into()),
            LONG,
        )
        .await
        .unwrap();
        c.frame(1_000, quiet()).await;
        c.stop().await;
        let minutes = c.store.list_recording_activity_minutes(1).await.unwrap();
        let kept: Vec<_> = minutes
            .iter()
            .map(|m| (m.minute, m.policy_revision.as_str()))
            .collect();
        assert_eq!(kept, [(0, "rev-start"), (0, "rev-2")], "{minutes:#?}");
    }

    /// A slow open is waited for, never raced: the frames behind it time
    /// out where their requester can see it, and the clip they then find is
    /// the one that open produced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delayed_open_is_waited_for_by_its_one_writer() {
        let mut c = cam(config(None), 0, true, false).await;
        let slow = c
            .frame_within(0, AnalysisOutcome::Success, 1, live(true), SHORT)
            .await;
        assert_eq!(slow.unwrap_err(), RecordingError::Timeout(SHORT));
        let queued = c
            .frame_within(500, AnalysisOutcome::Success, 1, live(false), SHORT)
            .await;
        assert_eq!(queued.unwrap_err(), RecordingError::Timeout(SHORT));
        assert_eq!(c.rec.calls(), ["opening"], "one writer at a time");
        c.rec.opens.add_permits(FREE);
        let later = c.frame(1_000, live(false)).await;
        assert_eq!(later.clip.map(|h| h.clip_id), Some(1));
        assert_eq!(c.rec.calls(), ["opening", "opened 1 512x288"]);
    }

    /// A close slower than its requester stays the controller's: the
    /// requester sees a timeout, and the person who arrives meanwhile gets a
    /// new clip only once the recorder has finished the old one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timed_out_close_keeps_the_clip_owned_until_it_ends() {
        let mut cfg = config(None);
        cfg.post_roll_secs = 0;
        let mut c = cam(cfg, 0, false, true).await;
        assert!(c.frame(0, live(true)).await.clip.is_some());
        let closing = c
            .frame_within(500, AnalysisOutcome::Success, 0, quiet(), SHORT)
            .await;
        assert_eq!(closing.unwrap_err(), RecordingError::Timeout(SHORT));
        // Someone new arrives while the recorder is still closing.
        let (mono, wall) = c.clock(1_000);
        let arrived = c.producer.arrived(mono, wall);
        let counts = EvidenceCounts::default();
        let born = c.producer.analysed(
            mono,
            wall,
            AnalysisOutcome::Success,
            counts,
            Some(live(true)),
        );
        let control = c.control.clone();
        let born = tokio::spawn(async move {
            control
                .request(RecordingCommand::Evidence(arrived), LONG)
                .await?;
            control
                .request(RecordingCommand::Evidence(born), LONG)
                .await
        });
        tokio::time::sleep(SHORT).await;
        assert_eq!(
            c.rec.calls(),
            ["opening", "opened 1 512x288", "closing 1"],
            "no second writer while the first is closing"
        );
        c.rec.closes.add_permits(FREE);
        let born = born.await.unwrap().unwrap();
        assert_eq!(born.clip.map(|h| h.clip_id), Some(2));
        assert_eq!(
            c.rec.calls(),
            [
                "opening",
                "opened 1 512x288",
                "closing 1",
                "closed 1 media=0",
                "opening",
                "opened 2 512x288",
            ]
        );
    }

    /// A stop that lands while an open is in flight waits for it and
    /// finalizes the clip it produced before it answers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_during_an_open_finalizes_what_it_opened() {
        let mut c = cam(config(None), 0, true, false).await;
        let slow = c
            .frame_within(0, AnalysisOutcome::Success, 1, live(true), SHORT)
            .await;
        assert!(slow.is_err());
        let control = c.control.clone();
        let stop = tokio::spawn(async move { control.request(RecordingCommand::Stop, LONG).await });
        tokio::time::sleep(SHORT).await;
        assert!(!stop.is_finished(), "the stop waits for the open");
        c.rec.opens.add_permits(FREE);
        let receipt = stop.await.unwrap().unwrap();
        assert!(receipt.clip.is_none() && receipt.error.is_none());
        assert_eq!(
            c.rec.calls(),
            [
                "opening",
                "opened 1 512x288",
                "closing 1",
                "closed 1 media=0"
            ]
        );
        tokio::time::timeout(LONG, c.stop.closed()).await.unwrap();
    }

    /// The supervisor is gone mid-open (its guard dropped): the open is
    /// dropped, as an aborted caller's always was, nothing else starts, and
    /// the stop request then lets the controller end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_gone_mid_open_drops_the_open_and_starts_nothing() {
        let mut c = cam(config(None), 0, true, false).await;
        let slow = c
            .frame_within(0, AnalysisOutcome::Success, 1, live(true), SHORT)
            .await;
        assert!(slow.is_err());
        drop(c.guard.take());
        tokio::time::sleep(SHORT).await;
        c.rec.opens.add_permits(FREE);
        tokio::time::sleep(SHORT).await;
        assert_eq!(
            c.rec.calls(),
            ["opening"],
            "the dropped open never completed"
        );
        c.stop.send(true).unwrap();
        tokio::time::timeout(LONG, c.stop.closed()).await.unwrap();
        assert_eq!(c.rec.calls(), ["opening"]);
    }

    /// A storage panic refuses the open on the receipt; once it clears the
    /// controller opens on its own clock, with no frame to prompt it, after
    /// legacy's ten-second stall.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_storage_panic_refuses_then_recovers_without_a_frame() {
        let mut c = cam(config(None), 0, false, false).await;
        c.rec.set_panic(true);
        let refused = c.frame(0, live(true)).await;
        assert_eq!(
            (refused.clip.is_none(), refused.error),
            (true, Some(RecordingError::Refused))
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            c.rec.calls(),
            ["opening", "refused"],
            "no retry while panicked"
        );
        c.rec.set_panic(false);
        let calls = c.until(13, "opened 1 512x288").await;
        assert_eq!(calls, ["opening", "refused", "opening", "opened 1 512x288"]);
    }

    /// An alert repeated (a retried command) opens one clip and links its
    /// event to it once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_duplicate_alert_opens_one_clip_and_links_to_it() {
        let mut c = cam(config(None), 0, false, false).await;
        let event = c.event(0).await;
        let motion = Motion {
            alerts: 1,
            ..quiet()
        };
        assert_eq!(c.frame(0, motion).await.clip.map(|h| h.clip_id), Some(1));
        for _ in 0..2 {
            let receipt = c.alert(0, &event).await;
            assert_eq!(
                (receipt.clip.map(|h| h.clip_id), receipt.error),
                (Some(1), None)
            );
        }
        assert_eq!(c.rec.calls(), ["opening", "opened 1 512x288"]);
        assert_eq!(c.linked(&event).await, Some(1));
    }

    /// A resize of the analysis frame and a stream epoch change each rotate
    /// the file: the next clip opens at the new size, and the alert linked
    /// to the first clip keeps its link.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resize_and_codec_epochs_rotate_the_file_and_keep_links() {
        let mut c = cam(config(None), 0, false, false).await;
        let event = c.event(0).await;
        assert!(c.frame(0, live(true)).await.clip.is_some());
        assert_eq!(c.alert(0, &event).await.clip.map(|h| h.clip_id), Some(1));
        let resized = EpochChange {
            captured_at: c.clock(4_000).1,
            frame_width: 1024,
            frame_height: 576,
        };
        let receipt = c
            .send(RecordingCommand::StreamEpochChanged(resized), LONG)
            .await
            .unwrap();
        assert!(receipt.clip.is_none(), "the resize closed the clip");
        assert_eq!(
            c.frame(4_500, live(false)).await.clip.map(|h| h.clip_id),
            Some(2)
        );
        c.rec.epoch_ended.lock().push(2);
        assert_eq!(
            c.frame(9_000, live(false)).await.clip.map(|h| h.clip_id),
            Some(3)
        );
        assert_eq!(
            c.rec.calls(),
            [
                "opening",
                "opened 1 512x288",
                "closing 1",
                "closed 1 media=0",
                "opening",
                "opened 2 1024x576",
                "closing 2",
                "closed 2 media=0",
                "opening",
                "opened 3 1024x576",
            ]
        );
        assert_eq!(c.linked(&event).await, Some(1), "rotation kept the link");
    }

    /// A command for another generation changes nothing, and its requester
    /// refuses a receipt from a generation it did not ask.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_generations_commands_and_replies_are_refused() {
        let c = cam(config(None), 0, false, false).await;
        let old = RecordingControl {
            tx: c.control.tx.clone(),
            generation: 5,
        };
        let (mono, wall) = c.clock(0);
        let mut producer = EvidenceProducer::new(5, None);
        let evidence = producer.analysed(
            mono,
            wall,
            AnalysisOutcome::Success,
            EvidenceCounts::default(),
            Some(live(true)),
        );
        let refused = old
            .request(RecordingCommand::Evidence(evidence), LONG)
            .await;
        assert_eq!(
            refused.unwrap_err(),
            RecordingError::StaleGeneration {
                sent: 5,
                running: 6
            }
        );
        assert!(c.rec.calls().is_empty(), "a stale command opened nothing");
        assert!(c.stop().await.clip.is_none());
    }

    /// R-4: observed records while the first analysis is still pending,
    /// leaves degraded coverage through the recovery gate, closes one
    /// post-roll after the recovery instant, and opens a new file from fresh
    /// evidence even though the tracker only reports an update.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_records_before_analysis_and_reopens_on_fresh_evidence() {
        let mut c = cam(observed(1), 0, false, false).await;
        c.until(1, "opened 1 512x288").await;
        let runtime = c.runtime().expect("observed reports its runtime");
        assert_eq!(runtime.mode, RecordingPolicyMode::Observed);
        c.healthy().await;
        let calls = c.until(3, "closed 1 ").await;
        assert_eq!(calls.len(), 4, "{calls:#?}");
        // The same person, re-associated: an update, never a birth.
        let receipt = c.now(AnalysisOutcome::Success, 1).await;
        assert_eq!(receipt.clip.map(|h| h.clip_id), Some(2));
        assert!(c.stop().await.clip.is_none());
    }

    /// R-4: a detector error is degraded coverage from that frame: the
    /// writer opens on the evidence that reports it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_opens_at_once_on_an_explicit_failure() {
        let mut c = cam(observed(1), 0, false, false).await;
        c.healthy().await;
        c.until(3, "closed 1 ").await;
        let receipt = c.now(AnalysisOutcome::Failed, 0).await;
        assert_eq!(receipt.clip.map(|h| h.clip_id), Some(2));
        let runtime = c.runtime().unwrap();
        assert_eq!(
            runtime.diagnostics.reason,
            Some(nexus_pipeline::recording_activity::RecordingReason::DegradedCoverage)
        );
        assert!(c.stop().await.clip.is_none());
    }

    /// With no post-roll and no pre-roll, one qualifying observation keeps
    /// its new clip until it holds three seconds of media, counted as
    /// `minimum_duration`, not activity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_brief_observation_is_held_for_the_minimum_duration() {
        let mut c = cam(observed(0), 0, false, false).await;
        c.healthy().await;
        c.until(3, "closed 1 ").await;
        let opened = Instant::now();
        assert_eq!(
            c.now(AnalysisOutcome::Success, 1)
                .await
                .clip
                .map(|h| h.clip_id),
            Some(2)
        );
        for _ in 0..3 {
            c.now(AnalysisOutcome::Success, 0).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            !c.rec
                .calls()
                .iter()
                .any(|call| call.starts_with("closing 2")),
            "held, not closed: {:#?}",
            c.rec.calls()
        );
        let held = c.runtime().unwrap().diagnostics;
        assert_eq!(
            held.reason,
            Some(nexus_pipeline::recording_activity::RecordingReason::MinimumDuration)
        );
        c.analysing_until(5, "closed 2 ").await;
        assert!(opened.elapsed() >= Duration::from_millis(2_900));
        let closed = c.rec.calls().pop().unwrap();
        assert!(
            *closed >= *"closed 2 media=3",
            "three seconds of media: {closed}"
        );
        let elapsed = c.runtime().unwrap().diagnostics.elapsed.legacy_open;
        assert!(elapsed.minimum_duration >= 2_000, "{elapsed:?}");
        assert!(elapsed.activity < 1_000, "{elapsed:?}");
    }

    /// Retained pre-roll already makes the clip long enough: it closes as
    /// soon as the activity ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enough_pre_roll_closes_a_brief_clip_at_once() {
        let mut c = cam(observed(0), 5_000, false, false).await;
        c.healthy().await;
        c.until(3, "closed 1 ").await;
        assert!(c.now(AnalysisOutcome::Success, 1).await.clip.is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let receipt = c.now(AnalysisOutcome::Success, 0).await;
        assert!(receipt.clip.is_none(), "{:#?}", c.rec.calls());
        assert!(c.rec.calls().iter().any(|call| call == "closed 2 media=5"));
    }

    /// A source lost before the minimum is reached ends the hold: the clip
    /// closes with the partial media it has rather than staying open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_lost_during_the_hold_closes_partial_media() {
        let mut c = cam(observed(0), 1_000, false, false).await;
        c.healthy().await;
        c.until(3, "closed 1 ").await;
        c.rec.source_lost.store(true, Ordering::SeqCst);
        assert!(c.now(AnalysisOutcome::Success, 1).await.clip.is_some());
        c.analysing_until(5, "closed 2 media=1").await;
    }

    /// Rollback from observed to legacy goes through the same controller:
    /// the open clip and its alert link carry on, and legacy's post-roll
    /// closes it later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rollback_to_legacy_keeps_the_open_clip_and_its_links() {
        let mut c = cam(observed(1), 0, false, false).await;
        c.until(1, "opened 1 512x288").await;
        let event = c.event(0).await;
        assert_eq!(
            c.alert(c.elapsed_ms(), &event)
                .await
                .clip
                .map(|h| h.clip_id),
            Some(1)
        );
        let receipt = c
            .send(
                RecordingCommand::PolicyChanged(None, "rev-legacy".into()),
                LONG,
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.clip.map(|h| h.clip_id),
            Some(1),
            "the clip carried on"
        );
        assert!(c.runtime().is_none(), "legacy reports no runtime");
        // Legacy's live track holds the clip; its post-roll then closes it.
        for _ in 0..6 {
            let ms = c.elapsed_ms();
            assert_eq!(
                c.frame(ms, live(false)).await.clip.map(|h| h.clip_id),
                Some(1)
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let ms = c.elapsed_ms();
        c.frame(ms, quiet()).await;
        assert!(
            c.frame(ms + 1_000, quiet()).await.clip.is_none(),
            "legacy's post-roll closed it"
        );
        let calls = c.rec.calls();
        assert_eq!(calls[..3], ["opening", "opened 1 512x288", "closing 1"]);
        assert!(
            calls[3].starts_with("closed 1 media=") && calls.len() == 4,
            "{calls:#?}"
        );
        assert_eq!(
            c.linked(&event).await,
            Some(1),
            "the link survived the rollback"
        );
    }

    /// R-7: with `record_motion_clip_on_alert` off, an observed alert links
    /// to the clip already open but never opens or renews one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_alerts_only_link_when_alert_recording_is_off() {
        let mut cfg = observed(1);
        cfg.record_motion_clip_on_alert = false;
        let mut c = cam(cfg, 0, false, false).await;
        c.until(1, "opened 1 512x288").await;
        let during = c.event(0).await;
        let receipt = c.alert(c.elapsed_ms(), &during).await;
        assert_eq!(receipt.clip.map(|h| h.clip_id), Some(1));
        c.analysing_until(5, "closed 1 ").await;
        let after = c.event(1).await;
        let receipt = c.alert(c.elapsed_ms(), &after).await;
        assert!(receipt.clip.is_none(), "{:#?}", c.rec.calls());
        let receipt = c.now(AnalysisOutcome::Success, 0).await;
        assert!(
            receipt.clip.is_none(),
            "renewed nothing: {:#?}",
            c.rec.calls()
        );
        assert_eq!(c.linked(&during).await, Some(1), "linkage kept");
        assert_eq!(c.linked(&after).await, None);
    }

    /// SPEC-089 — the runtime reports the revision of the saved policy the
    /// controller applied: the one it started with, then each live change's,
    /// including a new revision of an unchanged policy (an ABA saved between
    /// two passes), without a restart of the measurement's generation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_runtime_reports_the_policy_revision_the_controller_applied() {
        let shadow = Some(RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 1_000,
        });
        let c = cam(config(shadow), 0, false, false).await;
        let applied = |c: &Cam| c.runtime().map(|r| (r.mode, r.policy_revision));
        assert_eq!(
            applied(&c),
            Some((RecordingPolicyMode::Shadow, "rev-start".to_owned()))
        );
        c.send(
            RecordingCommand::PolicyChanged(shadow, "rev-aba".into()),
            LONG,
        )
        .await
        .unwrap();
        assert_eq!(
            applied(&c),
            Some((RecordingPolicyMode::Shadow, "rev-aba".to_owned()))
        );
        let observed = Some(RecordingPolicyConfig {
            mode: RecordingPolicyMode::Observed,
            analysis_stale_after_ms: 1_000,
        });
        c.send(
            RecordingCommand::PolicyChanged(observed, "rev-3".into()),
            LONG,
        )
        .await
        .unwrap();
        assert_eq!(
            applied(&c),
            Some((RecordingPolicyMode::Observed, "rev-3".to_owned())),
            "observed runs observed, under its own revision"
        );
        c.send(RecordingCommand::PolicyChanged(None, "rev-4".into()), LONG)
            .await
            .unwrap();
        assert_eq!(applied(&c), None, "legacy reports no runtime");
    }
}
