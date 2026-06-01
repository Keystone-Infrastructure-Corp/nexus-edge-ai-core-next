//! M3.2 — same-camera detector ensemble.
//!
//! An [`EnsembleDetector`] holds N inner detectors and fans every frame
//! out to all of them in parallel, then merges results with class-aware
//! NMS. The ensemble is the production answer to "I want yolo_world for
//! categorical PPE detection AND yoloe-visual for a specific employee
//! uniform on the same camera, with one CEL rule that can talk about
//! either label". M3.1 left the door open by saying mixed-mode-on-one-
//! camera is M3.2 — this is M3.2.
//!
//! ## Hot-path contract
//! * `detect()` runs every member concurrently with
//!   [`futures::future::join_all`]; total wall time is `max(member_i)`
//!   plus the merge/NMS pass. Members are expected to be CPU-bound
//!   (ORT sessions) — `block_in_place` inside each member keeps the
//!   tokio runtime responsive even when several yolo-family sessions
//!   are pegged at once.
//! * Member failures are **fail-soft per-member**: a single member
//!   returning `InferenceError` is logged and dropped, the rest of the
//!   ensemble still emits its detections. This matches the engine's
//!   "never starve the pipeline on one bad detector" stance — same
//!   shape as `DetectorPool::fail_soft`.
//! * `push_camera_config` fans out to every member sequentially. Order
//!   is the config-declared order; a member's failure is its own
//!   problem (the trait method returns `()`).
//!
//! ## NMS choice
//! Class-aware (per-`label`) NMS, mirroring `yoloe::nms_per_class`. Two
//! members producing detections with the same label and overlapping
//! boxes are deduplicated; detections with different labels (the
//! common ensemble case: "hardhat" from yolo_world + "amazon_van" from
//! yoloe_visual) survive together. NMS IoU defaults to `0.5` per
//! tradition — overridable on construction for ensembles whose
//! members favour different IoU regimes.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use nexus_config::CameraConfigUpdate;
use nexus_types::{Detection, Frame};
use tracing::warn;

use crate::detectors::{Detector, InferenceError};

/// Default IoU threshold for the merge-time NMS. Matches the closed-
/// vocab default used in `yolo::YoloOrtDetector` so an ensemble of one
/// closed-vocab member behaves identically to the bare detector.
pub const DEFAULT_ENSEMBLE_NMS_IOU: f32 = 0.5;

/// One ordered list of detectors fanned out per frame.
pub struct EnsembleDetector {
    members: Vec<Arc<dyn Detector>>,
    nms_iou: f32,
    /// M_PERF_CROWD Phase C3 — optional spatial bucket size (pixels)
    /// for the merge-time NMS pass. `None` keeps the pre-C3 naive O(N²)
    /// path bit-identical.
    nms_spatial_bucket_size_px: Option<u32>,
}

impl EnsembleDetector {
    /// Build with explicit members + NMS IoU + optional spatial bucket
    /// size. Member order is preserved only for diagnostics — the
    /// merged output is class-aware NMS'd and order-invariant.
    pub fn new(
        members: Vec<Arc<dyn Detector>>,
        nms_iou: f32,
        nms_spatial_bucket_size_px: Option<u32>,
    ) -> Self {
        Self {
            members,
            nms_iou,
            nms_spatial_bucket_size_px,
        }
    }

    /// Number of inner detectors. Useful for telemetry and tests.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the ensemble has no members — `detect()` short-circuits
    /// to `Ok(vec![])` in that case (same shape as the empty-prompt
    /// fail-soft contract on yolo_world / yoloe_visual).
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

#[async_trait]
impl Detector for EnsembleDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        if self.members.is_empty() {
            return Ok(Vec::new());
        }
        // join_all borrows from frame/prompts; the outer `.await`
        // pins those borrows in place for the duration of the fan-out.
        let futs = self.members.iter().map(|m| m.detect(frame, prompts));
        let results = join_all(futs).await;

        let mut merged: Vec<Detection> = Vec::new();
        for (idx, r) in results.into_iter().enumerate() {
            match r {
                Ok(dets) => merged.extend(dets),
                Err(e) => {
                    let name = self.members.get(idx).map(|m| m.name()).unwrap_or("unknown");
                    warn!(
                        member_index = idx,
                        member = name,
                        error = %e,
                        "ensemble: member detector failed; continuing with others"
                    );
                }
            }
        }
        Ok(crate::nms::nms_per_label(
            merged,
            self.nms_iou,
            self.nms_spatial_bucket_size_px,
        ))
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        for m in &self.members {
            m.push_camera_config(update).await;
        }
    }

    fn name(&self) -> &'static str {
        "ensemble"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::{BBox, PixelFormat};

    /// Test-only detector that emits a fixed list of detections every
    /// call. Lets us script ensemble inputs deterministically without
    /// pulling MockDetector's hard-coded one-box behaviour.
    struct StaticDetector {
        out: Vec<Detection>,
        label: &'static str,
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
            self.label
        }
    }

    /// A member that always fails — verifies the ensemble fails soft
    /// rather than propagating one bad member to the pipeline.
    struct FailingDetector;
    #[async_trait]
    impl Detector for FailingDetector {
        async fn detect(
            &self,
            _frame: &Frame,
            _prompts: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            Err(InferenceError::Failed("simulated".into()))
        }
        fn name(&self) -> &'static str {
            "failing"
        }
    }

    fn frame() -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 1,
            captured_at: Utc::now(),
            width: 640,
            height: 480,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; (640 * 480 * 3) as usize]),
            trace_id: "ensemble-test".into(),
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

    #[tokio::test]
    async fn empty_ensemble_returns_empty() {
        let ens = EnsembleDetector::new(vec![], DEFAULT_ENSEMBLE_NMS_IOU, None);
        let out = ens.detect(&frame(), &[]).await.expect("ok");
        assert!(out.is_empty());
        assert!(ens.is_empty());
        assert_eq!(ens.name(), "ensemble");
    }

    #[tokio::test]
    async fn ensemble_merges_distinct_labels() {
        let a = StaticDetector {
            out: vec![det("hardhat", 0.9, 10.0, 10.0, 50.0, 50.0)],
            label: "a",
        };
        let b = StaticDetector {
            out: vec![det("amazon_van", 0.85, 100.0, 100.0, 300.0, 300.0)],
            label: "b",
        };
        let ens = EnsembleDetector::new(
            vec![Arc::new(a), Arc::new(b)],
            DEFAULT_ENSEMBLE_NMS_IOU,
            None,
        );
        let out = ens.detect(&frame(), &[]).await.expect("ok");
        // Different labels, no overlap suppression — both survive.
        assert_eq!(out.len(), 2, "got {out:?}");
        let labels: Vec<&str> = out.iter().map(|d| d.label.as_str()).collect();
        assert!(labels.contains(&"hardhat"));
        assert!(labels.contains(&"amazon_van"));
    }

    #[tokio::test]
    async fn ensemble_nms_same_label_overlap() {
        // Two members both emit a "person" box at nearly the same spot.
        // Class-aware NMS should drop the lower-confidence one.
        let a = StaticDetector {
            out: vec![det("person", 0.91, 100.0, 100.0, 200.0, 200.0)],
            label: "a",
        };
        let b = StaticDetector {
            out: vec![det("person", 0.62, 105.0, 105.0, 205.0, 205.0)],
            label: "b",
        };
        let ens = EnsembleDetector::new(
            vec![Arc::new(a), Arc::new(b)],
            DEFAULT_ENSEMBLE_NMS_IOU,
            None,
        );
        let out = ens.detect(&frame(), &[]).await.expect("ok");
        assert_eq!(
            out.len(),
            1,
            "duplicate person box must be suppressed: {out:?}"
        );
        assert!((out[0].confidence - 0.91).abs() < 1e-6);
    }

    #[tokio::test]
    async fn ensemble_member_failure_is_soft() {
        let a = StaticDetector {
            out: vec![det("hardhat", 0.9, 10.0, 10.0, 50.0, 50.0)],
            label: "a",
        };
        let ens = EnsembleDetector::new(
            vec![Arc::new(a), Arc::new(FailingDetector)],
            DEFAULT_ENSEMBLE_NMS_IOU,
            None,
        );
        let out = ens
            .detect(&frame(), &[])
            .await
            .expect("ensemble keeps going");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "hardhat");
    }
}
