//! YOLOE (text-mode) ORT smoke test — loads `models/yoloe26_s.onnx` +
//! the matching `models-manifest.json`, runs it on a synthesized RGB
//! frame, and verifies the open-vocab session shape works end-to-end.
//!
//! Skipped automatically when the `ort` feature is off OR
//! `NEXUS_TEST_YOLOE_MODEL` is not set / does not exist. Locally:
//!
//! ```bash
//! ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
//!   NEXUS_TEST_YOLOE_MODEL=$PWD/models/yoloe26_s.onnx \
//!   NEXUS_TEST_YOLOE_MANIFEST=$PWD/models/models-manifest.json \
//!   cargo test --locked -p nexus-inference --features ort,ep-cpu \
//!   yoloe_smoke -- --nocapture
//! ```
//!
//! Like the YOLO-World variant, the assertion is intentionally weak —
//! a synthesized frame rarely fires a strong open-vocab detection, so
//! we only require the session ran and any returned detections are
//! structurally valid and labelled with one of the prompts we passed in.

#![cfg(feature = "ort")]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use nexus_inference::yoloe::{load_vocab_from_manifest_public, YoloeDetector};
use nexus_inference::Detector;
use nexus_types::{Frame, PixelFormat};

fn paths() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var("NEXUS_TEST_YOLOE_MODEL")
        .ok()
        .map(PathBuf::from)?;
    let manifest = std::env::var("NEXUS_TEST_YOLOE_MANIFEST")
        .ok()
        .map(PathBuf::from)?;
    if !model.exists() {
        eprintln!(
            "[yoloe_smoke] NEXUS_TEST_YOLOE_MODEL={} not found, skipping",
            model.display()
        );
        return None;
    }
    if !manifest.exists() {
        eprintln!(
            "[yoloe_smoke] NEXUS_TEST_YOLOE_MANIFEST={} not found, skipping",
            manifest.display()
        );
        return None;
    }
    Some((model, manifest))
}

fn synth_frame(w: u32, h: u32) -> Frame {
    // Same shape as yolo_world_smoke — diagonal gradient + vertical bar
    // through the centre so any class-conditioned detector has at least
    // one structured region to chew on.
    let mut data = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 3) as usize;
            let v = ((x + y) % 255) as u8;
            let bar = (x as i32 - (w / 2) as i32).abs() < 16;
            let r = if bar { 200 } else { v };
            let g = if bar { 50 } else { 255 - v };
            let b = if bar { 50 } else { (v / 2).wrapping_add(64) };
            data[off] = r;
            data[off + 1] = g;
            data[off + 2] = b;
        }
    }
    Frame {
        camera_id: 1,
        frame_id: 1,
        captured_at: Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(data),
        trace_id: "yoloe-smoke".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yoloe_smoke_runs_on_synthetic_frame() {
    let Some((model, manifest)) = paths() else {
        return; // skip when artifacts not staged locally
    };

    let vocab = load_vocab_from_manifest_public(&manifest, "yoloe26_s")
        .expect("vocab must load from manifest");
    assert!(!vocab.is_empty(), "manifest yoloe26_s.prompts[] is empty");

    let det = YoloeDetector::open(&model, 640, 640, 0.10, 0.50, None, vocab.clone(), &[])
        .expect("yoloe session must open");

    let prompts: Vec<String> = vocab.iter().take(3).cloned().collect();
    let frame = synth_frame(1280, 720);
    let dets = det.detect(&frame, &prompts).await.expect("detect ok");

    eprintln!(
        "[yoloe_smoke] {} detections (subset={:?}, vocab_len={})",
        dets.len(),
        prompts,
        vocab.len()
    );

    for d in &dets {
        assert!(d.confidence >= 0.0 && d.confidence <= 1.0, "{d:?}");
        assert!(d.bbox.x1 >= 0.0 && d.bbox.x2 <= frame.width as f32, "{d:?}");
        assert!(
            d.bbox.y1 >= 0.0 && d.bbox.y2 <= frame.height as f32,
            "{d:?}"
        );
        // Label MUST be one of the prompts we asked for; that's the
        // open-vocab subset filter contract.
        assert!(
            prompts.iter().any(|p| p.eq_ignore_ascii_case(&d.label)),
            "label {:?} not in subset {:?}",
            d.label,
            prompts,
        );
    }
}
