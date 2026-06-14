//! `HailoYoloDetector` — Hailo-8 backed YOLO detector (M_HAILO_EP).
//!
//! Mirrors the `YoloOrtDetector` surface so the router substitutes
//! transparently when the resolved model file ends in `.hef`.
//!
//! Wire path:
//!   * `from_config` defers to `yolo.rs::resolve_hailo_hef`, which
//!     picks the size-matched `yolo26n_<W>_hailo.hef` (pack v4+) or
//!     falls back to the legacy `yolo26n.hef` (pack v3). When that
//!     returns `Some` AND `ep_priority` contains `"hailo"`, this
//!     detector is built. Otherwise `build_detector_for_yolo` keeps
//!     its existing ORT/ONNX path.
//!   * `open(hef_path, frame_w, frame_h, threshold)` opens a HailoRT
//!     `InferSession`. The session owns the vdevice + HEF + vstreams.
//!   * Per-frame: bilinear resize the RGB24 source to the HEF's input
//!     dims (640×640 / 960×960 / 1280×1280), then `infer_blocking`
//!     returns normalized NMS_BY_CLASS detections decoded into our
//!     `Detection` wire type via the COCO→domain label map shared
//!     with `yolo.rs`.
//!
//! Concurrency: `InferSession::infer_blocking` takes `&mut self` so
//! we wrap in a `parking_lot::Mutex` and call from
//! `tokio::task::block_in_place` like the ORT path. **Unlike** the
//! ORT path, the Hailo-8 chip can host only one vdevice at a time
//! when HailoRT is in non-scheduled mode (the default in our HEFs).
//! Every call to `hailo_create_vdevice` claims the single physical
//! device exclusively; a second call from the same process returns
//! `HAILO_OUT_OF_PHYSICAL_DEVICES(74)` and leaves the engine with no
//! working backend.
//!
//! The router with `backend = Pool` + `fail_soft = true` builds N
//! pool detectors + 1 fail-soft fallback, so even the simplest T24
//! config (workers=1) triggers two `build` calls. To make that work
//! without a vdevice-leasing protocol with HailoRT, we keep a
//! process-wide cache of opened detectors keyed by canonical HEF
//! path: pool slot 0 and the fallback share the same Arc; all
//! cameras pointing at the same model share the same Arc. The
//! per-call score_threshold cannot diverge across sharers (the
//! threshold lives inside the cached detector); if a second caller
//! requests a different threshold we log a warning and return the
//! cached detector unchanged. In practice every camera on a host
//! shares the host's single Hailo HEF, and the per-camera operator
//! threshold tuning happens at the rule layer downstream of the
//! detector — so this is the correct sharing boundary.

#![cfg(feature = "ep-hailo")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use nexus_config::InferenceConfig;
use nexus_hailo_backend::{
    decode_detections, Detection as HailoDetection, InferSession, OutputLayout, Telemetry,
};
use nexus_types::{BBox, Detection, Frame, PixelFormat};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::yolo::{bgr_to_rgb, map_coco_to_domain_label};

/// Process-wide cache of live HailoYoloDetector instances, keyed by
/// canonicalized HEF path. Holds `Weak` refs so a detector drops as
/// soon as every router layer that referenced it goes away. Hailo-8
/// allows at most ONE `hailo_create_vdevice` per process, so in
/// steady state there is one entry here — `build_detector_for_hailo_yolo`
/// and `telemetry_snapshot` both consult it.
static SESSION_CACHE: OnceLock<Mutex<HashMap<PathBuf, Weak<HailoYoloDetector>>>> = OnceLock::new();

fn session_cache() -> &'static Mutex<HashMap<PathBuf, Weak<HailoYoloDetector>>> {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hailo-8 YOLO detector.
pub struct HailoYoloDetector {
    session: Mutex<InferSession>,
    /// Network input geometry — set from the HEF, not the operator
    /// config. Reads back as 640×640 / 960×960 / 1280×1280 depending
    /// on which size HEF the pack staged.
    input_w: u32,
    input_h: u32,
    /// Operator-supplied threshold applied AFTER on-chip NMS — the
    /// chip also has its own threshold (defaulted at HEF compile time),
    /// but we honor the engine config so behavior matches the ORT path.
    score_threshold: f32,
    output_layout: OutputLayout,
    _model_path: PathBuf,
}

impl HailoYoloDetector {
    /// Build from a resolved [`InferenceConfig`]. Caller must have
    /// already established that the pack-resolved HEF
    /// (`yolo26n_<W>_hailo.hef` or legacy `yolo26n.hef`) exists —
    /// the dispatcher in `crate::yolo::build_detector_for_yolo` does
    /// that check before calling.
    pub fn from_config(cfg: &InferenceConfig, hef_path: &Path) -> Result<Self, InferenceError> {
        Self::open(hef_path, cfg.model.score_threshold)
    }

    /// Open a session against the given HEF file.
    pub fn open(model_path: &Path, score_threshold: f32) -> Result<Self, InferenceError> {
        let session = InferSession::open(model_path, None, None).map_err(|e| {
            InferenceError::ModelLoad(format!("hailo open {}: {e}", model_path.display()))
        })?;
        let (h, w, c) = session.input_shape();
        if c != 3 {
            return Err(InferenceError::ModelLoad(format!(
                "hailo HEF expects {c}-channel input; YOLO detector requires 3 (RGB)"
            )));
        }
        let output_layout = session.output_layout().clone();
        match &output_layout {
            OutputLayout::NmsByClass { .. }
            | OutputLayout::NmsByScore { .. }
            | OutputLayout::RawYolo26 { .. } => {}
            OutputLayout::Other => {
                return Err(InferenceError::ModelLoad(format!(
                    "hailo HEF {} has an unsupported output layout for YOLO postproc",
                    model_path.display()
                )));
            }
        }
        info!(
            target: "hailo",
            model = %model_path.display(),
            input = format!("{w}x{h}x{c}"),
            output_layout = ?output_layout,
            score_threshold,
            "HailoYoloDetector opened",
        );
        Ok(Self {
            session: Mutex::new(session),
            input_w: w,
            input_h: h,
            score_threshold,
            output_layout,
            _model_path: model_path.to_path_buf(),
        })
    }
}

#[async_trait]
impl Detector for HailoYoloDetector {
    async fn detect(
        &self,
        frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let input_w = self.input_w;
        let input_h = self.input_h;
        let frame_w = frame.width;
        let frame_h = frame.height;
        let score_threshold = self.score_threshold;
        let output_layout = self.output_layout.clone();

        let rgb = match frame.format {
            PixelFormat::Rgb24 => frame.data.as_ref().clone(),
            PixelFormat::Bgr24 => bgr_to_rgb(frame.data.as_ref()),
            other => return Err(InferenceError::UnsupportedFormat(other)),
        };

        let session = &self.session;
        tokio::task::block_in_place(|| {
            let mut sess = session.lock();
            run_hailo(
                &mut sess,
                &rgb,
                frame_w,
                frame_h,
                input_w,
                input_h,
                score_threshold,
                output_layout,
            )
        })
    }

    fn name(&self) -> &'static str {
        "yolo_hailo"
    }
}

fn run_hailo(
    session: &mut InferSession,
    rgb: &[u8],
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    score_threshold: f32,
    output_layout: OutputLayout,
) -> Result<Vec<Detection>, InferenceError> {
    // Preprocess: bilinear resize RGB24 → uint8 NHWC.
    let input = resize_rgb_u8_nhwc(rgb, frame_w, frame_h, input_w, input_h)?;
    let expected = session.input_frame_size();
    if input.len() != expected {
        return Err(InferenceError::Failed(format!(
            "hailo input size mismatch: prepared {} bytes; device expects {}",
            input.len(),
            expected
        )));
    }

    let buffers = session
        .infer_blocking(&input)
        .map_err(|e| InferenceError::Failed(format!("hailo infer: {e}")))?;

    // Decode the per-output buffers. Cap at 1024 detections per frame
    // — far above anything sane; protects against a misconfigured HEF
    // emitting a huge buffer of zeros.
    let raw = decode_detections(buffers, &output_layout, 1024);

    let mut out: Vec<Detection> = Vec::with_capacity(raw.len());
    for det in raw {
        if det.score < score_threshold {
            continue;
        }
        let label = match map_coco_to_domain_label(det.class_id as i32) {
            Some(l) => l,
            None => continue,
        };
        // HailoRT NMS output is normalized to [0,1] against the
        // network input dims (after on-chip resize/letterbox). We
        // ignore the network input dims and rescale directly to
        // frame pixel space.
        let x1 = (det.x_min * frame_w as f32).max(0.0);
        let y1 = (det.y_min * frame_h as f32).max(0.0);
        let x2 = (det.x_max * frame_w as f32).min(frame_w as f32);
        let y2 = (det.y_max * frame_h as f32).min(frame_h as f32);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        out.push(Detection {
            label: label.into(),
            confidence: det.score.clamp(0.0, 1.0),
            bbox: BBox { x1, y1, x2, y2 },
            attributes: Default::default(),
        });
    }

    debug!(out = out.len(), "hailo postprocess done");
    Ok(out)
}

/// Bilinear resize RGB24 → uint8 NHWC. Output buffer is `dst_h * dst_w * 3`
/// bytes, contiguous row-major with channel-last (the layout HailoRT
/// expects for `HAILO_FORMAT_ORDER_NHWC` + `HAILO_FORMAT_TYPE_UINT8`).
fn resize_rgb_u8_nhwc(
    rgb: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, InferenceError> {
    if rgb.len() != (src_w as usize) * (src_h as usize) * 3 {
        return Err(InferenceError::Failed(format!(
            "rgb buffer wrong size: got {} expected {}",
            rgb.len(),
            (src_w as usize) * (src_h as usize) * 3
        )));
    }
    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 3];
    let sx = src_w as f32 / dst_w as f32;
    let sy = src_h as f32 / dst_h as f32;
    let src_stride = src_w as usize * 3;
    let dst_stride = dst_w as usize * 3;

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
            let i00 = y0 * src_stride + x0 * 3;
            let i01 = y0 * src_stride + x1 * 3;
            let i10 = y1 * src_stride + x0 * 3;
            let i11 = y1 * src_stride + x1 * 3;
            let o = y * dst_stride + x * 3;
            for c in 0..3 {
                let v00 = rgb[i00 + c] as f32;
                let v01 = rgb[i01 + c] as f32;
                let v10 = rgb[i10 + c] as f32;
                let v11 = rgb[i11 + c] as f32;
                let v0 = v00 * (1.0 - dx) + v01 * dx;
                let v1 = v10 * (1.0 - dx) + v11 * dx;
                let v = (v0 * (1.0 - dy) + v1 * dy).round().clamp(0.0, 255.0);
                out[o + c] = v as u8;
            }
        }
    }
    Ok(out)
}

/// Same `Arc<dyn Detector>`-returning shape as `build_detector_for_yolo`.
///
/// Implements the process-wide HEF cache described in the module
/// doc: every (canonicalized) HEF path resolves to at most one
/// underlying `HailoYoloDetector`. The cache holds weak refs so the
/// detector drops (releasing the vdevice) once every router layer
/// that referenced it goes away — important for the OTA hot-reload
/// path where a new generation of detectors replaces an old one.
pub fn build_detector_for_hailo_yolo(
    cfg: &InferenceConfig,
    hef_path: &Path,
) -> Result<Arc<dyn Detector>, InferenceError> {
    let cache = session_cache();

    // Canonicalize so symlinks (e.g. `/opt/nexus/current/...`) and
    // relative paths land on the same cache key. Falls back to the
    // raw path if the file is missing — `HailoYoloDetector::open`
    // will then surface the real error.
    let key = hef_path
        .canonicalize()
        .unwrap_or_else(|_| hef_path.to_path_buf());

    let mut guard = cache.lock();
    if let Some(existing) = guard.get(&key).and_then(Weak::upgrade) {
        if (existing.score_threshold - cfg.model.score_threshold).abs() > f32::EPSILON {
            warn!(
                target: "hailo",
                cached = existing.score_threshold,
                requested = cfg.model.score_threshold,
                model = %key.display(),
                "reusing cached HailoYoloDetector despite differing score_threshold — \
                 Hailo-8 hosts only one vdevice per process; downstream rule layer \
                 should apply per-camera thresholds",
            );
        } else {
            debug!(
                target: "hailo",
                model = %key.display(),
                "reusing cached HailoYoloDetector",
            );
        }
        return Ok(existing as Arc<dyn Detector>);
    }

    let detector: Arc<HailoYoloDetector> = match HailoYoloDetector::from_config(cfg, &key) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            // Drop the cache lock before returning the mock so a
            // subsequent successful open can still populate the cache.
            drop(guard);
            warn!("hailo YOLO detector unavailable, falling back to mock: {e}");
            return Ok(Arc::new(crate::detectors::MockDetector::new()));
        }
    };
    guard.insert(key, Arc::downgrade(&detector));
    Ok(detector as Arc<dyn Detector>)
}

/// Read live telemetry from the first live Hailo detector in
/// `SESSION_CACHE` — used by the System tab on the local admin UI.
///
/// Returns `None` when no detector has been built yet (engine starting
/// up before the first pipeline is wired) or when the Hailo path is not
/// the active backend on this host. Returns `Some(Err(...))` when a
/// detector exists but the underlying FFI calls failed; the caller
/// surfaces that as a UI-side reason string.
///
/// In steady state the cache has exactly one entry (single-vdevice
/// constraint), so this walks at most one Weak.
pub fn telemetry_snapshot() -> Option<Result<Telemetry, InferenceError>> {
    let guard = session_cache().lock();
    for weak in guard.values() {
        if let Some(det) = weak.upgrade() {
            let mut session = det.session.lock();
            return Some(
                session
                    .telemetry()
                    .map_err(|e| InferenceError::Failed(format!("hailo telemetry: {e}"))),
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_basic_shape() {
        let rgb: Vec<u8> = (0..(4 * 4)).flat_map(|_| [255u8, 0, 0]).collect();
        let out = resize_rgb_u8_nhwc(&rgb, 4, 4, 2, 2).unwrap();
        assert_eq!(out.len(), 2 * 2 * 3);
        // All red pixels should still be red after resize.
        for y in 0..2 {
            for x in 0..2 {
                let i = (y * 2 + x) * 3;
                assert_eq!(out[i], 255);
                assert_eq!(out[i + 1], 0);
                assert_eq!(out[i + 2], 0);
            }
        }
    }
}
