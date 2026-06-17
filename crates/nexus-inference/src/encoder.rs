//! M3.1 Phase D3: image encoder loader for YOLOE visual prompts.
//!
//! The YOLOE visual-prompt pipeline is a two-ONNX split:
//!
//! * `yoloe26_s_image_encoder.onnx` — image crop → embedding
//!   vector. Runs in the engine process (this module), invoked
//!   from the admin POST `/api/v1/admin/visual-prompts` handler ONCE per
//!   uploaded reference image. The embedding is persisted to
//!   `visual_prompts.embedding_blob`.
//!
//! * `yoloe26_s_vp.onnx` — embedding + frame → detections. Runs
//!   in the worker (Phase E), per-frame, hot path.
//!
//! The split lets the worker stay slim — encoder weights (~50 MB)
//! never get loaded into the per-camera worker process. The
//! encoder lives behind a `OnceCell<Arc<ImageEncoder>>` in the
//! engine state: first POST initialises it, subsequent ones hit
//! the warm session.
//!
//! Design decisions:
//!
//! * **Sync `encode()` under a `Mutex<Session>`** — matches the
//!   YOLO-World detector pattern. The caller wraps in
//!   `tokio::task::spawn_blocking` if it needs to keep the
//!   reactor responsive (admin upload handler does).
//!
//! * **Input shape == 640×640 RGB NCHW float32** — same shape as
//!   the YOLOE detectors. Bilinear resize lives here as a copy
//!   of `yoloe::preprocess_nchw`; deduplicating would require
//!   making the yoloe version `pub`, which we'd rather avoid
//!   until M3.2 stabilises the shape contract.
//!
//! * **Output is a 1-D `Vec<f32>`** of length `embedding_dim`. The
//!   YOLOE-26-S encoder ships with `embedding_dim = 512`. We
//!   read whatever the session reports and validate against the
//!   manifest's declared `embedding_dim` so the operator sees a
//!   loud error if the artifact + manifest fall out of sync.

#![cfg(feature = "ort")]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ndarray::Array4;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info};

use crate::detectors::InferenceError;
use crate::execution_providers;

/// One image-encoder ONNX session. Cheap to clone (the underlying
/// `Session` lives behind a `Mutex` so concurrent admin uploads
/// serialize through it — encoding is fast (~10 ms on CPU for a
/// 640×640 crop), so a single session is plenty).
pub struct ImageEncoder {
    session: Mutex<Session>,
    input_w: u32,
    input_h: u32,
    /// Length of every embedding the session emits. Validated on
    /// first run; the operator-supplied manifest value is the
    /// canonical source of truth.
    embedding_dim: usize,
    /// Stable id for "which encoder produced this embedding". Persisted
    /// alongside `embedding_blob` so the engine can detect drift when
    /// the encoder model rolls forward.
    model_id: String,
    /// Cached for diagnostics.
    _model_path: PathBuf,
}

impl ImageEncoder {
    /// Load the encoder ONNX from the given path. `embedding_dim` is
    /// the value the operator declared in the manifest; the loader
    /// uses it for storage validation on every encode call.
    /// `ep_priority` mirrors the detector's EP selection — `&[]` ==
    /// CPU only, which is what the admin handler typically wants
    /// (it runs on the control plane, not the camera worker).
    pub fn load(
        model_path: &Path,
        embedding_dim: usize,
        model_id: impl Into<String>,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
        Self::load_with_dims(model_path, 640, 640, embedding_dim, model_id, ep_priority)
    }

    /// Same as [`load`] but with explicit input dimensions. Use for
    /// non-default crop sizes (the YOLOE-26-M encoder bumps to
    /// 768×768).
    pub fn load_with_dims(
        model_path: &Path,
        input_w: u32,
        input_h: u32,
        embedding_dim: usize,
        model_id: impl Into<String>,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
        if embedding_dim == 0 {
            return Err(InferenceError::ModelLoad(
                "embedding_dim must be > 0".into(),
            ));
        }
        let (eps, ep_names) = execution_providers::selected_for_priority(ep_priority);
        let session = Session::builder()
            .map_err(|e| InferenceError::ModelLoad(format!("session builder: {e}")))?
            // ORT_ENABLE_ALL (99) — valid on every ONNX Runtime ABI.
            // NOT `Level3`: in ort 2.0-rc that maps to ORT_ENABLE_LAYOUT (3),
            // a level introduced in ONNX Runtime 1.22 that the ROCm 1.21
            // runtime rejects with "graph_optimization_level is not valid".
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| InferenceError::ModelLoad(format!("opt level: {e}")))?
            .with_execution_providers(eps)
            .map_err(|e| InferenceError::ModelLoad(format!("EP register: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| {
                InferenceError::ModelLoad(format!("load {}: {e}", model_path.display()))
            })?;
        let model_id = model_id.into();
        info!(
            model = %model_path.display(),
            input_w, input_h, embedding_dim, model_id = %model_id,
            ep_requested = ?ep_priority,
            ep_registered = ?ep_names,
            "yoloe image encoder ready"
        );
        Ok(Self {
            session: Mutex::new(session),
            input_w,
            input_h,
            embedding_dim,
            model_id,
            _model_path: model_path.to_path_buf(),
        })
    }

    /// The stable id of the loaded encoder. Persist alongside every
    /// embedding so future rolls can be detected.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Declared embedding dimension. Used by the store layer to
    /// validate the BLOB length on every read.
    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// Encode a pre-decoded RGB24 image. `width` and `height` are
    /// the source pixel dimensions; the encoder resamples to its
    /// own `input_w` × `input_h` internally. Caller is responsible
    /// for cropping the reference object out of the source frame
    /// before passing it in — the encoder does not localize.
    ///
    /// This is a sync call wrapping the ORT session under a Mutex.
    /// The admin HTTP handler must wrap the call in
    /// `tokio::task::spawn_blocking` to keep the reactor moving.
    pub fn encode_rgb(
        &self,
        rgb: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>, InferenceError> {
        let expected = (width as usize) * (height as usize) * 3;
        if rgb.len() != expected {
            return Err(InferenceError::Failed(format!(
                "rgb buffer wrong size: got {} expected {} (w={}, h={})",
                rgb.len(),
                expected,
                width,
                height
            )));
        }
        let nchw = preprocess_nchw(rgb, width, height, self.input_w, self.input_h)?;
        let input = TensorRef::from_array_view(nchw.view())
            .map_err(|e| InferenceError::Failed(format!("tensor wrap: {e}")))?;
        let mut sess = self.session.lock();
        let outputs = sess
            .run(ort::inputs![input])
            .map_err(|e| InferenceError::Failed(format!("session run: {e}")))?;
        let (_name, value) = outputs
            .iter()
            .next()
            .ok_or_else(|| InferenceError::Failed("encoder: no outputs".into()))?;
        let view = value
            .try_extract_array::<f32>()
            .map_err(|e| InferenceError::Failed(format!("extract array: {e}")))?;
        // Encoder may emit [1, embedding_dim] or [embedding_dim]. Flatten.
        let values: Vec<f32> = view.iter().copied().collect();
        if values.len() != self.embedding_dim {
            return Err(InferenceError::Failed(format!(
                "encoder emitted {} values but manifest declared embedding_dim={}; \
                 regenerate models-manifest.json or re-export the encoder",
                values.len(),
                self.embedding_dim
            )));
        }
        debug!(
            embedding_dim = values.len(),
            input_w = self.input_w,
            input_h = self.input_h,
            "yoloe image encoder produced embedding"
        );
        Ok(values)
    }
}

/// Bilinear resize RGB → NCHW float32. Duplicate of
/// `yoloe::preprocess_nchw` (intentional — see module-level doc).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_rejects_wrong_buffer_size() {
        let rgb = vec![0u8; 10];
        let err = preprocess_nchw(&rgb, 4, 4, 8, 8).expect_err("must error");
        match err {
            InferenceError::Failed(msg) => assert!(msg.contains("wrong size"), "got {msg}"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn preprocess_handles_identity_scale() {
        let rgb = vec![128u8; 4 * 4 * 3];
        let tensor = preprocess_nchw(&rgb, 4, 4, 4, 4).expect("identity ok");
        assert_eq!(tensor.dim(), (1, 3, 4, 4));
        // 128 / 255 normalize
        let mid = tensor[[0, 0, 2, 2]];
        assert!((mid - (128.0 / 255.0)).abs() < 1e-6, "got {mid}");
    }
}
