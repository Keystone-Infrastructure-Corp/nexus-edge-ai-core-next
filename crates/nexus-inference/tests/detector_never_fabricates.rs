//! Regression test for the 2026-08 synthetic-alert-flood incident.
//!
//! ## What happened
//!
//! An operator-persisted `inference.model` row pinned a legacy square
//! input shape (640x640). A later release replaced the square model
//! shapes with an exact-16:9 ladder, so the square `.onnx` stopped
//! shipping in the model pack. The resolver correctly hard-failed — but
//! the detector builder caught that error and silently substituted
//! `MockDetector`, which fabricates one `person` box on *every frame of
//! every camera*.
//!
//! The rules engine cannot distinguish a synthetic detection from a real
//! one. A single stale config row therefore produced ~19,000 alerts for
//! people who were never there, across 29 cameras, with the only
//! evidence being one `WARN` line at boot.
//!
//! ## The invariant these tests pin
//!
//! A detector that cannot load its model must produce **zero**
//! detections and mark engine health degraded. It must never invent
//! detections. Asking for the mock *by name* stays supported, because
//! that is an explicit, intentional test configuration.

use std::sync::Arc;

use nexus_config::{InferenceBackendKind, InferenceConfig, ModelConfig, PoolWorkerKind};
use nexus_inference::{build, health, Detector, UnavailableDetector};
use nexus_types::{Frame, PixelFormat};

fn frame() -> Frame {
    Frame {
        camera_id: 1,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        width: 640,
        height: 360,
        format: PixelFormat::Rgb24,
        data: Arc::new(vec![0u8; 640 * 360 * 3]),
        trace_id: "detector-never-fabricates".into(),
    }
}

fn cfg_with_kind(kind: &str) -> InferenceConfig {
    InferenceConfig {
        backend: InferenceBackendKind::InProcess,
        pool_worker_kind: PoolWorkerKind::Thread,
        workers: 1,
        restart_backoff_ms: 0,
        fail_soft: false,
        ep_priority: vec!["cpu".into()],
        model: ModelConfig {
            kind: kind.into(),
            ..Default::default()
        },
    }
}

/// The core invariant: the degraded detector emits nothing at all.
#[tokio::test]
async fn unavailable_detector_never_fabricates_detections() {
    let det = UnavailableDetector::new();
    let out = det
        .detect(&frame(), &[])
        .await
        .expect("detect must not err");
    assert!(
        out.is_empty(),
        "a detector that failed to load its model MUST report zero detections; \
         emitting any box here is what caused the synthetic alert flood"
    );
    assert_eq!(det.name(), "unavailable");
}

/// An unresolvable model kind must degrade loudly, not silently mock.
#[test]
fn unknown_model_kind_degrades_instead_of_mocking() {
    health::clear_degraded("definitely_not_a_real_kind");

    let layer = build(&cfg_with_kind("definitely_not_a_real_kind"))
        .expect("engine must still boot so it can report the problem");

    assert_eq!(
        layer.detector.name(),
        "unavailable",
        "an unknown model kind must NOT resolve to the synthetic mock detector"
    );
    assert!(
        health::is_degraded(),
        "the failure must be visible to the health endpoint and cloud heartbeat"
    );
    assert!(
        health::degradations()
            .iter()
            .any(|d| d.kind == "definitely_not_a_real_kind"),
        "the degradation must name the offending model kind"
    );

    health::clear_degraded("definitely_not_a_real_kind");
}

/// Requesting the mock explicitly is still legitimate — this is how
/// tests and bare dev boxes run without a model pack.
#[test]
fn explicit_mock_kind_is_still_honoured() {
    let layer = build(&cfg_with_kind("mock")).expect("mock builds");
    assert_eq!(layer.detector.name(), "mock");
}
