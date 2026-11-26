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

/// A car the detector reports, at a normalized center. `id` is the track id
/// the scripted tracker reports it under; an unseen car is a coasting track
/// there, and absent for the real tracker.
#[derive(Clone, Copy)]
struct Car {
    x: f32,
    y: f32,
    id: u64,
    seen: bool,
}

fn car(x: f32, y: f32) -> Car {
    Car {
        x,
        y,
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
}

#[async_trait]
impl ClipRecorder for Recorder {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        self.opened.lock().push(args.started_at);
        self.stub.open(args).await
    }

    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
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
                let (cx, cy, bw, bh) = (c.x * w, c.y * h, CAR_W * w, CAR_H * h);
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
}

impl Scenario {
    fn v2(shots: Vec<Shot>) -> Self {
        Self {
            shots,
            zones: stalls(),
            policy: Some(policy()),
            scripted_ids: false,
        }
    }
}

struct Outcome {
    /// `FRAME_METADATA` of shot `k`.
    metadata: Vec<FrameMetadata>,
    alerts: Vec<AlertEvent>,
    opened: Vec<DateTime<Utc>>,
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

    fn died_before(&self, ms: u64) -> usize {
        let until = wall0() + chrono::Duration::milliseconds(ms as i64);
        self.motion
            .iter()
            .filter(|r| r.kind == MotionEventKind::Died && r.captured_at < until)
            .count()
    }
}

/// Run the shots, then one empty frame 500 ms after the last whose
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
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: s.zones,
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");

    let mono0 = Instant::now();
    let n = s.shots.len();
    let last_ms = s.shots.last().map_or(0, |shot| shot.ms);
    let mut shots = s.shots;
    shots.push(Shot {
        ms: last_ms + 500,
        size: shots.last().map_or(SIZE, |shot| shot.size),
        cars: Vec::new(),
    });
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
                .map(|a| {
                    (
                        a["center_x"].as_f64().unwrap() as f32,
                        a["center_y"].as_f64().unwrap() as f32,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let opened = recorder.opened.lock().clone();
    Outcome {
        metadata: tracked,
        alerts: fired,
        opened,
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
