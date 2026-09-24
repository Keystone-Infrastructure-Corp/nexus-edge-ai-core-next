//! Pure output decoding — no HailoRT, no FFI, no device.
//!
//! Lives outside the `linux + feature = "linked"` gate that covers `imp`,
//! because none of this touches HailoRT: it is byte-slice arithmetic over
//! tensors the runtime has already dequantised.
//!
//! Keeping it in `imp` meant no PR-gating job ever type-checked it: `imp` is
//! gated on `all(target_os = "linux", feature = "linked")`, and the only job
//! that enables `linked` is the release build (`release.yml`, via
//! `ep-hailo`), which runs at tag time. The decoder therefore shipped with
//! zero test coverage. Moving it here is what makes the tests below possible
//! and puts it in front of `cargo clippy`/`cargo test` on every target.

use crate::Detection;

/// Logistic activation, applied to the class branch of a `RawYolo26` HEF.
///
/// The shipped HEFs are compiled by `tools/models/gen_yolo26n_hailo.py`, which
/// cuts the graph at `cv3.X.2/Conv` — documented there as
/// `class pre-sigmoid, c=80`. The chip therefore emits raw logits and the
/// activation has to happen here. It never did: the decoder's own doc claimed
/// "post-sigmoid probabilities", nothing applied one, and
/// `hailo_yolo.rs`'s `clamp(0.0, 1.0)` quietly absorbed every logit above 1.0.
///
/// Measured on a Hailo-8 core before this fix: 103,232 detections carried
/// **8 distinct confidence values**, 40.4% of them exactly `1.000000`, the
/// minimum exactly `0.332578` — two steps of the class branch's `0.1662890`
/// dequantisation ladder. Rule thresholds could not discriminate at all.
#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputLayout {
    /// `HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS` (22). Single output buffer:
    ///   for each class C in 0..num_classes:
    ///     float32 bbox_count
    ///     bbox_count × { f32 y_min, x_min, y_max, x_max, score }
    /// Class id is implicit from buffer position.
    NmsByClass {
        num_classes: u32,
        max_bboxes_per_class: u32,
    },
    /// `HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE` (23). Single output buffer:
    ///   uint16 bbox_count
    ///   bbox_count × hailo_detection_t { f32 ymin, xmin, ymax, xmax, score; u16 class_id }
    NmsByScore { max_bboxes_total: u32 },
    /// Anchor-free yolo26-style raw heads with on-chip DFL fold. Each
    /// scale pairs a 4-channel box tensor (per-cell l,t,r,b in cell units)
    /// with an N-channel class tensor (per-cell raw logits; see `sigmoid`).
    /// All tensors are FLOAT32 NHWC. The caller (typically
    /// `decode_detections`) runs the anchor-free decode + class-agnostic
    /// NMS on CPU. This is what the public Hailo Model Zoo yolo26n.hef
    /// emits (no on-chip NMS). Naming tracks the model id in the manifest
    /// (`yolo26n`), not the underlying head architecture family.
    RawYolo26 {
        num_classes: u32,
        scales: Vec<RawYolo26Scale>,
    },
    /// Unsupported by the YOLO detector path — caller must handle raw output.
    Other,
}

/// Shape metadata for one output vstream. Plain data — the runtime fills it
/// in, the decoder reads it, neither needs HailoRT to describe it.
#[derive(Debug, Clone)]
pub struct OutputStreamInfo {
    pub name: String,
    pub h: u32,
    pub w: u32,
    pub c: u32,
    pub frame_size: usize,
}

/// One anchor-free yolo26 scale (box tensor + class tensor at a given stride).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawYolo26Scale {
    pub stride: u32,
    pub h: u32,
    pub w: u32,
    /// Index into `InferSession::output_buffers()` for the box (4-channel) tensor.
    pub box_idx: usize,
    /// Index into `InferSession::output_buffers()` for the class (num_classes-channel) tensor.
    pub score_idx: usize,
}

// ---------------------------------------------------------------------------

/// Decode the per-output buffers produced by `InferSession::infer_blocking`
/// into flat detections. For NMS-fused HEFs this is a near-zero-cost
/// repack of the on-chip output; for raw yolo26 HEFs it runs the
/// anchor-free decode + class-agnostic NMS on CPU.
///
/// `max_detections` caps the output (saves a malloc blow on pathological
/// frames and bounds the NMS pass cost).
pub fn decode_detections(
    buffers: &[Vec<u8>],
    layout: &OutputLayout,
    max_detections: usize,
) -> Vec<Detection> {
    match layout {
        OutputLayout::NmsByClass { num_classes, .. } => {
            if let Some(buf) = buffers.first() {
                decode_nms_by_class(buf, *num_classes as usize, max_detections)
            } else {
                Vec::new()
            }
        }
        OutputLayout::NmsByScore { .. } => buffers
            .first()
            .map(|b| decode_nms_by_score(b, max_detections))
            .unwrap_or_default(),
        OutputLayout::RawYolo26 {
            num_classes,
            scales,
        } => decode_yolo26_raw(buffers, *num_classes, scales, max_detections),
        OutputLayout::Other => Vec::new(),
    }
}

fn decode_nms_by_class(buf: &[u8], num_classes: usize, max_detections: usize) -> Vec<Detection> {
    // Layout: for each class C:
    //   f32 bbox_count
    //   bbox_count × {f32 y_min, x_min, y_max, x_max, score}  (20 bytes each)
    let mut out = Vec::new();
    let mut off = 0usize;
    for class_id in 0..num_classes {
        if off + 4 > buf.len() {
            break;
        }
        let count_f = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        off += 4;
        let count = count_f.round() as usize;
        for _ in 0..count {
            if out.len() >= max_detections {
                return out;
            }
            if off + 20 > buf.len() {
                return out;
            }
            let y_min = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let x_min =
                f32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
            let y_max =
                f32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
            let x_max =
                f32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);
            let score =
                f32::from_le_bytes([buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19]]);
            off += 20;
            out.push(Detection {
                y_min,
                x_min,
                y_max,
                x_max,
                score,
                class_id: class_id as u16,
            });
        }
    }
    out
}

fn decode_nms_by_score(buf: &[u8], max_detections: usize) -> Vec<Detection> {
    // Layout: u16 count; count × hailo_detection_t (packed):
    //   {f32 y_min, x_min, y_max, x_max, score; u16 class_id}  (22 bytes each, packed)
    if buf.len() < 2 {
        return Vec::new();
    }
    let count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let mut out = Vec::with_capacity(count.min(max_detections));
    let mut off = 2usize;
    for _ in 0..count {
        if out.len() >= max_detections {
            break;
        }
        if off + 22 > buf.len() {
            break;
        }
        let y_min = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let x_min = f32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let y_max = f32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let x_max =
            f32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);
        let score =
            f32::from_le_bytes([buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19]]);
        let class_id = u16::from_le_bytes([buf[off + 20], buf[off + 21]]);
        off += 22;
        out.push(Detection {
            y_min,
            x_min,
            y_max,
            x_max,
            score,
            class_id,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Raw yolo26 anchor-free decoder (multi-output HEFs)
// ---------------------------------------------------------------------------

/// Pair box (c=4) and class (c=num_classes) tensors by spatial shape.
/// Returns `(num_classes, scales)`.
///
/// The public Hailo Model Zoo yolo26n.hef has six outputs at three
/// strides: 80x80x{4,80} (stride 8), 40x40x{4,80} (stride 16),
/// 20x20x{4,80} (stride 32). We don't trust the HEF declaration order
/// — we pair purely by shape, so a regenerated HEF that swaps order
/// or adds a fourth scale still decodes correctly.
#[cfg_attr(not(all(target_os = "linux", feature = "linked")), allow(dead_code))]
pub(crate) fn build_yolo26_scales(
    output_infos: &[OutputStreamInfo],
    input_w: u32,
) -> Result<(u32, Vec<RawYolo26Scale>), String> {
    if output_infos.len() < 2 || !output_infos.len().is_multiple_of(2) {
        return Err(format!(
            "expected an even number of outputs (box + class pairs), got {}",
            output_infos.len()
        ));
    }
    // Find num_classes — the class (non-box) tensors all share the same c.
    let num_classes = output_infos
        .iter()
        .find(|i| i.c != 4 && i.c != 0)
        .map(|i| i.c)
        .ok_or_else(|| "no class tensor found (no output with c ≠ 4)".to_string())?;
    let mut scales: Vec<RawYolo26Scale> = Vec::new();
    let mut consumed = vec![false; output_infos.len()];
    for (i, info_i) in output_infos.iter().enumerate() {
        if consumed[i] || info_i.c != 4 {
            continue;
        }
        // Find the class tensor with matching (h, w).
        let mut pair_idx: Option<usize> = None;
        for (j, info_j) in output_infos.iter().enumerate() {
            if !consumed[j]
                && j != i
                && info_j.c == num_classes
                && info_j.h == info_i.h
                && info_j.w == info_i.w
            {
                pair_idx = Some(j);
                break;
            }
        }
        let j = pair_idx.ok_or_else(|| {
            format!(
                "box output {}x{}x4 has no matching class tensor",
                info_i.h, info_i.w
            )
        })?;
        consumed[i] = true;
        consumed[j] = true;
        if info_i.w == 0 {
            return Err("box output has zero width".into());
        }
        let stride = input_w / info_i.w;
        if stride == 0 || stride * info_i.w != input_w {
            return Err(format!(
                "box output {}x{} doesn't divide network input width {} cleanly",
                info_i.h, info_i.w, input_w
            ));
        }
        scales.push(RawYolo26Scale {
            stride,
            h: info_i.h,
            w: info_i.w,
            box_idx: i,
            score_idx: j,
        });
    }
    if scales.is_empty() {
        return Err("no valid box+class pairs found".into());
    }
    // Sort by stride so the decoder hits low→high (largest grid first).
    scales.sort_by_key(|s| s.stride);
    Ok((num_classes, scales))
}

/// Anchor-free yolo26 decoder. Inputs are FLOAT32 NHWC tensors.
/// Box tensor encodes (l, t, r, b) per cell in CELL units (already
/// DFL-projected by the chip). Class tensor encodes per-class **raw
/// logits** — the HEF is cut at `cv3.X.2/Conv`, before the activation —
/// so `sigmoid` is applied here to the winning class.
///
/// Coords are returned in [0, 1] normalized to the *network input*
/// space, matching the convention used by NMS-fused HEFs.
fn decode_yolo26_raw(
    buffers: &[Vec<u8>],
    num_classes: u32,
    scales: &[RawYolo26Scale],
    max_detections: usize,
) -> Vec<Detection> {
    // Score floor: anything below this is discarded before NMS to keep
    // the candidate set bounded. yolo26n's default training threshold
    // is 0.001 but per-class >0.25 is the standard inference cutoff.
    // Compared against the SIGMOID-ACTIVATED score, so this really is a
    // probability — see `sigmoid`. It was effectively a logit cutoff until
    // that activation was added (BUG-220).
    const SCORE_FLOOR: f32 = 0.20;
    const IOU_THRESHOLD: f32 = 0.70;

    // Network input dims = max(stride * grid_w) across scales.
    let input_w = scales.iter().map(|s| s.stride * s.w).max().unwrap_or(640) as f32;
    let input_h = scales.iter().map(|s| s.stride * s.h).max().unwrap_or(640) as f32;
    let inv_w = 1.0 / input_w;
    let inv_h = 1.0 / input_h;
    let nc = num_classes as usize;

    let mut candidates: Vec<Detection> = Vec::new();
    for scale in scales {
        let box_buf = match buffers.get(scale.box_idx) {
            Some(b) => b,
            None => continue,
        };
        let cls_buf = match buffers.get(scale.score_idx) {
            Some(b) => b,
            None => continue,
        };
        let h = scale.h as usize;
        let w = scale.w as usize;
        let stride = scale.stride as f32;
        let need_box = h * w * 4 * 4;
        let need_cls = h * w * nc * 4;
        if box_buf.len() < need_box || cls_buf.len() < need_cls {
            // Truncated/oversized buffer — skip this scale rather than
            // panic; postproc continues on the other scales.
            continue;
        }
        for gy in 0..h {
            for gx in 0..w {
                let cls_base = (gy * w + gx) * nc * 4;
                // Best class for this cell.
                // Seeded at -inf, not 0.0: the tensor holds logits, so a cell
                // whose classes are all negative has a real (small) maximum.
                // Seeding at 0.0 would both hide that and, once the floor
                // below is in probability space, report it as sigmoid(0) = 0.5.
                let mut best_logit = f32::NEG_INFINITY;
                let mut best_class: u16 = 0;
                for c in 0..nc {
                    let o = cls_base + c * 4;
                    let s = f32::from_le_bytes([
                        cls_buf[o],
                        cls_buf[o + 1],
                        cls_buf[o + 2],
                        cls_buf[o + 3],
                    ]);
                    if s > best_logit {
                        best_logit = s;
                        best_class = c as u16;
                    }
                }
                // Activate, then compare. `SCORE_FLOOR` is documented as a
                // probability ("per-class >0.25 is the standard inference
                // cutoff") and now is one. Flooring on the raw logit instead
                // would make the minimum emittable confidence
                // sigmoid(0.20) = 0.5498, which is above
                // `bytetrack.high_confidence` (0.5) — every detection would
                // land in ByteTrack's high bucket and its low-confidence
                // recovery pass would never run again.
                let best_score = sigmoid(best_logit);
                if best_score < SCORE_FLOOR {
                    continue;
                }
                // Decode box: (l, t, r, b) in cell units → normalized xyxy.
                let box_base = (gy * w + gx) * 4 * 4;
                let l = f32::from_le_bytes([
                    box_buf[box_base],
                    box_buf[box_base + 1],
                    box_buf[box_base + 2],
                    box_buf[box_base + 3],
                ]);
                let t = f32::from_le_bytes([
                    box_buf[box_base + 4],
                    box_buf[box_base + 5],
                    box_buf[box_base + 6],
                    box_buf[box_base + 7],
                ]);
                let r = f32::from_le_bytes([
                    box_buf[box_base + 8],
                    box_buf[box_base + 9],
                    box_buf[box_base + 10],
                    box_buf[box_base + 11],
                ]);
                let b = f32::from_le_bytes([
                    box_buf[box_base + 12],
                    box_buf[box_base + 13],
                    box_buf[box_base + 14],
                    box_buf[box_base + 15],
                ]);
                let cx = gx as f32 + 0.5;
                let cy = gy as f32 + 0.5;
                let x1 = ((cx - l) * stride * inv_w).clamp(0.0, 1.0);
                let y1 = ((cy - t) * stride * inv_h).clamp(0.0, 1.0);
                let x2 = ((cx + r) * stride * inv_w).clamp(0.0, 1.0);
                let y2 = ((cy + b) * stride * inv_h).clamp(0.0, 1.0);
                if x2 <= x1 || y2 <= y1 {
                    continue;
                }
                candidates.push(Detection {
                    y_min: y1,
                    x_min: x1,
                    y_max: y2,
                    x_max: x2,
                    // Already activated above. Sigmoid is monotonic, so the
                    // argmax and the NMS ordering are unaffected.
                    score: best_score,
                    class_id: best_class,
                });
            }
        }
    }

    nms_greedy(candidates, IOU_THRESHOLD, max_detections)
}

/// Class-agnostic greedy IoU NMS. Standard YOLOv8 inference uses
/// per-class NMS; the cost difference is negligible at our detection
/// volumes and class-agnostic is friendlier to the downstream tracker.
fn nms_greedy(
    mut dets: Vec<Detection>,
    iou_threshold: f32,
    max_detections: usize,
) -> Vec<Detection> {
    if dets.is_empty() {
        return dets;
    }
    dets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Detection> = Vec::with_capacity(dets.len().min(max_detections));
    'outer: for d in dets {
        if keep.len() >= max_detections {
            break;
        }
        for k in &keep {
            if iou(&d, k) >= iou_threshold {
                continue 'outer;
            }
        }
        keep.push(d);
    }
    keep
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let ix1 = a.x_min.max(b.x_min);
    let iy1 = a.y_min.max(b.y_min);
    let ix2 = a.x_max.min(b.x_max);
    let iy2 = a.y_max.min(b.y_max);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let area_a = (a.x_max - a.x_min) * (a.y_max - a.y_min);
    let area_b = (b.x_max - b.x_min) * (b.y_max - b.y_min);
    let union = area_a + area_b - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a one-cell, one-scale raw yolo26 buffer pair with a chosen class
    /// logit, so the decode path can be driven with no device and no HEF.
    fn one_cell(logit: f32) -> (Vec<Vec<u8>>, OutputLayout) {
        let nc = 2usize;
        // box tensor: 1x1 cell, 4 channels (l, t, r, b) in cell units.
        let mut boxes = Vec::new();
        for v in [0.5f32, 0.5, 0.5, 0.5] {
            boxes.extend_from_slice(&v.to_le_bytes());
        }
        // class tensor: 1x1 cell, nc channels. Class 1 wins.
        let mut cls = Vec::new();
        for v in [logit - 1.0, logit] {
            cls.extend_from_slice(&v.to_le_bytes());
        }
        let layout = OutputLayout::RawYolo26 {
            num_classes: nc as u32,
            scales: vec![RawYolo26Scale {
                stride: 8,
                h: 1,
                w: 1,
                box_idx: 0,
                score_idx: 1,
            }],
        };
        (vec![boxes, cls], layout)
    }

    /// The bug. A raw logit must be reported as a probability, not passed
    /// through to be clamped at 1.0 downstream.
    ///
    /// Values are the ladder actually measured on a Hailo-8 core: the class
    /// branch dequantises in steps of 0.1662890, so step 6 is 0.997735 and
    /// everything from step 7 up used to arrive as exactly 1.000000. On that
    /// core 40.4% of 103,232 detections reported 1.000000 and only 8 distinct
    /// values existed in total.
    #[test]
    fn a_raw_logit_is_reported_as_a_probability() {
        for (logit, expect) in [
            (0.997735_f32, 0.7306_f32), // ladder step 6
            (1.164023, 0.7621),         // step 7 — used to clamp to 1.000000
            (1.330312, 0.7909),         // step 8 — used to clamp to 1.000000
            (3.32578, 0.9653),          // step 20
        ] {
            let (bufs, layout) = one_cell(logit);
            let d = decode_detections(&bufs, &layout, 16);
            assert_eq!(d.len(), 1, "logit {logit}");
            assert!(
                (d[0].score - expect).abs() < 1e-3,
                "logit {logit} => {}, expected ~{expect}",
                d[0].score
            );
            // Not asserting < 1.0 in general: f32 sigmoid rounds to exactly
            // 1.0 for logits above ~17. These ladder values are far below that.
            assert!(d[0].score < 0.999, "must not saturate at the ladder values");
        }
    }

    /// Distinct logits above the old clamp must stay distinguishable. This is
    /// the property rule thresholds need and the one the clamp destroyed.
    #[test]
    fn logits_that_used_to_all_clamp_to_one_stay_distinct() {
        let mut seen = Vec::new();
        for step in 7..=12 {
            let (bufs, layout) = one_cell(step as f32 * 0.166289);
            seen.push(decode_detections(&bufs, &layout, 16)[0].score);
        }
        for w in seen.windows(2) {
            assert!(w[1] > w[0], "monotonic: {seen:?}");
        }
        assert!(
            seen.iter().all(|s| *s < 1.0),
            "none may reach 1.0: {seen:?}"
        );
    }

    /// `SCORE_FLOOR` is documented as a probability and must behave as one.
    /// Flooring on the raw logit instead would put the minimum emittable
    /// confidence at sigmoid(0.20) = 0.5498 — above
    /// `bytetrack.high_confidence` (0.5) — so every Hailo detection would
    /// land in the high bucket and ByteTrack's low-confidence recovery pass
    /// would never run again.
    #[test]
    fn the_score_floor_is_a_probability_not_a_logit() {
        // sigmoid(-2.0) = 0.119 — below a 0.20 probability floor.
        let (bufs, layout) = one_cell(-2.0);
        assert!(
            decode_detections(&bufs, &layout, 16).is_empty(),
            "0.119 is below SCORE_FLOOR and must be dropped"
        );
        // sigmoid(-1.0) = 0.269 — above it, and BELOW the 0.5498 that a
        // logit-space floor would have imposed. This is the detection a
        // logit floor silently loses.
        let (bufs, layout) = one_cell(-1.0);
        let d = decode_detections(&bufs, &layout, 16);
        assert_eq!(d.len(), 1, "0.269 clears a probability floor of 0.20");
        assert!(
            d[0].score < 0.5,
            "and it must stay below bytetrack.high_confidence so the low \
             bucket is reachable; got {}",
            d[0].score
        );
    }

    /// A cell whose classes are all strongly negative is background and must
    /// be dropped. Seeding the argmax at 0.0 instead of -inf would report it
    /// as sigmoid(0) = 0.5 and flood every frame.
    #[test]
    fn an_all_negative_cell_is_background_not_a_half_confidence_detection() {
        let (bufs, layout) = one_cell(-8.0);
        assert!(
            decode_detections(&bufs, &layout, 16).is_empty(),
            "sigmoid(-8) = 0.0003; a 0.0-seeded argmax would have said 0.5"
        );
    }

    /// Sigmoid is monotonic, so activating the winner cannot change which
    /// class won.
    #[test]
    fn activation_does_not_change_the_argmax() {
        let (bufs, layout) = one_cell(2.0);
        assert_eq!(decode_detections(&bufs, &layout, 16)[0].class_id, 1);
    }
}
