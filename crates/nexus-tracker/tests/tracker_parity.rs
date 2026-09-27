//! The two tracker backends must agree on the `TrackedObject` contract, so
//! switching `tracker.backend` changes association quality, never what a rule
//! or sink can read. Each test runs the same detections through both.

use nexus_config::{TrackerBackendKind, TrackerConfig};
use nexus_tracker::{build_tracker, Tracker};
use nexus_types::{BBox, Detection, TrackedObject};
use serde_json::{json, Map, Value};

fn tracker(backend: TrackerBackendKind) -> Box<dyn Tracker> {
    build_tracker(&TrackerConfig {
        backend,
        ..Default::default()
    })
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
        attributes,
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
        let from_iou = iou.update(dets.clone());
        let from_byte = byte.update(dets.clone());
        for d in &dets {
            let a = by_detection(&from_iou, &d.bbox);
            let b = by_detection(&from_byte, &d.bbox);
            assert_eq!(
                a.attributes, d.attributes,
                "frame {i}: iou-naive copies them"
            );
            let detector_keys: Map<String, Value> = b
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
