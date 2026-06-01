//! G1 (M_TILE_REINFER) Phase A2 — tile geometry + coord mapping.
//!
//! Pure functions used by the tile executor (Phase A3) and the
//! supervisor wiring (Phase B2) to:
//!   1. pick which sub-regions of the supervisor frame deserve a
//!      stage-2 inference pass (`pick_tiles`), based on the density
//!      of stage-1 detections;
//!   2. extract those sub-regions as fresh `Frame`s the detector
//!      can consume (`crop_to_tile_rgb`); and
//!   3. map crop-space detections returned by the detector back
//!      into supervisor-frame coordinates (`map_tile_dets_to_frame`).
//!
//! This module is `unsafe`-free, allocation-explicit, and has no
//! async runtime / IO dependencies — it's the math layer the rest
//! of G1 builds on. See `docs/edge-core/M_TILE_REINFER.md` in the
//! cloud repo for the wedge-plan rationale.

use std::sync::Arc;

use nexus_types::{BBox, Detection, Frame, PixelFormat};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TileError {
    /// `crop_to_tile_rgb` was handed a frame whose pixel format is
    /// not one the detector can consume. The supervisor's frame
    /// source converts YUV to RGB upstream of the detector chain,
    /// so this should not happen at runtime — it's defensive.
    #[error("unsupported pixel format for tile crop: {0:?}")]
    UnsupportedFormat(PixelFormat),
    /// The requested ROI extends past the parent frame bounds. The
    /// caller (`pick_tiles`) should never produce such an ROI; this
    /// is defensive against direct callers.
    #[error("ROI ({roi:?}) extends past parent frame {parent_w}x{parent_h}")]
    RoiOutOfBounds {
        roi: TileRoi,
        parent_w: u32,
        parent_h: u32,
    },
    /// Zero-area ROI — degenerate, would produce an empty crop.
    #[error("ROI has zero area: {0:?}")]
    EmptyRoi(TileRoi),
}

/// Pixel-space sub-region of a supervisor frame.
///
/// `x`/`y` are the top-left origin in parent-frame pixels; `w`/`h`
/// are the crop dimensions. `x + w <= parent_w` and `y + h <=
/// parent_h` are caller obligations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRoi {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl TileRoi {
    pub fn area(&self) -> u32 {
        self.w.saturating_mul(self.h)
    }

    /// True iff the centroid `(cx, cy)` (parent-frame pixels) falls
    /// inside the ROI. Right/bottom edges are exclusive — matches
    /// row-major grid partitioning so a centroid on a cell boundary
    /// is assigned to exactly one cell.
    fn contains_centroid(&self, cx: f32, cy: f32) -> bool {
        let x0 = self.x as f32;
        let y0 = self.y as f32;
        let x1 = (self.x + self.w) as f32;
        let y1 = (self.y + self.h) as f32;
        cx >= x0 && cx < x1 && cy >= y0 && cy < y1
    }
}

/// Tile grid preset.
///
/// **Square grids only** so each cell preserves the parent's 16:9
/// aspect ratio. A 3×2 grid on a 960×540 parent would yield 320×270
/// cells (~6:5), which the detector would have to re-letterbox —
/// wasting the resolution gain that is the whole point of G1. We
/// keep the preset list short and curated; operators don't need
/// arbitrary control over this knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileGridConfig {
    /// 2 rows × 2 cols = 4 tiles, each at half supervisor resolution.
    G2x2,
    /// 3 rows × 3 cols = 9 tiles, each at one-third supervisor resolution.
    /// Picks up smaller / further objects than `G2x2` at the cost of
    /// up to 9 stage-2 inferences per crowded frame.
    G3x3,
}

impl TileGridConfig {
    /// `(rows, cols)`.
    pub fn dims(&self) -> (u32, u32) {
        match self {
            TileGridConfig::G2x2 => (2, 2),
            TileGridConfig::G3x3 => (3, 3),
        }
    }
}

impl From<nexus_config::TileGridConfig> for TileGridConfig {
    fn from(cfg: nexus_config::TileGridConfig) -> Self {
        match cfg {
            nexus_config::TileGridConfig::G2x2 => TileGridConfig::G2x2,
            nexus_config::TileGridConfig::G3x3 => TileGridConfig::G3x3,
        }
    }
}

/// Partition `(parent_w, parent_h)` into the cells implied by
/// `grid`, returning them in row-major order. Right/bottom edge
/// cells absorb any remainder from integer division so the union
/// of cells covers the full parent frame exactly.
fn grid_cells(parent_w: u32, parent_h: u32, grid: TileGridConfig) -> Vec<TileRoi> {
    let (rows, cols) = grid.dims();
    let cell_w = parent_w / cols;
    let cell_h = parent_h / rows;
    let mut out = Vec::with_capacity((rows * cols) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let x = c * cell_w;
            let y = r * cell_h;
            // Rightmost / bottommost row absorbs the integer-division
            // remainder so coverage is exact (no orphaned strip).
            let w = if c == cols - 1 { parent_w - x } else { cell_w };
            let h = if r == rows - 1 { parent_h - y } else { cell_h };
            out.push(TileRoi { x, y, w, h });
        }
    }
    out
}

/// Pick the top-`max_tiles` grid cells by stage-1 detection density.
///
/// Cells with zero detections are dropped (a tile with nothing in
/// it is just wasted compute). Ties on density break by row-major
/// order so the returned set is deterministic.
///
/// Returns an empty vec when `stage1.is_empty()`, `max_tiles == 0`,
/// or the parent frame is degenerate.
pub fn pick_tiles(
    stage1: &[Detection],
    parent_w: u32,
    parent_h: u32,
    grid: TileGridConfig,
    max_tiles: u32,
) -> Vec<TileRoi> {
    if stage1.is_empty() || max_tiles == 0 || parent_w == 0 || parent_h == 0 {
        return Vec::new();
    }
    let cells = grid_cells(parent_w, parent_h, grid);
    // (count, raster_order, cell)
    let mut scored: Vec<(u32, usize, TileRoi)> = cells
        .into_iter()
        .enumerate()
        .map(|(idx, cell)| {
            let count = stage1
                .iter()
                .filter(|d| {
                    let (cx, cy) = d.bbox.center();
                    cell.contains_centroid(cx, cy)
                })
                .count() as u32;
            (count, idx, cell)
        })
        .filter(|(count, _, _)| *count > 0)
        .collect();
    // Sort: density DESC, then raster order ASC.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(max_tiles as usize);
    scored.into_iter().map(|(_, _, cell)| cell).collect()
}

/// Extract the ROI sub-region of `parent` as a fresh `Frame`.
///
/// The returned `Frame` carries the same `camera_id`, `frame_id`,
/// `captured_at`, `format`, and `trace_id` as the parent — those
/// are properties of the *capture moment*, not the spatial extent.
/// `width`/`height` are the crop dimensions, NOT the parent's.
///
/// Allocates a fresh `Vec<u8>` for the crop's pixel data because
/// the underlying buffer is row-strided and the detector requires
/// a contiguous tightly-packed buffer. The allocation is wrapped
/// in `Arc` to match the existing `Frame::data` contract.
///
/// Only `Rgb24` / `Bgr24` are supported — the YUV formats are
/// converted upstream of the detector chain by the frame source,
/// so a tile crop should never see them in production. Returning
/// an error here is defensive.
pub fn crop_to_tile_rgb(parent: &Frame, roi: TileRoi) -> Result<Frame, TileError> {
    if !matches!(parent.format, PixelFormat::Rgb24 | PixelFormat::Bgr24) {
        return Err(TileError::UnsupportedFormat(parent.format));
    }
    if roi.w == 0 || roi.h == 0 {
        return Err(TileError::EmptyRoi(roi));
    }
    if roi.x + roi.w > parent.width || roi.y + roi.h > parent.height {
        return Err(TileError::RoiOutOfBounds {
            roi,
            parent_w: parent.width,
            parent_h: parent.height,
        });
    }
    let bytes_per_px = 3usize; // both supported formats are 3-byte interleaved
    let parent_stride = parent.width as usize * bytes_per_px;
    let roi_stride = roi.w as usize * bytes_per_px;
    let roi_x_byte = roi.x as usize * bytes_per_px;
    let mut buf = Vec::with_capacity(roi_stride * roi.h as usize);
    for row in 0..roi.h as usize {
        let src_y = roi.y as usize + row;
        let start = src_y * parent_stride + roi_x_byte;
        let end = start + roi_stride;
        buf.extend_from_slice(&parent.data[start..end]);
    }
    Ok(Frame {
        camera_id: parent.camera_id,
        frame_id: parent.frame_id,
        captured_at: parent.captured_at,
        width: roi.w,
        height: roi.h,
        format: parent.format,
        data: Arc::new(buf),
        trace_id: parent.trace_id.clone(),
    })
}

/// Translate crop-space detection bboxes into parent-frame space.
///
/// The detector's contract (per `Detector::detect_crop`, A1) is
/// that returned bboxes are in *crop* coordinates. To merge them
/// with stage-1 detections we shift each bbox by the ROI origin.
/// No scaling is needed — the detector already un-letterboxed the
/// output to crop dimensions inside its `detect_crop` impl.
pub fn map_tile_dets_to_frame(tile_dets: &[Detection], roi: TileRoi) -> Vec<Detection> {
    let dx = roi.x as f32;
    let dy = roi.y as f32;
    tile_dets
        .iter()
        .map(|d| Detection {
            label: d.label.clone(),
            confidence: d.confidence,
            bbox: BBox {
                x1: d.bbox.x1 + dx,
                y1: d.bbox.y1 + dy,
                x2: d.bbox.x2 + dx,
                y2: d.bbox.y2 + dy,
            },
            attributes: d.attributes.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::PixelFormat;
    use serde_json::Map;

    // ---- helpers -----------------------------------------------------------

    fn det_at(cx: f32, cy: f32) -> Detection {
        Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - 5.0,
                y1: cy - 5.0,
                x2: cx + 5.0,
                y2: cy + 5.0,
            },
            attributes: Map::new(),
        }
    }

    fn solid_frame(w: u32, h: u32, fill: u8) -> Frame {
        let n = (w * h * 3) as usize;
        Frame {
            camera_id: 11,
            frame_id: 1,
            captured_at: Utc::now(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![fill; n]),
            trace_id: "tile-test".into(),
        }
    }

    // Paints a unique colour per quadrant of a 960×540 frame so the
    // crop test can verify it pulled the right bytes.
    fn quadrant_frame() -> Frame {
        let w = 960u32;
        let h = 540u32;
        let mut data = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let q = match (x < w / 2, y < h / 2) {
                    (true, true) => [10u8, 20, 30],    // top-left
                    (false, true) => [40, 50, 60],     // top-right
                    (true, false) => [70, 80, 90],     // bottom-left
                    (false, false) => [100, 110, 120], // bottom-right
                };
                let idx = ((y * w + x) * 3) as usize;
                data[idx..idx + 3].copy_from_slice(&q);
            }
        }
        Frame {
            camera_id: 11,
            frame_id: 1,
            captured_at: Utc::now(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(data),
            trace_id: "tile-test-quadrants".into(),
        }
    }

    // ---- pick_tiles --------------------------------------------------------

    #[test]
    fn pick_tiles_empty_input_returns_empty() {
        let out = pick_tiles(&[], 960, 540, TileGridConfig::G2x2, 3);
        assert!(out.is_empty());
    }

    #[test]
    fn pick_tiles_max_zero_returns_empty() {
        let dets = vec![det_at(100.0, 100.0)];
        let out = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn pick_tiles_picks_highest_density_cell_first() {
        // 5 dets clustered in top-left quadrant, 1 in top-right.
        let mut dets: Vec<Detection> = (0..5).map(|i| det_at(50.0 + i as f32, 50.0)).collect();
        dets.push(det_at(700.0, 50.0));
        let out = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 2);
        assert_eq!(out.len(), 2);
        // Top-left cell first (5 dets), then top-right (1 det).
        assert_eq!(
            out[0],
            TileRoi {
                x: 0,
                y: 0,
                w: 480,
                h: 270
            }
        );
        assert_eq!(
            out[1],
            TileRoi {
                x: 480,
                y: 0,
                w: 480,
                h: 270
            }
        );
    }

    #[test]
    fn pick_tiles_caps_at_max_tiles() {
        // One det in every cell of a 2x2 grid.
        let dets = vec![
            det_at(100.0, 100.0), // top-left
            det_at(700.0, 100.0), // top-right
            det_at(100.0, 400.0), // bottom-left
            det_at(700.0, 400.0), // bottom-right
        ];
        let out = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 3);
        assert_eq!(
            out.len(),
            3,
            "max_tiles=3 must cap to 3 even with 4 candidates"
        );
    }

    #[test]
    fn pick_tiles_skips_empty_cells() {
        // Only top-left has detections. Even with max_tiles=4, only 1 tile out.
        let dets = vec![det_at(100.0, 100.0)];
        let out = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].x, 0);
        assert_eq!(out[0].y, 0);
    }

    #[test]
    fn pick_tiles_ties_break_by_raster_order() {
        // One det per cell — all tied at density=1. Expect row-major order.
        let dets = vec![
            det_at(700.0, 400.0), // bottom-right (last in raster)
            det_at(100.0, 100.0), // top-left (first in raster)
            det_at(700.0, 100.0), // top-right (second)
            det_at(100.0, 400.0), // bottom-left (third)
        ];
        let out = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 4);
        let xs_ys: Vec<_> = out.iter().map(|r| (r.x, r.y)).collect();
        assert_eq!(
            xs_ys,
            vec![(0, 0), (480, 0), (0, 270), (480, 270)],
            "ties must break by row-major raster order"
        );
    }

    #[test]
    fn pick_tiles_g3x3_partitions_with_remainder_on_edges() {
        // 1280×720 → cell_w=426, cell_h=240. Right cells must absorb the
        // 2-pixel remainder so coverage is exact.
        let dets = vec![det_at(640.0, 360.0)]; // dead centre
        let out = pick_tiles(&dets, 1280, 720, TileGridConfig::G3x3, 9);
        assert_eq!(out.len(), 1);
        let centre = out[0];
        assert_eq!(centre.x, 426);
        assert_eq!(centre.y, 240);
        // Sum of widths across a row must equal parent_w (no orphan strip).
        let cells = grid_cells(1280, 720, TileGridConfig::G3x3);
        let row0: Vec<_> = cells.iter().take(3).collect();
        assert_eq!(row0[0].w + row0[1].w + row0[2].w, 1280);
        // Sum of heights down a column must equal parent_h.
        let col0: Vec<_> = cells.iter().step_by(3).take(3).collect();
        assert_eq!(col0[0].h + col0[1].h + col0[2].h, 720);
    }

    // ---- crop_to_tile_rgb --------------------------------------------------

    #[test]
    fn crop_extracts_correct_subregion() {
        let parent = quadrant_frame();
        // Crop a 100×100 region wholly inside the bottom-right quadrant.
        let roi = TileRoi {
            x: 600,
            y: 350,
            w: 100,
            h: 100,
        };
        let crop = crop_to_tile_rgb(&parent, roi).expect("crop");
        assert_eq!(crop.width, 100);
        assert_eq!(crop.height, 100);
        assert_eq!(crop.data.len(), 100 * 100 * 3);
        // Every pixel should be the bottom-right colour [100, 110, 120].
        for px in crop.data.chunks_exact(3) {
            assert_eq!(px, &[100, 110, 120], "wrong quadrant data");
        }
    }

    #[test]
    fn crop_preserves_metadata() {
        let parent = solid_frame(960, 540, 42);
        let roi = TileRoi {
            x: 10,
            y: 20,
            w: 100,
            h: 50,
        };
        let crop = crop_to_tile_rgb(&parent, roi).expect("crop");
        assert_eq!(crop.camera_id, parent.camera_id);
        assert_eq!(crop.frame_id, parent.frame_id);
        assert_eq!(crop.captured_at, parent.captured_at);
        assert_eq!(crop.format, parent.format);
        assert_eq!(crop.trace_id, parent.trace_id);
    }

    #[test]
    fn crop_rejects_yuv_format() {
        // The frame source converts YUV to RGB before the detector chain,
        // so this is a defensive guard.
        let mut parent = solid_frame(960, 540, 0);
        parent.format = PixelFormat::Nv12;
        let err = crop_to_tile_rgb(
            &parent,
            TileRoi {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
        )
        .unwrap_err();
        assert_eq!(err, TileError::UnsupportedFormat(PixelFormat::Nv12));
    }

    #[test]
    fn crop_rejects_oob_roi() {
        let parent = solid_frame(100, 100, 0);
        let err = crop_to_tile_rgb(
            &parent,
            TileRoi {
                x: 50,
                y: 50,
                w: 80,
                h: 80,
            },
        )
        .unwrap_err();
        assert!(matches!(err, TileError::RoiOutOfBounds { .. }));
    }

    #[test]
    fn crop_rejects_zero_area_roi() {
        let parent = solid_frame(100, 100, 0);
        let err = crop_to_tile_rgb(
            &parent,
            TileRoi {
                x: 10,
                y: 10,
                w: 0,
                h: 50,
            },
        )
        .unwrap_err();
        assert!(matches!(err, TileError::EmptyRoi(_)));
    }

    // ---- map_tile_dets_to_frame -------------------------------------------

    #[test]
    fn map_tile_dets_offsets_bboxes_by_roi_origin() {
        let roi = TileRoi {
            x: 100,
            y: 50,
            w: 480,
            h: 270,
        };
        let crop_space = vec![Detection {
            label: "car".into(),
            confidence: 0.7,
            bbox: BBox {
                x1: 10.0,
                y1: 20.0,
                x2: 60.0,
                y2: 80.0,
            },
            attributes: Map::new(),
        }];
        let mapped = map_tile_dets_to_frame(&crop_space, roi);
        assert_eq!(mapped.len(), 1);
        let m = &mapped[0];
        assert_eq!(m.label, "car");
        assert_eq!(m.confidence, 0.7);
        assert_eq!(m.bbox.x1, 110.0);
        assert_eq!(m.bbox.y1, 70.0);
        assert_eq!(m.bbox.x2, 160.0);
        assert_eq!(m.bbox.y2, 130.0);
    }

    #[test]
    fn map_tile_dets_handles_empty() {
        let out = map_tile_dets_to_frame(
            &[],
            TileRoi {
                x: 5,
                y: 5,
                w: 10,
                h: 10,
            },
        );
        assert!(out.is_empty());
    }

    // ---- roundtrip ---------------------------------------------------------

    #[test]
    fn roundtrip_pick_crop_map_preserves_object_in_parent_space() {
        // Stage-1 found a person at (700, 400) in the parent (bottom-right
        // quadrant). pick_tiles should return that cell; crop_to_tile_rgb
        // should extract it; a synthetic crop-space detection at the
        // centre of the crop should map back to the centre of that cell
        // in parent-frame space.
        let dets = vec![det_at(700.0, 400.0)];
        let tiles = pick_tiles(&dets, 960, 540, TileGridConfig::G2x2, 1);
        assert_eq!(tiles.len(), 1);
        let tile = tiles[0];
        assert_eq!(
            tile,
            TileRoi {
                x: 480,
                y: 270,
                w: 480,
                h: 270
            }
        );

        let parent = quadrant_frame();
        let crop = crop_to_tile_rgb(&parent, tile).expect("crop");
        assert_eq!(crop.width, 480);
        assert_eq!(crop.height, 270);

        // Pretend the stage-2 detector found something dead centre of
        // the crop (240, 135 in crop space). Map back.
        let crop_dets = vec![Detection {
            label: "person".into(),
            confidence: 0.95,
            bbox: BBox {
                x1: 235.0,
                y1: 130.0,
                x2: 245.0,
                y2: 140.0,
            },
            attributes: Map::new(),
        }];
        let parent_dets = map_tile_dets_to_frame(&crop_dets, tile);
        assert_eq!(parent_dets.len(), 1);
        let (cx, cy) = parent_dets[0].bbox.center();
        // Expected centre = tile origin + crop-centre = (480+240, 270+135) = (720, 405).
        assert!((cx - 720.0).abs() < 0.001);
        assert!((cy - 405.0).abs() < 0.001);
    }
}
