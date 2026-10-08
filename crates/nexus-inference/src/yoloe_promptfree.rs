//! M3.3 — Prompt-free auto-class detector.
//!
//! Composes any inner [`Detector`] (a real [`crate::yoloe::YoloeDetector`]
//! under `--features ort`, a [`crate::detectors::MockDetector`] otherwise)
//! and runs it in **prompt-free** mode:
//!
//!   * The per-camera prompt subset is never applied. The inner
//!     detector's `detect()` is always called with an empty
//!     `prompts` slice, which `YoloeDetector` treats as "report every
//!     class baked into the ONNX vocabulary".
//!   * `push_camera_config` strips `update.prompts` before forwarding
//!     so an operator that accidentally re-fills the prompt field on
//!     a prompt-free camera doesn't narrow the active vocab back to a
//!     subset.
//!   * After the inner detector returns, results are sorted by
//!     confidence (desc) and truncated to `top_k` if configured.
//!     `top_k = None` keeps everything; `Some(k)` caps the per-frame
//!     output at the K most-confident objects.
//!
//! ## Rule editor surface
//! `Detection.label` carries the inner detector's model-supplied
//! label exactly as it would in `kind = "yoloe"`. CEL rules can
//! already filter / rank by label:
//!
//! ```cel
//! // Any vehicle-ish label, by regex
//! object.label.matches("(?i)car|truck|bus|van|forklift")
//! // Confidence-gated alert on labels that aren't background-y
//! object.confidence > 0.6 && !object.label.matches("person|tree|cone")
//! ```
//!
//! cel-interpreter's standard string functions (`matches`,
//! `contains`, `startsWith`, `endsWith`) cover the "alert on labels
//! matching regex" requirement from the M3 design doc — no new CEL
//! ops needed. The deferred-to-future "alert when any new label
//! first appears" behaviour is a stateful first-seen-set tracked at
//! the rule engine level and is **not** part of M3.3.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_config::CameraConfigUpdate;
use nexus_types::{Detection, Frame};
use tracing::debug;

use crate::detectors::{Detector, InferenceError};

pub struct YoloePromptFreeDetector {
    inner: Arc<dyn Detector>,
    top_k: Option<usize>,
}

impl YoloePromptFreeDetector {
    pub fn new(inner: Arc<dyn Detector>, top_k: Option<usize>) -> Self {
        Self { inner, top_k }
    }

    pub fn top_k(&self) -> Option<usize> {
        self.top_k
    }
}

#[async_trait]
impl Detector for YoloePromptFreeDetector {
    async fn detect(
        &self,
        frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        // Always run the inner detector against the full baked vocab.
        // We deliberately ignore the caller's `prompts` slice — the
        // supervisor never sends one for a prompt-free camera, but a
        // misconfigured ensemble parent could.
        let mut dets = self.inner.detect(frame, &[]).await?;
        dets.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(k) = self.top_k {
            dets.truncate(k);
        }
        Ok(dets)
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        // Forward generation + visual_prompts + model, but strip
        // prompts so the inner can't be narrowed back to a vocab
        // subset.
        let mut u = update.clone();
        if !u.prompts.is_empty() {
            debug!(
                camera = update.camera_id,
                stripped = u.prompts.len(),
                "yoloe_promptfree: ignoring per-camera prompts (full-vocab mode)"
            );
            u.prompts.clear();
        }
        self.inner.push_camera_config(&u).await;
    }

    fn name(&self) -> &'static str {
        "yoloe_promptfree"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::{BBox, PixelFormat};

    /// Static detector that emits a scripted list every call. Lets us
    /// drive prompt-free post-processing deterministically without
    /// pulling MockDetector's single-box behaviour.
    struct StaticDetector {
        out: Vec<Detection>,
        last_prompts: parking_lot::Mutex<Option<Vec<String>>>,
    }

    #[async_trait]
    impl Detector for StaticDetector {
        async fn detect(
            &self,
            _frame: &Frame,
            prompts: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            *self.last_prompts.lock() = Some(prompts.to_vec());
            Ok(self.out.clone())
        }
        async fn push_camera_config(&self, _u: &CameraConfigUpdate) {}
        fn name(&self) -> &'static str {
            "static"
        }
    }

    fn det(label: &str, conf: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: conf,
            bbox: BBox {
                x1: 0.0,
                y1: 0.0,
                x2: 10.0,
                y2: 10.0,
            },
            attributes: Default::default(),
        }
    }

    fn frame() -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 1,
            captured_at: Utc::now(),
            captured_mono: std::time::Instant::now(),
            width: 16,
            height: 16,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 16 * 16 * 3]),
            trace_id: "promptfree-test".into(),
        }
    }

    #[tokio::test]
    async fn top_k_truncates_lowest_confidence() {
        let inner = Arc::new(StaticDetector {
            out: vec![det("a", 0.3), det("b", 0.9), det("c", 0.6), det("d", 0.1)],
            last_prompts: parking_lot::Mutex::new(None),
        });
        let det_ = YoloePromptFreeDetector::new(inner.clone(), Some(2));
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "b");
        assert_eq!(out[1].label, "c");
        assert_eq!(det_.name(), "yoloe_promptfree");
    }

    #[tokio::test]
    async fn top_k_none_keeps_everything_sorted() {
        let inner = Arc::new(StaticDetector {
            out: vec![det("a", 0.3), det("b", 0.9), det("c", 0.6)],
            last_prompts: parking_lot::Mutex::new(None),
        });
        let det_ = YoloePromptFreeDetector::new(inner, None);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 3);
        // Sorted desc by confidence.
        assert_eq!(out[0].label, "b");
        assert_eq!(out[1].label, "c");
        assert_eq!(out[2].label, "a");
    }

    #[tokio::test]
    async fn caller_prompts_are_dropped_at_the_boundary() {
        let inner = Arc::new(StaticDetector {
            out: vec![det("a", 0.5)],
            last_prompts: parking_lot::Mutex::new(None),
        });
        let det_ = YoloePromptFreeDetector::new(inner.clone(), None);
        let _ = det_
            .detect(&frame(), &["something".into(), "else".into()])
            .await
            .unwrap();
        let seen = inner.last_prompts.lock().clone();
        assert_eq!(seen, Some(Vec::<String>::new()));
    }

    #[tokio::test]
    async fn push_camera_config_strips_prompts() {
        // We can't observe the inner's stripped update directly
        // without a custom mock — wire one that records the prompts
        // it received.
        struct RecordingDetector {
            last: parking_lot::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl Detector for RecordingDetector {
            async fn detect(
                &self,
                _f: &Frame,
                _p: &[String],
            ) -> Result<Vec<Detection>, InferenceError> {
                Ok(vec![])
            }
            async fn push_camera_config(&self, u: &CameraConfigUpdate) {
                *self.last.lock() = u.prompts.clone();
            }
            fn name(&self) -> &'static str {
                "rec"
            }
        }
        let rec = Arc::new(RecordingDetector {
            last: parking_lot::Mutex::new(vec!["sentinel".into()]),
        });
        let det_ = YoloePromptFreeDetector::new(rec.clone(), Some(5));
        let update = CameraConfigUpdate {
            camera_id: 7,
            generation: 1,
            prompts: vec!["accidentally".into(), "set".into()],
            visual_prompts: vec![],
            model: nexus_config::ModelConfig {
                kind: "yoloe_promptfree".into(),
                ..Default::default()
            },
        };
        det_.push_camera_config(&update).await;
        let seen = rec.last.lock().clone();
        assert!(seen.is_empty(), "prompts must be stripped, got {seen:?}");
    }
}
