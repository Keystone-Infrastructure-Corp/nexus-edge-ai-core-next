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
//!   the YOLOE detectors, and the same bilinear resize
//!   (`crate::yolo::preprocess_nchw`, shared by every ORT model).
//!
//! * **Output is a 1-D `Vec<f32>`** of length `embedding_dim`. The
//!   YOLOE-26-S encoder ships with `embedding_dim = 512`. We
//!   read whatever the session reports and validate against the
//!   manifest's declared `embedding_dim` so the operator sees a
//!   loud error if the artifact + manifest fall out of sync.

#![cfg(feature = "ort")]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info};

use crate::detectors::InferenceError;
use crate::session_tuning::{self, SessionTuning};
use crate::yolo::preprocess_nchw;

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
        // The encoder is an admin-side, one-shot-per-upload session that
        // runs *alongside* the live detector + re-ID sessions. It must
        // not assume it owns the box, so it shares the same conservative
        // threading policy as everything else. There is no operator knob
        // because there is no `[encoder]` config block — the defaults
        // are the right answer for a control-plane session.
        let built =
            session_tuning::build_session(model_path, ep_priority, &SessionTuning::default())
                .map_err(InferenceError::ModelLoad)?;
        let model_id = model_id.into();
        info!(
            model = %model_path.display(),
            input_w, input_h, embedding_dim, model_id = %model_id,
            ep_requested = ?ep_priority,
            ep_registered = ?built.ep_names,
            intra_threads = built.intra_threads,
            "yoloe image encoder ready"
        );
        Ok(Self {
            session: Mutex::new(built.session),
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
