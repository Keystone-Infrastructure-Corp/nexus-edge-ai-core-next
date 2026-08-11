//! Engine-core smoke for the re-ID appearance embedder: open the REAL
//! ORT-backed `DinoV2Extractor` against the pack's `dinov2_s_224.onnx`
//! and run `.extract()` on a synthetic crop — the same path the engine
//! (`nexus-reid`) uses to produce per-track appearance embeddings.
//! Verifies a finite, 384-dim, L2-normalised CLS token comes back.
//!
//! ```bash
//! ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
//!   cargo test --locked -p nexus-reid --features ep-cpu \
//!   dinov2_core_smoke -- --nocapture
//! ```
#![cfg(feature = "ort")]

use std::path::PathBuf;
use std::sync::Arc;

use nexus_inference::session_tuning::SessionTuning;
use nexus_reid::ort_dinov2::DinoV2Extractor;
use nexus_reid::Extractor;
use nexus_types::{BBox, Frame, PixelFormat};

fn dinov2_onnx() -> PathBuf {
    if let Ok(p) = std::env::var("NEXUS_TEST_PACK") {
        return PathBuf::from(p).join("dinov2_s_224.onnx");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../models")
        .canonicalize()
        .expect("workspace models/ dir must exist")
        .join("dinov2_s_224.onnx")
}

fn synth_frame(w: u32, h: u32) -> Frame {
    let mut data = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 3) as usize;
            let v = ((x + y) % 255) as u8;
            data[off] = v;
            data[off + 1] = 255 - v;
            data[off + 2] = (v / 2).wrapping_add(64);
        }
    }
    Frame {
        camera_id: 1,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(data),
        trace_id: "dinov2-core-smoke".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dinov2_extractor_runs_on_core() {
    let model = dinov2_onnx();
    if !model.exists() {
        eprintln!("[dinov2-core] {} not found, skipping", model.display());
        return;
    }
    let ex = DinoV2Extractor::open(
        &model,
        "dinov2-s-v1",
        &["cpu".into()],
        SessionTuning::default(),
    )
    .expect("dinov2 extractor must open on the core");
    let frame = synth_frame(640, 384);
    let bbox = BBox {
        x1: 40.0,
        y1: 40.0,
        x2: 320.0,
        y2: 300.0,
    };
    let emb = ex.extract(&frame, &bbox).await.expect("extract must run");
    let norm: f32 = emb.vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    eprintln!(
        "[dinov2-core] PASS model_id={} dim={} L2norm={:.5}",
        emb.model_id, emb.dim, norm
    );
    assert_eq!(emb.dim, 384, "DINOv2-S must emit 384-dim CLS token");
    assert_eq!(emb.vec.len(), 384);
    assert!(
        emb.vec.iter().all(|v| v.is_finite()),
        "embedding must be finite"
    );
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "embedding must be L2-normalised, got {norm}"
    );
}
