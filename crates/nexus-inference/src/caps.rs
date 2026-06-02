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

/// Drop detections whose bbox area (in supervisor-frame pixels) is below
/// `min_area_px`. Exposed as a pure function so the M_TILE_REINFER (G1)
/// cascade can re-apply the cap on the merged stage-1 + stage-2 vector
/// without rebuilding a wrapper detector. Idempotent on already-filtered
/// input. A `min_area_px` of zero is a no-op.
pub fn apply_min_bbox_area(detections: &mut Vec<Detection>, min_area_px: u32) {
    if min_area_px == 0 {
        return;
    }
    let threshold = min_area_px as f32;
    detections.retain(|d| {
        let w = (d.bbox.x2 - d.bbox.x1).max(0.0);
        let h = (d.bbox.y2 - d.bbox.y1).max(0.0);
        w * h >= threshold
    });
}

/// Sort detections by confidence desc and truncate to the K most-confident.
/// Exposed as a pure function so the G1 cascade can re-apply the cap on
/// the merged stage-1 + stage-2 vector to enforce `top_k` GLOBALLY across
/// both stages rather than per-stage. Idempotent on already-truncated
/// input.
pub fn apply_top_k(detections: &mut Vec<Detection>, k: usize) {
    if detections.len() > k {
        detections.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        detections.truncate(k);
    }
}

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
        apply_min_bbox_area(&mut dets, self.min_area_px);
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
        apply_top_k(&mut dets, self.k);
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

    // ---- Pure-helper parity tests ----
    //
    // These guard the M_TILE_REINFER (G1) Phase B2.1 contract: the
    // post-merge cascade re-application uses these helpers, so they
    // MUST produce bit-identical output to the long-shipped wrapper
    // detectors. If a wrapper test above changes semantics, the
    // matching parity test below must change in lockstep.
    fn fixture_inputs() -> Vec<Vec<Detection>> {
        vec![
            // Empty input.
            vec![],
            // All below area, all above conf — area path drops everything.
            vec![
                det("a", 0.9, 0.0, 0.0, 5.0, 5.0),
                det("b", 0.8, 0.0, 0.0, 5.0, 5.0),
            ],
            // Mixed sizes + confidences (worst-case for both paths).
            vec![
                det("a", 0.3, 0.0, 0.0, 10.0, 10.0),
                det("b", 0.9, 0.0, 0.0, 10.0, 10.0),
                det("c", 0.6, 0.0, 0.0, 4.0, 4.0),
                det("d", 0.1, 0.0, 0.0, 10.0, 10.0),
                det("e", 0.95, 0.0, 0.0, 4.0, 4.0),
            ],
            // Already-sorted by confidence desc.
            vec![
                det("a", 0.9, 0.0, 0.0, 10.0, 10.0),
                det("b", 0.5, 0.0, 0.0, 10.0, 10.0),
                det("c", 0.2, 0.0, 0.0, 10.0, 10.0),
            ],
        ]
    }

    #[tokio::test]
    async fn apply_min_bbox_area_matches_wrapper() {
        for input in fixture_inputs() {
            for thresh in [0u32, 1, 100, 10_000] {
                let inner = Arc::new(StaticDetector { out: input.clone() });
                let wrapper_out = MinBBoxAreaDetector::new(inner, thresh)
                    .detect(&frame(), &[])
                    .await
                    .expect("ok");
                let mut helper_out = input.clone();
                apply_min_bbox_area(&mut helper_out, thresh);
                assert_eq!(
                    helper_out.len(),
                    wrapper_out.len(),
                    "len mismatch for thresh={thresh}, input={input:?}"
                );
                for (h, w) in helper_out.iter().zip(wrapper_out.iter()) {
                    assert_eq!(h.label, w.label);
                    assert_eq!(h.confidence, w.confidence);
                }
            }
        }
    }

    #[tokio::test]
    async fn apply_top_k_matches_wrapper() {
        for input in fixture_inputs() {
            for k in [1usize, 2, 3, 10] {
                let inner = Arc::new(StaticDetector { out: input.clone() });
                let wrapper_out = TopKDetector::new(inner, k)
                    .detect(&frame(), &[])
                    .await
                    .expect("ok");
                let mut helper_out = input.clone();
                apply_top_k(&mut helper_out, k);
                assert_eq!(
                    helper_out.len(),
                    wrapper_out.len(),
                    "len mismatch for k={k}, input={input:?}"
                );
                for (h, w) in helper_out.iter().zip(wrapper_out.iter()) {
                    assert_eq!(h.label, w.label);
                    assert_eq!(h.confidence, w.confidence);
                }
            }
        }
    }

    #[test]
    fn apply_top_k_idempotent_on_already_capped() {
        // The G1 cascade re-applies apply_top_k on the merged vector;
        // if the inner wrapper already capped to ≤k, the second call
        // is a no-op (avoids the sort).
        let mut v = vec![
            det("a", 0.9, 0.0, 0.0, 10.0, 10.0),
            det("b", 0.7, 0.0, 0.0, 10.0, 10.0),
        ];
        let before = v.clone();
        apply_top_k(&mut v, 5);
        assert_eq!(v.len(), 2);
        // Order untouched because len ≤ k.
        for (a, b) in v.iter().zip(before.iter()) {
            assert_eq!(a.label, b.label);
        }
    }

    #[test]
    fn apply_top_k_global_across_cascade_stages() {
        // Models the G1 cascade contract: stage-1 returned ≤k and each
        // tile returned ≤k. Without post-merge re-application, the
        // tracker sees up to k × (1 + max_tiles); with the re-apply
        // it sees the globally top-k confidence-ranked subset.
        let stage1 = vec![
            det("s1_a", 0.30, 0.0, 0.0, 10.0, 10.0),
            det("s1_b", 0.40, 0.0, 0.0, 10.0, 10.0),
            det("s1_c", 0.20, 0.0, 0.0, 10.0, 10.0),
        ];
        let tile1 = vec![
            det("t1_a", 0.95, 0.0, 0.0, 10.0, 10.0),
            det("t1_b", 0.85, 0.0, 0.0, 10.0, 10.0),
        ];
        let tile2 = vec![
            det("t2_a", 0.75, 0.0, 0.0, 10.0, 10.0),
            det("t2_b", 0.65, 0.0, 0.0, 10.0, 10.0),
        ];
        let mut merged = stage1;
        merged.extend(tile1);
        merged.extend(tile2);
        assert_eq!(merged.len(), 7, "pre-cap merged length");
        apply_top_k(&mut merged, 3);
        assert_eq!(merged.len(), 3);
        // Globally top 3 by confidence: t1_a (0.95), t1_b (0.85), t2_a (0.75).
        assert_eq!(merged[0].label, "t1_a");
        assert_eq!(merged[1].label, "t1_b");
        assert_eq!(merged[2].label, "t2_a");
    }
}
