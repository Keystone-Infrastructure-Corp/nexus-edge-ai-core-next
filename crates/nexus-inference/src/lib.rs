//! Inference layer.
//!
//! This crate owns three orthogonal axes:
//!
//! 1. **What runs** — the [`Detector`] trait. Implementations include
//!    [`MockDetector`], [`OpenVocabDetector`], and [`ClassifierEnsembleDetector`].
//! 2. **Where it runs** — the [`DetectorBackend`] trait. Implementations include
//!    [`InProcessBackend`], [`ThreadIsolatedBackend`], and [`WorkerProcessBackend`].
//!    Backends wrap a [`Detector`] in an isolation strategy.
//! 3. **How many** — the [`DetectorPool`]. Holds N backends + a fallback,
//!    routes work round-robin, fails soft to the fallback, and fan-pushes
//!    config updates to every slot. This is the W-DETECT D6/D7/D9c pattern.

#![forbid(unsafe_code)]

pub mod backends;
pub mod detectors;
pub mod pool;

pub use backends::{
    BackendState, DetectorBackend, InProcessBackend, ThreadIsolatedBackend, WorkerProcessBackend,
};
pub use detectors::{
    ClassifierEnsembleDetector, Detector, InferenceError, MockDetector, OpenVocabDetector,
};
pub use pool::{BackendStatus, DetectorPool};

use std::sync::Arc;

use nexus_config::{InferenceBackendKind, InferenceConfig};
use tracing::{info, warn};

/// Result of [`build`] — the pipeline-facing detector plus an optional
/// pool handle for the OPS API. When `inference.backend = "in_process"`
/// the pool is `None`.
pub struct InferenceLayer {
    pub detector: Arc<dyn Detector>,
    pub pool: Option<Arc<DetectorPool>>,
}

/// Build the inference layer per config.
pub fn build(cfg: &InferenceConfig) -> Result<InferenceLayer, InferenceError> {
    match cfg.backend {
        InferenceBackendKind::InProcess => {
            info!(model = %cfg.model.kind, "building in-process detector");
            Ok(InferenceLayer {
                detector: build_detector(cfg)?,
                pool: None,
            })
        }
        InferenceBackendKind::Pool => {
            let n = cfg.workers.max(1);
            info!(workers = n, model = %cfg.model.kind, "building DetectorPool");
            let mut backends: Vec<Arc<dyn DetectorBackend>> = Vec::with_capacity(n);
            for slot in 0..n {
                let det = build_detector(cfg)?;
                backends.push(Arc::new(ThreadIsolatedBackend::start(slot as i32, det, cfg)?));
            }
            let fallback = if cfg.fail_soft {
                let det = build_detector(cfg)?;
                Some(Arc::new(InProcessBackend::new(-1, det)) as Arc<dyn DetectorBackend>)
            } else {
                None
            };
            let pool = Arc::new(DetectorPool::new(backends, fallback));
            Ok(InferenceLayer {
                detector: pool.clone() as Arc<dyn Detector>,
                pool: Some(pool),
            })
        }
    }
}

fn build_detector(cfg: &InferenceConfig) -> Result<Arc<dyn Detector>, InferenceError> {
    match cfg.model.kind.as_str() {
        // Closed-vocab YOLOv26-nano shipped as the default — matches v1's
        // `models/yolo26n_dynamic.onnx` driven by the model-pack manifest's
        // 320 / 640 / 1280 presets. M0 stubs to MockDetector; M1 swaps in
        // the real ORT session.
        "yolo" | "yolo26n" | "closed_vocab" => Ok(Arc::new(MockDetector::new())),
        // Open-vocab path (YOLO-World style) — separate Detector impl,
        // explicit opt-in via config.
        "open_vocab" | "yolo_world" => Ok(Arc::new(OpenVocabDetector::new(cfg)?)),
        // PPE-style attribute heads (`ppe_v1.onnx` is the v1 ship).
        "classifier_ensemble" | "ppe" => Ok(Arc::new(ClassifierEnsembleDetector::new(cfg)?)),
        "mock" => Ok(Arc::new(MockDetector::new())),
        other => {
            warn!(kind = %other, "unknown model kind, falling back to mock");
            Ok(Arc::new(MockDetector::new()))
        }
    }
}
