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

// `deny` not `forbid` so the optional `yolo` module can opt in to the
// `unsafe` blocks emitted by `ort::inputs!` macro expansions; everything
// outside that one module is still unsafe-free.
#![deny(unsafe_code)]

pub mod backends;
pub mod detectors;
pub mod pool;
pub mod router;
pub mod worker_proto;
#[cfg(feature = "ort")]
pub mod yolo;
#[cfg(feature = "ort")]
pub mod yolo_world;

pub use backends::{
    BackendState, DetectorBackend, InProcessBackend, ThreadIsolatedBackend, WorkerProcessBackend,
};
pub use detectors::{
    ClassifierEnsembleDetector, Detector, InferenceError, MockDetector, OpenVocabDetector,
};
pub use pool::{BackendStatus, DetectorPool};
pub use router::InferenceRouter;
#[cfg(feature = "ort")]
pub use yolo::YoloOrtDetector;
#[cfg(feature = "ort")]
pub use yolo_world::YoloWorldDetector;

use std::sync::Arc;

use nexus_config::{InferenceBackendKind, InferenceConfig, PoolWorkerKind};
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
            info!(
                workers = n,
                model = %cfg.model.kind,
                worker_kind = ?cfg.pool_worker_kind,
                "building DetectorPool"
            );
            let mut backends: Vec<Arc<dyn DetectorBackend>> = Vec::with_capacity(n);
            for slot in 0..n {
                let backend: Arc<dyn DetectorBackend> = match cfg.pool_worker_kind {
                    PoolWorkerKind::Thread => {
                        let det = build_detector(cfg)?;
                        Arc::new(ThreadIsolatedBackend::start(slot as i32, det, cfg)?)
                    }
                    PoolWorkerKind::Process => {
                        // The worker binary builds its own Detector from
                        // env (NEXUS_WORKER_MODEL_KIND). No detector built
                        // in this process for the slot.
                        Arc::new(WorkerProcessBackend::start(
                            slot as i32,
                            cfg.model.kind.as_str(),
                        )?)
                    }
                };
                backends.push(backend);
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
        // 320 / 640 / 1280 presets. Real ORT path lights up when the `ort`
        // cargo feature is on AND inference.model.pack_path is set; without
        // either we keep the MockDetector so the engine still boots on a
        // bare dev box.
        "yolo" | "yolo26n" | "closed_vocab" => {
            // Two cfg arms with mutually-exclusive bodies — the explicit
            // `return` in the `ort` arm keeps the function single-exit
            // and avoids a type mismatch when the feature is off and the
            // mock fallback runs instead.
            #[cfg(feature = "ort")]
            #[allow(clippy::needless_return)]
            {
                return crate::yolo::build_detector_for_yolo(cfg);
            }
            #[cfg(not(feature = "ort"))]
            {
                warn!(
                    kind = %cfg.model.kind,
                    "ort feature not compiled in; using mock for closed-vocab kind"
                );
                Ok(Arc::new(MockDetector::new()))
            }
        }
        // Open-vocab path (YOLO-World) — real ORT impl when the feature is
        // on AND inference.model.pack_path is set. Same fallback shape as
        // the closed-vocab arm above: keep MockDetector if either is
        // missing so the engine still boots on a bare dev box.
        "open_vocab" | "yolo_world" => {
            #[cfg(feature = "ort")]
            #[allow(clippy::needless_return)]
            {
                return crate::yolo_world::build_detector_for_yolo_world(cfg);
            }
            #[cfg(not(feature = "ort"))]
            {
                warn!(
                    kind = %cfg.model.kind,
                    "ort feature not compiled in; using OpenVocabDetector mock body"
                );
                Ok(Arc::new(OpenVocabDetector::new(cfg)?))
            }
        }
        // PPE-style attribute heads (`ppe_v1.onnx` is the v1 ship).
        "classifier_ensemble" | "ppe" => Ok(Arc::new(ClassifierEnsembleDetector::new(cfg)?)),
        "mock" => Ok(Arc::new(MockDetector::new())),
        other => {
            warn!(kind = %other, "unknown model kind, falling back to mock");
            Ok(Arc::new(MockDetector::new()))
        }
    }
}
