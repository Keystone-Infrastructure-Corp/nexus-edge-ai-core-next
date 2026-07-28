//! Real ORT-backed YOLO detector.
//!
//! Mirrors the v1 `nexus-edge-ai-core` `YoloDetector` close enough that
//! the same models/yolo26n_<W>x<H>.onnx + same COCO→domain label table
//! produces equivalent detections — only the host language changes.
//!
//! Wiring (gated by the `ort` cargo feature):
//!
//!   * Construct: `YoloOrtDetector::open(model_path, input_w, input_h, threshold)?`
//!     loads the ONNX file with `Session::builder().commit_from_file(...)`.
//!     The session is held behind a `parking_lot::Mutex` because
//!     `ort::Session::run` takes `&mut self`. One detector instance per
//!     pool slot keeps lock contention to zero.
//!
//!   * Preprocess: plain bilinear resize of the input frame (currently
//!     RGB24 only) to (input_w × input_h), normalize 0..1, NCHW. v1 uses
//!     `cv::dnn::blobFromImage` which is the same path — no letterbox.
//!     Box coordinates therefore scale by the simple
//!     `(image_dim / input_dim)` factor.
//!
//!   * Postprocess: detect output column count and pick layout:
//!     - 6 cols  → NMS-free Ultralytics export (yolo10/11/26):
//!       `[x1, y1, x2, y2, score, class_id]`
//!     - ≥85     → YOLOv5: `[cx, cy, w, h, objectness, c0..cN]`
//!     - else    → YOLOv8: `[cx, cy, w, h, c0..cN]`
//!
//!     The shipped `yolo26n_<W>x<H>.onnx` exports are NMS-free (6 cols),
//!     so that's the hot path.
//!
//!   * Filtering: confidence threshold + COCO→domain label mapping. Every
//!     class id we don't have a domain label for is dropped (v1 does the
//!     same — `mapCocoToDomainLabel` returns "" for unknown ids).

#![cfg(feature = "ort")]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ndarray::{s, Array2, Array4};
use nexus_config::InferenceConfig;
use nexus_types::{BBox, Detection, Frame, PixelFormat};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::execution_providers;

/// Real ORT-backed YOLO detector.
pub struct YoloOrtDetector {
    session: Mutex<Session>,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    /// Cached for diagnostics; not used at inference time.
    _model_path: PathBuf,
}

impl YoloOrtDetector {
    /// Build from a resolved [`InferenceConfig`]. Picks the ONNX artifact
    /// inside `model.pack_path` by matching the configured
    /// `(input_width, input_height)` against the native-16:9 ladder
    /// files the engine ships:
    ///
    ///   * 512×288   → `yolo26n_512x288.onnx`
    ///   * 1024×576  → `yolo26n_1024x576.onnx`
    ///   * 1536×864  → `yolo26n_1536x864.onnx`
    ///   * 2048×1152 → `yolo26n_2048x1152.onnx`
    ///
    /// Any other shape hard-fails unless the dev escape hatch
    /// `NEXUS_ALLOW_MODEL_SIZE_FALLBACK=1` is set (then it falls back to
    /// whichever ladder file exists). Returns `ModelLoad` if none of the
    /// candidates exist — we never silently fall back to mock under prod
    /// config.
    ///
    /// Note: static-shape ONNX is mandatory for the Intel NPU plugin
    /// (which otherwise silently routes every op to CPU, observed on
    /// k13 Lunar Lake under v0.1.18). Dynamic-shape fallback is the
    /// dev escape hatch, not a prod path.
    pub fn from_config(cfg: &InferenceConfig) -> Result<Self, InferenceError> {
        let pack = cfg.model.pack_path.as_ref().ok_or_else(|| {
            InferenceError::ModelLoad(
                "yolo detector needs inference.model.pack_path; \
                 point it at the directory holding the yolo26n_<W>x<H>.onnx files"
                    .into(),
            )
        })?;
        let path = resolve_yolo26n_path(pack, cfg.model.input_width, cfg.model.input_height)?;
        Self::open(
            &path,
            cfg.model.input_width,
            cfg.model.input_height,
            cfg.model.score_threshold,
            &cfg.ep_priority,
        )
    }

    /// Open a session against the given ONNX file. `ep_priority` is
    /// the operator-supplied EP list from `[inference]` in nexus.toml
    /// — see [`crate::execution_providers::selected_for_priority`]
    /// for how it gets translated to ORT dispatchers. Pass `&[]` for
    /// CPU-only (the default fallback path).
    pub fn open(
        model_path: &Path,
        input_w: u32,
        input_h: u32,
        score_threshold: f32,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
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
        // ORT 2.0 silently skips an EP that fails to attach at runtime
        // (no .so / no device). `ep_registered` is the list we *asked*
        // the session builder to register; the actual runtime EP per
        // node isn't surfaced through the ort 2.0 Session API — if a
        // silent fallback is suspected, set `RUST_LOG=ort=debug`.
        info!(
            model = %model_path.display(),
            input_w, input_h, score_threshold,
            ep_requested = ?ep_priority,
            ep_registered = ?ep_names,
            "yolo ORT detector ready"
        );
        Ok(Self {
            session: Mutex::new(session),
            input_w,
            input_h,
            score_threshold,
            _model_path: model_path.to_path_buf(),
        })
    }
}

/// Pick the right `yolo26n_<W>x<H>.onnx` inside `pack` for the requested
/// input shape. **Strict by default**: only the exact-shape file matches.
///
/// Strategy:
///   1. Try the exact-shape file (`yolo26n_<W>x<H>.onnx`). This is the
///      only path that hits in production — the release tarball stages
///      every native-16:9 ladder rung (512x288 … 2048x1152) from
///      `models/`, so every shipped default and every per-camera UI
///      override is satisfiable.
///   2. **Dev escape hatch only.** Cross-shape fallback (use 512x288
///      when 1024x576 was asked for, etc.) is gated behind
///      `NEXUS_ALLOW_MODEL_SIZE_FALLBACK=1`. The reason: when an
///      operator configures a camera at 1024x576 on Lunar Lake but
///      only the 512x288 file is staged, silently degrading either
///      (a) ships a lower-accuracy model the operator did NOT pick, or
///      (b) routes the model through a mismatched-shape pathway which
///      the Intel NPU plugin silently shunts to CPU (observed on Lunar
///      Lake k13 under v0.1.18–v0.1.20). Both are worse failure modes
///      than refusing to start with a clear error pointing at the
///      missing file.
///   3. With the escape hatch on, try every ladder rung in
///      512x288 / 1024x576 / 1536x864 / 2048x1152 order.
///
/// Returns `ModelLoad` if no candidate matches (or only an out-of-size
/// candidate exists with the escape hatch off). The selected path is
/// logged at INFO so operators can see exactly which ONNX served any
/// given camera identity.
fn resolve_yolo26n_path(
    pack: &Path,
    input_w: u32,
    input_h: u32,
) -> Result<PathBuf, InferenceError> {
    // Native 16:9 shapes — the shipped exports are 1×3×H×W on the
    // exact-16:9 ∩ stride-32 ladder. Pick the shape-matched file.
    let primary = pack.join(format!("yolo26n_{input_w}x{input_h}.onnx"));
    if primary.exists() {
        info!(
            requested_w = input_w,
            requested_h = input_h,
            chosen = %primary.display(),
            "yolo: picked shape-matched ONNX"
        );
        return Ok(primary);
    }

    // From here down: only reachable when the operator's pack is
    // missing the shape they asked for. That is a misconfiguration. Hard
    // fail unless the dev escape hatch is set.
    if !size_fallback_allowed() {
        let listing = list_pack_yolo26n_files(pack);
        return Err(InferenceError::ModelLoad(format!(
            "yolo: pack {pack} is missing the shape-matched ONNX \
             `yolo26n_{input_w}x{input_h}.onnx`. Available in pack: [{listing}]. \
             Fix: regenerate the missing file with \
             `python tools/models/gen_yolo26n.py --all-static` and \
             re-stage the model pack. For a dev escape hatch (NOT for \
             production — Intel NPU plugin silently CPU-falls-back on \
             dynamic shapes), set `NEXUS_ALLOW_MODEL_SIZE_FALLBACK=1`.",
            pack = pack.display(),
            input_w = input_w,
            input_h = input_h,
            listing = listing,
        )));
    }

    // Dev mode only. Try every ship-shape file.
    for (w, h) in [(512u32, 288u32), (1024, 576), (1536, 864), (2048, 1152)] {
        let candidate = pack.join(format!("yolo26n_{w}x{h}.onnx"));
        if candidate.exists() {
            warn!(
                requested_w = input_w,
                requested_h = input_h,
                fallback = %candidate.display(),
                "yolo: NEXUS_ALLOW_MODEL_SIZE_FALLBACK=1 — requested \
                 shape has no matching ONNX in pack; falling back to \
                 nearest available. This silently CPU-falls-back on \
                 Intel NPU — do not use in production."
            );
            return Ok(candidate);
        }
    }

    let listing = list_pack_yolo26n_files(pack);
    Err(InferenceError::ModelLoad(format!(
        "no yolo26n_*.onnx in pack {}; expected yolo26n_{input_w}x{input_h}.onnx \
         (or one of yolo26n_{{512x288,1024x576,1536x864,2048x1152}}.onnx). Available: [{}]",
        pack.display(),
        listing
    )))
}

/// `true` iff the operator has explicitly enabled cross-size /
/// dynamic-shape fallback for development. Production must leave this
/// unset so a misconfigured pack fails the engine at boot rather than
/// silently degrading to CPU.
fn size_fallback_allowed() -> bool {
    matches!(
        std::env::var("NEXUS_ALLOW_MODEL_SIZE_FALLBACK").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

/// Enumerate the yolo26n_*.onnx files actually present in a pack.
/// Used to make resolver error messages diagnose themselves —
/// operators can see at a glance what they DID stage when the engine
/// refuses to boot for a missing size.
fn list_pack_yolo26n_files(pack: &Path) -> String {
    let mut found: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(pack) {
        for ent in entries.flatten() {
            if let Some(name) = ent.file_name().to_str() {
                if name.starts_with("yolo26n") && name.ends_with(".onnx") {
                    found.push(name.to_string());
                }
            }
        }
    }
    found.sort();
    if found.is_empty() {
        "<none>".to_string()
    } else {
        found.join(", ")
    }
}

#[async_trait]
impl Detector for YoloOrtDetector {
    async fn detect(
        &self,
        frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        // Take owned copies of everything we need across the runtime hop.
        let input_w = self.input_w;
        let input_h = self.input_h;
        let frame_w = frame.width;
        let frame_h = frame.height;
        let score_threshold = self.score_threshold;
        let format = frame.format;

        let rgb = match format {
            PixelFormat::Rgb24 => frame.data.as_ref().clone(),
            PixelFormat::Bgr24 => bgr_to_rgb(frame.data.as_ref()),
            other => return Err(InferenceError::UnsupportedFormat(other)),
        };

        // ort sessions are !Sync and `run` takes &mut self, so do the
        // work on a blocking thread and acquire the mutex there.
        let session_for_blocking: &Mutex<Session> = &self.session;
        // SAFETY-equivalent: we hand the reference into a blocking task via
        // `tokio::task::block_in_place`, which keeps us on the same thread
        // (no Send required) — this is the cheapest way to call &mut from
        // async without restructuring the Detector trait.
        tokio::task::block_in_place(|| {
            let mut sess = session_for_blocking.lock();
            run_yolo(
                &mut sess,
                &rgb,
                frame_w,
                frame_h,
                input_w,
                input_h,
                score_threshold,
            )
        })
    }

    fn name(&self) -> &'static str {
        "yolo_ort"
    }
}

fn run_yolo(
    session: &mut Session,
    rgb: &[u8],
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
) -> Result<Vec<Detection>, InferenceError> {
    // Preprocess: bilinear resize → NCHW float32 / 255.0.
    let nchw = preprocess_nchw(rgb, frame_w, frame_h, input_w, input_h)?;
    let input = TensorRef::from_array_view(nchw.view())
        .map_err(|e| InferenceError::Failed(format!("tensor wrap: {e}")))?;

    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| InferenceError::Failed(format!("session run: {e}")))?;

    // First output only — yolo26n exports a single det tensor.
    let (_name, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| InferenceError::Failed("no outputs".into()))?;
    let view = value
        .try_extract_array::<f32>()
        .map_err(|e| InferenceError::Failed(format!("extract array: {e}")))?;

    let shape: Vec<usize> = view.shape().to_vec();
    // Orient to [num_anchors, features] regardless of export
    // orientation (shape-dynamic — see `orient_pred_rows`).
    let pred: Array2<f32> = crate::detectors::orient_pred_rows(view).ok_or_else(|| {
        InferenceError::Failed(format!(
            "unexpected output shape {:?} (want 2-D or 3-D)",
            shape
        ))
    })?;

    let (rows, cols) = pred.dim();
    if rows == 0 || cols < 6 {
        return Ok(Vec::new());
    }

    let nms_free = cols == 6;
    let yolov5_layout = cols >= 85;

    let x_scale = frame_w as f32 / input_w as f32;
    let y_scale = frame_h as f32 / input_h as f32;

    let mut out: Vec<Detection> = Vec::with_capacity(16);
    for r in 0..rows {
        let row = pred.slice(s![r, ..]);
        let (class_id, score, x1, y1, x2, y2) = if nms_free {
            // [x1, y1, x2, y2, score, class_id] in input-image space.
            let s = row[4];
            if s < score_threshold {
                continue;
            }
            (row[5] as i32, s, row[0], row[1], row[2], row[3])
        } else {
            let cx = row[0];
            let cy = row[1];
            let w = row[2];
            let h = row[3];
            let (class_id, score) = if yolov5_layout {
                let objectness = row[4];
                if objectness <= 0.0 {
                    continue;
                }
                let mut best_class = 0.0f32;
                let mut best_id: i32 = -1;
                for c in 5..cols {
                    let v = row[c];
                    if v > best_class {
                        best_class = v;
                        best_id = (c - 5) as i32;
                    }
                }
                (best_id, objectness * best_class)
            } else {
                let mut best_class = 0.0f32;
                let mut best_id: i32 = -1;
                for c in 4..cols {
                    let v = row[c];
                    if v > best_class {
                        best_class = v;
                        best_id = (c - 4) as i32;
                    }
                }
                (best_id, best_class)
            };
            if score < score_threshold || class_id < 0 {
                continue;
            }
            (
                class_id,
                score,
                cx - w * 0.5,
                cy - h * 0.5,
                cx + w * 0.5,
                cy + h * 0.5,
            )
        };

        let label = match map_coco_to_domain_label(class_id) {
            Some(l) => l,
            None => continue,
        };

        let bbox = BBox {
            x1: (x1 * x_scale).max(0.0),
            y1: (y1 * y_scale).max(0.0),
            x2: (x2 * x_scale).min(frame_w as f32),
            y2: (y2 * y_scale).min(frame_h as f32),
        };
        if bbox.width() <= 0.0 || bbox.height() <= 0.0 {
            continue;
        }

        out.push(Detection {
            label: label.into(),
            confidence: score.clamp(0.0, 1.0),
            bbox,
            attributes: Default::default(),
        });
    }

    debug!(out = out.len(), rows, cols, "yolo postprocess done");
    Ok(out)
}

/// Bilinear resize RGB → NCHW float32, in-place into a fresh ndarray.
/// Hot path; do as little allocation as possible, no per-pixel allocs.
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
        // Sample center-pixel: src_y = (y + 0.5) * sy - 0.5
        let src_yf = ((y as f32) + 0.5) * sy - 0.5;
        let y0 = src_yf.floor().clamp(0.0, (src_h - 1) as f32) as usize;
        let y1 = (y0 + 1).min(src_h as usize - 1);
        let dy = (src_yf - y0 as f32).clamp(0.0, 1.0);

        for x in 0..dst_w as usize {
            let src_xf = ((x as f32) + 0.5) * sx - 0.5;
            let x0 = src_xf.floor().clamp(0.0, (src_w - 1) as f32) as usize;
            let x1 = (x0 + 1).min(src_w as usize - 1);
            let dx = (src_xf - x0 as f32).clamp(0.0, 1.0);

            // Four-corner indices in the source RGB buffer.
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

pub(crate) fn bgr_to_rgb(buf: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (i, chunk) in buf.chunks_exact(3).enumerate() {
        let off = i * 3;
        out[off] = chunk[2];
        out[off + 1] = chunk[1];
        out[off + 2] = chunk[0];
    }
    out
}

/// COCO class id → Tier 1 domain label. Mirrors the table in
/// nexus-edge-ai-core/src/inference/pipeline_utils.cpp::mapCocoToDomainLabel.
/// Keep in sync with `models/labels.taxonomy.json` (and the v1 table); if
/// you change one, change both.
pub(crate) fn map_coco_to_domain_label(class_id: i32) -> Option<&'static str> {
    Some(match class_id {
        0 => "person",
        1 => "vehicle.bicycle",
        2 => "vehicle.car",
        3 => "vehicle.motorcycle",
        5 => "vehicle.bus",
        7 => "vehicle.truck",
        14 => "animal.bird",
        15 => "animal.cat",
        16 => "animal.dog",
        24 => "carried.backpack",
        26 => "carried.handbag",
        28 => "carried.suitcase",
        _ => return None,
    })
}

/// Holds an `Arc<dyn Detector>` so the `build` path can swap freely
/// between mock and real ORT impls without a typed cast at the call site.
///
/// M_HAILO_EP: if `inference.ep_priority` puts `"hailo"` first AND a
/// shape-matched HEF (`yolo26n_<W>x<H>_hailo.hef`) exists in the model
/// pack, we dispatch to the Hailo-8 backed detector instead of the ORT
/// one. This keeps a single `kind = "yolo"` config entry working for
/// both x86_64 + Intel/AMD GPU boxes (ONNX) and Hailo-8 M.2 boxes (HEF)
/// — the operator just changes `ep_priority` in their tier toml.
pub fn build_detector_for_yolo(cfg: &InferenceConfig) -> Result<Arc<dyn Detector>, InferenceError> {
    #[cfg(feature = "ep-hailo")]
    {
        if let Some(hef) = resolve_hailo_hef(cfg) {
            info!(
                model = %hef.display(),
                ep_priority = ?cfg.ep_priority,
                "yolo dispatch: using Hailo-8 backend"
            );
            // If Hailo open fails, fall through to ORT/CPU chain instead of
            // using mock. This allows `ep_priority = ["hailo", "rocm", "cpu"]`
            // to degrade gracefully: try Hailo, then ROCm, then CPU.
            match crate::hailo_yolo::build_detector_for_hailo_yolo(cfg, &hef) {
                Ok(detector) => return Ok(detector),
                Err(e) => {
                    warn!("hailo backend failed: {e}, falling through to ORT chain");
                    // Continue to ORT path below
                }
            }
        }
    }
    match YoloOrtDetector::from_config(cfg) {
        Ok(d) => Ok(Arc::new(d)),
        Err(e) => {
            warn!("yolo ORT detector unavailable, falling back to mock: {e}");
            Ok(Arc::new(crate::detectors::MockDetector::new()))
        }
    }
}

/// Look for a shape-matched yolo26n HEF artifact next to the ONNX pack.
/// Returns `Some(path)` only when the operator opted in via
/// `ep_priority` AND a usable HEF exists on disk; otherwise falls
/// through to ONNX.
///
/// Resolution: `yolo26n_<W>x<H>_hailo.hef` — exact shape match against
/// `(cfg.model.input_width, cfg.model.input_height)`, so a camera
/// configured at 1024×576 dispatches to that HEF and one at 512×288 to
/// its HEF on the same host. No cross-shape or legacy-square fallback:
/// a missing shape falls through to ONNX rather than silently running a
/// wrong-shape HEF.
///
/// Why the priority + file check: a host that ships the HEF in its
/// model pack but doesn't have a Hailo chip would otherwise silently
/// get the Hailo path and fail to open it. Requiring `ep_priority` to
/// mention "hailo" keeps the dispatcher under operator control.
#[cfg(feature = "ep-hailo")]
fn resolve_hailo_hef(cfg: &InferenceConfig) -> Option<PathBuf> {
    let wants_hailo = cfg
        .ep_priority
        .iter()
        .any(|ep| ep.eq_ignore_ascii_case("hailo"));
    if !wants_hailo {
        return None;
    }
    let pack = cfg.model.pack_path.as_ref()?;
    let w = cfg.model.input_width;
    let h = cfg.model.input_height;
    let shape_matched = pack.join(format!("yolo26n_{w}x{h}_hailo.hef"));
    shape_matched.exists().then_some(shape_matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn preprocess_basic_shape() {
        // 4x4 red image -> 2x2 NCHW
        let rgb: Vec<u8> = (0..(4 * 4)).flat_map(|_| [255u8, 0, 0]).collect();
        let t = preprocess_nchw(&rgb, 4, 4, 2, 2).unwrap();
        assert_eq!(t.shape(), &[1, 3, 2, 2]);
        // Channel 0 (R) should be ~1.0 everywhere.
        for y in 0..2 {
            for x in 0..2 {
                assert!((t[[0, 0, y, x]] - 1.0).abs() < 1e-3);
                assert!(t[[0, 1, y, x]].abs() < 1e-3);
                assert!(t[[0, 2, y, x]].abs() < 1e-3);
            }
        }
    }

    #[test]
    fn coco_table_known_ids() {
        assert_eq!(map_coco_to_domain_label(0), Some("person"));
        assert_eq!(map_coco_to_domain_label(2), Some("vehicle.car"));
        assert_eq!(map_coco_to_domain_label(99), None);
    }

    /// Guard the env var so tests don't leak `NEXUS_ALLOW_MODEL_SIZE_FALLBACK`
    /// into each other when run by `cargo test` (which interleaves tests
    /// on the same process). Stores the prior value and restores it on
    /// Drop. Tests using the guard MUST acquire `ENV_LOCK` first.
    struct EnvVarGuard {
        key: &'static str,
        prior: Option<String>,
    }
    impl EnvVarGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prior = std::env::var(key).ok();
            // SAFETY: Single-threaded under ENV_LOCK; env mutation is
            // benign at process scope.
            unsafe {
                std::env::set_var(key, val);
            }
            Self { key, prior }
        }
        fn unset(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prior }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    // Single global lock around tests that touch the
    // NEXUS_ALLOW_MODEL_SIZE_FALLBACK env var. Avoids cross-test
    // pollution since cargo test runs in one process by default.
    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"fake-onnx").unwrap();
    }

    #[test]
    fn resolver_picks_exact_shape_match() {
        let _g = ENV_LOCK.lock();
        let _strict = EnvVarGuard::unset("NEXUS_ALLOW_MODEL_SIZE_FALLBACK");
        let tmp = TempDir::new().unwrap();
        let pack = tmp.path();
        touch(pack, "yolo26n_512x288.onnx");
        touch(pack, "yolo26n_1024x576.onnx");
        touch(pack, "yolo26n_1536x864.onnx");

        let p = resolve_yolo26n_path(pack, 1024, 576).unwrap();
        assert_eq!(p, pack.join("yolo26n_1024x576.onnx"));
    }

    #[test]
    fn resolver_strict_refuses_cross_shape_fallback() {
        let _g = ENV_LOCK.lock();
        let _strict = EnvVarGuard::unset("NEXUS_ALLOW_MODEL_SIZE_FALLBACK");
        let tmp = TempDir::new().unwrap();
        let pack = tmp.path();
        // Pack only has 512x288 but operator asks for 1024x576.
        touch(pack, "yolo26n_512x288.onnx");

        let err = resolve_yolo26n_path(pack, 1024, 576).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("yolo26n_1024x576.onnx"),
            "error should name the missing file: {msg}"
        );
        assert!(
            msg.contains("yolo26n_512x288.onnx"),
            "error should list what IS staged so operators can self-diagnose: {msg}"
        );
        assert!(
            msg.contains("NEXUS_ALLOW_MODEL_SIZE_FALLBACK"),
            "error should mention the dev escape hatch: {msg}"
        );
    }

    #[test]
    fn resolver_dev_escape_hatch_allows_cross_shape_fallback() {
        let _g = ENV_LOCK.lock();
        let _on = EnvVarGuard::set("NEXUS_ALLOW_MODEL_SIZE_FALLBACK", "1");
        let tmp = TempDir::new().unwrap();
        let pack = tmp.path();
        touch(pack, "yolo26n_512x288.onnx");

        // With escape hatch on, asking for 1024x576 falls back to the
        // nearest available ladder rung (512x288).
        let p = resolve_yolo26n_path(pack, 1024, 576).unwrap();
        assert_eq!(p, pack.join("yolo26n_512x288.onnx"));
    }

    #[test]
    fn resolver_hard_fails_on_empty_pack_even_with_escape_hatch() {
        let _g = ENV_LOCK.lock();
        let _on = EnvVarGuard::set("NEXUS_ALLOW_MODEL_SIZE_FALLBACK", "1");
        let tmp = TempDir::new().unwrap();
        let pack = tmp.path();
        // No files staged at all.
        let err = resolve_yolo26n_path(pack, 512, 288).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no yolo26n_*.onnx in pack"), "{msg}");
    }
}
