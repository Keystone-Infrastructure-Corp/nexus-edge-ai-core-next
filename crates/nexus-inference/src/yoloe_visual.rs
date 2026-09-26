//! M3.1 Phase E: visual-prompt YOLOE detector.
//!
//! Sibling to [`crate::yoloe::YoloeDetector`] (text-prompt mode). Where
//! the text path bakes its vocabulary into the ONNX at export time and
//! per-camera config supplies a *subset* of those baked classes, the
//! visual path:
//!
//! 1. Loads `yoloe26_s_vp_<W>x<H>.onnx` whose graph takes **two** inputs:
//!    `(image_tensor, visual_prompt_embeddings)`. The embeddings tensor
//!    is `[1, N, D]` where `N` is the number of attached visual
//!    prompts for the camera and `D` is the encoder's embedding
//!    dimension (typically 512 for yoloe26_s).
//! 2. Pulls those `(label, embedding)` pairs from a
//!    [`VisualPromptStore`] indexed by `CameraId` on every
//!    `push_camera_config`. The detector keeps NO knowledge of how
//!    those embeddings were produced — that's the encoder's job (see
//!    [`crate::encoder::ImageEncoder`]).
//! 3. Class slot `c ∈ 0..N` in the output tensor corresponds to the
//!    operator-supplied label at `bindings[c].label`. There is no
//!    "baked vocab" lookup — the label is the prompt's `name` field
//!    verbatim.
//! 4. Empty-prompt fail-soft (E2): a camera with zero attached
//!    visual prompts returns `Ok(vec![])` from `detect` *without
//!    running the session*. This matches the closed-vocab YOLO
//!    behaviour when its enabled set is empty: an unconfigured
//!    camera should never crash the pipeline.
//!
//! The state shape is `ArcSwap<HashMap<CameraId,
//! Arc<Vec<VisualPromptBinding>>>>` so the hot path lifts a single
//! `Arc<Vec<_>>` per frame (wait-free) and the per-camera fan-push
//! is copy-on-write at the outer HashMap level.
//!
//! Why a trait instead of a direct `nexus_store::Store` reference:
//! `nexus-inference` and `nexus-store` are sibling crates with no
//! current dependency edge between them. Introducing one (in either
//! direction) drags far more API than this phase needs. The trait
//! is two methods wide and lets the engine plug a Store-backed
//! impl, while the worker process (Phase F) plugs a thin
//! SQLite-direct impl from its own `NEXUS_WORKER_DB_URL`.

#![cfg(feature = "ort")]
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use ndarray::{s, Array2, Array3, Ix2};
use nexus_config::{CameraConfigUpdate, InferenceConfig};
use nexus_types::{BBox, CameraId, Detection, Frame};
use ort::session::Session;
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::session_tuning::{self, SessionTuning};
use crate::visual_prompts::{VisualPromptBinding, VisualPromptStore};
use crate::yolo::{frame_rgb, preprocess_nchw};

/// One YOLOE visual-prompt ONNX session + a per-camera binding map.
pub struct YoloeVisualDetector {
    session: Mutex<Session>,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    nms_iou_threshold: f32,
    /// M_PERF_CROWD Phase C3 — optional spatial bucket size (pixels)
    /// for the shared NMS pass. `None` keeps the pre-C3 naive O(N²)
    /// path bit-identical.
    nms_spatial_bucket_size_px: Option<u32>,
    /// Embedding dimension this session expects. Bindings whose
    /// embedding length doesn't match are dropped with a warn-log.
    embedding_dim: usize,
    /// Per-camera bindings. `Arc<Vec<_>>` lifted by the hot path.
    /// Wait-free swap for hot reload via [`push_camera_config`].
    bindings_per_camera: ArcSwap<HashMap<CameraId, Arc<Vec<VisualPromptBinding>>>>,
    /// Where to look up `(label, embedding)` pairs on every
    /// `push_camera_config`.
    store: Arc<dyn VisualPromptStore>,
    /// Cached for diagnostics.
    _model_path: PathBuf,
}

impl YoloeVisualDetector {
    /// Build from an [`InferenceConfig`] + a [`VisualPromptStore`].
    /// Resolves the ONNX as `model.pack_path / yoloe26_s_vp_<W>x<H>.onnx`
    /// (native-16:9 ladder). Returns an error any other way — same
    /// contract as [`crate::yoloe::YoloeDetector::from_config`].
    pub fn from_config(
        cfg: &InferenceConfig,
        embedding_dim: usize,
        store: Arc<dyn VisualPromptStore>,
    ) -> Result<Self, InferenceError> {
        let pack = cfg.model.pack_path.as_ref().ok_or_else(|| {
            InferenceError::ModelLoad(
                "yoloe_visual detector needs inference.model.pack_path; \
                 point it at the directory holding yoloe26_s_vp_<W>x<H>.onnx + \
                 models-manifest.json"
                    .into(),
            )
        })?;
        let onnx_path = pack.join(format!(
            "yoloe26_s_vp_{}x{}.onnx",
            cfg.model.input_width, cfg.model.input_height
        ));
        Self::open(
            &onnx_path,
            cfg.model.input_width,
            cfg.model.input_height,
            cfg.model.score_threshold,
            default_visual_nms_iou_threshold(),
            cfg.model.nms_spatial_bucket_size_px,
            embedding_dim,
            store,
            &cfg.ep_priority,
            SessionTuning::new(
                cfg.ort_intra_threads,
                cfg.ort_allow_spinning,
                cfg.ort_session_count(),
            ),
        )
    }

    /// Open a session against the given ONNX. `embedding_dim` is the
    /// length each binding's `embedding` slice MUST have; the value
    /// comes from the matching encoder's `models-manifest.json` entry
    /// (validated via [`crate::encoder::ImageEncoder::embedding_dim`]
    /// at upload time). `ep_priority` selects ORT execution providers.
    /// `tuning` controls the session's thread pools — see
    /// [`crate::session_tuning`].
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        model_path: &Path,
        input_w: u32,
        input_h: u32,
        score_threshold: f32,
        nms_iou_threshold: f32,
        nms_spatial_bucket_size_px: Option<u32>,
        embedding_dim: usize,
        store: Arc<dyn VisualPromptStore>,
        ep_priority: &[String],
        tuning: SessionTuning,
    ) -> Result<Self, InferenceError> {
        if embedding_dim == 0 {
            return Err(InferenceError::ModelLoad(
                "yoloe_visual: embedding_dim must be > 0".into(),
            ));
        }
        let built = session_tuning::build_session(model_path, ep_priority, &tuning)
            .map_err(InferenceError::ModelLoad)?;
        info!(
            model = %model_path.display(),
            input_w, input_h, score_threshold, nms_iou_threshold,
            nms_spatial_bucket_size_px,
            embedding_dim,
            ep_requested = ?ep_priority,
            ep_registered = ?built.ep_names,
            intra_threads = built.intra_threads,
            "yoloe_visual ORT detector ready"
        );
        Ok(Self {
            session: Mutex::new(built.session),
            input_w,
            input_h,
            score_threshold,
            nms_iou_threshold,
            nms_spatial_bucket_size_px,
            embedding_dim,
            bindings_per_camera: ArcSwap::from_pointee(HashMap::new()),
            store,
            _model_path: model_path.to_path_buf(),
        })
    }
}

#[async_trait]
impl Detector for YoloeVisualDetector {
    async fn detect(
        &self,
        frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let camera_id = frame.camera_id;
        // E2 fail-soft: no bindings ⇒ no detections, NO session run.
        let bindings = match self.bindings_per_camera.load().get(&camera_id) {
            Some(b) if !b.is_empty() => Arc::clone(b),
            _ => return Ok(Vec::new()),
        };

        let input_w = self.input_w;
        let input_h = self.input_h;
        let frame_w = frame.width;
        let frame_h = frame.height;
        let score_threshold = self.score_threshold;
        let nms_iou = self.nms_iou_threshold;
        let nms_bucket = self.nms_spatial_bucket_size_px;
        let embedding_dim = self.embedding_dim;

        // Borrowed for RGB24 (the supervisor contract); only BGR24 pays
        // for a converted copy.
        let rgb = frame_rgb(frame)?;

        let session_for_blocking: &Mutex<Session> = &self.session;
        tokio::task::block_in_place(|| {
            let mut sess = session_for_blocking.lock();
            run_yoloe_visual(
                &mut sess,
                &rgb,
                frame_w,
                frame_h,
                input_w,
                input_h,
                score_threshold,
                nms_iou,
                nms_bucket,
                embedding_dim,
                &bindings,
            )
        })
    }

    async fn push_camera_config(&self, update: &CameraConfigUpdate) {
        let camera_id = update.camera_id;
        let raw = match self.store.list_for_camera(camera_id).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    camera = camera_id,
                    error = %e,
                    "yoloe_visual: store.list_for_camera failed; \
                     leaving previous bindings in place"
                );
                return;
            }
        };
        // Validate every embedding length against the session's
        // declared `embedding_dim`. A mismatched binding would crash
        // the per-frame VPE tensor build — drop it loud instead.
        let mut keep: Vec<VisualPromptBinding> = Vec::with_capacity(raw.len());
        for b in raw {
            if b.embedding.len() != self.embedding_dim {
                warn!(
                    camera = camera_id,
                    label = %b.label,
                    got = b.embedding.len(),
                    expected = self.embedding_dim,
                    "yoloe_visual: dropping binding with wrong embedding_dim; \
                     re-encode the reference image with the current encoder"
                );
                continue;
            }
            keep.push(b);
        }

        // Copy-on-write swap so the inference hot path stays wait-free.
        let mut next: HashMap<CameraId, Arc<Vec<VisualPromptBinding>>> =
            (**self.bindings_per_camera.load()).clone();
        if keep.is_empty() {
            next.remove(&camera_id);
        } else {
            next.insert(camera_id, Arc::new(keep));
        }
        let new_len = next.get(&camera_id).map(|b| b.len()).unwrap_or(0);
        self.bindings_per_camera.store(Arc::new(next));
        debug!(
            camera = camera_id,
            generation = update.generation,
            binding_count = new_len,
            "yoloe_visual per-camera bindings updated"
        );
    }

    fn name(&self) -> &'static str {
        "yoloe_visual"
    }
}

#[allow(clippy::too_many_arguments)]
fn run_yoloe_visual(
    session: &mut Session,
    rgb: &[u8],
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    nms_iou_threshold: f32,
    nms_spatial_bucket_size_px: Option<u32>,
    embedding_dim: usize,
    bindings: &[VisualPromptBinding],
) -> Result<Vec<Detection>, InferenceError> {
    debug_assert!(
        !bindings.is_empty(),
        "fail-soft was supposed to filter this"
    );
    let nchw = preprocess_nchw(rgb, frame_w, frame_h, input_w, input_h)?;
    let vpe = build_vpe_tensor(bindings, embedding_dim)?;

    let image_input = TensorRef::from_array_view(nchw.view())
        .map_err(|e| InferenceError::Failed(format!("image tensor wrap: {e}")))?;
    let vpe_input = TensorRef::from_array_view(vpe.view())
        .map_err(|e| InferenceError::Failed(format!("vpe tensor wrap: {e}")))?;

    let outputs = session
        .run(ort::inputs![image_input, vpe_input])
        .map_err(|e| InferenceError::Failed(format!("session run: {e}")))?;

    let (_name, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| InferenceError::Failed("yoloe_visual: no outputs".into()))?;
    let view = value
        .try_extract_array::<f32>()
        .map_err(|e| InferenceError::Failed(format!("extract array: {e}")))?;

    // Same YOLOv8-style head as text mode: `[1, 4 + N, anchors]`
    // where N == bindings.len(). Transpose-as-needed.
    let shape: Vec<usize> = view.shape().to_vec();
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
            "yoloe_visual: unexpected output shape {:?} (want 2-D or 3-D)",
            shape
        ))
    })?;

    let (rows, cols) = pred.dim();
    let n_classes = cols.saturating_sub(4);
    if rows == 0 || n_classes == 0 {
        return Ok(Vec::new());
    }
    if n_classes != bindings.len() {
        return Err(InferenceError::Failed(format!(
            "yoloe_visual: bindings/output mismatch: bindings has {} but \
             output has {} classes; the session graph likely has a fixed \
             N — re-export yoloe26_s_vp.onnx with dynamic N",
            bindings.len(),
            n_classes
        )));
    }

    let x_scale = frame_w as f32 / input_w as f32;
    let y_scale = frame_h as f32 / input_h as f32;

    let mut candidates: Vec<Detection> = Vec::with_capacity(64);
    for r in 0..rows {
        let row = pred.slice(s![r, ..]);
        let cx = row[0];
        let cy = row[1];
        let w = row[2];
        let h = row[3];

        // Argmax across the binding slots (no subset filter — every
        // attached binding is in-play for visual mode; toggling off is
        // a detach, not a per-frame mask).
        let mut best_id: i32 = -1;
        let mut best = 0.0f32;
        for c in 0..n_classes {
            let v = row[4 + c];
            if v > best {
                best = v;
                best_id = c as i32;
            }
        }
        if best_id < 0 || best < score_threshold {
            continue;
        }

        let label = bindings[best_id as usize].label.clone();
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
            confidence: best.clamp(0.0, 1.0),
            bbox,
            attributes: Default::default(),
        });
    }

    let kept = crate::nms::nms_per_label(candidates, nms_iou_threshold, nms_spatial_bucket_size_px);
    debug!(
        out = kept.len(),
        rows,
        cols,
        bindings = bindings.len(),
        "yoloe_visual postprocess done"
    );
    Ok(kept)
}

/// Pack `bindings`' embeddings into the `[1, N, D]` tensor the visual
/// ONNX expects. Returns an error if any binding's embedding length
/// disagrees with `embedding_dim` — `push_camera_config` already
/// filters those, but defense-in-depth here in case someone bypasses
/// the trait impl.
fn build_vpe_tensor(
    bindings: &[VisualPromptBinding],
    embedding_dim: usize,
) -> Result<Array3<f32>, InferenceError> {
    let n = bindings.len();
    let mut tensor = Array3::<f32>::zeros((1, n, embedding_dim));
    for (i, b) in bindings.iter().enumerate() {
        if b.embedding.len() != embedding_dim {
            return Err(InferenceError::Failed(format!(
                "vpe tensor: binding {} has {} dims, expected {}",
                i,
                b.embedding.len(),
                embedding_dim
            )));
        }
        for (j, v) in b.embedding.iter().enumerate() {
            tensor[[0, i, j]] = *v;
        }
    }
    Ok(tensor)
}

/// Default IoU for class-aware NMS in visual mode. Same 0.50 as text
/// mode — the YOLOE visual head shares the YOLO-World scoring
/// distribution.
fn default_visual_nms_iou_threshold() -> f32 {
    0.50
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(label: &str, dim: usize, fill: f32) -> VisualPromptBinding {
        VisualPromptBinding {
            label: label.to_string(),
            embedding: Arc::from(vec![fill; dim].into_boxed_slice()),
        }
    }

    #[test]
    fn build_vpe_packs_in_order() {
        let bindings = vec![binding("excavator", 4, 0.25), binding("crane", 4, -0.75)];
        let tensor = build_vpe_tensor(&bindings, 4).expect("ok");
        assert_eq!(tensor.dim(), (1, 2, 4));
        assert!((tensor[[0, 0, 0]] - 0.25).abs() < 1e-6);
        assert!((tensor[[0, 1, 2]] - (-0.75)).abs() < 1e-6);
    }

    #[test]
    fn build_vpe_rejects_mismatched_embedding_dim() {
        let bindings = vec![
            binding("a", 4, 0.0),
            VisualPromptBinding {
                label: "b".into(),
                embedding: Arc::from(vec![0.0; 3].into_boxed_slice()),
            },
        ];
        let err = build_vpe_tensor(&bindings, 4).expect_err("must error");
        match err {
            InferenceError::Failed(msg) => {
                assert!(msg.contains("expected 4"), "got: {msg}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
