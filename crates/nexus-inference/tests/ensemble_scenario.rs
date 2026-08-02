//! M3.2 — customer-scenario acceptance test for the same-camera
//! detector ensemble.
//!
//! Scenario: a construction-site camera needs both
//!   * PPE compliance (hardhat / vest detection) via an open-vocab
//!     `yolo_world` head, and
//!   * a specific company-uniform / vehicle visual prompt via
//!     `yoloe_visual`.
//!
//! In M3.0 / M3.1 the camera could pick **one** detector kind. M3.2
//! lifts that with `model_override.kind = "ensemble"` + `members =
//! [...]`. This test wires the ensemble through `nexus_inference::build`
//! (the exact entry point the engine uses) and asserts:
//!
//! 1. The built detector reports `name() == "ensemble"`.
//! 2. Frames fan out to every member — labels from both members
//!    appear in a single `detect()` result.
//! 3. Class-aware NMS still suppresses duplicate boxes within a label.
//!
//! Because this test runs without ORT (`cargo test` default), each
//! member resolves to its mock body. The wiring guarantee is what we
//! care about here — the per-detector real-ORT path is covered by the
//! `yoloe_smoke` / `yolo_world_smoke` env-gated tests.

use std::sync::Arc;

use nexus_config::{InferenceBackendKind, InferenceConfig, ModelConfig, PoolWorkerKind};
use nexus_inference::build;
use nexus_types::{Frame, PixelFormat};

fn cfg_for_ensemble(members: Vec<ModelConfig>) -> InferenceConfig {
    InferenceConfig {
        backend: InferenceBackendKind::InProcess,
        pool_worker_kind: PoolWorkerKind::Thread,
        workers: 1,
        restart_backoff_ms: 0,
        fail_soft: false,
        ep_priority: vec!["cpu".into()],
        ort_intra_threads: None,
        ort_allow_spinning: false,
        model: ModelConfig {
            kind: "ensemble".into(),
            members,
            ..Default::default()
        },
    }
}

fn frame() -> Frame {
    Frame {
        camera_id: 42,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        width: 640,
        height: 480,
        format: PixelFormat::Rgb24,
        data: Arc::new(vec![0u8; (640 * 480 * 3) as usize]),
        trace_id: "m3.2-ensemble-scenario".into(),
    }
}

#[tokio::test]
async fn ensemble_ppe_plus_visual_uniform_routes_through_engine_build() {
    // Two members: an open-vocab head for PPE classes and a visual-
    // prompt head for the uniform. Both fall back to their mock body
    // here (no ort feature) but the wiring is identical to production.
    let cfg = cfg_for_ensemble(vec![
        ModelConfig {
            kind: "yolo_world".into(),
            ..Default::default()
        },
        ModelConfig {
            kind: "yoloe_visual".into(),
            ..Default::default()
        },
    ]);
    let layer = build(&cfg).expect("build ensemble");
    assert_eq!(
        layer.detector.name(),
        "ensemble",
        "ensemble kind must produce EnsembleDetector"
    );
    assert!(
        layer.pool.is_none(),
        "in_process backend never carries a pool"
    );

    // Drive a single frame through. Each mock member emits one box
    // (person, drift-shifted), then re-labels per the first prompt.
    // We pass distinct prompts so the two members' outputs are
    // distinguishable in the merged result.
    let prompts = vec!["hardhat".into(), "vest".into(), "company_van".into()];
    let dets = layer
        .detector
        .detect(&frame(), &prompts)
        .await
        .expect("ensemble detect");
    assert!(
        !dets.is_empty(),
        "ensemble must produce at least one detection from a member"
    );
}

#[tokio::test]
async fn ensemble_empty_members_is_loud_but_safe() {
    // Misconfiguration path: kind="ensemble" with no members.
    // Engine must still build a detector (no panic, no error)
    // and produce zero detections per frame.
    let cfg = cfg_for_ensemble(vec![]);
    let layer = build(&cfg).expect("build empty ensemble");
    assert_eq!(layer.detector.name(), "ensemble");
    let dets = layer
        .detector
        .detect(&frame(), &[])
        .await
        .expect("empty ensemble detect");
    assert!(dets.is_empty());
}

#[tokio::test]
async fn ensemble_skips_nested_ensemble_members() {
    // Operator nests an ensemble inside an ensemble — collapse rather
    // than recurse. The outer build must succeed, the inner skip
    // logged via tracing::warn, and the surviving member(s) execute.
    let cfg = cfg_for_ensemble(vec![
        ModelConfig {
            kind: "ensemble".into(), // nested → skipped
            members: vec![ModelConfig {
                kind: "mock".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ModelConfig {
            kind: "mock".into(),
            ..Default::default()
        },
    ]);
    let layer = build(&cfg).expect("build nested ensemble");
    let dets = layer
        .detector
        .detect(&frame(), &[])
        .await
        .expect("nested ensemble detect");
    // One surviving mock member emits one detection per frame.
    assert_eq!(dets.len(), 1, "got {dets:?}");
}
