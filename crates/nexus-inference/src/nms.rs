//! Class-aware non-maximum suppression with optional spatial bucketing.
//!
//! Promoted out of `yoloe.rs`, `yoloe_visual.rs`, `yolo_world.rs`, and
//! `ensemble.rs` in M_PERF_CROWD Phase C3 — those four sites previously
//! held byte-identical copies of [`iou`] and [`nms_per_label`] because
//! the original homes were `#[cfg(feature = "ort")]` gated and
//! `ensemble.rs` is not. This module is cfg-free on purpose so all four
//! callers can share it.
//!
//! The bucketed path is opt-in via `bucket_size_px`. Bit-identical to
//! the naive path when `bucket_size_px >= max_bbox_dim` (the
//! neighbourhood is guaranteed to contain every IoU > 0 candidate).

use nexus_types::{BBox, Detection};
use std::collections::HashMap;

/// IoU of two axis-aligned bboxes. Returns 0 for disjoint inputs or
/// when the union area is non-positive (degenerate bbox).
pub fn iou(a: &BBox, b: &BBox) -> f32 {
    let ix1 = a.x1.max(b.x1);
    let iy1 = a.y1.max(b.y1);
    let ix2 = a.x2.min(b.x2);
    let iy2 = a.y2.min(b.y2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let area_a = a.width() * a.height();
    let area_b = b.width() * b.height();
    let union = area_a + area_b - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Class-aware non-maximum suppression. Sorts descending by confidence,
/// then for each detection drops every later detection of the same
/// label whose IoU exceeds `iou_threshold`.
///
/// `bucket_size_px`:
///   - `None` (or `Some(0)`) — naive O(N²) path. Preserves the exact
///     behaviour of the pre-C3 in-module helpers.
///   - `Some(size)` — bucket detections by bbox centre into a `size`-px
///     grid; per-survivor suppression scan only visits the 3×3
///     neighbourhood. Output is bit-identical to the naive path when
///     `size >= max_bbox_dim_in_input`.
pub fn nms_per_label(
    mut dets: Vec<Detection>,
    iou_threshold: f32,
    bucket_size_px: Option<u32>,
) -> Vec<Detection> {
    if dets.len() <= 1 {
        return dets;
    }
    dets.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    match bucket_size_px {
        None | Some(0) => nms_naive(dets, iou_threshold),
        Some(size) => nms_bucketed(dets, iou_threshold, size),
    }
}

fn nms_naive(dets: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
    let mut keep = Vec::with_capacity(dets.len());
    let mut suppressed = vec![false; dets.len()];
    for i in 0..dets.len() {
        if suppressed[i] {
            continue;
        }
        keep.push(dets[i].clone());
        for (j, suppressed_j) in suppressed.iter_mut().enumerate().skip(i + 1) {
            if *suppressed_j {
                continue;
            }
            if dets[i].label != dets[j].label {
                continue;
            }
            if iou(&dets[i].bbox, &dets[j].bbox) >= iou_threshold {
                *suppressed_j = true;
            }
        }
    }
    keep
}

fn nms_bucketed(dets: Vec<Detection>, iou_threshold: f32, bucket_size_px: u32) -> Vec<Detection> {
    let size = bucket_size_px as f32;
    let n = dets.len();

    // Per-detection grid cell, keyed by bbox centre.
    let mut cells: Vec<(i32, i32)> = Vec::with_capacity(n);
    for d in &dets {
        let cx = (d.bbox.x1 + d.bbox.x2) * 0.5;
        let cy = (d.bbox.y1 + d.bbox.y2) * 0.5;
        cells.push(((cx / size).floor() as i32, (cy / size).floor() as i32));
    }

    // cell -> indices into `dets` falling in that cell. Same-cell
    // entries stay in input order (which is conf-descending after the
    // outer sort), so the per-cell vector iteration matches the naive
    // visit order.
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (idx, &cell) in cells.iter().enumerate() {
        grid.entry(cell).or_default().push(idx);
    }

    let mut suppressed = vec![false; n];
    let mut keep = Vec::with_capacity(n);
    for i in 0..n {
        if suppressed[i] {
            continue;
        }
        keep.push(dets[i].clone());
        let (gx, gy) = cells[i];
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(idxs) = grid.get(&(gx + dx, gy + dy)) else {
                    continue;
                };
                for &j in idxs {
                    // Only lower-conf, not-yet-kept, same-label candidates.
                    if j <= i || suppressed[j] {
                        continue;
                    }
                    if dets[i].label != dets[j].label {
                        continue;
                    }
                    if iou(&dets[i].bbox, &dets[j].bbox) >= iou_threshold {
                        suppressed[j] = true;
                    }
                }
            }
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(label: &str, conf: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: conf,
            bbox: BBox { x1, y1, x2, y2 },
            attributes: Default::default(),
        }
    }

    #[test]
    fn iou_disjoint_zero() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        let b = BBox {
            x1: 20.0,
            y1: 20.0,
            x2: 30.0,
            y2: 30.0,
        };
        assert!(iou(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn iou_identical_one() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        assert!((iou(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nms_empty_input_unchanged() {
        let out = nms_per_label(vec![], 0.5, None);
        assert!(out.is_empty());
    }

    #[test]
    fn nms_single_input_unchanged() {
        let dets = vec![det("person", 0.9, 0.0, 0.0, 10.0, 10.0)];
        let out = nms_per_label(dets.clone(), 0.5, None);
        assert_eq!(out.len(), 1);
        assert!((out[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn nms_naive_keeps_highest_scoring_overlap() {
        let dets = vec![
            det("person", 0.9, 0.0, 0.0, 10.0, 10.0),
            det("person", 0.5, 1.0, 1.0, 11.0, 11.0),
        ];
        let out = nms_per_label(dets, 0.5, None);
        assert_eq!(out.len(), 1);
        assert!((out[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn nms_naive_keeps_different_labels() {
        let dets = vec![
            det("person", 0.9, 0.0, 0.0, 10.0, 10.0),
            det("vehicle", 0.8, 1.0, 1.0, 11.0, 11.0),
        ];
        let out = nms_per_label(dets, 0.5, None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn nms_bucketed_matches_naive_on_overlapping_cluster() {
        // 10 overlapping person bboxes — all suppressed except the top.
        let dets: Vec<Detection> = (0..10)
            .map(|i| {
                let f = i as f32;
                det("person", 0.99 - f * 0.05, f, f, f + 50.0, f + 50.0)
            })
            .collect();
        let naive = nms_per_label(dets.clone(), 0.5, None);
        // bucket_size_px=64 >> max_bbox_dim (=50) → 3×3 neighbourhood
        // contains every overlap candidate.
        let bucketed = nms_per_label(dets, 0.5, Some(64));
        assert_eq!(naive.len(), bucketed.len());
        for (a, b) in naive.iter().zip(bucketed.iter()) {
            assert_eq!(a.label, b.label);
            assert!((a.confidence - b.confidence).abs() < 1e-6);
        }
    }

    #[test]
    fn nms_bucketed_keeps_spatially_separated_same_label() {
        // 9 person bboxes spaced 200 px apart on a 3×3 grid — none
        // overlap. Bucket=64 puts them in well-separated cells.
        let mut dets = Vec::new();
        for gx in 0..3 {
            for gy in 0..3 {
                let x = (gx as f32) * 200.0;
                let y = (gy as f32) * 200.0;
                dets.push(det("person", 0.5, x, y, x + 50.0, y + 50.0));
            }
        }
        let naive = nms_per_label(dets.clone(), 0.5, None);
        let bucketed = nms_per_label(dets, 0.5, Some(64));
        assert_eq!(naive.len(), 9);
        assert_eq!(bucketed.len(), 9);
    }

    #[test]
    fn nms_bucketed_matches_naive_random_mixed() {
        // Deterministic pseudo-random sweep — 200 mixed-label bboxes
        // in a 1280×720 frame. Property: naive == bucketed for any
        // bucket_size ≥ max_bbox_dim (here 80).
        let mut dets = Vec::with_capacity(200);
        // tiny LCG so the test is reproducible without rand dep
        let mut state: u32 = 0xDEAD_BEEF;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        for i in 0..200 {
            let r = next();
            let label = if r & 1 == 0 { "person" } else { "vehicle" };
            let x = (r >> 1) as f32 % 1200.0;
            let y = next() as f32 % 640.0;
            let w = 20.0 + (next() % 60) as f32; // 20..80
            let h = 20.0 + (next() % 60) as f32;
            let conf = 0.1 + ((i as f32) * 0.003);
            dets.push(det(label, conf, x, y, x + w, y + h));
        }
        let naive = nms_per_label(dets.clone(), 0.45, None);
        let bucketed = nms_per_label(dets, 0.45, Some(96)); // > 80 max dim
        assert_eq!(naive.len(), bucketed.len());
        for (a, b) in naive.iter().zip(bucketed.iter()) {
            assert_eq!(a.label, b.label);
            assert!(
                (a.confidence - b.confidence).abs() < 1e-6,
                "naive vs bucketed diverged: {a:?} vs {b:?}"
            );
            assert!((a.bbox.x1 - b.bbox.x1).abs() < 1e-6);
            assert!((a.bbox.y1 - b.bbox.y1).abs() < 1e-6);
        }
    }

    #[test]
    fn nms_bucketed_size_zero_falls_back_to_naive() {
        let dets = vec![
            det("person", 0.9, 0.0, 0.0, 10.0, 10.0),
            det("person", 0.5, 1.0, 1.0, 11.0, 11.0),
        ];
        let out = nms_per_label(dets, 0.5, Some(0));
        assert_eq!(out.len(), 1);
        assert!((out[0].confidence - 0.9).abs() < 1e-6);
    }
}
