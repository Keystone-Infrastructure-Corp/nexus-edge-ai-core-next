//! ORT-backed DINOv2-S appearance embedding extractor.
//!
//! Compiled only with `--features ort`. The session is opened once
//! at construction time and reused across calls behind a
//! [`parking_lot::Mutex`] — ORT sessions are not `Sync` so the
//! mutex is mandatory, but ORT also blocks the OS thread on
//! `session.run()` so we wrap the call in
//! [`tokio::task::block_in_place`] to keep the runtime healthy.
//!
//! **No model file ships with this crate.** The ONNX comes from
//! `models/dinov2_s_224.onnx` of the engine's model pack — landing
//! that file (along with the matching `models-manifest.json` entry,
//! the `pack_version` bump, and the `.github/workflows/release.yml`
//! asset list patch) is Phase 5.6 slice 4c. Until then, calling
//! [`DinoV2Extractor::open`] against a non-existent file simply
//! returns [`ExtractorError::ModelLoad`] without panicking, which
//! is exactly what the unit tests in this module exercise.

// Same opt-in as `nexus_inference::yoloe` — `ort::inputs!` expands
// to unsafe blocks. Nothing in this module touches `unsafe` directly.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ndarray::Array4;
use nexus_inference::execution_providers;
use nexus_types::{BBox, Frame};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info};

use crate::{
    apply_imagenet_normalize, crop_and_resize, frame_to_rgb_borrowed_or_owned, l2_normalise_mut,
    Embedding, Extractor, ExtractorError,
};

/// DINOv2-S backbone CLS-token extractor.
///
/// Input shape: `[1, 3, 224, 224]` float32 (ImageNet mean/std
/// normalised). Output shape: `[1, 384]` float32 (CLS token). The
/// output is L2-normalised in postprocessing so callers can use plain
/// dot product as cosine similarity.
pub struct DinoV2Extractor {
    session: Mutex<Session>,
    model_id: String,
    input_w: u32,
    input_h: u32,
    expected_dim: usize,
    _model_path: PathBuf,
}

impl DinoV2Extractor {
    /// Open a DINOv2-S ORT session.
    ///
    /// * `model_path` — path to the ONNX, normally
    ///   `models/dinov2_s_224.onnx` resolved from the engine config.
    /// * `model_id` — model id from `models-manifest.json` (carried
    ///   into every [`Embedding::model_id`]). Typically
    ///   `"dinov2_s_224"`.
    /// * `ep_priority` — execution-provider priority list, same
    ///   semantics as the nexus-inference detectors. Pass `&[]` for
    ///   CPU-only.
    pub fn open(
        model_path: &Path,
        model_id: impl Into<String>,
        ep_priority: &[String],
    ) -> Result<Self, ExtractorError> {
        let (eps, ep_names) = execution_providers::selected_for_priority(ep_priority);
        let session = Session::builder()
            .map_err(|e| ExtractorError::ModelLoad(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| ExtractorError::ModelLoad(format!("opt level: {e}")))?
            .with_execution_providers(eps)
            .map_err(|e| ExtractorError::ModelLoad(format!("EP register: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| {
                ExtractorError::ModelLoad(format!("load {}: {e}", model_path.display()))
            })?;
        let model_id = model_id.into();
        info!(
            model = %model_path.display(),
            model_id = %model_id,
            input_w = 224,
            input_h = 224,
            ep_requested = ?ep_priority,
            ep_registered = ?ep_names,
            "dinov2 ORT extractor ready"
        );
        Ok(Self {
            session: Mutex::new(session),
            model_id,
            input_w: 224,
            input_h: 224,
            expected_dim: 384,
            _model_path: model_path.to_path_buf(),
        })
    }
}

#[async_trait]
impl Extractor for DinoV2Extractor {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.expected_dim
    }

    async fn extract(&self, frame: &Frame, bbox: &BBox) -> Result<Embedding, ExtractorError> {
        let rgb = frame_to_rgb_borrowed_or_owned(frame)?;
        let crop = crop_and_resize(
            rgb.as_slice(),
            frame.width,
            frame.height,
            bbox,
            self.input_w,
            self.input_h,
        )?;
        let mut nchw_flat = vec![0f32; 3 * (self.input_w as usize) * (self.input_h as usize)];
        apply_imagenet_normalize(&crop, self.input_w, self.input_h, &mut nchw_flat);
        let nchw = Array4::<f32>::from_shape_vec(
            (1, 3, self.input_h as usize, self.input_w as usize),
            nchw_flat,
        )
        .map_err(|e| ExtractorError::InferenceFailed(format!("shape into ndarray: {e}")))?;

        let model_id = self.model_id.clone();
        let expected_dim = self.expected_dim;
        let session_for_blocking: &Mutex<Session> = &self.session;
        let result = tokio::task::block_in_place(|| -> Result<Vec<f32>, ExtractorError> {
            let mut sess = session_for_blocking.lock();
            run_dinov2(&mut sess, &nchw, expected_dim)
        })?;

        debug!(model_id = %model_id, dim = result.len(), "dinov2 embedding emitted");
        let mut vec = result;
        l2_normalise_mut(&mut vec);
        Ok(Embedding {
            model_id,
            dim: vec.len(),
            vec,
        })
    }

    /// M_PERF_CROWD B4 — batched extraction. Stacks the input crops
    /// into one `(B, 3, 224, 224)` tensor and issues a single ORT
    /// call, amortising the ~3-5ms per-call session overhead across
    /// `B` inferences. At `B=16` on the N150 iGPU the per-crop cost
    /// drops from ~8ms to ~3ms.
    ///
    /// Per-item preprocessing errors (unsupported pixel format,
    /// invalid bbox, frame buffer size mismatch) do NOT poison the
    /// rest of the batch — they're reported in the slot
    /// corresponding to the failing input and the surviving items
    /// proceed to the single batched ORT call. A whole-batch ORT
    /// failure (session-run error) is propagated to every survivor
    /// slot (preprocess-failed slots keep their original error).
    async fn extract_batch(
        &self,
        items: &[(Arc<Frame>, BBox)],
    ) -> Vec<Result<Embedding, ExtractorError>> {
        if items.is_empty() {
            return Vec::new();
        }
        // Phase 1 — per-item preprocess. We keep `Vec<f32>` for
        // successes and the original error for failures so the
        // result order matches `items` exactly.
        let per_item_floats: usize = 3 * (self.input_w as usize) * (self.input_h as usize);
        let mut prep: Vec<Result<Vec<f32>, ExtractorError>> = Vec::with_capacity(items.len());
        for (frame, bbox) in items {
            let r = (|| -> Result<Vec<f32>, ExtractorError> {
                let rgb = frame_to_rgb_borrowed_or_owned(frame)?;
                let crop = crop_and_resize(
                    rgb.as_slice(),
                    frame.width,
                    frame.height,
                    bbox,
                    self.input_w,
                    self.input_h,
                )?;
                let mut nchw_flat = vec![0f32; per_item_floats];
                apply_imagenet_normalize(&crop, self.input_w, self.input_h, &mut nchw_flat);
                Ok(nchw_flat)
            })();
            prep.push(r);
        }

        // Phase 2 — gather survivors into a single (B, 3, H, W)
        // tensor. `survivor_idx[k]` = original item index of the
        // k-th batched input.
        let mut survivor_idx: Vec<usize> = Vec::with_capacity(prep.len());
        let mut survivor_data: Vec<f32> = Vec::with_capacity(prep.len() * per_item_floats);
        for (i, slot) in prep.iter().enumerate() {
            if let Ok(v) = slot {
                survivor_idx.push(i);
                survivor_data.extend_from_slice(v);
            }
        }

        // Short-circuit: no preprocess survivors — just map prep
        // errors out (no ORT call needed).
        if survivor_idx.is_empty() {
            return prep
                .into_iter()
                .map(|p| match p {
                    Err(e) => Err(e),
                    Ok(_) => unreachable!("survivor_idx empty implies all prep errored"),
                })
                .collect();
        }

        // Phase 3 — single batched ORT call.
        let batch = survivor_idx.len();
        let nchw = match Array4::<f32>::from_shape_vec(
            (batch, 3, self.input_h as usize, self.input_w as usize),
            survivor_data,
        ) {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("shape into ndarray (batched): {e}");
                return splice_batch_error(prep, &survivor_idx, &msg);
            }
        };
        let model_id = self.model_id.clone();
        let expected_dim = self.expected_dim;
        let session_for_blocking: &Mutex<Session> = &self.session;
        let batched_result =
            tokio::task::block_in_place(|| -> Result<Vec<Vec<f32>>, ExtractorError> {
                let mut sess = session_for_blocking.lock();
                run_dinov2_batch(&mut sess, &nchw, expected_dim)
            });
        let survivors = match batched_result {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                return splice_batch_error(prep, &survivor_idx, &msg);
            }
        };

        debug!(
            model_id = %model_id,
            batch = batch,
            dim = expected_dim,
            "dinov2 embeddings batched"
        );

        // Phase 4 — stitch results back into the original input
        // order. Survivor slots get L2-normalised embeddings; prep
        // failure slots keep their original error.
        debug_assert_eq!(survivors.len(), batch);
        let mut survivor_iter = survivors.into_iter();
        let mut out: Vec<Result<Embedding, ExtractorError>> = Vec::with_capacity(prep.len());
        for slot in prep.into_iter() {
            match slot {
                Err(e) => out.push(Err(e)),
                Ok(_) => {
                    let mut vec = survivor_iter
                        .next()
                        .expect("survivor_iter must match survivor_idx len");
                    l2_normalise_mut(&mut vec);
                    out.push(Ok(Embedding {
                        model_id: model_id.clone(),
                        dim: vec.len(),
                        vec,
                    }));
                }
            }
        }
        out
    }
}

/// Replace each survivor's slot with a freshly-constructed
/// `InferenceFailed(msg)` while leaving preprocess-failure slots
/// untouched. Used when the batched ORT call (or its shape-build
/// precondition) fails as a whole.
fn splice_batch_error(
    prep: Vec<Result<Vec<f32>, ExtractorError>>,
    survivor_idx: &[usize],
    msg: &str,
) -> Vec<Result<Embedding, ExtractorError>> {
    let survivor_set: std::collections::HashSet<usize> = survivor_idx.iter().copied().collect();
    prep.into_iter()
        .enumerate()
        .map(|(i, slot)| match slot {
            Err(e) => Err(e),
            Ok(_) => {
                debug_assert!(survivor_set.contains(&i));
                Err(ExtractorError::InferenceFailed(msg.to_string()))
            }
        })
        .collect()
}

/// M_PERF_CROWD B4 — batched DINOv2 inference. Accepts a
/// `(B, 3, 224, 224)` tensor and returns `B` raw (not yet
/// L2-normalised) embeddings in input order. Caller normalises.
pub fn run_dinov2_batch(
    session: &mut Session,
    nchw: &Array4<f32>,
    expected_dim: usize,
) -> Result<Vec<Vec<f32>>, ExtractorError> {
    let batch = nchw.shape()[0];
    let input = TensorRef::from_array_view(nchw.view())
        .map_err(|e| ExtractorError::InferenceFailed(format!("tensor wrap (batched): {e}")))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| ExtractorError::InferenceFailed(format!("session run (batched): {e}")))?;
    let (_name, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| ExtractorError::InferenceFailed("no outputs (batched)".into()))?;
    let view = value
        .try_extract_array::<f32>()
        .map_err(|e| ExtractorError::InferenceFailed(format!("extract array (batched): {e}")))?;
    let shape: Vec<usize> = view.shape().to_vec();
    let flat: Vec<f32> = view.iter().copied().collect();
    // Acceptable batched output shapes mirror the single-call ones:
    //   [B, 384]                       — bare CLS token (preferred)
    //   [B, N, 384] where N=patches    — last_hidden_state; CLS is patch 0
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(batch);
    match shape.as_slice() {
        [b, d] if *b == batch && *d == expected_dim => {
            for i in 0..batch {
                let start = i * expected_dim;
                out.push(flat[start..start + expected_dim].to_vec());
            }
        }
        [b, patches, d] if *b == batch && *d == expected_dim => {
            let stride = patches * expected_dim;
            for i in 0..batch {
                let start = i * stride;
                out.push(flat[start..start + expected_dim].to_vec());
            }
        }
        other => {
            return Err(ExtractorError::UnexpectedDim {
                got: other.iter().product(),
                expected: batch * expected_dim,
            });
        }
    }
    Ok(out)
}

/// Single inference step. Public for the integration tests that ship
/// alongside the model in 5.6 4c.
pub fn run_dinov2(
    session: &mut Session,
    nchw: &Array4<f32>,
    expected_dim: usize,
) -> Result<Vec<f32>, ExtractorError> {
    let input = TensorRef::from_array_view(nchw.view())
        .map_err(|e| ExtractorError::InferenceFailed(format!("tensor wrap: {e}")))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| ExtractorError::InferenceFailed(format!("session run: {e}")))?;

    // DINOv2-S exports a single output named "cls_token" (or
    // "last_hidden_state" depending on export config) of shape [1, 384].
    let (_name, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| ExtractorError::InferenceFailed("no outputs".into()))?;
    let view = value
        .try_extract_array::<f32>()
        .map_err(|e| ExtractorError::InferenceFailed(format!("extract array: {e}")))?;
    let shape: Vec<usize> = view.shape().to_vec();

    // Acceptable output shapes:
    //   [1, 384]                       — bare CLS token (preferred)
    //   [1, N, 384] where N=patch_count — last_hidden_state; CLS is index 0
    let v: Vec<f32> = match shape.as_slice() {
        [1, d] if *d == expected_dim => view.iter().copied().collect(),
        [1, _patches, d] if *d == expected_dim => {
            // Grab CLS token (first patch slot).
            view.iter().take(expected_dim).copied().collect()
        }
        other => {
            return Err(ExtractorError::UnexpectedDim {
                got: other.iter().product(),
                expected: expected_dim,
            });
        }
    };

    if v.len() != expected_dim {
        return Err(ExtractorError::UnexpectedDim {
            got: v.len(),
            expected: expected_dim,
        });
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn open_missing_model_returns_model_load_error_not_panic() {
        let bogus = PathBuf::from("/tmp/__nexus_reid_does_not_exist__.onnx");
        let result = DinoV2Extractor::open(&bogus, "dinov2_s_224", &[]);
        match result {
            Err(ExtractorError::ModelLoad(msg)) => {
                assert!(
                    msg.contains("__nexus_reid_does_not_exist__"),
                    "error msg should reference path: {msg}"
                );
            }
            Err(other) => panic!("expected ModelLoad, got {other:?}"),
            Ok(_) => panic!("expected ModelLoad error, got Ok"),
        }
    }
}
