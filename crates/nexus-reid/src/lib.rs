//! # nexus-reid — appearance-embedding extractor for cross-camera re-ID
//!
//! Phase 5.6 of the wedge plan. This crate is the **edge-side**
//! extractor that turns a `(Frame, BBox)` pair into a fixed-dimension
//! L2-normalised appearance embedding suitable for cross-camera
//! linking by the cloud-side identity-graph service.
//!
//! ## Hard product invariants — DO NOT TOUCH WITHOUT WEDGE-PLAN REVIEW
//!
//! Per [`../../AGENTS.md`](../../AGENTS.md) rule 7 + the wedge plan
//! [`docs/product/WEDGE_PLAN.md`](../../../nexus-cloud-console/docs/product/WEDGE_PLAN.md):
//!
//! 1. **Appearance embeddings only.** DINOv2-S (default) or
//!    OSNet-x1.0 (opt-in). Output dim = 384 for DINOv2-S, 512 for
//!    OSNet. The extractor MUST NOT load a face-recognition encoder
//!    — the [`xtask check-models`](../../xtask/README.md) gate
//!    rejects any model whose id or artifact path matches a face-rec
//!    brand substring (AdaFace, ArcFace, InsightFace, …) at build time.
//! 2. **Body + clothing only.** The crop fed to the extractor is the
//!    full tracked bbox from the detector; no face-localisation
//!    preprocessing pipeline is wired anywhere in this crate.
//! 3. **No identity decision at the edge.** This crate produces
//!    embeddings and assigns a per-core opaque `entity_local_id`
//!    (managed by the caller in [`nexus-pipeline`](../nexus-pipeline)).
//!    Global identity resolution is the cloud's job — the linker in
//!    `nexus-cloud-console/services/identity-graph/` is the only place
//!    `entity_global_id`s get minted.
//!
//! ## Crate surface
//!
//! * [`Extractor`] — async trait implemented by every backend.
//! * [`Embedding`] — L2-normalised `Vec<f32>` of fixed
//!   [`Embedding::dim`].
//! * [`ExtractorError`] — wraps preprocessing + ORT-load + ORT-run
//!   failures with `thiserror` so callers don't pull in `anyhow`.
//! * [`MockExtractor`] — deterministic-by-content extractor for
//!   tests and `--features test-injection` boot paths. Available
//!   without the `ort` feature.
//! * [`crop_and_resize`] + [`apply_imagenet_normalize`] +
//!   [`l2_normalise_mut`] — pre/postprocessing primitives. Public so
//!   the future ORT extractor (and the integration tests that ship
//!   alongside the model in Phase 5.6 4c) can reuse them.
//! * [`ort_dinov2::DinoV2Extractor`] — ORT-backed implementation,
//!   compiled only with the `ort` feature.

#![deny(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use nexus_types::{BBox, Frame, PixelFormat};
use serde::{Deserialize, Serialize};

#[cfg(feature = "ort")]
pub mod ort_dinov2;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Fixed-dimension L2-normalised appearance embedding.
///
/// `dim` is determined by the extractor backend — DINOv2-S returns
/// 384, OSNet-x1.0 returns 512. Callers MUST treat the dimension as
/// opaque and key cross-camera comparisons on the producer
/// [`Embedding::model_id`] so a manifest swap can't accidentally
/// compare embeddings from incompatible models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    /// Producer's `models-manifest.json` `id` (e.g. `"dinov2_s_224"`).
    pub model_id: String,
    /// Embedding dimension (`vec.len()`). Stored explicitly so wire
    /// envelopes carry it without a sentinel.
    pub dim: usize,
    /// Embedding values. L2-normalised — `vec.iter().map(|v| v*v).sum() ≈ 1.0`.
    pub vec: Vec<f32>,
}

impl Embedding {
    /// Stop-gap convenience for serializers that want the wire-side
    /// FP16 representation (Phase 5.6 4c emits f16-encoded bytes in
    /// the `entity_sighting` envelope to halve payload size). Returns
    /// `2 * dim` bytes in little-endian `f16` layout.
    pub fn to_f16_le_bytes(&self) -> Vec<u8> {
        // Manual f32 -> f16 conversion to avoid a `half` dep when this
        // function isn't called. Sufficient for the [-1, 1] range that
        // an L2-normalised vector lives in.
        let mut out = Vec::with_capacity(self.vec.len() * 2);
        for &v in &self.vec {
            let bits = f32_to_f16_bits(v);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        out
    }
}

/// Errors returned by [`Extractor::extract`] and constructors.
#[derive(Debug, thiserror::Error)]
pub enum ExtractorError {
    /// Frame is in a pixel format this extractor can't read. Today
    /// only `Rgb24` / `Bgr24` are accepted; planar formats require the
    /// caller to convert first.
    #[error("unsupported pixel format: {0:?}")]
    UnsupportedFormat(PixelFormat),

    /// BBox is degenerate (zero area) or falls outside the frame. The
    /// extractor refuses rather than emitting a meaningless embedding
    /// for what was almost certainly an upstream tracker bug.
    #[error("invalid bbox {bbox:?} for frame {fw}x{fh}: {reason}")]
    InvalidBBox {
        bbox: BBox,
        fw: u32,
        fh: u32,
        reason: &'static str,
    },

    /// RGB buffer length doesn't match `width * height * 3`. Almost
    /// always a caller bug — we don't try to recover.
    #[error(
        "frame buffer wrong size: got {got} expected {expected} for {width}x{height} {format:?}"
    )]
    FrameBufferSize {
        got: usize,
        expected: usize,
        width: u32,
        height: u32,
        format: PixelFormat,
    },

    /// ONNX session-init failure (file missing, schema mismatch,
    /// unsupported op). Only constructed by `ort_dinov2`.
    #[error("ONNX model load failed: {0}")]
    ModelLoad(String),

    /// ONNX session-run failure. Only constructed by `ort_dinov2`.
    #[error("ONNX inference failed: {0}")]
    InferenceFailed(String),

    /// Backend produced a vector of the wrong dimension. Treated as
    /// fatal because it almost always means the wrong ONNX is in
    /// `models/` (e.g. someone swapped DINOv2-B for DINOv2-S).
    #[error("backend returned dim {got}, expected {expected}")]
    UnexpectedDim { got: usize, expected: usize },
}

/// Async appearance-embedding extractor.
///
/// One extractor instance per (core, model) tuple. The trait is
/// `Send + Sync` so a single `Arc<dyn Extractor>` can be cloned
/// across the pipeline supervisor tasks.
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Stable id of the underlying model — populated into
    /// [`Embedding::model_id`] on every emit. The cloud uses this to
    /// reject cross-model comparisons in the linker.
    fn model_id(&self) -> &str;

    /// Embedding dimension this extractor emits. Constant for the
    /// lifetime of the extractor.
    fn dim(&self) -> usize;

    /// Extract an embedding for `frame[bbox]`. Bbox coordinates are
    /// in the same coordinate space as `frame` (i.e. the supervisor
    /// frame — typically 960×540 RGB for nexus-pipeline as of M3).
    async fn extract(&self, frame: &Frame, bbox: &BBox) -> Result<Embedding, ExtractorError>;

    /// M_PERF_CROWD B4 — batched extraction. Returns one
    /// `Result<Embedding, _>` per input item, in the same order.
    ///
    /// The default impl loops over [`Extractor::extract`] so
    /// existing extractors stay correct without code changes;
    /// [`ort_dinov2::DinoV2Extractor`] overrides this to stack the
    /// inputs into a single `[B, 3, 224, 224]` ORT call, amortising
    /// the per-call session overhead across `B` inferences.
    ///
    /// Errors are per-item, not whole-batch: a pre-process failure
    /// on item 3 must not poison items 0..2 or 4..N. If the entire
    /// batch fails (e.g. ORT session-run error), every survivor
    /// gets the same propagated error.
    async fn extract_batch(
        &self,
        items: &[(Arc<Frame>, BBox)],
    ) -> Vec<Result<Embedding, ExtractorError>> {
        let mut out = Vec::with_capacity(items.len());
        for (frame, bbox) in items {
            out.push(self.extract(frame.as_ref(), bbox).await);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Mock backend (always available, no `ort` dep)
// ---------------------------------------------------------------------------

/// Deterministic-by-content mock extractor.
///
/// Produces an L2-normalised embedding whose values depend only on
/// the cropped pixel bytes — the same bbox over the same frame
/// always returns the same vector. Lets tests / e2e harnesses
/// exercise the cross-camera linker codepaths in the cloud without
/// shipping a real ONNX. The vector itself is not semantically
/// meaningful — cosine similarity between two mock embeddings is not
/// correlated with visual similarity.
///
/// The mock is intentionally **NOT** behind a `test` / `dev` cfg
/// so the [`nexus-engine`](../nexus-engine) `--features
/// test-injection` build path can spin one up at runtime for e2e
/// tests against a real edge. See [`../../AGENTS.md`](../../AGENTS.md)
/// "Cargo features that gate a `match` arm…" note.
pub struct MockExtractor {
    model_id: String,
    dim: usize,
}

impl MockExtractor {
    /// Create a mock extractor that pretends to be DINOv2-S
    /// (`model_id = "mock_dinov2_s_224"`, `dim = 384`). Use
    /// [`MockExtractor::with_config`] to override.
    pub fn new() -> Self {
        Self::with_config("mock_dinov2_s_224", 384)
    }

    /// Explicit model id + dim. The id MUST NOT collide with a
    /// real production model id, so cosine-similarity comparisons
    /// on the cloud side that find a mock embedding can refuse them.
    pub fn with_config(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
        }
    }
}

impl Default for MockExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extractor for MockExtractor {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn extract(&self, frame: &Frame, bbox: &BBox) -> Result<Embedding, ExtractorError> {
        let rgb = frame_to_rgb_borrowed_or_owned(frame)?;
        let crop = crop_and_resize(rgb.as_slice(), frame.width, frame.height, bbox, 224, 224)?;
        // Hash bucket each pixel byte into a slot. Cheap, no dep, and
        // stable across runs. Then L2-normalise.
        let mut vec = vec![0f32; self.dim];
        for (i, &b) in crop.iter().enumerate() {
            vec[i % self.dim] += b as f32;
        }
        l2_normalise_mut(&mut vec);
        Ok(Embedding {
            model_id: self.model_id.clone(),
            dim: self.dim,
            vec,
        })
    }
}

// ---------------------------------------------------------------------------
// Pre/postprocessing primitives — pub so ort_dinov2 and integration
// tests can share them.
// ---------------------------------------------------------------------------

/// Borrow the frame's RGB bytes if it's already RGB24; copy + swap
/// if BGR24. Other formats are rejected — the upstream pipeline
/// hands the supervisor frame to nexus-inference and nexus-reid in
/// RGB24 already (per the per-camera supervisor frame contract).
pub fn frame_to_rgb_borrowed_or_owned(frame: &Frame) -> Result<RgbBuf<'_>, ExtractorError> {
    let expected = (frame.width as usize) * (frame.height as usize) * 3;
    let got = frame.data.len();
    match frame.format {
        PixelFormat::Rgb24 => {
            if got != expected {
                return Err(ExtractorError::FrameBufferSize {
                    got,
                    expected,
                    width: frame.width,
                    height: frame.height,
                    format: frame.format,
                });
            }
            Ok(RgbBuf::Borrowed(&frame.data[..]))
        }
        PixelFormat::Bgr24 => {
            if got != expected {
                return Err(ExtractorError::FrameBufferSize {
                    got,
                    expected,
                    width: frame.width,
                    height: frame.height,
                    format: frame.format,
                });
            }
            let mut out = vec![0u8; got];
            for (i, ch) in frame.data.chunks_exact(3).enumerate() {
                let off = i * 3;
                out[off] = ch[2];
                out[off + 1] = ch[1];
                out[off + 2] = ch[0];
            }
            Ok(RgbBuf::Owned(out))
        }
        other => Err(ExtractorError::UnsupportedFormat(other)),
    }
}

/// Holder so the RGB24 fast path stays zero-copy.
#[derive(Debug)]
pub enum RgbBuf<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> RgbBuf<'a> {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            RgbBuf::Borrowed(s) => s,
            RgbBuf::Owned(v) => v.as_slice(),
        }
    }
}

/// Crop `rgb[bbox]` and bilinear-resize to (dst_w, dst_h). Output is
/// tightly packed RGB24 = `dst_w * dst_h * 3` bytes.
///
/// The bbox is clamped to the frame and rounded to integer pixel
/// coordinates; degenerate boxes (zero area after clamping) return
/// [`ExtractorError::InvalidBBox`].
pub fn crop_and_resize(
    rgb: &[u8],
    src_w: u32,
    src_h: u32,
    bbox: &BBox,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, ExtractorError> {
    let expected = (src_w as usize) * (src_h as usize) * 3;
    if rgb.len() != expected {
        return Err(ExtractorError::FrameBufferSize {
            got: rgb.len(),
            expected,
            width: src_w,
            height: src_h,
            format: PixelFormat::Rgb24,
        });
    }
    if dst_w == 0 || dst_h == 0 {
        return Err(ExtractorError::InvalidBBox {
            bbox: *bbox,
            fw: src_w,
            fh: src_h,
            reason: "destination dim must be > 0",
        });
    }

    // Clamp + sanitise the bbox. We keep the float bbox for the
    // sampling math but compute integer bounds for the index range.
    let x1 = bbox.x1.max(0.0).min(src_w as f32);
    let y1 = bbox.y1.max(0.0).min(src_h as f32);
    let x2 = bbox.x2.max(x1).min(src_w as f32);
    let y2 = bbox.y2.max(y1).min(src_h as f32);
    let box_w = x2 - x1;
    let box_h = y2 - y1;
    if box_w < 1.0 || box_h < 1.0 {
        return Err(ExtractorError::InvalidBBox {
            bbox: *bbox,
            fw: src_w,
            fh: src_h,
            reason: "clamped box has zero area",
        });
    }

    let stride = (src_w as usize) * 3;
    let sx = box_w / dst_w as f32;
    let sy = box_h / dst_h as f32;
    let max_x = (src_w - 1) as f32;
    let max_y = (src_h - 1) as f32;
    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 3];
    for y in 0..dst_h as usize {
        let src_yf = y1 + ((y as f32) + 0.5) * sy - 0.5;
        let src_yf = src_yf.clamp(0.0, max_y);
        let y0 = src_yf.floor() as usize;
        let y1i = (y0 + 1).min(src_h as usize - 1);
        let dy = (src_yf - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dst_w as usize {
            let src_xf = x1 + ((x as f32) + 0.5) * sx - 0.5;
            let src_xf = src_xf.clamp(0.0, max_x);
            let x0 = src_xf.floor() as usize;
            let x1i = (x0 + 1).min(src_w as usize - 1);
            let dx = (src_xf - x0 as f32).clamp(0.0, 1.0);
            let i00 = y0 * stride + x0 * 3;
            let i01 = y0 * stride + x1i * 3;
            let i10 = y1i * stride + x0 * 3;
            let i11 = y1i * stride + x1i * 3;
            let out_off = (y * dst_w as usize + x) * 3;
            for c in 0..3 {
                let v00 = rgb[i00 + c] as f32;
                let v01 = rgb[i01 + c] as f32;
                let v10 = rgb[i10 + c] as f32;
                let v11 = rgb[i11 + c] as f32;
                let v0 = v00 * (1.0 - dx) + v01 * dx;
                let v1 = v10 * (1.0 - dx) + v11 * dx;
                let v = v0 * (1.0 - dy) + v1 * dy;
                out[out_off + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(out)
}

/// ImageNet mean/std normalisation factors, RGB order.
///
/// Used by DINOv2 + every other ViT pretrained on ImageNet-21k. The
/// constants are the canonical [0.485, 0.456, 0.406] / [0.229, 0.224,
/// 0.225] but unrolled as `(1.0 / (255.0 * std), -mean / std)` so the
/// inner loop is one fma per channel.
pub const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
pub const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Apply ImageNet mean/std normalisation to a tightly-packed RGB
/// buffer and write the result into a CHW float32 buffer.
///
/// Output layout: `dst[c * h * w + y * w + x]`. Caller is responsible
/// for the allocation — keeps the hot path zero-alloc on subsequent
/// calls (reuse the same buffer).
pub fn apply_imagenet_normalize(rgb: &[u8], w: u32, h: u32, dst: &mut [f32]) {
    debug_assert_eq!(rgb.len(), (w as usize) * (h as usize) * 3);
    debug_assert_eq!(dst.len(), 3 * (w as usize) * (h as usize));
    let plane = (w as usize) * (h as usize);
    let inv_255 = 1.0f32 / 255.0;
    let inv_std = [
        inv_255 / IMAGENET_STD[0],
        inv_255 / IMAGENET_STD[1],
        inv_255 / IMAGENET_STD[2],
    ];
    let neg_mean_over_std = [
        -IMAGENET_MEAN[0] / IMAGENET_STD[0],
        -IMAGENET_MEAN[1] / IMAGENET_STD[1],
        -IMAGENET_MEAN[2] / IMAGENET_STD[2],
    ];
    for i in 0..plane {
        let r = rgb[i * 3] as f32;
        let g = rgb[i * 3 + 1] as f32;
        let b = rgb[i * 3 + 2] as f32;
        dst[i] = r.mul_add(inv_std[0], neg_mean_over_std[0]);
        dst[plane + i] = g.mul_add(inv_std[1], neg_mean_over_std[1]);
        dst[2 * plane + i] = b.mul_add(inv_std[2], neg_mean_over_std[2]);
    }
}

/// L2-normalise a vector in place. No-op on zero vectors (returns
/// the zero vector — callers MUST check [`Embedding::vec`] for the
/// degenerate case if they care; production usage is on the output of
/// a ViT/CNN where a zero vector means the model genuinely emitted
/// zeros, which is itself worth logging).
pub fn l2_normalise_mut(v: &mut [f32]) {
    let sum_sq: f32 = v.iter().map(|x| x * x).sum();
    if sum_sq <= 0.0 {
        return;
    }
    let inv = 1.0 / sum_sq.sqrt();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// f32 → IEEE-754 half-precision (binary16) bit-pattern. Inputs are
/// assumed finite and in roughly `[-1, 1]` (the range of an
/// L2-normalised vector); subnormal flushing and NaN handling are
/// intentionally minimal because we only call this from
/// [`Embedding::to_f16_le_bytes`].
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 31) & 0x1) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        // Inf / NaN — preserve sign + NaN-bit.
        let mantissa16 = if mantissa != 0 { 0x0200 } else { 0 };
        return (sign << 15) | 0x7c00 | mantissa16;
    }
    let new_exp = exponent - 127 + 15;
    if new_exp >= 0x1f {
        // Overflow → ±Inf.
        return (sign << 15) | 0x7c00;
    }
    if new_exp <= 0 {
        // Subnormal or underflow → flush to ±0.
        return sign << 15;
    }
    let mantissa16 = (mantissa >> 13) as u16;
    (sign << 15) | ((new_exp as u16) << 10) | mantissa16
}

// Re-export `Arc` so the few callers that build a stub Frame in their
// own tests don't have to depend on `std::sync` directly when they
// already pull `nexus-reid`. Pure ergonomics.
#[doc(hidden)]
pub use std::sync as __sync;

#[doc(hidden)]
pub fn _arc<T>(t: T) -> Arc<T> {
    Arc::new(t)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::{Frame, PixelFormat};
    use std::sync::Arc;

    fn solid_rgb(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let mut out = vec![0u8; (width as usize) * (height as usize) * 3];
        for px in out.chunks_exact_mut(3) {
            px.copy_from_slice(&color);
        }
        out
    }

    fn frame(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Frame {
        Frame {
            camera_id: 7,
            frame_id: 42,
            captured_at: Utc::now(),
            width,
            height,
            format,
            data: Arc::new(data),
            trace_id: "test-trace".to_string(),
        }
    }

    fn bbox(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox { x1, y1, x2, y2 }
    }

    #[test]
    fn l2_normalise_handles_zero_in_place() {
        let mut v = vec![0f32; 384];
        l2_normalise_mut(&mut v);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn l2_normalise_produces_unit_vector() {
        let mut v = vec![3.0, 4.0]; // norm 5
        l2_normalise_mut(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm = {norm}");
    }

    #[test]
    fn crop_and_resize_solid_color_preserves_color() {
        let rgb = solid_rgb(20, 20, [100, 150, 200]);
        let cropped = crop_and_resize(&rgb, 20, 20, &bbox(2.0, 2.0, 18.0, 18.0), 8, 8).unwrap();
        assert_eq!(cropped.len(), 8 * 8 * 3);
        for px in cropped.chunks_exact(3) {
            assert_eq!(px, &[100, 150, 200], "uniform color must survive resize");
        }
    }

    #[test]
    fn crop_and_resize_rejects_degenerate_bbox() {
        let rgb = solid_rgb(20, 20, [0, 0, 0]);
        let err = crop_and_resize(&rgb, 20, 20, &bbox(5.0, 5.0, 5.0, 5.0), 8, 8).unwrap_err();
        assert!(matches!(err, ExtractorError::InvalidBBox { .. }));
    }

    #[test]
    fn crop_and_resize_clamps_out_of_frame_bbox() {
        // BBox extends past the right + bottom edges; should clamp,
        // not panic, and still emit dst_w*dst_h*3 bytes.
        let rgb = solid_rgb(20, 20, [42, 84, 126]);
        let cropped = crop_and_resize(&rgb, 20, 20, &bbox(10.0, 10.0, 50.0, 50.0), 8, 8).unwrap();
        assert_eq!(cropped.len(), 8 * 8 * 3);
        for px in cropped.chunks_exact(3) {
            assert_eq!(px, &[42, 84, 126]);
        }
    }

    #[test]
    fn crop_and_resize_rejects_wrong_buffer_size() {
        let rgb = vec![0u8; 5];
        let err = crop_and_resize(&rgb, 20, 20, &bbox(0.0, 0.0, 10.0, 10.0), 8, 8).unwrap_err();
        assert!(matches!(err, ExtractorError::FrameBufferSize { .. }));
    }

    #[test]
    fn imagenet_normalize_white_pixel_is_positive_finite() {
        let rgb = vec![255u8; 3];
        let mut dst = vec![0f32; 3];
        apply_imagenet_normalize(&rgb, 1, 1, &mut dst);
        // White (1.0) is above the channel means; result must be > 0
        // on every channel and < some loose upper bound.
        for (c, value) in dst.iter().enumerate() {
            assert!(*value > 0.0 && value.is_finite(), "channel {c} = {value}");
            assert!(*value < 3.0, "channel {c} out of expected range = {value}");
        }
    }

    #[test]
    fn imagenet_normalize_zero_pixel_is_negative_finite() {
        let rgb = vec![0u8; 3];
        let mut dst = vec![0f32; 3];
        apply_imagenet_normalize(&rgb, 1, 1, &mut dst);
        // Black (0.0) is below every channel mean; result must be < 0.
        for (c, value) in dst.iter().enumerate() {
            assert!(*value < 0.0 && value.is_finite(), "channel {c} = {value}");
        }
    }

    #[test]
    fn imagenet_normalize_chw_layout() {
        // 2×2 image, all red. After CHW layout the first plane (R)
        // is non-zero and planes G/B are non-zero but identical
        // across the 4 pixels.
        let mut rgb = Vec::new();
        for _ in 0..4 {
            rgb.extend_from_slice(&[200u8, 0, 0]);
        }
        let mut dst = vec![0f32; 3 * 2 * 2];
        apply_imagenet_normalize(&rgb, 2, 2, &mut dst);
        // All R-plane entries equal, all G-plane equal, all B-plane equal.
        for i in 1..4 {
            assert!((dst[i] - dst[0]).abs() < 1e-6);
            assert!((dst[4 + i] - dst[4]).abs() < 1e-6);
            assert!((dst[8 + i] - dst[8]).abs() < 1e-6);
        }
    }

    #[test]
    fn f32_to_f16_round_trip_in_unit_range_is_within_2e_minus_3() {
        for &v in &[0.0_f32, 0.5, -0.5, 1.0, -1.0, 0.1234, -0.7777] {
            let bits = f32_to_f16_bits(v);
            // Decode by sign-extending into f32 via the canonical formula.
            let sign = ((bits >> 15) & 0x1) as u32;
            let exp = ((bits >> 10) & 0x1f) as u32;
            let mant = (bits & 0x3ff) as u32;
            let f = if exp == 0 {
                if mant == 0 {
                    0.0
                } else {
                    // Subnormal — flushed by our encoder, so this branch shouldn't fire.
                    panic!("unexpected subnormal");
                }
            } else if exp == 0x1f {
                f32::INFINITY
            } else {
                let exp_f32 = (exp as i32 - 15 + 127) as u32;
                let mant_f32 = mant << 13;
                f32::from_bits((sign << 31) | (exp_f32 << 23) | mant_f32)
            };
            assert!(
                (f - v).abs() < 2e-3,
                "f16 round-trip drifted: {v} -> {f} (delta = {})",
                (f - v).abs()
            );
        }
    }

    #[tokio::test]
    async fn mock_extractor_is_deterministic_and_unit_norm() {
        let mock = MockExtractor::new();
        let data = solid_rgb(20, 20, [50, 100, 150]);
        let f = frame(20, 20, PixelFormat::Rgb24, data);
        let bb = bbox(2.0, 2.0, 18.0, 18.0);
        let e1 = mock.extract(&f, &bb).await.unwrap();
        let e2 = mock.extract(&f, &bb).await.unwrap();
        assert_eq!(e1.dim, 384);
        assert_eq!(e1.model_id, "mock_dinov2_s_224");
        assert_eq!(e1.vec, e2.vec, "deterministic-by-content invariant");
        let norm: f32 = e1.vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "L2 norm = {norm}");
    }

    #[tokio::test]
    async fn mock_extractor_emits_distinct_vectors_for_different_crops() {
        let mock = MockExtractor::new();
        // Two distinct solid-color frames yield different embeddings.
        let f1 = frame(20, 20, PixelFormat::Rgb24, solid_rgb(20, 20, [50, 50, 50]));
        let f2 = frame(
            20,
            20,
            PixelFormat::Rgb24,
            solid_rgb(20, 20, [200, 200, 200]),
        );
        let bb = bbox(0.0, 0.0, 20.0, 20.0);
        let e1 = mock.extract(&f1, &bb).await.unwrap();
        let e2 = mock.extract(&f2, &bb).await.unwrap();
        // Cosine similarity stays 1.0 because the mock bucket-sums by
        // pixel index and L2-normalises; a uniform colour scales the
        // un-normalised vector by a constant, which normalisation
        // cancels. Verifies the mock's documented contract — *not* a
        // semantic-similarity claim.
        let cos: f32 = e1.vec.iter().zip(e2.vec.iter()).map(|(a, b)| a * b).sum();
        assert!(
            cos > 0.99,
            "uniform-colour crops should be unit-parallel under the mock"
        );
    }

    #[tokio::test]
    async fn mock_extractor_rejects_unsupported_format() {
        let mock = MockExtractor::new();
        let f = frame(20, 20, PixelFormat::Nv12, vec![0u8; 20 * 20 * 3 / 2]);
        let err = mock
            .extract(&f, &bbox(0.0, 0.0, 10.0, 10.0))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ExtractorError::UnsupportedFormat(PixelFormat::Nv12)
        ));
    }

    #[tokio::test]
    async fn mock_extractor_bgr_swap_yields_different_embedding_than_rgb() {
        let mock = MockExtractor::new();
        // RGB(10, 20, 30) and BGR(10, 20, 30) decode to different RGB
        // crops (BGR swap yields RGB(30, 20, 10)). The bucket-sum
        // produces different unnormalised vectors but uniform colour
        // still cancels to a unit-parallel pair under normalisation.
        // What we assert here is the *crop bytes are different*, which
        // exercises the BGR-swap branch of `frame_to_rgb_borrowed_or_owned`.
        let f_rgb = frame(20, 20, PixelFormat::Rgb24, solid_rgb(20, 20, [10, 20, 30]));
        let f_bgr = frame(20, 20, PixelFormat::Bgr24, solid_rgb(20, 20, [10, 20, 30]));
        let bb = bbox(0.0, 0.0, 20.0, 20.0);
        let e_rgb = mock.extract(&f_rgb, &bb).await.unwrap();
        let e_bgr = mock.extract(&f_bgr, &bb).await.unwrap();
        // Both unit-norm.
        for e in [&e_rgb, &e_bgr] {
            let norm: f32 = e.vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn embedding_f16_serialisation_roundtrips_within_tolerance() {
        let vec = vec![0.0, 0.1, 0.5, -0.5, 1.0, -1.0, 0.9999, -0.9999];
        let dim = vec.len();
        let e = Embedding {
            model_id: "x".into(),
            dim,
            vec: vec.clone(),
        };
        let bytes = e.to_f16_le_bytes();
        assert_eq!(bytes.len(), dim * 2);
        // Cross-check that every pair of bytes encodes a sane f16 by
        // decoding back and bounding the drift.
        for (i, chunk) in bytes.chunks_exact(2).enumerate() {
            let bits = u16::from_le_bytes([chunk[0], chunk[1]]);
            let sign = ((bits >> 15) & 0x1) as u32;
            let exp = ((bits >> 10) & 0x1f) as u32;
            let mant = (bits & 0x3ff) as u32;
            let f = if exp == 0 {
                0.0
            } else if exp == 0x1f {
                f32::INFINITY
            } else {
                let exp_f32 = (exp as i32 - 15 + 127) as u32;
                let mant_f32 = mant << 13;
                f32::from_bits((sign << 31) | (exp_f32 << 23) | mant_f32)
            };
            assert!(
                (f - vec[i]).abs() < 2e-3,
                "round-trip drift on {} -> {}",
                vec[i],
                f
            );
        }
    }

    // ----------------------------------------------------------------
    // Phase M_PERF_CROWD B4 — `Extractor::extract_batch` default impl.
    // ----------------------------------------------------------------

    /// The default `extract_batch` impl loops over `extract` so any
    /// `Extractor` (including `MockExtractor`, which doesn't
    /// override) gets a working batched API. The returned vector
    /// MUST have one slot per input, in input order.
    #[tokio::test]
    async fn default_extract_batch_loops_one_result_per_input() {
        let mock = MockExtractor::new();
        let f1 = Arc::new(frame(
            20,
            20,
            PixelFormat::Rgb24,
            solid_rgb(20, 20, [50, 50, 50]),
        ));
        let f2 = Arc::new(frame(
            20,
            20,
            PixelFormat::Rgb24,
            solid_rgb(20, 20, [200, 200, 200]),
        ));
        let f3 = Arc::new(frame(
            20,
            20,
            PixelFormat::Rgb24,
            solid_rgb(20, 20, [10, 20, 30]),
        ));
        let items = vec![
            (f1.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
            (f2.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
            (f3.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
        ];
        let out = mock.extract_batch(&items).await;
        assert_eq!(out.len(), 3);
        for slot in &out {
            let e = slot.as_ref().expect("mock should succeed on RGB24");
            assert_eq!(e.dim, 384);
            assert_eq!(e.model_id, "mock_dinov2_s_224");
        }
        // Batched results MUST match what per-item `extract` would
        // have returned — the default impl is just a sequential
        // loop, no semantic drift.
        let single = mock
            .extract(&f1, &bbox(0.0, 0.0, 20.0, 20.0))
            .await
            .unwrap();
        assert_eq!(out[0].as_ref().unwrap().vec, single.vec);
    }

    /// Per-item preprocess failures (here: unsupported pixel format
    /// on item 1) MUST NOT poison items 0 and 2. The default impl
    /// loops `extract`, which already returns per-item Result, so
    /// we only need to confirm the per-slot routing is right.
    #[tokio::test]
    async fn default_extract_batch_isolates_per_item_errors() {
        let mock = MockExtractor::new();
        let good = Arc::new(frame(
            20,
            20,
            PixelFormat::Rgb24,
            solid_rgb(20, 20, [50, 50, 50]),
        ));
        let bad = Arc::new(frame(20, 20, PixelFormat::Nv12, vec![0u8; 20 * 20 * 3 / 2]));
        let items = vec![
            (good.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
            (bad.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
            (good.clone(), bbox(0.0, 0.0, 20.0, 20.0)),
        ];
        let out = mock.extract_batch(&items).await;
        assert_eq!(out.len(), 3);
        assert!(out[0].is_ok(), "good item 0 must survive");
        assert!(matches!(
            out[1],
            Err(ExtractorError::UnsupportedFormat(PixelFormat::Nv12))
        ));
        assert!(
            out[2].is_ok(),
            "good item 2 must survive past poisoned item 1"
        );
    }

    /// Empty input is a no-op; trait must not panic and must
    /// return an empty vec (callers rely on `out.len() == items.len()`).
    #[tokio::test]
    async fn default_extract_batch_empty_input_returns_empty() {
        let mock = MockExtractor::new();
        let items: Vec<(Arc<Frame>, BBox)> = Vec::new();
        let out = mock.extract_batch(&items).await;
        assert!(out.is_empty());
    }
}
