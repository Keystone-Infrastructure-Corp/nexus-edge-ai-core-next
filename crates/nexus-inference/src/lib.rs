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
pub mod caps;
pub mod coco_labels;
pub mod detectors;
#[cfg(feature = "ort")]
pub mod encoder;
pub mod ensemble;
#[cfg(feature = "ort")]
pub mod execution_providers;
pub mod fire_smoke;
pub mod health;
pub mod label_space;
pub mod nms;
pub mod pool;
pub mod router;
#[cfg(feature = "ort")]
pub mod session_tuning;
pub mod tamper;
pub mod visual_prompts;
pub mod visual_store_sqlite;
pub mod worker_proto;
#[cfg(feature = "ort")]
pub mod yolo;
#[cfg(feature = "ort")]
pub mod yolo_world;
#[cfg(feature = "ort")]
pub mod yoloe;
pub mod yoloe_promptfree;
#[cfg(feature = "ort")]
pub mod yoloe_visual;
// M_HAILO_EP — Hailo-8 backed YOLO detector. Cfg-gated to keep
// non-Hailo builds (notably macOS dev boxes) from pulling
// nexus-hailo-backend at all. Also gated on `ort` because we share
// the COCO label table and the BGR→RGB helper with `yolo.rs`.
#[cfg(all(feature = "ep-hailo", feature = "ort"))]
pub mod hailo_yolo;

pub use backends::{
    BackendState, DetectorBackend, InProcessBackend, ThreadIsolatedBackend, WorkerProcessBackend,
};
pub use caps::{MinBBoxAreaDetector, TopKDetector};
pub use detectors::{
    label_matches_any_prompt, ClassifierEnsembleDetector, Detector, InferenceError, MockDetector,
    OpenVocabDetector, UnavailableDetector,
};
#[cfg(feature = "ort")]
pub use encoder::ImageEncoder;
pub use health::{degradations, is_degraded, DetectorDegradation};
pub use pool::{BackendStatus, DetectorPool};
pub use router::InferenceRouter;
pub use visual_prompts::{InMemoryVisualPromptStore, VisualPromptBinding, VisualPromptStore};
pub use visual_store_sqlite::StoreBackedVisualPromptStore;
#[cfg(feature = "ort")]
pub use yolo::YoloOrtDetector;
#[cfg(feature = "ort")]
pub use yolo_world::YoloWorldDetector;
#[cfg(feature = "ort")]
pub use yoloe::YoloeDetector;
pub use yoloe_promptfree::YoloePromptFreeDetector;

// ---------------------------------------------------------------------------
// Hailo telemetry surface — exposed unconditionally so `nexus-engine`'s
// `system_metrics` handler can compile on every feature-flag combination
// (macOS dev, Linux without ep-hailo, Linux with ep-hailo). When the
// real backend is absent, [`hailo_telemetry_snapshot`] returns `None`.
// ---------------------------------------------------------------------------

/// Live snapshot of every Hailo accelerator backing the active
/// inference session. Returned by [`hailo_telemetry_snapshot`].
#[derive(Debug, Clone)]
pub struct HailoTelemetrySnapshot {
    pub devices: Vec<HailoDeviceTelemetry>,
    /// Failure reason when [`devices`] is empty because the underlying
    /// FFI call failed — surfaced into the System tab as the operator
    /// hint. `None` when devices populated normally.
    pub status: Option<String>,
    /// Inferences per second served by the active session, measured
    /// as a delta between consecutive snapshot polls. 0.0 on the first
    /// poll after open (no prior sample to delta against) and when
    /// `status` is set.
    pub inferences_per_sec: f32,
    /// Lifetime total inferences served by the active session.
    pub frames_total: u64,
    /// Fraction of wall-clock time the session spent inside
    /// `infer_blocking` between the previous and current poll,
    /// expressed as 0–100. HailoRT 4.x exposes no per-chip
    /// utilization counter, so this busy% is the next-best operator
    /// signal; the System tab renders it as the prominent
    /// "utilization" tile (matches the NPU and GPU cards). 0.0 on the
    /// first poll after open and when `status` is set.
    pub utilization_pct: f32,
}

/// Per-chip telemetry. Identity fields are always populated when the
/// device handle resolves; live readings may be `None` on FFI failure.
#[derive(Debug, Clone)]
pub struct HailoDeviceTelemetry {
    pub board_name: String,
    pub serial: String,
    /// Firmware version as "major.minor.revision" (e.g. "4.23.0").
    pub fw_version: String,
    pub part_number: String,
    pub product_name: String,
    pub temperature_c: Option<f32>,
    pub power_w: Option<f32>,
}

/// Read live Hailo telemetry off the active inference session. Returns
/// `None` when the engine was built without the ep-hailo feature, when
/// no Hailo detector has been instantiated yet (engine still starting
/// up), or when the host has no Hailo card.
#[cfg(all(feature = "ep-hailo", feature = "ort"))]
pub fn hailo_telemetry_snapshot() -> Option<HailoTelemetrySnapshot> {
    match hailo_yolo::telemetry_snapshot()? {
        Ok(t) => Some(HailoTelemetrySnapshot {
            devices: t
                .devices
                .into_iter()
                .map(|d| HailoDeviceTelemetry {
                    board_name: d.board_name,
                    serial: d.serial,
                    fw_version: format!("{}.{}.{}", d.fw_version.0, d.fw_version.1, d.fw_version.2),
                    part_number: d.part_number,
                    product_name: d.product_name,
                    temperature_c: d.temperature_c,
                    power_w: d.power_w,
                })
                .collect(),
            status: None,
            inferences_per_sec: t.inferences_per_sec,
            frames_total: t.frames_total,
            utilization_pct: t.utilization_pct,
        }),
        Err(e) => Some(HailoTelemetrySnapshot {
            devices: Vec::new(),
            status: Some(e.to_string()),
            inferences_per_sec: 0.0,
            frames_total: 0,
            utilization_pct: 0.0,
        }),
    }
}

#[cfg(not(all(feature = "ep-hailo", feature = "ort")))]
pub fn hailo_telemetry_snapshot() -> Option<HailoTelemetrySnapshot> {
    None
}

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
                            &cfg.ep_priority,
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
    build_detector_with_context(cfg, &BuildContext::default())
}

/// Context plumbed through detector construction for kinds that need
/// extra deps the bare `InferenceConfig` doesn't carry (e.g. yoloe
/// visual-prompt embeddings backed by SQLite).
#[derive(Default, Clone)]
pub struct BuildContext {
    pub visual_prompt_store: Option<Arc<dyn VisualPromptStore>>,
    pub visual_embedding_dim: Option<usize>,
}

/// Same as [`build`] but accepts a [`BuildContext`] so yoloe-visual and
/// related kinds can resolve their per-camera prompt store.
pub fn build_with_context(
    cfg: &InferenceConfig,
    ctx: &BuildContext,
) -> Result<InferenceLayer, InferenceError> {
    match cfg.backend {
        InferenceBackendKind::InProcess => {
            info!(model = %cfg.model.kind, "building in-process detector");
            Ok(InferenceLayer {
                detector: build_detector_with_context(cfg, ctx)?,
                pool: None,
            })
        }
        InferenceBackendKind::Pool => {
            // Pool-mode visual-prompt wiring (per-slot store handoff) is
            // not yet implemented — fall back to the plain `build` path.
            // The router will warn if a camera asked for yoloe_visual on
            // a pool backend; for now the default in-process path is the
            // expected production shape.
            build(cfg)
        }
    }
}

fn build_detector_with_context(
    cfg: &InferenceConfig,
    ctx: &BuildContext,
) -> Result<Arc<dyn Detector>, InferenceError> {
    // M_PERF_CROWD Phase B1 — wrap whatever the per-kind dispatch
    // returns with the universal per-frame caps. Order: inner →
    // MinBBoxArea → TopK. The area filter runs first so it can prune
    // tiny far-field noise before the top-k truncation makes its
    // confidence-ranked cut on what remains.
    let inner = build_detector_kind(cfg, ctx)?;
    let inner = if let Some(min_area) = cfg.model.min_bbox_area_px {
        if min_area > 0 {
            Arc::new(MinBBoxAreaDetector::new(inner, min_area)) as Arc<dyn Detector>
        } else {
            inner
        }
    } else {
        inner
    };
    let inner = if let Some(k) = cfg.model.top_k {
        // yoloe_promptfree already applies top_k internally; the outer
        // wrapper is idempotent (already sorted+truncated input stays
        // sorted+truncated) so we don't special-case the kind here.
        Arc::new(TopKDetector::new(inner, k)) as Arc<dyn Detector>
    } else {
        inner
    };
    Ok(inner)
}

fn build_detector_kind(
    cfg: &InferenceConfig,
    _ctx: &BuildContext,
) -> Result<Arc<dyn Detector>, InferenceError> {
    match cfg.model.kind.as_str() {
        // Closed-vocab YOLOv26-nano shipped as the default — matches v1's
        // `models/yolo26n_<size>.onnx` driven by the model-pack manifest's
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
        // M3.1 — yoloe (open-vocab text-prompt detector). Real ORT path
        // requires the `ort` feature AND inference.model.pack_path; mock
        // fallback otherwise so the engine still boots on a bare dev box.
        "yoloe" => {
            #[cfg(feature = "ort")]
            #[allow(clippy::needless_return)]
            {
                return crate::yoloe::build_detector_for_yoloe(cfg);
            }
            #[cfg(not(feature = "ort"))]
            {
                warn!(
                    kind = %cfg.model.kind,
                    "ort feature not compiled in; using mock for yoloe kind"
                );
                Ok(Arc::new(MockDetector::new()))
            }
        }
        // M3.1 — yoloe_visual (visual-prompt detector). Needs both the
        // `ort` feature AND a VisualPromptStore plumbed through
        // BuildContext; mock fallback when either is missing so the
        // engine still boots without tripping the pool/router on a dev
        // box that has no embeddings table.
        "yoloe_visual" => {
            #[cfg(feature = "ort")]
            {
                if let (Some(store), Some(dim)) =
                    (_ctx.visual_prompt_store.clone(), _ctx.visual_embedding_dim)
                {
                    match crate::yoloe_visual::YoloeVisualDetector::from_config(cfg, dim, store) {
                        Ok(d) => {
                            crate::health::clear_degraded("yoloe_visual");
                            return Ok(Arc::new(d));
                        }
                        Err(e) => {
                            return Ok(crate::health::degraded_detector("yoloe_visual", e));
                        }
                    }
                }
                Ok(crate::health::degraded_detector(
                    "yoloe_visual",
                    "requires a VisualPromptStore + embedding_dim in BuildContext",
                ))
            }
            #[cfg(not(feature = "ort"))]
            {
                warn!(
                    kind = %cfg.model.kind,
                    "ort feature not compiled in; using mock for yoloe_visual kind"
                );
                Ok(Arc::new(MockDetector::new()))
            }
        }
        // M3.3 — yoloe_promptfree wraps an inner yoloe (or, when the
        // inner model fails to load, an UnavailableDetector) with a
        // top-k post-NMS truncation. The wrapper's name() is what the
        // router reports, so dispatch must produce it either way.
        "yoloe_promptfree" => {
            let inner: Arc<dyn Detector> = {
                #[cfg(feature = "ort")]
                {
                    crate::yoloe::build_detector_for_yoloe(cfg)
                        .unwrap_or_else(|e| crate::health::degraded_detector("yoloe_promptfree", e))
                }
                #[cfg(not(feature = "ort"))]
                {
                    Arc::new(MockDetector::new())
                }
            };
            Ok(Arc::new(YoloePromptFreeDetector::new(
                inner,
                cfg.model.top_k,
            )))
        }
        // M3.2 — same-camera detector ensemble. Each member is itself a
        // ModelConfig; build them by recursion via a derived
        // InferenceConfig that swaps in the member's model. Nested
        // ensembles are skipped with a warning rather than recursed
        // into — matches the "ensemble member skips nested ensemble"
        // acceptance test and the worker's identical guard.
        "ensemble" => {
            let mut members: Vec<Arc<dyn Detector>> = Vec::with_capacity(cfg.model.members.len());
            for member_cfg in &cfg.model.members {
                if member_cfg.kind == "ensemble" {
                    warn!(
                        "ensemble member with kind=\"ensemble\" skipped \
                         (no nested ensembles)"
                    );
                    continue;
                }
                let mut derived = cfg.clone();
                derived.model = member_cfg.clone();
                match build_detector_with_context(&derived, _ctx) {
                    Ok(det) => members.push(det),
                    Err(e) => warn!(
                        member_kind = %member_cfg.kind,
                        "ensemble member build failed; skipped: {e}"
                    ),
                }
            }
            Ok(Arc::new(crate::ensemble::EnsembleDetector::new(
                members,
                crate::ensemble::DEFAULT_ENSEMBLE_NMS_IOU,
                cfg.model.nms_spatial_bucket_size_px,
            )))
        }
        "mock" => Ok(Arc::new(MockDetector::new())),
        other => Ok(crate::health::degraded_detector(
            other,
            format!(
                "unknown inference.model.kind {other:?}; expected one of yolo, yolo_world, \
                 yoloe, yoloe_visual, yoloe_promptfree, classifier_ensemble, ensemble, mock"
            ),
        )),
    }
}
