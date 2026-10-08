//! The two tracker backends must agree on the `TrackedObject` contract, so
//! switching `tracker.backend` changes association quality, never what a rule
//! or sink can read. Each test runs the same detections through both.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use nexus_config::{TrackerBackendKind, TrackerConfig};
use nexus_tracker::{build_tracker, Tracker};
use nexus_types::{Attributes, BBox, Detection, TrackedObject};
use serde_json::{json, Map, Value};

fn tracker(backend: TrackerBackendKind) -> Box<dyn Tracker> {
    build_tracker(&TrackerConfig {
        backend,
        ..Default::default()
    })
}

/// The monotonic capture stamp of frame `i` of a 10 fps camera.
fn at(i: u32) -> Instant {
    static T0: LazyLock<Instant> = LazyLock::new(Instant::now);
    *T0 + Duration::from_millis(u64::from(i) * 100)
}

fn det(label: &str, x: f32, attributes: Map<String, Value>) -> Detection {
    Detection {
        label: label.into(),
        confidence: 0.9,
        bbox: BBox {
            x1: x,
            y1: 0.0,
            x2: x + 40.0,
            y2: 80.0,
        },
        attributes: attributes.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    }
}

/// The emitted object whose raw detection box is `bbox`.
fn by_detection<'a>(objects: &'a [TrackedObject], bbox: &BBox) -> &'a TrackedObject {
    objects
        .iter()
        .find(|o| o.detection_bbox.as_ref() == Some(bbox))
        .expect("every detection is emitted as a matched track")
}

/// A detector's attributes (a classifier score, a PPE flag) describe the
/// detection on this frame. IouNaive copies them onto the track; ByteTrack
/// must too, so a rule reading `object.attributes['ppe.hardhat']` behaves the
/// same on either backend.
#[test]
fn both_trackers_carry_this_frames_detection_attributes() {
    let iou = tracker(TrackerBackendKind::IouNaive);
    let byte = tracker(TrackerBackendKind::Bytetrack);
    for i in 0..5u32 {
        let x = i as f32;
        let dets = vec![
            det(
                "person",
                10.0 + x,
                Map::from_iter([
                    ("ppe.hardhat".to_string(), json!(i % 2 == 0)),
                    ("classifier.frame".to_string(), json!(i)),
                ]),
            ),
            det("vehicle.car", 400.0 + x, Map::new()),
        ];
        let from_iou = iou.update(dets.clone(), at(i));
        let from_byte = byte.update(dets.clone(), at(i));
        for d in &dets {
            let a = by_detection(&from_iou, &d.bbox);
            let b = by_detection(&from_byte, &d.bbox);
            assert_eq!(
                a.attributes, d.attributes,
                "frame {i}: iou-naive copies them"
            );
            let detector_keys: Attributes = b
                .attributes
                .iter()
                .filter(|(k, _)| !k.starts_with("tracking."))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(
                detector_keys, a.attributes,
                "frame {i} {}: bytetrack must carry the same detection attributes",
                d.label
            );
        }
    }
}

/// `age_ms` is how long the camera has watched the object: the difference
/// between two frames' capture stamps, however fast they were processed. This
/// loop runs far faster than the 10 fps it replays.
#[test]
fn both_trackers_age_tracks_in_frame_time() {
    for backend in [TrackerBackendKind::IouNaive, TrackerBackendKind::Bytetrack] {
        let t = tracker(backend);
        for i in 0..5u32 {
            let out = t.update(vec![det("person", 10.0 + i as f32, Map::new())], at(i));
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].age_ms, u64::from(i) * 100, "{backend:?} frame {i}");
        }
    }
}

/// A stamp older than the track's first frame keeps the track, and its age
/// and IouNaive's TTL saturate at zero instead of panicking or wrapping. The
/// trackers do not rely on the frames reaching them in stamp order.
#[test]
fn an_older_stamp_keeps_the_track_and_holds_its_age_at_zero() {
    for backend in [TrackerBackendKind::IouNaive, TrackerBackendKind::Bytetrack] {
        let t = tracker(backend);
        let first = t.update(vec![det("person", 10.0, Map::new())], at(10));
        let back = t.update(vec![det("person", 11.0, Map::new())], at(0));
        assert_eq!(back.len(), 1, "{backend:?}");
        assert_eq!(
            back[0].track_id, first[0].track_id,
            "{backend:?}: an older stamp dropped the track"
        );
        assert_eq!(back[0].age_ms, 0, "{backend:?}: age on an older stamp");
    }
}
