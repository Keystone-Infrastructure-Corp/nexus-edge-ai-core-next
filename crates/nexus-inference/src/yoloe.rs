//! Real ORT-backed open-vocabulary detector (YOLOE-26, text-prompt mode).
//!
//! Mirrors the architecture of [`crate::yolo_world::YoloWorldDetector`] but
//! targets the YOLOE-26 family — successor to YOLO-World v2 with a
//! roughly 2-3 % mAP lift on COCO and (the headline reason we ship it)
//! a separate **visual-prompt** path that takes a reference-image
//! embedding instead of a text label. Phase B (this module) wires the
//! text-prompt path only; Phase E adds the visual-prompt session in a
//! sibling struct so the two never share Mutex<Session> state.
//!
//! Per the M3 design:
//!
//! * The full prompt **vocabulary** is baked into the ONNX at *export*
//!   time (`tools/models/gen_yoloe.py`). After export the model
//!   behaves like a closed-vocab YOLOv8 with `C = len(vocab)` classes.
//!   We read the vocabulary from the model-pack manifest (`prompts`
//!   block) at session-init time so the Rust side knows which class
//!   index each prompt maps to.
//!
//! * Per-camera config supplies a **subset** of those prompts. The
//!   [`Detector::detect`] path filters detections to that subset and
//!   relabels each `Detection.label` with the prompt string (not the
//!   numeric class id), which is exactly the contract `RuleEngine`
//!   already expects.
//!
//! * The per-camera prompt subset is held in an `ArcSwap` so
//!   [`push_camera_config`] is wait-free for the inference hot path.
//!   Lookups go through a `HashMap<prompt_lowercase, class_idx>`
//!   built once at startup; per-frame work is O(detections) not
//!   O(prompts).

#![cfg(feature = "ort")]
#![allow(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use ndarray::{s, Array2, Array4, Ix2};
use nexus_config::{CameraConfigUpdate, InferenceConfig};
use nexus_types::{BBox, CameraId, Detection, Frame, PixelFormat};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::execution_providers;

/// One YOLOE ONNX session + the prompt vocabulary it was exported
/// with + a per-camera subset filter.
pub struct YoloeDetector {
    session: Mutex<Session>,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    nms_iou_threshold: f32,
    /// M_PERF_CROWD Phase C3 — optional spatial bucket size (pixels)
    /// for the shared NMS pass. `None` keeps the pre-C3 naive O(N²)
    /// path bit-identical.
    nms_spatial_bucket_size_px: Option<u32>,
    /// Full vocabulary baked into the ONNX, in class-index order.
    vocab: Vec<String>,
    /// Lowercase → class index, for fast lookup of per-camera prompt subsets.
    vocab_index: HashMap<String, usize>,
    /// Per-camera subset of class indices to keep. Empty = keep all.
    /// Wait-free swap for hot reload via [`push_camera_config`].
    enabled_per_camera: ArcSwap<HashMap<CameraId, Vec<usize>>>,
    /// Cached for diagnostics.
    _model_path: PathBuf,
}

impl YoloeDetector {
    /// Build from an [`InferenceConfig`]. Resolves the ONNX as
    /// `model.pack_path / yoloe26_s.onnx` and the vocabulary from
    /// the matching entry in `models-manifest.json`. Returns an error
    /// any other way so we never silently fall through to a mock under
    /// prod config — same contract as `YoloOrtDetector::from_config`.
    pub fn from_config(cfg: &InferenceConfig) -> Result<Self, InferenceError> {
        let pack = cfg.model.pack_path.as_ref().ok_or_else(|| {
            InferenceError::ModelLoad(
                "yoloe detector needs inference.model.pack_path; \
                 point it at the directory holding yoloe26_s.onnx + \
                 models-manifest.json"
                    .into(),
            )
        })?;
        let onnx_path = pack.join("yoloe26_s.onnx");
        let manifest_path = pack.join("models-manifest.json");
        let vocab = load_vocab_from_manifest(&manifest_path, "yoloe26_s")?;
        Self::open(
            &onnx_path,
            cfg.model.input_width,
            cfg.model.input_height,
            cfg.model.score_threshold,
            default_nms_iou_threshold(),
            cfg.model.nms_spatial_bucket_size_px,
            vocab,
            &cfg.ep_priority,
        )
    }

    /// Open a session against the given ONNX and bind the supplied
    /// vocabulary. `ep_priority` controls which ORT execution
    /// providers are registered — see
    /// [`crate::execution_providers::selected_for_priority`]. Pass
    /// `&[]` for CPU-only (the default fallback path).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        model_path: &Path,
        input_w: u32,
        input_h: u32,
        score_threshold: f32,
        nms_iou_threshold: f32,
        nms_spatial_bucket_size_px: Option<u32>,
        vocab: Vec<String>,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
        if vocab.is_empty() {
            return Err(InferenceError::ModelLoad(
                "yoloe vocab is empty — gen_yoloe.py must run first \
                 and write the prompts[] block into models-manifest.json"
                    .into(),
            ));
        }
        let (eps, ep_names) = execution_providers::selected_for_priority(ep_priority);
        let session = Session::builder()
            .map_err(|e| InferenceError::ModelLoad(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| InferenceError::ModelLoad(format!("opt level: {e}")))?
            .with_execution_providers(eps)
            .map_err(|e| InferenceError::ModelLoad(format!("EP register: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| {
                InferenceError::ModelLoad(format!("load {}: {e}", model_path.display()))
            })?;
        let vocab_index = build_vocab_index(&vocab);
        info!(
            model = %model_path.display(),
            input_w, input_h, score_threshold, nms_iou_threshold,
            nms_spatial_bucket_size_px,
            vocab_len = vocab.len(),
            ep_requested = ?ep_priority,
            ep_registered = ?ep_names,
            "yoloe ORT detector ready"
        );
        Ok(Self {
            session: Mutex::new(session),
            input_w,
            input_h,
            score_threshold,
            nms_iou_threshold,
            nms_spatial_bucket_size_px,
            vocab,
            vocab_index,
            enabled_per_camera: ArcSwap::from_pointee(HashMap::new()),
            _model_path: model_path.to_path_buf(),
        })
    }

    /// Convert an arbitrary list of operator-supplied prompts into the
    /// class-index subset the detector will keep. Unknown prompts are
    /// dropped with a warn-once log.
    fn resolve_prompts(&self, prompts: &[String]) -> Vec<usize> {
        if prompts.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<usize> = Vec::with_capacity(prompts.len());
        for p in prompts {
            let key = p.trim().to_ascii_lowercase();
            match self.vocab_index.get(&key) {
                Some(&idx) => out.push(idx),
                None => {
                    warn!(
                        prompt = %p,
                        "yoloe: prompt not in baked vocab, dropping. \
                         Regenerate the ONNX with this prompt to enable it."
                    );
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[async_trait]
impl Detector for YoloeDetector {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let input_w = self.input_w;
        let input_h = self.input_h;
        let frame_w = frame.width;
        let frame_h = frame.height;
        let score_threshold = self.score_threshold;
        let nms_iou = self.nms_iou_threshold;
        let nms_bucket = self.nms_spatial_bucket_size_px;
        let format = frame.format;
        let camera_id = frame.camera_id;

        // Decide the enabled class-id subset for this frame. The rule:
        //   1. If the supervisor passed a non-empty `prompts` slice, take
        //      it verbatim (this is what the trait contract says — the
        //      caller knows what it wants).
        //   2. Otherwise consult the per-camera subset table. Empty
        //      subset means "keep every class in the baked vocab" — same
        //      semantics v1 uses for the closed-vocab YOLO path.
        let enabled: Vec<usize> = if !prompts.is_empty() {
            self.resolve_prompts(prompts)
        } else {
            self.enabled_per_camera
                .load()
                .get(&camera_id)
                .cloned()
                .unwrap_or_default()
        };

        // Borrow the source RGB buffer when it's already in the right
        // pixel order; only the BGR path needs to allocate. `frame.data`
        // is `Arc<Vec<u8>>` so the Rgb24 branch is a zero-copy borrow
        // — saves ~1.5 MB alloc + memcpy per frame per camera.
        let rgb: Cow<'_, [u8]> = match format {
            PixelFormat::Rgb24 => Cow::Borrowed(&frame.data[..]),
            PixelFormat::Bgr24 => Cow::Owned(bgr_to_rgb(&frame.data)),
            other => return Err(InferenceError::UnsupportedFormat(other)),
        };

        let session_for_blocking: &Mutex<Session> = &self.session;
        let vocab = &self.vocab;
        tokio::task::block_in_place(|| {
            let mut sess = session_for_blocking.lock();
            run_yoloe(
                &mut sess,
                &rgb,
                frame_w,
                frame_h,
                input_w,
                input_h,
                score_threshold,
                nms_iou,
                nms_bucket,
                vocab,
                &enabled,
            )
        })
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        let resolved = self.resolve_prompts(&update.prompts);
        // Copy-on-write swap so the inference hot path stays wait-free.
        let mut next: HashMap<CameraId, Vec<usize>> = (**self.enabled_per_camera.load()).clone();
        if resolved.is_empty() {
            next.remove(&update.camera_id);
        } else {
            next.insert(update.camera_id, resolved);
        }
        self.enabled_per_camera.store(Arc::new(next));
        debug!(
            camera = update.camera_id,
            generation = update.generation,
            prompt_count = update.prompts.len(),
            "yoloe per-camera prompt subset updated"
        );
    }

    fn name(&self) -> &'static str {
        "yoloe"
    }
}

#[allow(clippy::too_many_arguments)]
fn run_yoloe(
    session: &mut Session,
    rgb: &[u8],
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    nms_iou_threshold: f32,
    nms_spatial_bucket_size_px: Option<u32>,
    vocab: &[String],
    enabled: &[usize],
) -> Result<Vec<Detection>, InferenceError> {
    let nchw = preprocess_nchw(rgb, frame_w, frame_h, input_w, input_h)?;
    let input = TensorRef::from_array_view(nchw.view())
        .map_err(|e| InferenceError::Failed(format!("tensor wrap: {e}")))?;

    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| InferenceError::Failed(format!("session run: {e}")))?;

    // YOLOE-26 exports come in two flavours:
    //
    //   (a) Raw YOLOv8 head — single output `[1, 4+C, N]` (or its
    //       transpose). NMS happens in Rust below. This is what
    //       `gen_yoloe.py` *requests* via `nms=False`, and what the
    //       legacy `yoloe-s.pt` checkpoint produced.
    //
    //   (b) seg-NMS-applied detect head — two outputs from the
    //       `yoloe-26{n,s,l,x}-seg.pt` family (the only ones still
    //       published upstream as of ultralytics 8.4.50). The seg
    //       checkpoints ignore `nms=False` and emit:
    //           output0: [1, 300, 4+1+1+M]    (xyxy, conf, cls, mask_coefs)
    //           output1: [1, M, H/4, W/4]     (mask prototypes — unused)
    //       Bboxes are in **input space** (640×640 by default); rows
    //       are sorted desc by conf and padded with zeros after the
    //       last real detection. We need to read them as-is — running
    //       our own NMS on top would be a no-op.
    //
    // Dispatch on output count + first-output rank.
    let n_outputs = outputs.len();
    let (_name, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| InferenceError::Failed("no outputs".into()))?;
    let view = value
        .try_extract_array::<f32>()
        .map_err(|e| InferenceError::Failed(format!("extract array: {e}")))?;

    let shape: Vec<usize> = view.shape().to_vec();

    // Path (b): seg-NMS-applied output. Two-output signature with a
    // 3-D first output whose last dim is small (e.g. 38 for M=32 mask
    // coefs) means the seg head already produced final detections.
    if n_outputs >= 2 && shape.len() == 3 && shape[0] == 1 && shape[2] >= 6 {
        let pred: Array2<f32> = view
            .slice(s![0, .., ..])
            .to_owned()
            .into_dimensionality::<Ix2>()
            .map_err(|e| {
                InferenceError::Failed(format!(
                    "yoloe seg-NMS reshape failed for shape {:?}: {e}",
                    shape
                ))
            })?;
        let x_scale = frame_w as f32 / input_w as f32;
        let y_scale = frame_h as f32 / input_h as f32;
        return Ok(parse_seg_nms_rows(
            &pred,
            score_threshold,
            x_scale,
            y_scale,
            frame_w as f32,
            frame_h as f32,
            vocab,
            enabled,
        ));
    }

    // Path (a): raw YOLOv8 head — shape [1, 4 + C, N]; transpose to
    // [N, 4+C] so the row loop matches the closed-vocab YOLO path.
    let pred: Array2<f32> = match shape.len() {
        3 => {
            let dim1 = shape[1];
            let dim2 = shape[2];
            if dim1 >= dim2 {
                view.slice(s![0, .., ..])
                    .to_owned()
                    .into_dimensionality::<Ix2>()
                    .ok()
            } else {
                view.slice(s![0, .., ..])
                    .to_owned()
                    .reversed_axes()
                    .into_dimensionality::<Ix2>()
                    .ok()
            }
        }
        2 => view.to_owned().into_dimensionality::<Ix2>().ok(),
        _ => None,
    }
    .ok_or_else(|| {
        InferenceError::Failed(format!(
            "unexpected output shape {:?} (want 2-D or 3-D)",
            shape
        ))
    })?;

    let (rows, cols) = pred.dim();
    let n_classes = cols.saturating_sub(4);
    if rows == 0 || n_classes == 0 {
        return Ok(Vec::new());
    }
    if n_classes != vocab.len() {
        return Err(InferenceError::Failed(format!(
            "vocab/output mismatch: vocab has {} classes but output has {}; \
             rerun gen_yoloe.py to refresh both",
            vocab.len(),
            n_classes
        )));
    }

    let x_scale = frame_w as f32 / input_w as f32;
    let y_scale = frame_h as f32 / input_h as f32;

    // Collect candidates (one per anchor, picking the best class restricted
    // to the enabled subset when supplied).
    let mut candidates: Vec<Detection> = Vec::with_capacity(64);
    for r in 0..rows {
        let row = pred.slice(s![r, ..]);
        let cx = row[0];
        let cy = row[1];
        let w = row[2];
        let h = row[3];

        let (best_id, best_score) = if enabled.is_empty() {
            // No filter: argmax across all classes.
            let mut best_id: i32 = -1;
            let mut best = 0.0f32;
            for c in 0..n_classes {
                let v = row[4 + c];
                if v > best {
                    best = v;
                    best_id = c as i32;
                }
            }
            (best_id, best)
        } else {
            // Filter: argmax restricted to enabled class indices.
            let mut best_id: i32 = -1;
            let mut best = 0.0f32;
            for &c in enabled {
                if c >= n_classes {
                    continue;
                }
                let v = row[4 + c];
                if v > best {
                    best = v;
                    best_id = c as i32;
                }
            }
            (best_id, best)
        };

        if best_id < 0 || best_score < score_threshold {
            continue;
        }

        let label = vocab.get(best_id as usize).cloned().unwrap_or_default();
        if label.is_empty() {
            continue;
        }

        let bbox = BBox {
            x1: ((cx - w * 0.5) * x_scale).max(0.0),
            y1: ((cy - h * 0.5) * y_scale).max(0.0),
            x2: ((cx + w * 0.5) * x_scale).min(frame_w as f32),
            y2: ((cy + h * 0.5) * y_scale).min(frame_h as f32),
        };
        if bbox.width() <= 0.0 || bbox.height() <= 0.0 {
            continue;
        }
        candidates.push(Detection {
            label,
            confidence: best_score.clamp(0.0, 1.0),
            bbox,
            attributes: Default::default(),
        });
    }

    // Class-aware NMS — YOLOE exports without the NMS op so we run
    // it here. Per-class to avoid suppressing a "person" with an
    // overlapping "vehicle" detection on the same anchor row.
    // M_PERF_CROWD Phase C3 — the shared `crate::nms` module owns the
    // implementation; `nms_spatial_bucket_size_px` opts into the
    // 3×3-neighbourhood fast path (bit-identical when set high enough).
    let kept = crate::nms::nms_per_label(candidates, nms_iou_threshold, nms_spatial_bucket_size_px);
    debug!(
        out = kept.len(),
        rows,
        cols,
        enabled = enabled.len(),
        "yoloe postprocess done"
    );
    Ok(kept)
}

/// Parse a seg-NMS-applied detect head output.
///
/// The yoloe-26 seg checkpoints (only flavour still published as of
/// ultralytics 8.4.50) export the detect head with NMS already applied
/// and the seg mask coefficients appended. Per-row layout in the
/// `[1, N, K]` first output (K = 6 + M, M = mask-proto channels, e.g.
/// 38 for M=32):
///
///   `[x1, y1, x2, y2, conf, class_id, m_0 .. m_{M-1}]`
///
/// Bbox is in **model-input space** (e.g. 640×640); rows are sorted
/// descending by conf and padded with zeros after the last real
/// detection, so a conf < threshold check on each row terminates
/// processing cleanly without us having to know `N_real` up front.
///
/// We don't run our own NMS here — ultralytics already did.
#[allow(clippy::too_many_arguments)]
fn parse_seg_nms_rows(
    pred: &Array2<f32>,
    score_threshold: f32,
    x_scale: f32,
    y_scale: f32,
    frame_w: f32,
    frame_h: f32,
    vocab: &[String],
    enabled: &[usize],
) -> Vec<Detection> {
    let (rows, cols) = pred.dim();
    if rows == 0 || cols < 6 {
        return Vec::new();
    }
    let mut out: Vec<Detection> = Vec::with_capacity(rows.min(64));
    for r in 0..rows {
        let row = pred.slice(s![r, ..]);
        let conf = row[4];
        if conf < score_threshold {
            // Rows are sorted desc by conf; everything after a
            // sub-threshold row is either also sub-threshold or
            // zero-padded. Bail early to save the rest of the loop.
            break;
        }
        let cls_id = row[5] as i64;
        if cls_id < 0 || (cls_id as usize) >= vocab.len() {
            continue;
        }
        let cls_idx = cls_id as usize;
        if !enabled.is_empty() && !enabled.contains(&cls_idx) {
            continue;
        }
        let x1_i = row[0];
        let y1_i = row[1];
        let x2_i = row[2];
        let y2_i = row[3];
        let bbox = BBox {
            x1: (x1_i * x_scale).max(0.0),
            y1: (y1_i * y_scale).max(0.0),
            x2: (x2_i * x_scale).min(frame_w),
            y2: (y2_i * y_scale).min(frame_h),
        };
        if bbox.width() <= 0.0 || bbox.height() <= 0.0 {
            continue;
        }
        out.push(Detection {
            label: vocab[cls_idx].clone(),
            confidence: conf.clamp(0.0, 1.0),
            bbox,
            attributes: Default::default(),
        });
    }
    debug!(
        out = out.len(),
        rows,
        cols,
        enabled = enabled.len(),
        "yoloe seg-NMS postprocess done"
    );
    out
}

/// Bilinear resize RGB → NCHW float32. Same shape as the closed-vocab
/// `crate::yolo::preprocess_nchw`; kept as a sibling rather than reused
/// across module boundaries because the closed-vocab one is `pub(super)`
/// only by accident — keeping them separate also lets a future YOLOE
/// preprocess (e.g. ImageNet mean/std normalize) diverge cleanly.
fn preprocess_nchw(
    rgb: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Array4<f32>, InferenceError> {
    if rgb.len() != (src_w as usize) * (src_h as usize) * 3 {
        return Err(InferenceError::Failed(format!(
            "rgb buffer wrong size: got {} expected {}",
            rgb.len(),
            (src_w as usize) * (src_h as usize) * 3
        )));
    }
    let mut tensor = Array4::<f32>::zeros((1, 3, dst_h as usize, dst_w as usize));
    let inv_255 = 1.0f32 / 255.0;
    let sx = src_w as f32 / dst_w as f32;
    let sy = src_h as f32 / dst_h as f32;
    for y in 0..dst_h as usize {
        let src_yf = ((y as f32) + 0.5) * sy - 0.5;
        let y0 = src_yf.floor().clamp(0.0, (src_h - 1) as f32) as usize;
        let y1 = (y0 + 1).min(src_h as usize - 1);
        let dy = (src_yf - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dst_w as usize {
            let src_xf = ((x as f32) + 0.5) * sx - 0.5;
            let x0 = src_xf.floor().clamp(0.0, (src_w - 1) as f32) as usize;
            let x1 = (x0 + 1).min(src_w as usize - 1);
            let dx = (src_xf - x0 as f32).clamp(0.0, 1.0);
            let stride = src_w as usize * 3;
            let i00 = y0 * stride + x0 * 3;
            let i01 = y0 * stride + x1 * 3;
            let i10 = y1 * stride + x0 * 3;
            let i11 = y1 * stride + x1 * 3;
            for c in 0..3 {
                let v00 = rgb[i00 + c] as f32;
                let v01 = rgb[i01 + c] as f32;
                let v10 = rgb[i10 + c] as f32;
                let v11 = rgb[i11 + c] as f32;
                let v0 = v00 * (1.0 - dx) + v01 * dx;
                let v1 = v10 * (1.0 - dx) + v11 * dx;
                let v = v0 * (1.0 - dy) + v1 * dy;
                tensor[[0, c, y, x]] = v * inv_255;
            }
        }
    }
    Ok(tensor)
}

fn bgr_to_rgb(buf: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (i, chunk) in buf.chunks_exact(3).enumerate() {
        let off = i * 3;
        out[off] = chunk[2];
        out[off + 1] = chunk[1];
        out[off + 2] = chunk[0];
    }
    out
}

fn build_vocab_index(vocab: &[String]) -> HashMap<String, usize> {
    let mut out = HashMap::with_capacity(vocab.len());
    for (i, p) in vocab.iter().enumerate() {
        out.insert(p.trim().to_ascii_lowercase(), i);
    }
    out
}

fn load_vocab_from_manifest(
    manifest_path: &Path,
    model_id: &str,
) -> Result<Vec<String>, InferenceError> {
    let bytes = std::fs::read(manifest_path).map_err(|e| {
        InferenceError::ModelLoad(format!("read manifest {}: {e}", manifest_path.display()))
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| InferenceError::ModelLoad(format!("parse manifest: {e}")))?;
    let models = manifest
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| InferenceError::ModelLoad("manifest missing models[]".into()))?;
    let entry = models
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model_id))
        .ok_or_else(|| InferenceError::ModelLoad(format!("model id {model_id} not in manifest")))?;
    let prompts = entry
        .get("prompts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            InferenceError::ModelLoad(format!(
                "manifest entry {model_id} missing prompts[]; \
                 rerun tools/models/gen_yoloe.py to populate it"
            ))
        })?;
    let out: Vec<String> = prompts
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if out.is_empty() {
        return Err(InferenceError::ModelLoad(format!(
            "manifest entry {model_id} has empty prompts[]"
        )));
    }
    Ok(out)
}

/// Default IoU threshold for class-aware NMS. v1 uses 0.45 for closed-vocab
/// YOLO; YOLOE benefits from a slightly looser 0.50 because its class
/// scores are more spread (text-conditioned scoring, inherited from the
/// YOLO-World lineage). Tunable per camera in M4.
fn default_nms_iou_threshold() -> f32 {
    0.50
}

/// Public re-export of the manifest vocab loader so the worker binary can
/// reuse the same parser without duplicating the JSON shape.
pub fn load_vocab_from_manifest_public(
    manifest_path: &Path,
    model_id: &str,
) -> Result<Vec<String>, InferenceError> {
    load_vocab_from_manifest(manifest_path, model_id)
}

/// Same `Arc<dyn Detector>`-returning shape as `build_detector_for_yolo`.
pub fn build_detector_for_yoloe(
    cfg: &InferenceConfig,
) -> Result<Arc<dyn Detector>, InferenceError> {
    match YoloeDetector::from_config(cfg) {
        Ok(d) => Ok(Arc::new(d)),
        Err(e) => {
            warn!("yoloe ORT detector unavailable, falling back to mock: {e}");
            Ok(Arc::new(crate::detectors::MockDetector::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_index_lowercases_and_trims() {
        let v = vec!["Person".to_string(), "  vehicle  ".to_string()];
        let idx = build_vocab_index(&v);
        assert_eq!(idx.get("person"), Some(&0));
        assert_eq!(idx.get("vehicle"), Some(&1));
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
        assert!(crate::nms::iou(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn iou_identical_one() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        assert!((crate::nms::iou(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nms_keeps_highest_scoring_overlap() {
        let dets = vec![
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 10.0,
                },
                attributes: Default::default(),
            },
            Detection {
                label: "person".into(),
                confidence: 0.5,
                bbox: BBox {
                    x1: 1.0,
                    y1: 1.0,
                    x2: 11.0,
                    y2: 11.0,
                },
                attributes: Default::default(),
            },
        ];
        let out = crate::nms::nms_per_label(dets, 0.5, None);
        assert_eq!(out.len(), 1);
        assert!((out[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn nms_keeps_different_classes() {
        let dets = vec![
            Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 10.0,
                },
                attributes: Default::default(),
            },
            Detection {
                label: "vehicle".into(),
                confidence: 0.8,
                bbox: BBox {
                    x1: 1.0,
                    y1: 1.0,
                    x2: 11.0,
                    y2: 11.0,
                },
                attributes: Default::default(),
            },
        ];
        let out = crate::nms::nms_per_label(dets, 0.5, None);
        assert_eq!(out.len(), 2);
    }
}
