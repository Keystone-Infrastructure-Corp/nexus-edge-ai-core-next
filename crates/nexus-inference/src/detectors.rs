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

impl MockDetector {
    pub fn new() -> Self {
        Self { counter: parking_lot::Mutex::new(0) }
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
        debug!(camera = update.camera_id, "open-vocab cfg push (gen={})", update.generation);
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
        Ok(Self { fallback: Arc::new(MockDetector::new()) })
    }
}

#[async_trait]
impl Detector for ClassifierEnsembleDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        // Treat prompts as the enabled-class list.
        let mut out = self.fallback.detect(frame, prompts).await?;
        if !prompts.is_empty() {
            out.retain(|d| prompts.iter().any(|p| p.eq_ignore_ascii_case(&d.label)));
        }
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "classifier_ensemble"
    }
}
