//! M3.3 acceptance — prompt-free `yoloe_promptfree` dispatch through
//! the inference router.
//!
//! Without `--features ort` the inner wraps `MockDetector`; the
//! exported behaviour we care about is dispatch + `top_k` plumbing,
//! both of which are observable in this build.

use nexus_config::{
    CameraBehavior, CameraConfig, CameraConfigUpdate, CameraDetector, CameraIngest,
    InferenceBackendKind, InferenceConfig, ModelConfig, PoolWorkerKind,
};
use nexus_inference::InferenceRouter;
use url::Url;

fn cfg() -> InferenceConfig {
    InferenceConfig {
        backend: InferenceBackendKind::InProcess,
        pool_worker_kind: PoolWorkerKind::Thread,
        workers: 1,
        restart_backoff_ms: 0,
        fail_soft: false,
        ep_priority: vec!["cpu".into()],
        model: ModelConfig {
            kind: "mock".into(),
            ..Default::default()
        },
    }
}

fn cam(id: i64, override_kind: Option<&str>, top_k: Option<usize>) -> CameraConfig {
    CameraConfig {
        id,
        name: format!("cam-{id}"),
        ingest: CameraIngest {
            url: Url::parse("virtual://test").unwrap(),
            enabled: true,
            max_fps: 0,
            codec: None,
        },
        detector: CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: override_kind.map(|k| ModelConfig {
                kind: k.into(),
                top_k,
                ..Default::default()
            }),
        },
        behavior: CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
            ..Default::default()
        },
        zones: vec![],
    }
}

#[tokio::test]
async fn promptfree_layer_resolves_and_strips_prompts() {
    let cams = vec![
        cam(1, None, None),
        cam(2, Some("yoloe_promptfree"), Some(3)),
    ];
    let router = InferenceRouter::build(&cfg(), &cams).expect("router");

    let kinds: Vec<String> = router.detectors().into_iter().map(|(k, _)| k).collect();
    assert!(
        kinds.iter().any(|k| k == "yoloe_promptfree"),
        "expected yoloe_promptfree layer; got {kinds:?}"
    );

    let det = router.detector_for_camera(&cams[1]);
    assert_eq!(det.name(), "yoloe_promptfree");

    // Reconciler hot-update with non-empty prompts must be tolerated
    // (the wrapper strips them before forwarding to the inner).
    let update = CameraConfigUpdate {
        camera_id: 2,
        generation: 1,
        prompts: vec!["accidentally".into(), "configured".into()],
        visual_prompts: vec![],
        model: ModelConfig {
            kind: "yoloe_promptfree".into(),
            top_k: Some(3),
            ..Default::default()
        },
    };
    det.push_camera_config(&update).await;
}
