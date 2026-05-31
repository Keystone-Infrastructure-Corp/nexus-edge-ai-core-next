//! M_PERF_CROWD Phase B1 — universal per-frame detection caps.
//!
//! Two thin wrappers composed at detector-construction time on top of
//! whatever real [`Detector`] a kind arm built. Both are kind-agnostic
//! so the operator gets the same `top_k_per_frame` / `min_bbox_area_px`
//! semantics whether the underlying model is `yolo`, `yolo_world`,
//! `yoloe`, `yoloe_promptfree`, or anything else.
//!
//! - [`MinBBoxAreaDetector`]: drop boxes whose width × height (in pixels,
//!   on the supervisor analysis frame) is below a threshold. Cheapest
//!   way to suppress far-field noise on a wide-angle lens without
//!   touching detector hyper-params. Applied *before* top-k so the
//!   confidence ordering survives the area filter.
//! - [`TopKDetector`]: sort by confidence desc, truncate to k. Promotes
//!   the [`crate::YoloePromptFreeDetector`] post-NMS logic to a
//!   universal wrapper. Idempotent if the inner already capped at ≤k.
//!
//! Zone-scoped overrides for `min_bbox_area_px` live in the
//! supervisor / nexus-tracker layer (see
//! `crates/nexus-tracker/src/zone_filter.rs::filter_zone_min_area`) —
//! per-zone overrides operate on **tracked** objects, this wrapper on
//! raw **detections**.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_config::CameraConfigUpdate;
use nexus_types::{Detection, Frame};

use crate::detectors::{Detector, InferenceError};

pub struct MinBBoxAreaDetector {
    inner: Arc<dyn Detector>,
    min_area_px: u32,
}

impl MinBBoxAreaDetector {
    pub fn new(inner: Arc<dyn Detector>, min_area_px: u32) -> Self {
        Self { inner, min_area_px }
    }

    pub fn min_area_px(&self) -> u32 {
        self.min_area_px
    }
}

#[async_trait]
impl Detector for MinBBoxAreaDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let mut dets = self.inner.detect(frame, prompts).await?;
        let threshold = self.min_area_px as f32;
        dets.retain(|d| {
            let w = (d.bbox.x2 - d.bbox.x1).max(0.0);
            let h = (d.bbox.y2 - d.bbox.y1).max(0.0);
            w * h >= threshold
        });
        Ok(dets)
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        self.inner.push_camera_config(update).await;
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

pub struct TopKDetector {
    inner: Arc<dyn Detector>,
    k: usize,
}

impl TopKDetector {
    pub fn new(inner: Arc<dyn Detector>, k: usize) -> Self {
        Self { inner, k }
    }

    pub fn k(&self) -> usize {
        self.k
    }
}

#[async_trait]
impl Detector for TopKDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let mut dets = self.inner.detect(frame, prompts).await?;
        if dets.len() > self.k {
            dets.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            dets.truncate(self.k);
        }
        Ok(dets)
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        self.inner.push_camera_config(update).await;
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::{BBox, PixelFormat};

    struct StaticDetector {
        out: Vec<Detection>,
    }

    #[async_trait]
    impl Detector for StaticDetector {
        async fn detect(
            &self,
            _frame: &Frame,
            _prompts: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            Ok(self.out.clone())
        }
        fn name(&self) -> &'static str {
            "static"
        }
    }

    fn det(label: &str, conf: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: conf,
            bbox: BBox { x1, y1, x2, y2 },
            attributes: Default::default(),
        }
    }

    fn frame() -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 1,
            captured_at: Utc::now(),
            width: 640,
            height: 360,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 640 * 360 * 3]),
            trace_id: "caps-test".into(),
        }
    }

    #[tokio::test]
    async fn min_area_drops_small_boxes() {
        let inner = Arc::new(StaticDetector {
            out: vec![
                det("a", 0.9, 0.0, 0.0, 5.0, 5.0),   // area 25 → drop
                det("b", 0.8, 0.0, 0.0, 10.0, 10.0), // area 100 → keep
                det("c", 0.7, 0.0, 0.0, 50.0, 50.0), // area 2500 → keep
            ],
        });
        let det_ = MinBBoxAreaDetector::new(inner, 100);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "b");
        assert_eq!(out[1].label, "c");
        assert_eq!(det_.name(), "static");
    }

    #[tokio::test]
    async fn min_area_zero_keeps_everything() {
        let inner = Arc::new(StaticDetector {
            out: vec![det("a", 0.9, 0.0, 0.0, 1.0, 1.0)],
        });
        let det_ = MinBBoxAreaDetector::new(inner, 0);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn min_area_drops_degenerate_boxes() {
        // A zero-width or zero-height box (degenerate detector output)
        // produces area 0 and is dropped at any positive threshold.
        let inner = Arc::new(StaticDetector {
            out: vec![
                det("zero_w", 0.9, 10.0, 10.0, 10.0, 50.0),
                det("zero_h", 0.9, 10.0, 10.0, 50.0, 10.0),
                det("inverted", 0.9, 50.0, 50.0, 10.0, 10.0),
                det("real", 0.9, 0.0, 0.0, 20.0, 20.0),
            ],
        });
        let det_ = MinBBoxAreaDetector::new(inner, 1);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "real");
    }

    #[tokio::test]
    async fn top_k_truncates_by_confidence() {
        let inner = Arc::new(StaticDetector {
            out: vec![
                det("a", 0.3, 0.0, 0.0, 10.0, 10.0),
                det("b", 0.9, 0.0, 0.0, 10.0, 10.0),
                det("c", 0.6, 0.0, 0.0, 10.0, 10.0),
                det("d", 0.1, 0.0, 0.0, 10.0, 10.0),
            ],
        });
        let det_ = TopKDetector::new(inner, 2);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "b");
        assert_eq!(out[1].label, "c");
        assert_eq!(det_.name(), "static");
    }

    #[tokio::test]
    async fn top_k_below_count_keeps_order_untouched() {
        // When len ≤ k we skip the sort+truncate; caller's order is
        // preserved (this matters because some upstream detectors emit
        // a specific ordering downstream callers may rely on).
        let inner = Arc::new(StaticDetector {
            out: vec![
                det("a", 0.3, 0.0, 0.0, 10.0, 10.0),
                det("b", 0.9, 0.0, 0.0, 10.0, 10.0),
            ],
        });
        let det_ = TopKDetector::new(inner, 10);
        let out = det_.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "a");
        assert_eq!(out[1].label, "b");
    }

    #[tokio::test]
    async fn min_area_then_top_k_composes() {
        // Compose order matches build_detector_with_context: inner →
        // MinBBoxArea → TopK. The area filter runs first, then top-k
        // truncates by confidence on whatever survived.
        let inner = Arc::new(StaticDetector {
            out: vec![
                det("tiny_hi", 0.99, 0.0, 0.0, 4.0, 4.0), // area 16 → drop
                det("big_lo", 0.10, 0.0, 0.0, 50.0, 50.0),
                det("big_mid", 0.50, 0.0, 0.0, 50.0, 50.0),
                det("big_hi", 0.90, 0.0, 0.0, 50.0, 50.0),
            ],
        });
        let area: Arc<dyn Detector> = Arc::new(MinBBoxAreaDetector::new(inner, 100));
        let topk = TopKDetector::new(area, 2);
        let out = topk.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(out.len(), 2);
        // tiny_hi gone (area), big_hi + big_mid kept (top 2 by conf).
        assert_eq!(out[0].label, "big_hi");
        assert_eq!(out[1].label, "big_mid");
    }
}
