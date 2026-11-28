//! Golden bytes for what the tracker chain puts on the wire and in the store.
//!
//! `TrackedObject.attributes` is serialized on the `FRAME_METADATA` bus topic
//! (`serde_json::to_value`), re-serialized by the SSE route and the
//! latest-frame API after the bus hands it back as a typed `FrameMetadata`,
//! and written by the supervisor as a motion event's `attributes_json`. The
//! fixture holds those bytes for the real ByteTrack -> annotator ->
//! static-filter -> motion-emitter chain over a scene that stamps every
//! attribute key the tree writes: the `tracking.*` keys, every `motion.*` key
//! including proximity, tools and anchor removal, `group.size`, the static
//! filter's `tracker.*` keys, and detector-supplied keys that are not
//! constants. It was captured before the attribute key type changed, so a
//! change to that type that alters one byte fails here.
//!
//! Separate fixtures hold the same scene under SPEC-077's `parking_policy_v2`
//! at 30 fps (the moving `vehicle_speed` truck is never static) and at 2 fps
//! (the car dwells, is hidden, then leaves and loses its anchor).
//!
//! `NEXUS_BLESS_GOLDEN=1 cargo test -p nexus-tracker --test attributes_golden`
//! rewrites the fixtures. Only do that for a change that is meant to alter the
//! output, and say so in the commit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use nexus_config::{
    AnnotatorConfig, ByteTrackConfig, ParkingPolicyV2, StaticObjectConfig, ZoneConfig, ZoneKind,
};
use nexus_tracker::{
    is_object_static, ByteTrackTracker, MotionEventEmitter, StaticObjectFilter, TrackAnnotator,
    Tracker,
};
use nexus_types::{BBox, Detection, Frame, FrameMetadata, PixelFormat, TrackedObject};
use serde::Deserialize;
use serde_json::json;

const W: u32 = 1536;
const H: u32 = 864;
const FRAMES: u64 = 22;
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/attributes_golden.txt"
);
const V2_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/attributes_golden_parking_v2.txt"
);
const V2_2FPS_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/attributes_golden_parking_v2_2fps.txt"
);
/// The legacy scene's frame interval: 30 fps.
const MS_30FPS: u64 = 33;

static MONO0: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn frame(i: u64, interval_ms: u64) -> Frame {
    Frame {
        camera_id: 1,
        frame_id: i,
        captured_at: Utc
            .timestamp_millis_opt(1_700_000_000_000 + (i * interval_ms) as i64)
            .unwrap(),
        captured_mono: *MONO0 + std::time::Duration::from_millis(i * interval_ms),
        width: W,
        height: H,
        format: PixelFormat::Rgb24,
        data: Arc::new(Vec::new()),
        trace_id: format!("t{i}"),
    }
}

fn det(label: &str, x1: f32, y1: f32, w: f32, h: f32) -> Detection {
    Detection {
        label: label.into(),
        confidence: 0.9,
        bbox: BBox {
            x1,
            y1,
            x2: x1 + w,
            y2: y1 + h,
        },
        attributes: Default::default(),
    }
}

/// A walker in the walkway carrying detector attributes (one of them not
/// ASCII, one nested, present every third frame); a person walking out of
/// the walkway past a ladder towards a parked car; the car, parked long
/// enough to become a static anchor and then driven away, so the anchor is
/// removed; a truck fast enough for `vehicle_speed`; and a person seen for a
/// few frames only, so its track is lost, retired and dies.
fn detections(i: u64) -> Vec<Detection> {
    let f = i as f32;
    let mut walker = det("person", 100.0 + 3.0 * f, 100.0, 50.0, 140.0);
    walker.attributes.insert("ppe.hardhat".into(), json!(true));
    walker
        .attributes
        .insert("classifier.score".into(), json!(0.87));
    if i.is_multiple_of(3) {
        walker
            .attributes
            .insert("zoné.ü".into(), json!({"b": [1, "x"], "a": null}));
    }
    let car_x = if i < 14 {
        640.0
    } else {
        640.0 + 45.0 * (i - 13) as f32
    };
    let mut out = vec![
        walker,
        det("person", 660.0, 330.0 + 8.0 * f, 50.0, 140.0),
        det("ladder", 700.0, 520.0, 30.0, 80.0),
        det("vehicle.car", car_x, 560.0, 220.0, 120.0),
        det("vehicle.truck", 100.0 + 12.0 * f, 760.0, 200.0, 80.0),
    ];
    if (2..8).contains(&i) {
        out.push(det("person", 1200.0, 150.0, 50.0, 140.0));
    }
    out
}

fn zones() -> Vec<ZoneConfig> {
    vec![
        ZoneConfig {
            id: "walkway".into(),
            name: "Walkway".into(),
            polygon: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.5), (0.0, 0.5)],
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: None,
        },
        ZoneConfig {
            id: "parking".into(),
            name: "Parking".into(),
            polygon: vec![(0.0, 0.6), (1.0, 0.6), (1.0, 1.0), (0.0, 1.0)],
            kind: ZoneKind::Dwell,
            min_bbox_area_px_override: None,
        },
    ]
}

/// SPEC-077 plan test values, not customer defaults.
fn parking_policy() -> ParkingPolicyV2 {
    ParkingPolicyV2 {
        dwell_secs: 5.0,
        max_speed_box_diagonals_per_sec: 0.05,
        max_displacement_box_diagonals: 0.10,
        max_observation_gap_secs: 3.0,
    }
}

/// The legacy scene's zones plus a parking zone over the lower lot, where
/// the car parks and the truck drives.
fn parking_zones() -> Vec<ZoneConfig> {
    let mut zones = zones();
    zones.push(ZoneConfig {
        id: "stalls".into(),
        name: "Stalls".into(),
        polygon: vec![(0.0, 0.6), (1.0, 0.6), (1.0, 1.0), (0.0, 1.0)],
        kind: ZoneKind::Parking,
        min_bbox_area_px_override: None,
    });
    zones
}

/// The legacy scene at 30 fps.
fn scene() -> (Vec<FrameMetadata>, Vec<String>) {
    scene_at(MS_30FPS, false)
}

/// One `FrameMetadata` per frame, and the motion emitter's decisions, as the
/// supervisor would publish and store them, with frames `interval_ms`
/// apart, under `parking_policy_v2` if `v2`.
fn scene_at(interval_ms: u64, v2: bool) -> (Vec<FrameMetadata>, Vec<String>) {
    let tracker = ByteTrackTracker::new(ByteTrackConfig {
        max_lost_frames: 3,
        ..Default::default()
    });
    let mut annotator = TrackAnnotator::new(AnnotatorConfig {
        parked_min_frames_to_flag: Some(5),
        tool_proximity_labels: vec!["ladder".into()],
        ..Default::default()
    });
    let mut sf = StaticObjectFilter::new(
        StaticObjectConfig {
            dwell_frames: Some(5),
            ..Default::default()
        },
        1,
        None,
    );
    let zones = if v2 { parking_zones() } else { zones() };
    if v2 {
        sf = sf.with_parking_policy(&parking_policy(), &zones).unwrap();
    }
    let mut emitter = MotionEventEmitter::new(1.0);
    let mut metas = Vec::new();
    let mut motion = Vec::new();
    for i in 0..FRAMES {
        let f = frame(i, interval_ms);
        let mut tracked = tracker.update(detections(i), f.captured_mono);
        let anchors = sf.anchors().to_vec();
        annotator.annotate(&f, &zones, &anchors, &mut tracked);
        sf.classify(&f, &mut tracked);
        let dynamic = tracked.iter().filter(|t| !is_object_static(t));
        for d in emitter.tick(1, dynamic, f.captured_at, f.captured_mono) {
            motion.push(format!(
                "motion {i} {:?} track {}: {}",
                d.kind,
                d.track_id,
                serde_json::to_string(&d.attributes).unwrap()
            ));
        }
        metas.push(FrameMetadata {
            camera_id: 1,
            frame_id: i,
            captured_at: f.captured_at,
            width: W,
            height: H,
            trace_id: f.trace_id.clone(),
            objects: Arc::new(tracked),
        });
    }
    (metas, motion)
}

fn render(metas: &[FrameMetadata], motion: &[String]) -> String {
    let mut out = String::new();
    for m in metas {
        out.push_str(&format!(
            "frame {}: {}\n",
            m.frame_id,
            serde_json::to_string(m).unwrap()
        ));
    }
    for line in motion {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Every key the tree writes appears in the scene, so the golden bytes cover
/// each of them.
#[test]
fn the_scene_stamps_every_attribute_key() {
    let (metas, _) = scene();
    let mut seen = std::collections::BTreeSet::new();
    for m in &metas {
        for o in m.objects.iter() {
            seen.extend(o.attributes.keys().map(|k| k.to_string()));
        }
    }
    let expected = [
        "classifier.score",
        "group.size",
        "motion.carrying_anchor_label",
        "motion.direction",
        "motion.dwell_seconds",
        "motion.near_static_vehicle_id",
        "motion.near_static_vehicle_seconds",
        "motion.parked_vehicle",
        "motion.removed_anchor_ids",
        "motion.speed_class",
        "motion.tool_in_proximity_confidence",
        "motion.tool_in_proximity_label",
        "motion.zone_ids",
        "motion.zone_state",
        "ppe.hardhat",
        "tracker.is_static",
        "tracker.movement_ema",
        "tracker.moving_consecutive_frames",
        "tracker.static_alert_epoch",
        "tracker.static_frames",
        "tracking.hit_streak",
        "tracking.lifecycle",
        "tracking.missed_frames",
        "tracking.predicted_only",
        "zoné.ü",
    ];
    let expected: std::collections::BTreeSet<String> =
        expected.iter().map(|k| k.to_string()).collect();
    assert_eq!(seen, expected);
}

#[test]
fn serialized_attributes_match_the_golden_bytes() {
    let (metas, motion) = scene();
    assert_golden(FIXTURE, &render(&metas, &motion));
}

/// SPEC-077: the same 30 fps scene under `parking_policy_v2`. Nothing has
/// dwelled 5 observed seconds, so nothing is static, the moving
/// `vehicle_speed` truck included.
#[test]
fn parking_policy_v2_attributes_match_their_golden_bytes() {
    let (metas, motion) = scene_at(MS_30FPS, true);
    assert_golden(V2_FIXTURE, &render(&metas, &motion));
}

/// SPEC-077: the scene's frames 500 ms apart under `parking_policy_v2`: the
/// car is static once it has dwelled 5 s and loses its anchor leaving; the
/// truck, 0.11 box diagonals a second, never is.
#[test]
fn parking_policy_v2_at_2_fps_matches_its_golden_bytes() {
    let (metas, motion) = scene_at(500, true);
    assert_golden(V2_2FPS_FIXTURE, &render(&metas, &motion));
}

#[test]
fn under_parking_policy_v2_the_vehicle_speed_truck_is_never_static() {
    for (interval_ms, speed) in [(MS_30FPS, Some("vehicle_speed")), (500, None)] {
        let (metas, _) = scene_at(interval_ms, true);
        let trucks: Vec<&TrackedObject> = metas
            .iter()
            .flat_map(|m| m.objects.iter())
            .filter(|o| o.label == "vehicle.truck")
            .collect();
        assert!(trucks.iter().all(|o| !is_object_static(o)));
        if let Some(speed) = speed {
            assert!(trucks
                .iter()
                .any(|o| o.attributes.get("motion.speed_class") == Some(&json!(speed))));
        }
        let cars = metas.iter().flat_map(|m| m.objects.iter());
        let hidden = cars.filter(|o| o.label == "vehicle.car" && is_object_static(o));
        assert_eq!(hidden.count() > 0, interval_ms == 500, "{interval_ms} ms");
    }
}

fn assert_golden(fixture: &str, got: &str) {
    if std::env::var_os("NEXUS_BLESS_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(fixture).parent().unwrap()).unwrap();
        std::fs::write(fixture, got).unwrap();
    }
    let want = std::fs::read_to_string(fixture).expect("fixture");
    for (n, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        assert_eq!(g, w, "line {} differs from the golden bytes", n + 1);
    }
    assert_eq!(got, want, "output differs from the golden bytes in length");
}

/// The bus path: `to_value` on publish, a typed `FrameMetadata` back out of
/// the `Value` on subscribe, then the SSE route serializes it again. That
/// must give the golden bytes. (The `Value` itself never leaves the process,
/// and it is not the golden bytes: it sorts struct fields and widens `f32`.)
///
/// Read back from text, every object's attributes are what a plain JSON
/// reader sees in that text. They are compared as values, not re-serialized:
/// serde_json's default float parser does not round-trip every `f64` to the
/// last digit (`11.093318898147235` reads back as `...237`), whatever the
/// key type.
#[test]
fn every_serialization_path_round_trips_to_the_same_bytes() {
    let (metas, _) = scene();
    for m in &metas {
        let direct = serde_json::to_string(m).unwrap();
        let value = serde_json::to_value(m).unwrap();
        let from_bus = FrameMetadata::deserialize(&value).unwrap();
        assert_eq!(serde_json::to_string(&from_bus).unwrap(), direct, "bus");
        assert_eq!(serde_json::to_value(&from_bus).unwrap(), value, "bus value");

        let parsed: FrameMetadata = serde_json::from_str(&direct).unwrap();
        let plain: serde_json::Value = serde_json::from_str(&direct).unwrap();
        assert_eq!(parsed.objects.len(), m.objects.len());
        for (j, o) in parsed.objects.iter().enumerate() {
            let want = plain["objects"][j].get("attributes").cloned();
            let got =
                (!o.attributes.is_empty()).then(|| serde_json::to_value(&o.attributes).unwrap());
            assert_eq!(got, want, "frame {} object {j}", m.frame_id);
        }
    }
}

/// Keys are data: a reader accepts any string, not only the constants.
#[test]
fn any_attribute_key_deserializes_and_round_trips() {
    let text = r#"{"track_id":1,"label":"x","confidence":0.5,"bbox":{"x1":0.0,"y1":0.0,"x2":1.0,"y2":1.0},"age_frames":0,"age_ms":0,"attributes":{"":0,"a\"b":"q","motion.speed_class":"walking","zz":[],"ключ":{"k":true}}}"#;
    let o: TrackedObject = serde_json::from_str(text).unwrap();
    assert_eq!(o.attributes.len(), 5);
    assert_eq!(o.attributes["ключ"], json!({"k": true}));
    assert_eq!(serde_json::to_string(&o).unwrap(), text);
}

/// An empty map writes no `attributes` field at all, and a missing field
/// reads back as an empty map. The golden scene never has an empty map.
#[test]
fn an_empty_attribute_map_is_not_written() {
    let text = r#"{"track_id":1,"label":"x","confidence":0.5,"bbox":{"x1":0.0,"y1":0.0,"x2":1.0,"y2":1.0},"age_frames":0,"age_ms":0}"#;
    let o: TrackedObject = serde_json::from_str(text).unwrap();
    assert!(o.attributes.is_empty());
    assert_eq!(serde_json::to_string(&o).unwrap(), text);

    let text = r#"{"label":"x","confidence":0.5,"bbox":{"x1":0.0,"y1":0.0,"x2":1.0,"y2":1.0}}"#;
    let d: Detection = serde_json::from_str(text).unwrap();
    assert!(d.attributes.is_empty());
    assert_eq!(serde_json::to_string(&d).unwrap(), text);
}
