//! G1 (M_TILE_REINFER) Phase A3 — tile-executor.
//!
//! Sequential async loop that runs `Detector::detect_crop` (A1) on
//! a slice of `TileRoi`s (A2's `pick_tiles` output) and returns
//! detections in parent-frame coordinates, ready to merge with the
//! stage-1 detection set.
//!
//! Why sequential, not parallel: the per-camera `DetectorPool`
//! upstream already serialises detector access via a single ORT
//! session; spawning N parallel tile futures here would just queue
//! at the session mutex, adding overhead without buying
//! parallelism. B4-style cross-frame batching is a later
//! optimisation (and even then, batching is the detector's
//! concern, not the executor's).
//!
//! No supervisor wiring yet — B2 will call into this function;
//! today nothing does. See `docs/edge-core/M_TILE_REINFER.md` in
//! the cloud repo for the wedge-plan rationale.

use nexus_inference::{detectors::InferenceError, Detector};
use nexus_types::{Detection, Frame};
use thiserror::Error;

use crate::tile::{crop_to_tile_rgb, map_tile_dets_to_frame, TileError, TileRoi};

#[derive(Debug, Error)]
pub enum TileExecError {
    /// Crop step failed (OOB ROI, unsupported pixel format, …).
    /// The caller (supervisor wiring, B2) decides whether to fall
    /// through to stage-1-only or skip the frame.
    #[error("tile crop failed: {0}")]
    Crop(#[from] TileError),
    /// Detector returned an error on a tile. Same fallback choice
    /// for the caller as `Crop`.
    #[error("tile inference failed: {0}")]
    Inference(#[from] InferenceError),
}

/// Run stage-2 inference on every tile in `tiles`, returning the
/// union of detections in *parent-frame* coordinates.
///
/// Semantics:
/// - Empty `tiles` → `Ok(vec![])`. Cheap no-op so the supervisor
///   can call this unconditionally and let `pick_tiles` decide
///   whether to engage the cascade.
/// - First failing tile aborts the whole call. The caller chooses
///   the fallback policy (fail-soft = use stage-1 only;
///   fail-loud = bubble up). The crowded-frame cascade is a
///   compute-optimality knob, not a correctness one.
/// - Tile order matches input order, but the returned detection
///   list is the *concatenation* of per-tile outputs — duplicate
///   bboxes across overlapping tiles are NOT deduped here.
///   Cross-tile NMS is the caller's job (B2 will route the
///   merged stage-1 + stage-2 set through the existing NMS +
///   B1-wrapper chain).
pub async fn run_tile_inference(
    detector: &(dyn Detector + Send + Sync),
    parent: &Frame,
    tiles: &[TileRoi],
    prompts: &[String],
) -> Result<Vec<Detection>, TileExecError> {
    let mut out: Vec<Detection> = Vec::new();
    for tile in tiles {
        let crop = crop_to_tile_rgb(parent, *tile)?;
        let dets = detector.detect_crop(&crop, prompts).await?;
        let mapped = map_tile_dets_to_frame(&dets, *tile);
        out.extend(mapped);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TileGridConfig;
    use nexus_inference::detectors::MockDetector;
    use nexus_types::{BBox, PixelFormat};
    use std::sync::Arc;

    fn parent(w: u32, h: u32) -> Frame {
        Frame {
            camera_id: 7,
            frame_id: 1,
            captured_at: chrono::Utc::now(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; (w * h * 3) as usize]),
            trace_id: "tile-exec-test".into(),
        }
    }

    #[tokio::test]
    async fn empty_tiles_returns_empty() {
        let det = MockDetector::new();
        let p = parent(960, 540);
        let out = run_tile_inference(&det, &p, &[], &[]).await.expect("ok");
        assert!(out.is_empty(), "empty tiles must produce empty output");
    }

    #[tokio::test]
    async fn single_tile_returns_parent_frame_coords() {
        // MockDetector emits one detection sized to `frame.width ×
        // frame.height` centred horizontally. With a bottom-right
        // tile, the mapped detection must land inside that tile's
        // bounds — proving the crop happened AND the coordinate
        // mapping happened.
        let det = MockDetector::new();
        let p = parent(960, 540);
        let tile = TileRoi {
            x: 480,
            y: 270,
            w: 480,
            h: 270,
        };
        let out = run_tile_inference(&det, &p, &[tile], &[])
            .await
            .expect("ok");
        assert_eq!(out.len(), 1, "mock emits exactly one detection per tile");
        let bbox = &out[0].bbox;
        // Detection must be inside the bottom-right tile (origin
        // 480, 270; extent 480, 270).
        assert!(bbox.x1 >= 480.0, "x1 {} below tile origin", bbox.x1);
        assert!(bbox.y1 >= 270.0, "y1 {} below tile origin", bbox.y1);
        assert!(bbox.x2 <= 960.0, "x2 {} past tile right", bbox.x2);
        assert!(bbox.y2 <= 540.0, "y2 {} past tile bottom", bbox.y2);
    }

    #[tokio::test]
    async fn multiple_tiles_merge_into_concatenated_set() {
        let det = MockDetector::new();
        let p = parent(960, 540);
        let tiles = vec![
            TileRoi {
                x: 0,
                y: 0,
                w: 480,
                h: 270,
            },
            TileRoi {
                x: 480,
                y: 0,
                w: 480,
                h: 270,
            },
            TileRoi {
                x: 0,
                y: 270,
                w: 480,
                h: 270,
            },
        ];
        let out = run_tile_inference(&det, &p, &tiles, &[]).await.expect("ok");
        // 3 tiles × 1 mock-detection each = 3 outputs, no dedup.
        assert_eq!(out.len(), 3);
        // Each output's centroid should fall inside its tile's bounds.
        for (i, tile) in tiles.iter().enumerate() {
            let (cx, cy) = out[i].bbox.center();
            assert!(
                cx >= tile.x as f32 && cx <= (tile.x + tile.w) as f32,
                "out[{i}] cx {cx} outside tile x range"
            );
            assert!(
                cy >= tile.y as f32 && cy <= (tile.y + tile.h) as f32,
                "out[{i}] cy {cy} outside tile y range"
            );
        }
    }

    #[tokio::test]
    async fn oob_tile_returns_crop_error() {
        let det = MockDetector::new();
        let p = parent(100, 100);
        let bad = TileRoi {
            x: 80,
            y: 80,
            w: 50,
            h: 50,
        };
        let err = run_tile_inference(&det, &p, &[bad], &[])
            .await
            .expect_err("OOB tile should fail");
        assert!(
            matches!(err, TileExecError::Crop(TileError::RoiOutOfBounds { .. })),
            "expected Crop(RoiOutOfBounds), got {err:?}"
        );
    }

    #[tokio::test]
    async fn integration_with_pick_tiles() {
        use crate::tile::pick_tiles;
        // Fake stage-1 set with two dense clusters.
        let stage1 = vec![
            // Top-left cluster (3 dets).
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 50.0,
                    y1: 50.0,
                    x2: 80.0,
                    y2: 80.0,
                },
                attributes: Default::default(),
            },
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 100.0,
                    y1: 100.0,
                    x2: 130.0,
                    y2: 130.0,
                },
                attributes: Default::default(),
            },
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 150.0,
                    y1: 150.0,
                    x2: 180.0,
                    y2: 180.0,
                },
                attributes: Default::default(),
            },
            // Bottom-right cluster (1 det).
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 700.0,
                    y1: 400.0,
                    x2: 730.0,
                    y2: 430.0,
                },
                attributes: Default::default(),
            },
        ];
        let tiles = pick_tiles(&stage1, 960, 540, TileGridConfig::G2x2, 2);
        assert_eq!(tiles.len(), 2);

        let det = MockDetector::new();
        let p = parent(960, 540);
        let out = run_tile_inference(&det, &p, &tiles, &[]).await.expect("ok");
        // 2 tiles × 1 mock-detection each = 2 outputs.
        assert_eq!(out.len(), 2);
        // First tile is top-left (highest density); its detection
        // should land in the top-left quadrant.
        let (cx0, cy0) = out[0].bbox.center();
        assert!(
            cx0 < 480.0 && cy0 < 270.0,
            "first tile detection should be top-left"
        );
    }
}
