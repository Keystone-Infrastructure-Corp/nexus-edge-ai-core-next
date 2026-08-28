//! Phase M_PERF_CROWD A1 — verify the FP16 wire encoding for
//! `entity_sighting.embedding_b64` round-trips with cosine similarity
//! ≥ 0.9995 against the source FP32 embedding. The 0.9995 floor is
//! the contract documented in `docs/edge-core/M_PERF_CROWD.md` §A1.
//!
//! The wire encoder lives in `nexus_reid::Embedding::to_f16_le_bytes`
//! (engine path) and is also mirrored verbatim inside
//! `nexus-cloud-client/src/sink.rs::f32_to_f16_bits` (sink path). The
//! cloud decoder lives in
//! `nexus-cloud-console/services/edge-gateway/src/entity_sighting.rs::f16_bits_to_f32`.
//! This test exercises only the engine path; the cloud-side decode is
//! covered by `nexus-cloud-console` PR #58's gateway tests.

use nexus_reid::{Embedding, Extractor, MockExtractor};
use nexus_types::{BBox, Frame, PixelFormat};
use std::sync::Arc;

/// IEEE-754 binary16 → f32. Inverse of
/// `nexus_reid::f32_to_f16_bits`; kept private to this test so the
/// helper isn't accidentally pulled into production code (engine
/// only encodes; the cloud decodes).
fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) & 0x1;
    let exp5 = u32::from((bits >> 10) & 0x1f);
    let mant10 = u32::from(bits & 0x03ff);
    let f32_bits = if exp5 == 0 && mant10 == 0 {
        sign << 31
    } else if exp5 == 0x1f {
        // ±Inf / NaN
        (sign << 31) | (0xff << 23) | (mant10 << 13)
    } else if exp5 == 0 {
        // Subnormal half — flush to zero (matches encoder).
        sign << 31
    } else {
        let exp32 = exp5 + (127 - 15);
        (sign << 31) | (exp32 << 23) | (mant10 << 13)
    };
    f32::from_bits(f32_bits)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn solid_rgb(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
    let mut out = vec![0u8; (w as usize) * (h as usize) * 3];
    for px in out.as_chunks_mut::<3>().0 {
        px.copy_from_slice(&color);
    }
    out
}

fn frame(w: u32, h: u32, color: [u8; 3]) -> Frame {
    Frame {
        camera_id: 1,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(solid_rgb(w, h, color)),
        trace_id: "fp16-parity".into(),
    }
}

fn roundtrip(emb: &Embedding) -> Vec<f32> {
    let bytes = emb.to_f16_le_bytes();
    assert_eq!(bytes.len(), emb.vec.len() * 2);
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| f16_bits_to_f32(u16::from_le_bytes(*c)))
        .collect()
}

#[tokio::test]
async fn fp16_roundtrip_preserves_cosine_above_0_9995() {
    // Three colors → three different "embeddings" out of the mock
    // extractor (which hashes the cropped pixels). Picks corners
    // of the unit sphere that aren't axis-aligned to make sure the
    // mantissa-truncation error stresses every dim.
    let extractor = MockExtractor::with_config("dinov2-s-v1", 384);
    let bbox = BBox {
        x1: 20.0,
        y1: 20.0,
        x2: 200.0,
        y2: 200.0,
    };
    for color in [[200, 30, 30], [60, 220, 90], [50, 80, 240]] {
        let f = frame(256, 256, color);
        let emb = extractor.extract(&f, &bbox).await.expect("extract ok");
        let decoded = roundtrip(&emb);
        let cos = cosine(&emb.vec, &decoded);
        assert!(
            cos >= 0.9995,
            "FP16 roundtrip cosine {cos} < 0.9995 for color {color:?}"
        );
    }
}

#[test]
fn fp16_handles_zero_and_unit_vectors() {
    // Zero vector → all zero bytes → decode back to zeros.
    let zero = Embedding {
        model_id: "dinov2-s-v1".into(),
        dim: 8,
        vec: vec![0.0; 8],
    };
    let decoded = roundtrip(&zero);
    assert!(decoded.iter().all(|x| *x == 0.0));

    // Sparse unit vector — every component representable exactly
    // in FP16 (powers of 2 in [-1, 1]).
    let unit = Embedding {
        model_id: "dinov2-s-v1".into(),
        dim: 4,
        vec: vec![1.0, -0.5, 0.25, -0.125],
    };
    let decoded = roundtrip(&unit);
    assert_eq!(decoded, unit.vec, "powers of 2 round-trip bit-exact");
}
