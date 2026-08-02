//! Detector trait + concrete model implementations.
//!
//! A `Detector` says *what* runs — the model, prompts, post-processing. The
//! `DetectorBackend` (next module) says *where* it runs — same process,
//! isolated thread, isolated process. Implementations of `Detector` are
//! pure (no global state, no async runtime requirements) so they can be
//! moved across thread / process boundaries cheaply.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_config::InferenceConfig;
use nexus_types::{BBox, Detection, Frame};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("model load: {0}")]
    ModelLoad(String),
    #[error("execution provider not available: {0}")]
    EpUnavailable(String),
    #[error("inference failed: {0}")]
    Failed(String),
    #[error("unsupported pixel format: {0:?}")]
    UnsupportedFormat(nexus_types::PixelFormat),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Orient a raw detector output tensor to a `[num_anchors, features]`
/// 2-D plane, transparently handling either export orientation.
///
/// YOLO-family "raw" heads (`nms=False`) export the detection tensor as
/// `[1, features, anchors]` — e.g. `[1, 4+C, N]` — or its transpose
/// `[1, anchors, features]`. `features` (`4 + num_classes`, tens) is
/// always far smaller than `anchors` (`N = Σ(W/s · H/s)`, thousands), so
/// we treat the LONGER axis as the anchor (row) axis.
///
/// This is shape-dynamic **by construction** — it never names an anchor
/// count. It holds for every native-16:9 rung (N = 3024 @ 512×288,
/// 9576 @ 1024×576, …) exactly as it did for the square N = 8400 @ 640².
/// A 2-D input is returned as-is; any other rank yields `None`.
#[cfg(feature = "ort")]
pub fn orient_pred_rows(view: ndarray::ArrayViewD<'_, f32>) -> Option<ndarray::Array2<f32>> {
    use ndarray::{s, Ix2};
    match view.shape().len() {
        3 => {
            let (dim1, dim2) = (view.shape()[1], view.shape()[2]);
            let plane = view.slice(s![0, .., ..]).to_owned();
            if dim1 >= dim2 {
                plane.into_dimensionality::<Ix2>().ok()
            } else {
                plane.reversed_axes().into_dimensionality::<Ix2>().ok()
            }
        }
        2 => view.to_owned().into_dimensionality::<Ix2>().ok(),
        _ => None,
    }
}

#[async_trait]
pub trait Detector: Send + Sync {
    /// Run detection on a single frame against an optional prompt list. The
    /// prompt list is meaningful for open-vocab models; ensemble detectors
    /// use it as a hint (which heads to enable).
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError>;

    /// Run detection on a pre-cropped frame — i.e. a sub-region of the
    /// supervisor frame produced by `nexus_pipeline::tile` for the G1
    /// crowded-scene tile re-inference path
    /// (`docs/edge-core/M_TILE_REINFER.md` in the cloud repo).
    ///
    /// Contract:
    /// - `crop` is a real `Frame` (same pixel format + camera_id +
    ///   trace_id as the parent supervisor frame; `width`/`height` are
    ///   the crop dimensions, NOT the parent dimensions).
    /// - Returned detection bboxes are in `crop` coordinate space. The
    ///   caller (the tile executor) maps them back into the parent
    ///   supervisor-frame coordinate system.
    /// - The default implementation delegates to `detect()`, which is
    ///   correct (a crop is a valid `Frame`) but does not save compute.
    ///   Per-detector overrides may skip full-frame letterboxing or
    ///   re-use a tile-sized session input to make the call cheaper.
    async fn detect_crop(
        &self,
        crop: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        self.detect(crop, prompts).await
    }

    /// Hot-update prompts / per-camera params. Default = no-op so detectors
    /// that don't care don't have to implement it.
    async fn push_camera_config(&self, _update: &nexus_config::CameraConfigUpdate) {}

    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// MockDetector — no GPU, no models. Deterministic for tests + dev boots.
// ---------------------------------------------------------------------------

pub struct MockDetector {
    counter: parking_lot::Mutex<u64>,
}

impl Default for MockDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDetector {
    pub fn new() -> Self {
        Self {
            counter: parking_lot::Mutex::new(0),
        }
    }
}

#[async_trait]
impl Detector for MockDetector {
    async fn detect(
        &self,
        frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let mut c = self.counter.lock();
        *c = c.wrapping_add(1);
        // Emit one stable detection per frame so trackers / rules see motion.
        let w = frame.width as f32;
        let h = frame.height as f32;
        let drift = (*c as f32 % 60.0) - 30.0;
        Ok(vec![Detection {
            label: "person".into(),
            confidence: 0.92,
            bbox: BBox {
                x1: (w * 0.4 + drift).max(0.0),
                y1: h * 0.4,
                x2: (w * 0.6 + drift).min(w),
                y2: h * 0.9,
            },
            attributes: Default::default(),
        }])
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

// ---------------------------------------------------------------------------
// UnavailableDetector — the production substitute for a detector whose
// model could not be loaded.
// ---------------------------------------------------------------------------

/// A detector that reports **zero** detections, forever.
///
/// This is what a failed model load degrades to in production. It exists
/// because the two obvious alternatives are both worse:
///
/// * **Substituting [`MockDetector`]** (the historical behaviour) makes
///   the engine fabricate a `person` box on every frame of every camera.
///   Downstream rules cannot tell a synthetic box from a real one, so a
///   single missing `.onnx` file becomes a fleet-wide flood of alerts
///   for people who were never there. Silently inventing evidence is the
///   worst possible failure mode for a security product.
/// * **Aborting engine startup** takes the whole box dark: no live view,
///   no recording, no cloud tunnel — and therefore no way for the
///   console to tell an operator *why* it went dark. It also trips a
///   systemd restart loop. That conflicts with the engine's fail-open
///   contract (`AGENTS.md` rule 5).
///
/// So the engine stays up, keeps recording and streaming, emits no
/// detections at all, and shouts about it: `ERROR` at the failure site
/// plus a [`crate::health`] entry that drives `status: "degraded"` on
/// the health endpoint and in the cloud console.
pub struct UnavailableDetector;

impl Default for UnavailableDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl UnavailableDetector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Detector for UnavailableDetector {
    async fn detect(
        &self,
        _frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        // Deliberately not an Err: the pipeline treats a detect() error
        // as a transient per-frame fault and retries/logs per frame,
        // which would produce one log line per frame per camera. The
        // condition is permanent for the life of the process and is
        // already reported once, loudly, via `health::record_degraded`.
        Ok(Vec::new())
    }

    fn name(&self) -> &'static str {
        "unavailable"
    }
}

// ---------------------------------------------------------------------------
// OpenVocabDetector — wraps an open-vocab ONNX model (e.g. YOLO-World).
//
// M0 ships the trait + a mock body. M1/M3 wires the real ORT session behind
// the same surface. The backend isolation layers don't change.
// ---------------------------------------------------------------------------

pub struct OpenVocabDetector {
    score_threshold: f32,
    fallback: Arc<MockDetector>,
}

impl OpenVocabDetector {
    pub fn new(cfg: &InferenceConfig) -> Result<Self, InferenceError> {
        debug!(
            input_w = cfg.model.input_width,
            input_h = cfg.model.input_height,
            "open-vocab detector initialised (M0 stub uses mock body)"
        );
        Ok(Self {
            score_threshold: cfg.model.score_threshold,
            fallback: Arc::new(MockDetector::new()),
        })
    }
}

#[async_trait]
impl Detector for OpenVocabDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let mut out = self.fallback.detect(frame, prompts).await?;
        out.retain(|d| d.confidence >= self.score_threshold);
        // Re-label using the first prompt so the test harness can see prompts flow through.
        if let Some(p) = prompts.first() {
            for d in out.iter_mut() {
                d.label = p.clone();
            }
        }
        Ok(out)
    }

    async fn push_camera_config(&self, update: &nexus_config::CameraConfigUpdate) {
        debug!(
            camera = update.camera_id,
            "open-vocab cfg push (gen={})", update.generation
        );
    }

    fn name(&self) -> &'static str {
        "open_vocab"
    }
}

// ---------------------------------------------------------------------------
// ClassifierEnsembleDetector — narrow specialists (PPE, vehicle, equipment).
//
// Co-exists with OpenVocabDetector; operator picks per-camera. M0 ships the
// trait + a mock body that re-labels detections with per-camera classes.
// ---------------------------------------------------------------------------

pub struct ClassifierEnsembleDetector {
    fallback: Arc<MockDetector>,
}

impl ClassifierEnsembleDetector {
    pub fn new(_cfg: &InferenceConfig) -> Result<Self, InferenceError> {
        Ok(Self {
            fallback: Arc::new(MockDetector::new()),
        })
    }
}

#[async_trait]
impl Detector for ClassifierEnsembleDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        // The per-camera `prompts` whitelist is enforced uniformly
        // for every detector kind by the pipeline supervisor (see
        // `label_matches_any_prompt`), so no retain is needed here.
        self.fallback.detect(frame, prompts).await
    }

    fn name(&self) -> &'static str {
        "classifier_ensemble"
    }
}

// ---------------------------------------------------------------------------
// Shared label/prompts matching used by the pipeline supervisor to enforce
// the per-camera `prompts` whitelist uniformly for every detector kind.
//
// Matching is case-insensitive and accepts either:
//   * an exact match against the full emitted label (`person`,
//     `vehicle.car`, `hardhat`), or
//   * a match against the last `.`-delimited segment of the label, so
//     operator-friendly bare nouns work for the closed-vocab YOLO/COCO
//     path that emits namespaced labels (`animal.dog`, `vehicle.truck`,
//     `carried.suitcase`). For open-vocab kinds (yolo_world, yoloe)
//     labels are unnamespaced, so the suffix branch is a no-op.
//
// An empty prompt list disables the filter entirely (the common case
// for cameras that haven't restricted their class set).
// ---------------------------------------------------------------------------

/// Returns `true` when `label` satisfies the per-camera `prompts`
/// whitelist. See module docs for matching rules. An empty `prompts`
/// slice is treated as "no filter" and always returns `true`.
pub fn label_matches_any_prompt(label: &str, prompts: &[String]) -> bool {
    if prompts.is_empty() {
        return true;
    }
    let tail = label.rsplit('.').next().unwrap_or(label);
    prompts
        .iter()
        .any(|p| p.eq_ignore_ascii_case(label) || p.eq_ignore_ascii_case(tail))
}

#[cfg(test)]
mod prompt_filter_tests {
    use super::*;

    #[test]
    fn empty_prompts_allows_everything() {
        assert!(label_matches_any_prompt("person", &[]));
        assert!(label_matches_any_prompt("vehicle.car", &[]));
        assert!(label_matches_any_prompt("", &[]));
    }

    #[test]
    fn exact_match_case_insensitive() {
        let prompts = vec!["Person".into(), "Hardhat".into()];
        assert!(label_matches_any_prompt("person", &prompts));
        assert!(label_matches_any_prompt("PERSON", &prompts));
        assert!(label_matches_any_prompt("hardhat", &prompts));
        assert!(!label_matches_any_prompt("vest", &prompts));
    }

    #[test]
    fn suffix_match_strips_namespace_for_coco_yolo() {
        // Operator writes the bare noun; closed-vocab YOLO emits the
        // namespaced label. Both directions should work.
        let prompts = vec!["dog".into(), "car".into(), "suitcase".into()];
        assert!(label_matches_any_prompt("animal.dog", &prompts));
        assert!(label_matches_any_prompt("vehicle.car", &prompts));
        assert!(label_matches_any_prompt("carried.suitcase", &prompts));
        assert!(!label_matches_any_prompt("animal.cat", &prompts));
        assert!(!label_matches_any_prompt("vehicle.truck", &prompts));
    }

    #[test]
    fn fully_qualified_prompts_still_match() {
        // Operators copying from the COCO taxonomy paste the
        // namespaced label verbatim; that must still work.
        let prompts = vec!["animal.dog".into(), "vehicle.car".into()];
        assert!(label_matches_any_prompt("animal.dog", &prompts));
        assert!(label_matches_any_prompt("vehicle.car", &prompts));
        assert!(!label_matches_any_prompt("person", &prompts));
    }

    #[test]
    fn unnamespaced_label_matches_unnamespaced_prompt() {
        // YOLO-World / YOLOe path: labels are bare nouns. Plain
        // exact match should win without the suffix branch firing.
        let prompts = vec!["excavator".into(), "crane".into()];
        assert!(label_matches_any_prompt("excavator", &prompts));
        assert!(label_matches_any_prompt("Crane", &prompts));
        assert!(!label_matches_any_prompt("forklift", &prompts));
    }
}
