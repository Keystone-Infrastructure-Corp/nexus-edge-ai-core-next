//! SPEC-077 `parking_policy_v2` through the real supervisor: a scripted
//! source and detector, the real tracker (or one that reports scripted track
//! ids), the static filter, the rules and the recording controller. Each test
//! asserts what an operator gets, alerts and recordings, as well as the
//! `tracker.is_static` flags on `FRAME_METADATA`.
//!
//! The source sends a frame only once the one before it has been tracked,
//! with synthetic capture stamps, so every expectation is in capture time.
//! Frames are at least 500 ms apart so the motion gate passes each one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{
    CameraBehavior, CameraConfig, ClipsConfig, ParkingPolicyV2, RuleConfig, RuleDebounce,
    RuleGates, RulePredicate, RulesConfig, StoreConfig, TrackerConfig, ZoneConfig, ZoneKind,
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
use nexus_tracker::{build_tracker, is_object_static, Tracker};
use nexus_types::{
    AlertEvent, BBox, CameraId, Detection, Frame, FrameMetadata, PixelFormat, TrackedObject,
};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::mpsc;
use url::Url;

/// The supervisor frame: the smallest rung of the 16:9 ladder.
const SIZE: (u32, u32) = (512, 288);
/// A car's box, as a fraction of the frame's width and height.
const CAR_W: f32 = 0.1;
const CAR_H: f32 = 0.12;

fn wall0() -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
}

/// The plan's test values, not customer defaults: dwell 5 s, speed <= 0.05
/// diagonals/s, drift <= 0.10 diagonals, observation gap <= 3 s.
fn policy() -> ParkingPolicyV2 {
    ParkingPolicyV2 {
        dwell_secs: 5.0,
        max_speed_box_diagonals_per_sec: 0.05,
        max_displacement_box_diagonals: 0.10,
        max_observation_gap_secs: 3.0,
    }
}

fn parking(id: &str, x0: f32, x1: f32) -> ZoneConfig {
    ZoneConfig {
        id: id.into(),
        name: id.into(),
        polygon: vec![(x0, 0.0), (x1, 0.0), (x1, 1.0), (x0, 1.0)],
        kind: ZoneKind::Parking,
        min_bbox_area_px_override: None,
    }
}

/// Stalls over the left half of the view; the right half is the lane.
fn stalls() -> Vec<ZoneConfig> {
    vec![parking("stalls", 0.0, 0.5)]
}

/// A car the detector reports, at a normalized center and box size. `id` is
/// the track id the scripted tracker reports it under; an unseen car is a
/// coasting track there, and absent for the real tracker.
#[derive(Clone, Copy)]
struct Car {
    x: f32,
    y: f32,
    size: (f32, f32),
    id: u64,
    seen: bool,
}

fn car(x: f32, y: f32) -> Car {
    Car {
        x,
        y,
        size: (CAR_W, CAR_H),
        id: 0,
        seen: true,
    }
}

/// One analysed frame: capture offset, size and what the detector sees.
#[derive(Clone)]
struct Shot {
    ms: u64,
    size: (u32, u32),
    cars: Vec<Car>,
}

/// Frames at `times` (capture ms) showing `cars(k, ms)` on frame `k`.
fn shots(times: &[u64], cars: impl Fn(usize, u64) -> Vec<Car>) -> Vec<Shot> {
    times
        .iter()
        .enumerate()
        .map(|(k, &ms)| Shot {
            ms,
            size: SIZE,
            cars: cars(k, ms),
        })
        .collect()
}

/// Capture ms every `interval_ms` from `from` until `to`.
fn every(interval_ms: u64, from: u64, to: u64) -> Vec<u64> {
    (from..=to).step_by(interval_ms as usize).collect()
}

/// A rule matching any vehicle older than `min_track_age_ms`.
fn vehicle_rule(id: &str, min_track_age_ms: u64) -> RuleConfig {
    RuleConfig {
        id: id.into(),
        name: id.into(),
        predicate: RulePredicate {
            when: "object.label.startsWith('vehicle.')".into(),
            severity: "high".into(),
        },
        gates: RuleGates::default(),
        debounce: RuleDebounce {
            min_track_age_ms,
            consecutive_frames: 1,
            cooldown_ms: 0,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
        talk_down: None,
    }
}

/// `any_vehicle` fires on sight; `lingering_vehicle` only on a vehicle
/// visible for longer than the dwell.
fn rules() -> Vec<RuleConfig> {
    vec![
        vehicle_rule("any_vehicle", 0),
        vehicle_rule("lingering_vehicle", 8_000),
    ]
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

/// Hands the supervisor the scripted source and records each clip it opens.
struct Recorder {
    stub: StubClipRecorder,
    source: Mutex<Option<Box<dyn FrameSource + Send>>>,
    opened: Mutex<Vec<DateTime<Utc>>>,
    closed: Mutex<Vec<DateTime<Utc>>>,
}

#[async_trait]
impl ClipRecorder for Recorder {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        self.opened.lock().push(args.started_at);
        self.stub.open(args).await
    }

    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        self.closed.lock().push(args.ended_at);
        self.stub.close(handle, args).await
    }

    fn set_panic(&self, panic: bool) {
        self.stub.set_panic(panic);
    }

    fn is_panic(&self) -> bool {
        self.stub.is_panic()
    }

    fn kind(&self) -> &'static str {
        "parking"
    }

    fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
        self.source.lock().take()
    }
}

/// Reports each frame's cars; the frame index is `frame_id - 1`.
struct Cars {
    shots: Vec<Vec<Car>>,
    /// Also report unseen cars, for the scripted tracker to coast.
    all: bool,
}

#[async_trait]
impl Detector for Cars {
    async fn detect(&self, f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        let cars = self
            .shots
            .get(f.frame_id as usize - 1)
            .cloned()
            .unwrap_or_default();
        let (w, h) = (f.width as f32, f.height as f32);
        Ok(cars
            .into_iter()
            .filter(|c| c.seen || self.all)
            .map(|c| {
                let (cx, cy, bw, bh) = (c.x * w, c.y * h, c.size.0 * w, c.size.1 * h);
                let mut d = Detection {
                    label: "vehicle.car".into(),
                    confidence: 0.9,
                    bbox: BBox {
                        x1: cx - bw / 2.0,
                        y1: cy - bh / 2.0,
                        x2: cx + bw / 2.0,
                        y2: cy + bh / 2.0,
                    },
                    attributes: Default::default(),
                };
                d.attributes.insert("test.id".into(), json!(c.id));
                d.attributes.insert("test.seen".into(), json!(c.seen));
                d
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "cars"
    }
}

/// Reports each detection under its scripted track id; an unseen car is a
/// coasting track, with no detection box.
struct ScriptedIds;

impl Tracker for ScriptedIds {
    fn update(&self, detections: Vec<Detection>, _: Instant) -> Vec<TrackedObject> {
        detections
            .into_iter()
            .map(|d| TrackedObject {
                track_id: d.attributes["test.id"].as_u64().unwrap(),
                detection_bbox: (d.attributes["test.seen"] == json!(true)).then_some(d.bbox),
                label: d.label,
                confidence: d.confidence,
                bbox: d.bbox,
                age_frames: 1,
                age_ms: 0,
                attributes: Default::default(),
            })
            .collect()
    }

    fn name(&self) -> &'static str {
        "scripted-ids"
    }
}

struct Scenario {
    shots: Vec<Shot>,
    zones: Vec<ZoneConfig>,
    policy: Option<ParkingPolicyV2>,
    scripted_ids: bool,
    anchor_ttl_secs: Option<u32>,
}

impl Scenario {
    fn v2(shots: Vec<Shot>) -> Self {
        Self {
            shots,
            zones: stalls(),
            policy: Some(policy()),
            scripted_ids: false,
            anchor_ttl_secs: None,
        }
    }
}

struct Outcome {
    /// `FRAME_METADATA` of shot `k`.
    metadata: Vec<FrameMetadata>,
    alerts: Vec<AlertEvent>,
    opened: Vec<DateTime<Utc>>,
    /// Each clip's capture ms when opened and when closed; `None`: still
    /// open when the camera stopped.
    clips: Vec<(u64, Option<u64>)>,
    motion: Vec<MotionEventRow>,
    /// Centers of the anchors on disk once the camera stopped.
    anchors: Vec<(f32, f32)>,
}

impl Outcome {
    /// The object nearest the normalized point on shot `k`, if one is within
    /// a box width of it.
    fn object_at(&self, k: usize, x: f32, y: f32) -> Option<&TrackedObject> {
        let m = &self.metadata[k];
        let (px, py) = (x * m.width as f32, y * m.height as f32);
        let near = CAR_W * m.width as f32;
        m.objects
            .iter()
            .map(|o| {
                let (cx, cy) = o.bbox.center();
                ((cx - px).hypot(cy - py), o)
            })
            .filter(|(d, _)| *d <= near)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, o)| o)
    }

    fn static_at(&self, k: usize, x: f32, y: f32) -> bool {
        is_object_static(self.object_at(k, x, y).expect("object in view"))
    }

    /// Shot indices on which `rule` fired.
    fn fired(&self, rule: &str) -> Vec<usize> {
        self.alerts
            .iter()
            .filter(|a| a.rule_id == rule)
            .map(|a| a.frame_id as usize - 1)
            .collect()
    }

    /// Capture ms of the motion rows of `kind`, in order.
    fn motion_at(&self, kind: MotionEventKind) -> Vec<u64> {
        let mut at: Vec<u64> = self
            .motion
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| offset_ms(r.captured_at))
            .collect();
        at.sort_unstable();
        at
    }

    fn died_before(&self, ms: u64) -> usize {
        let until = wall0() + chrono::Duration::milliseconds(ms as i64);
        self.motion
            .iter()
            .filter(|r| r.kind == MotionEventKind::Died && r.captured_at < until)
            .count()
    }
}

/// Capture ms of a wall stamp.
fn offset_ms(at: DateTime<Utc>) -> u64 {
    (at - wall0()).num_milliseconds() as u64
}

/// An anchor center from the registry, to the hundredth of a pixel.
fn center(a: &serde_json::Value) -> (f32, f32) {
    let px = |v: &serde_json::Value| ((v.as_f64().unwrap() * 100.0).round() / 100.0) as f32;
    (px(&a["center_x"]), px(&a["center_y"]))
}

/// Run the shots, then a repeat of the last one 500 ms later whose
/// metadata shows every shot has been fully handled, then stop the camera.
/// `dir` holds the store and the anchor registry, so a second run on the
/// same `dir` is the camera's next generation.
async fn run(s: Scenario, dir: &Path) -> Outcome {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(1024));
    let mut metadata = bus
        .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
        .await
        .expect("subscribe frame.metadata");
    let mut alerts = bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .expect("subscribe alert.event");
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", dir.join("nexus.db").display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .expect("open store"),
    );
    let cam = CameraConfig {
        id: 1,
        name: "parking".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("virtual://local").unwrap(),
            analysis_url: None,
            enabled: true,
            max_fps: 20,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: CameraBehavior {
            parking_lot_mode: true,
            parking_policy_v2: s.policy,
            anchor_ttl_secs: s.anchor_ttl_secs,
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: s.zones,
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");

    let mono0 = Instant::now();
    let n = s.shots.len();
    let mut shots = s.shots;
    let mut last = shots.last().cloned().expect("a shot");
    last.ms += 500;
    let stopped_ms = last.ms;
    shots.push(last);
    let frames: Vec<Frame> = shots
        .iter()
        .enumerate()
        .map(|(k, shot)| Frame {
            camera_id: 1,
            frame_id: k as u64 + 1,
            captured_at: wall0() + chrono::Duration::milliseconds(shot.ms as i64),
            captured_mono: mono0 + Duration::from_millis(shot.ms),
            width: shot.size.0,
            height: shot.size.1,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0; (shot.size.0 * shot.size.1 * 3) as usize]),
            trace_id: format!("shot-{k}"),
        })
        .collect();
    let (go, go_rx) = mpsc::unbounded_channel();
    let recorder = Arc::new(Recorder {
        stub: StubClipRecorder::new(store.clone(), dir.join("clips")),
        source: Mutex::new(Some(Box::new(Scripted { frames, go: go_rx }))),
        opened: Mutex::new(Vec::new()),
        closed: Mutex::new(Vec::new()),
    });
    let tracker_cfg = TrackerConfig::default();
    let tracker: Arc<dyn Tracker> = if s.scripted_ids {
        Arc::new(ScriptedIds)
    } else {
        Arc::from(build_tracker(&tracker_cfg))
    };
    let handle = spawn_camera(
        cam,
        Arc::new(Cars {
            shots: shots.iter().map(|shot| shot.cars.clone()).collect(),
            all: s.scripted_ids,
        }),
        None,
        tracker,
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        ClipsConfig::default(),
        dir.to_path_buf(),
        Arc::new(RuleEvaluator::new(&RulesConfig::default(), &rules()).expect("compile rules")),
        store.clone(),
        recorder.clone(),
        bus.clone(),
        Arc::new(LatestFrameCache::new()),
        Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        SIZE.0,
        SIZE.1,
        Arc::new(nexus_pipeline::NoopSightingHook),
        SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(nexus_pipeline::NoopSinkRouter),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    );

    let mut tracked = Vec::new();
    for k in 0..=n {
        go.send(()).expect("source running");
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), metadata.next())
                .await
                .unwrap_or_else(|_| panic!("shot {k} tracked within 10 s"))
                .expect("bus open")
                .expect("frame.metadata decodes");
            if m.frame_id == k as u64 + 1 {
                tracked.push(m);
                break;
            }
        }
    }
    handle.stop.send(true).expect("supervisor running");
    tokio::time::timeout(Duration::from_secs(10), handle.task)
        .await
        .expect("supervisor stops")
        .expect("supervisor did not panic");
    tracked.truncate(n);

    let mut fired = Vec::new();
    while let Ok(Some(a)) = tokio::time::timeout(Duration::from_millis(100), alerts.next()).await {
        fired.push(a.expect("alert.event decodes"));
    }
    let day = chrono::Duration::days(1);
    let motion = store
        .list_motion_events_for_camera(1, wall0() - day, wall0() + day, 10_000)
        .await
        .expect("list motion rows");
    let registry = dir.join("static_objects").join("cam-1.json");
    let anchors = std::fs::read(&registry)
        .ok()
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).expect("registry json"))
        .map(|doc| {
            doc["anchors"]
                .as_array()
                .expect("anchors")
                .iter()
                .map(center)
                .collect()
        })
        .unwrap_or_default();
    let opened = recorder.opened.lock().clone();
    let closed = recorder.closed.lock().clone();
    assert_eq!(opened.len(), closed.len(), "every clip closed by the stop");
    // The stop closes an open clip on the wall clock, after every shot.
    let clips = opened
        .iter()
        .zip(&closed)
        .map(|(&o, &c)| {
            let c = (c <= wall0() + chrono::Duration::milliseconds(stopped_ms as i64))
                .then(|| offset_ms(c));
            (offset_ms(o), c)
        })
        .collect();
    Outcome {
        metadata: tracked,
        alerts: fired,
        opened,
        clips,
        motion,
        anchors,
    }
}

/// SPEC-077: with a parking zone over the stalls only, a car queueing at
/// the gate, in the lane, for far longer than the dwell is never hidden: a
/// rule for vehicles visible longer than the dwell fires on it, and its
/// motion never ends while it stands there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_car_queueing_in_the_lane_is_never_hidden() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 20_000);
    let o = run(
        Scenario::v2(shots(&times, |_, _| vec![car(0.75, 0.5)])),
        dir.path(),
    )
    .await;
    for (k, ms) in times.iter().enumerate() {
        assert!(!o.static_at(k, 0.75, 0.5), "hidden at {ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0], "one alert on arrival");
    let lingering = times.iter().position(|&ms| ms >= 8_000).unwrap();
    assert_eq!(o.fired("lingering_vehicle"), vec![lingering]);
    assert_eq!(o.opened.len(), 1, "one recording, opened on arrival");
    assert_eq!(o.opened[0], wall0());
    assert_eq!(
        o.died_before(times[times.len() - 1]),
        0,
        "motion never ends"
    );
    assert!(o.anchors.is_empty(), "no anchor in the lane");
}

/// The stall every single-car scenario parks in: (128, 144) px at 512x288.
fn in_stall() -> Car {
    car(0.25, 0.5)
}

fn first_at(times: &[u64], from_ms: u64) -> usize {
    times
        .iter()
        .position(|&ms| ms >= from_ms)
        .expect("a frame then")
}

/// Write an anchor registry as an earlier generation would have left it.
fn seed_anchors(dir: &Path, anchors: &[(f32, f32)]) {
    let registry = dir.join("static_objects");
    std::fs::create_dir_all(&registry).unwrap();
    let anchors: Vec<_> = anchors
        .iter()
        .map(|&(x, y)| {
            json!({"label": "vehicle.car", "center_x": x, "center_y": y,
                   "last_seen_unix_ms": wall0().timestamp_millis()})
        })
        .collect();
    let doc = json!({"version": 1, "camera_id": 1, "anchors": anchors});
    std::fs::write(registry.join("cam-1.json"), doc.to_string()).unwrap();
}

/// A car arriving at a stall whose last car left unseen, so its anchor is
/// still on disk: the arrival is alerted and recorded, and only its own
/// observed dwell hides it, at the stall's existing anchor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_arrival_is_alerted_and_recorded_until_its_own_dwell() {
    let dir = tempfile::tempdir().expect("tmpdir");
    seed_anchors(dir.path(), &[(128.0, 144.0)]);
    let times = every(690, 0, 15_000);
    let o = run(
        Scenario::v2(shots(&times, |_, _| vec![in_stall()])),
        dir.path(),
    )
    .await;
    let parked = first_at(&times, 5_000);
    for (k, ms) in times.iter().enumerate() {
        assert_eq!(o.static_at(k, 0.25, 0.5), k >= parked, "{ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0], "alerted on arrival");
    assert!(
        o.fired("lingering_vehicle").is_empty(),
        "hidden once parked"
    );
    assert_eq!(o.opened, vec![wall0()], "recorded from arrival");
    assert_eq!(o.anchors, vec![(128.0, 144.0)], "the stall's one anchor");
}

/// An established parked car whose detection drops out for three frames:
/// coasting credits nothing, so on reacquisition it is visible again, in
/// the same alert episode, until a fresh observed dwell hides it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_car_lost_and_reacquired_is_visible_until_a_fresh_dwell() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 25_000);
    let unseen = |ms: u64| (9_000..11_000).contains(&ms);
    let o = run(
        Scenario::v2(shots(&times, |_, ms| {
            vec![Car {
                seen: !unseen(ms),
                ..in_stall()
            }]
        })),
        dir.path(),
    )
    .await;
    let back = first_at(&times, 11_000);
    for (k, &ms) in times.iter().enumerate() {
        let want = (5_000..9_000).contains(&ms) || ms >= times[back] + 5_000;
        assert_eq!(o.static_at(k, 0.25, 0.5), want, "{ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0], "one parked episode");
    assert_eq!(
        o.fired("lingering_vehicle"),
        vec![back],
        "seen again, visible and older than the dwell"
    );
    // Its motion restarts once it is lost, and ends at its fresh dwell. The
    // parked car's post-roll would close the clip at 15 870 ms; visible
    // again, it keeps the clip open past the post-roll after that dwell.
    let lost = first_at(&times, 9_000);
    let dwelled = first_at(&times, times[back] + 5_000);
    assert_eq!(o.motion_at(MotionEventKind::Born), vec![0, times[lost]]);
    assert_eq!(
        o.motion_at(MotionEventKind::Died),
        vec![5_520, times[dwelled]]
    );
    assert_eq!(o.clips, vec![(0, None)]);
    assert_eq!(o.anchors, vec![(128.0, 144.0)], "reclaimed, not duplicated");
}

/// A car crawling down the aisle past two parked cars, through each one's
/// anchor radius: one alert for its whole pass, never hidden, and both
/// parked cars stay hidden with their anchors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passing_car_is_alerted_once_and_takes_no_anchor() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 25_000);
    let passer_y = |ms: u64| 0.05 + (ms as f32 - 8_000.0) / 1_000.0 * 0.05;
    let o = run(
        Scenario::v2(shots(&times, |_, ms| {
            let mut cars = vec![car(0.2, 0.3), car(0.2, 0.7)];
            if ms >= 8_000 {
                cars.push(car(0.26, passer_y(ms)));
            }
            cars
        })),
        dir.path(),
    )
    .await;
    let passing = first_at(&times, 8_000);
    for (k, &ms) in times.iter().enumerate().skip(first_at(&times, 5_000)) {
        assert!(
            o.static_at(k, 0.2, 0.3) && o.static_at(k, 0.2, 0.7),
            "{ms} ms"
        );
        if k >= passing {
            assert!(
                !o.static_at(k, 0.26, passer_y(ms)),
                "passer hidden at {ms} ms"
            );
        }
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, 0, passing]);
    assert_eq!(o.fired("lingering_vehicle"), vec![first_at(&times, 16_000)]);
    // The parked cars' post-roll would close the clip at 15 870 ms; the
    // passer keeps it open for its whole pass.
    assert_eq!(o.clips, vec![(0, None)]);
    assert_eq!(
        o.motion_at(MotionEventKind::Born),
        vec![0, 0, times[passing]]
    );
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520, 5_520]);
    assert_eq!(o.anchors, vec![(102.4, 86.4), (102.4, 201.6)]);
}

/// A parked car seen driving out of its stall erases its anchor, is
/// alerted again and recorded leaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_car_driving_away_erases_its_anchor_and_alerts_again() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 40_000);
    let leaving = first_at(&times, 30_000);
    let x = |k: usize| 0.25 + 0.03 * k.saturating_sub(leaving - 1) as f32;
    let o = run(
        Scenario::v2(shots(&times, |k, _| vec![car(x(k), 0.5)])),
        dir.path(),
    )
    .await;
    for (k, &ms) in times.iter().enumerate() {
        let want = (5_000..30_000).contains(&ms);
        assert_eq!(o.static_at(k, x(k), 0.5), want, "{ms} ms");
    }
    // 15 px a frame: out of the 40 px anchor radius on its third frame.
    assert_eq!(o.fired("any_vehicle"), vec![0, leaving + 2]);
    assert_eq!(
        o.opened,
        vec![
            wall0(),
            wall0() + chrono::Duration::milliseconds(times[leaving] as i64)
        ],
        "recorded on arrival and again leaving"
    );
    assert!(
        o.anchors.is_empty(),
        "the stall is free for the next arrival"
    );
}

/// A tracker that hands a parked car's id to another object: the reused id
/// inherits neither the parked verdict nor the anchor, and is alerted as
/// a new episode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_track_id_inherits_no_parked_verdict_or_anchor() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 20_000);
    let gone = first_at(&times, 10_000);
    let x = |k: usize| 0.4 + 0.01 * (k - gone) as f32;
    let mut s = Scenario::v2(shots(&times, |k, _| {
        let id = 7;
        match k.cmp(&gone) {
            std::cmp::Ordering::Less => vec![Car { id, ..in_stall() }],
            std::cmp::Ordering::Equal => vec![],
            std::cmp::Ordering::Greater => vec![Car {
                id,
                ..car(x(k), 0.5)
            }],
        }
    }));
    s.scripted_ids = true;
    let o = run(s, dir.path()).await;
    for (k, &ms) in times.iter().enumerate() {
        if k < gone {
            assert_eq!(o.static_at(k, 0.25, 0.5), ms >= 5_000, "{ms} ms");
        } else if k > gone {
            assert!(!o.static_at(k, x(k), 0.5), "reused id hidden at {ms} ms");
        }
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, gone + 1]);
    // The reused id is born again and keeps the clip open past the parked
    // car's post-roll (15 870 ms).
    assert_eq!(o.clips, vec![(0, None)]);
    assert_eq!(o.motion_at(MotionEventKind::Born), vec![0, times[gone + 1]]);
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520]);
    assert_eq!(
        o.anchors,
        vec![(128.0, 144.0)],
        "left to expire, not erased"
    );
}

/// A tracker that hands a large parked car's id to another car standing 64
/// px away, after a 2 s stall in the stream: inside both motion bounds of
/// the box's 729 px diagonal, but past the 60 px ID-reuse step. The arrival
/// inherits no dwell: it is alerted and recorded until its own dwell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_id_within_the_motion_bounds_is_alerted_and_recorded_until_its_own_dwell() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let mut times = every(690, 0, 16_000);
    let jump = times.len();
    times.extend(every(690, 17_870, 25_000));
    let x = |k: usize| if k < jump { 0.25 } else { 0.3125 };
    // A 512x518 px box on a 1024x576 frame.
    let mut shots = shots(&times, |k, _| {
        vec![Car {
            id: 7,
            size: (0.5, 0.9),
            ..car(x(k), 0.5)
        }]
    });
    for shot in &mut shots {
        shot.size = (1024, 576);
    }
    let mut s = Scenario::v2(shots);
    s.scripted_ids = true;
    let o = run(s, dir.path()).await;
    for (k, &ms) in times.iter().enumerate() {
        let want = (5_000..16_000).contains(&ms) || ms >= times[jump] + 5_000;
        assert_eq!(o.static_at(k, x(k), 0.5), want, "{ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, jump]);
    // Post-roll ends the parked car's clip; the arrival opens its own.
    assert_eq!(o.clips, vec![(0, Some(15_870)), (17_870, None)]);
    assert_eq!(o.motion_at(MotionEventKind::Born), vec![0, 17_870]);
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520, 23_390]);
    assert_eq!(
        o.anchors,
        vec![(256.0, 288.0), (320.0, 288.0)],
        "the parked car's anchor is left to expire"
    );
}

/// A tracker that hands a large parked car's id to a box 30 px away, inside
/// the 40 px anchor radius and both motion bounds, while the car stays put
/// under a new id. Either may be the car, so the old id inherits no parked
/// verdict: it is alerted and recorded, as the new id is, until its own
/// dwell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_id_inside_the_anchor_radius_is_alerted_and_recorded_until_its_own_dwell() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 30_000);
    let swap = first_at(&times, 10_000);
    // 256 px, then 286 px, on a 1024x576 frame; a 512x518 px box.
    let (parked_x, moved_x) = (0.25, 0.25 + 30.0 / 1024.0);
    let big = |id, x| Car {
        id,
        size: (0.5, 0.9),
        ..car(x, 0.5)
    };
    let mut shots = shots(&times, |k, _| {
        if k < swap {
            vec![big(7, parked_x)]
        } else {
            vec![big(7, moved_x), big(9, parked_x)]
        }
    });
    for shot in &mut shots {
        shot.size = (1024, 576);
    }
    let mut s = Scenario::v2(shots);
    s.scripted_ids = true;
    let o = run(s, dir.path()).await;
    let dwelled = first_at(&times, times[swap] + 5_000);
    for (k, &ms) in times.iter().enumerate() {
        if k < swap {
            assert_eq!(o.static_at(k, parked_x, 0.5), ms >= 5_000, "{ms} ms");
        } else {
            let want = k >= dwelled;
            assert_eq!(o.static_at(k, moved_x, 0.5), want, "old id at {ms} ms");
            assert_eq!(o.static_at(k, parked_x, 0.5), want, "new id at {ms} ms");
        }
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, swap, swap]);
    // Both ids are born at the swap and die at their own dwell; the clip
    // stays open from the parked car's arrival until the post-roll after.
    let (at, died) = (times[swap], times[dwelled]);
    assert_eq!(o.motion_at(MotionEventKind::Born), vec![0, at, at]);
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520, died, died]);
    assert_eq!(o.clips, vec![(0, Some(26_220))]);
    assert_eq!(o.anchors, vec![(256.0, 288.0)]);
}

/// A stream resized mid-dwell: every candidate starts over in the new
/// geometry, so the car is visible, and alerted as the new box it is, until
/// it dwells again, at its anchor rescaled to the new geometry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resized_stream_starts_every_dwell_over() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let times = every(690, 0, 20_000);
    let resized = first_at(&times, 10_000);
    let mut shots = shots(&times, |_, _| vec![in_stall()]);
    for shot in &mut shots[resized..] {
        shot.size = (1024, 576);
    }
    let o = run(Scenario::v2(shots), dir.path()).await;
    for (k, &ms) in times.iter().enumerate() {
        let want = (5_000..10_000).contains(&ms) || ms >= times[resized] + 5_000;
        assert_eq!(o.static_at(k, 0.25, 0.5), want, "{ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, resized]);
    // The resize gives birth to the car's new box, and to its old one, which
    // the tracker coasts and v2 never hides; that keeps the clip open. The
    // new box dies at its own dwell.
    assert_eq!(o.clips, vec![(0, None)]);
    assert_eq!(
        o.motion_at(MotionEventKind::Born),
        vec![0, times[resized], times[resized]]
    );
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520, 15_870]);
    assert_eq!(
        o.anchors,
        vec![(256.0, 288.0)],
        "the anchor keeps its place in the view, not duplicated"
    );
}

/// Removing a parking zone (a new generation of the camera): the car in
/// it is never hidden again and its anchor is neither matched nor
/// refreshed, so it expires; the car in the remaining zone dwells again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_parking_zone_hides_nothing_and_its_anchor_expires() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let both = || vec![car(0.12, 0.5), car(0.37, 0.5)];
    let first = every(690, 0, 10_000);
    let mut s = Scenario::v2(shots(&first, |_, _| both()));
    s.zones = vec![parking("a", 0.0, 0.25), parking("b", 0.25, 0.5)];
    let o = run(s, dir.path()).await;
    let last = first.len() - 1;
    assert!(o.static_at(last, 0.12, 0.5) && o.static_at(last, 0.37, 0.5));
    assert_eq!(o.clips, vec![(0, None)]);
    assert_eq!(o.motion_at(MotionEventKind::Died), vec![5_520, 5_520]);
    assert_eq!(o.anchors.len(), 2);

    let second = every(690, 12_000, 50_000);
    let mut s = Scenario::v2(shots(&second, |_, _| both()));
    s.zones = vec![parking("a", 0.0, 0.25)];
    s.anchor_ttl_secs = Some(30);
    let o = run(s, dir.path()).await;
    for (k, &ms) in second.iter().enumerate() {
        assert_eq!(o.static_at(k, 0.12, 0.5), ms >= 17_000, "{ms} ms");
        assert!(!o.static_at(k, 0.37, 0.5), "hidden at {ms} ms");
    }
    assert_eq!(o.fired("any_vehicle"), vec![0, 0], "a new generation");
    assert_eq!(
        o.fired("lingering_vehicle"),
        vec![first_at(&second, 20_000)]
    );
    // The car in zone b is recorded throughout; only zone a's car dies.
    assert_eq!(o.clips, vec![(12_000, None)]);
    assert_eq!(
        o.motion_at(MotionEventKind::Born),
        vec![0, 0, 12_000, 12_000],
        "both generations' rows"
    );
    assert_eq!(
        o.motion_at(MotionEventKind::Died),
        vec![5_520, 5_520, 17_520]
    );
    assert_eq!(o.anchors, vec![(61.44, 144.0)], "zone b's anchor expired");
}

// ---- The plan's vectors at every analysis rate and two resolutions ----
//
// Replayed through ByteTrack, the v2 filter and the rules in the
// supervisor's order, without the supervisor: its motion gate passes at most
// 8 frames a second.

const RATES: [f64; 5] = [0.5, 1.45, 2.0, 8.0, 30.0];

/// One replayed vector: a vehicle's box size and its center at capture
/// second `t` on a 512x288 frame (`None`: not detected), with or without
/// +/-3 px of alternating jitter.
struct Vector {
    name: &'static str,
    secs: f64,
    size: (u32, u32),
    box_px: (f32, f32),
    jitter: bool,
    policy: ParkingPolicyV2,
    at: fn(f64) -> Option<(f32, f32)>,
}

/// One replayed frame: capture second, whether the vehicle was detected,
/// static, and whether `any_vehicle` fired.
#[derive(Debug, PartialEq)]
struct Sample {
    t: f64,
    seen: bool,
    hidden: bool,
    fired: bool,
}

/// Capture ms from 0 to `secs` at `fps`, each interval scaled by a
/// repeating 1, 0.7, 1.3, 0.85, 1.15: nonuniform, at the same mean rate.
fn nonuniform_ms(fps: f64, secs: f64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut ms: f64 = 0.0;
    for k in 0.. {
        if ms > secs * 1_000.0 {
            break;
        }
        out.push(ms.round() as u64);
        ms += 1_000.0 / fps * [1.0, 0.7, 1.3, 0.85, 1.15][k % 5];
    }
    out
}

/// `v` at `fps`, with every position and size (and the static filter's
/// pixel radii) multiplied by `scale`.
fn replay(v: &Vector, fps: f64, scale: f32) -> Vec<Sample> {
    let defaults = TrackerConfig::default();
    let tracker = nexus_tracker::ByteTrackTracker::new(defaults.bytetrack);
    let base = defaults.static_object;
    let cfg = nexus_config::StaticObjectConfig {
        match_distance_pixels: (base.match_distance_pixels as f32 * scale) as u32,
        track_id_reuse_reset_pixels: (base.track_id_reuse_reset_pixels as f32 * scale) as u32,
        ..base
    };
    let mut filter = nexus_tracker::StaticObjectFilter::new(cfg, 1, None)
        .with_parking_policy(&v.policy, &stalls())
        .expect("valid policy");
    let rules = RuleEvaluator::new(&RulesConfig::default(), &[vehicle_rule("any_vehicle", 0)])
        .expect("compile rules");
    let (w, h) = (
        (v.size.0 as f32 * scale) as u32,
        (v.size.1 as f32 * scale) as u32,
    );
    let mono0 = Instant::now();
    nonuniform_ms(fps, v.secs)
        .into_iter()
        .enumerate()
        .map(|(k, ms)| {
            let t = ms as f64 / 1_000.0;
            let mono = mono0 + Duration::from_millis(ms);
            let jitter = match (v.jitter, k % 2) {
                (false, _) => 0.0,
                (true, 0) => 3.0,
                (true, _) => -3.0,
            };
            let dets: Vec<Detection> = (v.at)(t)
                .map(|(x, y)| {
                    let (x, y) = ((x + jitter) * scale, y * scale);
                    let (bw, bh) = (v.box_px.0 * scale / 2.0, v.box_px.1 * scale / 2.0);
                    Detection {
                        label: "vehicle.car".into(),
                        confidence: 0.9,
                        bbox: BBox {
                            x1: x - bw,
                            y1: y - bh,
                            x2: x + bw,
                            y2: y + bh,
                        },
                        attributes: Default::default(),
                    }
                })
                .into_iter()
                .collect();
            let seen = !dets.is_empty();
            let frame = Frame {
                camera_id: 1,
                frame_id: k as u64 + 1,
                captured_at: wall0() + chrono::Duration::milliseconds(ms as i64),
                captured_mono: mono,
                width: w,
                height: h,
                format: PixelFormat::Rgb24,
                data: Arc::new(Vec::new()),
                trace_id: format!("replay-{k}"),
            };
            let mut tracked = tracker.update(dets, mono);
            filter.classify(&frame, &mut tracked);
            let fired = !rules
                .evaluate(
                    1,
                    frame.frame_id,
                    frame.captured_at,
                    mono,
                    &frame.trace_id,
                    w,
                    h,
                    &[],
                    &tracked,
                )
                .is_empty();
            Sample {
                t,
                seen,
                hidden: tracked.iter().any(is_object_static),
                fired,
            }
        })
        .collect()
}

/// A 120x90 box: a 150 px diagonal.
fn plan_vector(name: &'static str, secs: f64, at: fn(f64) -> Option<(f32, f32)>) -> Vector {
    Vector {
        name,
        secs,
        size: SIZE,
        box_px: (120.0, 90.0),
        jitter: true,
        policy: policy(),
        at,
    }
}

/// The first sample at or after `t`.
fn first_from(samples: &[Sample], t: f64) -> Option<f64> {
    samples.iter().map(|s| s.t).find(|&s| s >= t)
}

fn hidden_from(samples: &[Sample]) -> Option<f64> {
    samples.iter().find(|s| s.hidden).map(|s| s.t)
}

fn fired_at(samples: &[Sample]) -> Vec<f64> {
    samples.iter().filter(|s| s.fired).map(|s| s.t).collect()
}

/// The plan's vectors decide alike at 0.5, 1.45, 2, 8 and 30 fps, on
/// nonuniform capture intervals, with jitter, and at twice the resolution;
/// each within one observation interval of where the plan places it.
#[test]
fn the_plan_vectors_decide_alike_at_every_rate_and_resolution() {
    let parked = plan_vector("parked in a stall", 12.0, |_| Some((100.0, 150.0)));
    let queued = plan_vector("queued in the lane", 20.0, |_| Some((400.0, 150.0)));
    let moving = plan_vector("moving through the stalls", 8.0, |t| {
        Some((60.0 + 20.0 * t as f32, 40.0 + 20.0 * t as f32))
    });
    let regained = plan_vector("seen, lost from 1 s to 8 s, seen", 15.0, |t| {
        (!(1.0..8.0).contains(&t)).then_some((100.0, 150.0))
    });
    let excursion = plan_vector("parked with a 30 px excursion at 3 s", 12.0, |t| {
        Some((
            if (3.0..4.0).contains(&t) {
                130.0
            } else {
                100.0
            },
            150.0,
        ))
    });
    let departure = plan_vector("parked, leaving at 12 s", 16.0, |t| {
        Some((100.0 + 60.0 * (t - 12.0).max(0.0) as f32, 150.0))
    });
    for fps in RATES {
        let interval = 1.3 / fps;
        let run = |v: &Vector| {
            let samples = replay(v, fps, 1.0);
            assert_eq!(
                samples,
                replay(v, fps, 2.0),
                "{} at {fps} fps: doubled",
                v.name
            );
            samples
        };
        let ctx = |v: &Vector| format!("{} at {fps} fps", v.name);

        let s = run(&parked);
        assert_eq!(hidden_from(&s), first_from(&s, 5.0), "{}", ctx(&parked));
        assert_eq!(fired_at(&s), vec![0.0], "{}", ctx(&parked));

        let s = run(&queued);
        assert_eq!(hidden_from(&s), None, "{}", ctx(&queued));
        assert_eq!(fired_at(&s), vec![0.0], "{}", ctx(&queued));

        assert_eq!(hidden_from(&run(&moving)), None, "{}", ctx(&moving));

        let s = run(&regained);
        let back = s.iter().find(|x| x.seen && x.t >= 8.0).unwrap().t;
        assert_eq!(
            hidden_from(&s),
            first_from(&s, back + 5.0),
            "{}",
            ctx(&regained)
        );

        let s = run(&excursion);
        let out = s.iter().filter(|x| (3.0..4.0).contains(&x.t));
        assert!(
            out.clone().all(|x| !x.hidden),
            "{}: excursion hidden",
            ctx(&excursion)
        );
        let from = match out.count() {
            0 => 5.0,
            _ => first_from(&s, 4.0).unwrap() + 5.0,
        };
        assert_eq!(hidden_from(&s), first_from(&s, from), "{}", ctx(&excursion));

        let s = run(&departure);
        assert_eq!(hidden_from(&s), first_from(&s, 5.0), "{}", ctx(&departure));
        // Out of the drift bound (15 px, plus 6 of jitter) by 12.35 s.
        let still = |x: &&Sample| (x.t >= 5.0 && x.t < 12.0) || x.t >= 12.35;
        for x in s.iter().filter(still) {
            assert_eq!(x.hidden, x.t < 12.0, "{} at {:.2} s", ctx(&departure), x.t);
        }
        // Silent while parked, then alerted once out of the 40 px anchor
        // radius (plus 6 of jitter), by 12.77 s. A track the tracker splits
        // off a fast departure at low rates alerts again (BUG-260).
        let fired = fired_at(&s);
        let left = fired.iter().copied().find(|&t| t > 0.0);
        assert_eq!(fired[0], 0.0, "{}", ctx(&departure));
        assert!(
            left.is_some_and(|t| t > 12.0 && t <= 12.77 + interval),
            "{}: {fired:?}",
            ctx(&departure)
        );
    }
}

/// SPEC-077: a 100x100 box moving 8 px/s, horizontally, vertically or
/// diagonally the same distance, on a 512x288 or a portrait 288x512 frame,
/// is 0.0566 diagonals/s: over the 0.05 bound, never hidden.
#[test]
fn an_8_px_a_second_crawl_is_never_hidden_in_any_direction_or_orientation() {
    let crawl = |name, size, at| Vector {
        name,
        secs: 12.0,
        size,
        box_px: (100.0, 100.0),
        jitter: false,
        policy: ParkingPolicyV2 {
            max_displacement_box_diagonals: 0.20,
            ..policy()
        },
        at,
    };
    for size in [SIZE, (288, 512)] {
        let vectors = [
            crawl("horizontal", size, |t| Some((60.0 + 8.0 * t as f32, 100.0))),
            crawl("vertical", size, |t| Some((80.0, 60.0 + 8.0 * t as f32))),
            crawl("diagonal", size, |t| {
                let s = t as f32 * 8.0 / std::f32::consts::SQRT_2;
                Some((60.0 + s, 60.0 + s))
            }),
        ];
        for v in &vectors {
            for fps in RATES {
                let s = replay(v, fps, 1.0);
                assert_eq!(s, replay(v, fps, 2.0), "{} {size:?} at {fps} fps", v.name);
                assert_eq!(hidden_from(&s), None, "{} {size:?} at {fps} fps", v.name);
            }
        }
    }
}
