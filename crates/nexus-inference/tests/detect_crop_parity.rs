//! G1 (M_TILE_REINFER) Phase A1 — parity test for the
//! `Detector::detect_crop` trait surface.
//!
//! Locks in two contracts that the supervisor's tile executor
//! (Phase A3 / B2) relies on:
//!
//! 1. **Default impl is `detect()`.** Detectors that do not (yet)
//!    override `detect_crop` MUST return the same result as a direct
//!    `detect()` call on the same `Frame`. Per-detector overrides
//!    (compute optimisations) are allowed to change the *cost* but
//!    not the *output*.
//!
//! 2. **Returned bboxes live in CROP space, not parent-frame space.**
//!    The caller (tile executor) is the one that maps tile-space
//!    detections back into the parent supervisor-frame coordinate
//!    system. A detector that returned parent-frame coords from
//!    `detect_crop` would break the mapper silently.
//!
//! These tests run against `MockDetector` because it has no ORT
//! dependency and emits one deterministic detection scaled to
//! `frame.width × frame.height` — exactly the shape the second
//! contract needs to verify.

use std::sync::Arc;

use nexus_inference::detectors::{Detector, MockDetector};
use nexus_types::{Frame, PixelFormat};

fn frame_of(w: u32, h: u32, trace: &str) -> Frame {
    Frame {
        camera_id: 7,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(vec![0u8; (w * h * 3) as usize]),
        trace_id: trace.into(),
    }
}

#[tokio::test]
async fn detect_crop_default_impl_matches_detect_on_same_frame() {
    // Fresh detector per call — `MockDetector` carries an internal
    // call counter that drifts the emitted bbox, so reusing one
    // instance across the two calls would race that counter, not the
    // contract under test. Two independent instances yield identical
    // outputs on the first call iff the default impl is the
    // identity-forward we documented in the trait surface.
    let det_a = MockDetector::new();
    let det_b = MockDetector::new();
    let f = frame_of(640, 360, "g1-a1-parity-same-frame");
    let a = det_a.detect(&f, &[]).await.expect("detect");
    let b = det_b.detect_crop(&f, &[]).await.expect("detect_crop");
    assert_eq!(a.len(), b.len(), "len mismatch — default impl drifted");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x.label, y.label, "label[{i}]");
        assert!(
            (x.confidence - y.confidence).abs() < f32::EPSILON,
            "conf[{i}]"
        );
        assert!((x.bbox.x1 - y.bbox.x1).abs() < f32::EPSILON, "x1[{i}]");
        assert!((x.bbox.y1 - y.bbox.y1).abs() < f32::EPSILON, "y1[{i}]");
        assert!((x.bbox.x2 - y.bbox.x2).abs() < f32::EPSILON, "x2[{i}]");
        assert!((x.bbox.y2 - y.bbox.y2).abs() < f32::EPSILON, "y2[{i}]");
    }
}

#[tokio::test]
async fn detect_crop_returns_bboxes_in_crop_space_not_parent_space() {
    let det = MockDetector::new();
    let parent_w = 1920u32;
    let parent_h = 1080u32;
    let crop_w = 640u32;
    let crop_h = 360u32;

    let crop = frame_of(crop_w, crop_h, "g1-a1-parity-crop-space");
    let dets = det.detect_crop(&crop, &[]).await.expect("detect_crop");
    assert!(!dets.is_empty(), "mock must emit ≥1 detection");

    for d in &dets {
        assert!(
            d.bbox.x2 <= crop_w as f32 + f32::EPSILON,
            "x2 {} exceeded crop width {} — detect_crop returned parent-frame coords?",
            d.bbox.x2,
            crop_w
        );
        assert!(
            d.bbox.y2 <= crop_h as f32 + f32::EPSILON,
            "y2 {} exceeded crop height {} — detect_crop returned parent-frame coords?",
            d.bbox.y2,
            crop_h
        );
        assert!(
            d.bbox.x2 <= parent_w as f32 * 0.5,
            "bbox x2 {} suspiciously close to parent width {} — looks like parent-frame coords",
            d.bbox.x2,
            parent_w
        );
        assert!(
            d.bbox.y2 <= parent_h as f32 * 0.5,
            "bbox y2 {} suspiciously close to parent height {} — looks like parent-frame coords",
            d.bbox.y2,
            parent_h
        );
    }
}
