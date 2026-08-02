//! YOLO ORT smoke test — loads `models/yolo26n_640.onnx` (or whichever
//! `yolo26n_*` size the env var points at), runs it on a synthesized
//! RGB frame, and verifies the session shape works end-to-end.
//!
//! Skipped automatically when the `ort` feature is off OR
//! `NEXUS_TEST_YOLO_MODEL` is not set / does not exist. Locally:
//!
//! ```bash
//! ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
//!   NEXUS_TEST_YOLO_MODEL=$PWD/models/yolo26n_640.onnx \
//!   cargo test --locked -p nexus-inference --features ort,ep-cpu \
//!   yolo_smoke -- --nocapture
//! ```
//!
//! The assertion is intentionally weak: a uniform grey image will rarely
//! produce a strong detection, so we only require the session ran and
//! returned a `Vec` (possibly empty). If a future change breaks
//! preprocessing or output shape decoding, the call itself will fail.

#![cfg(feature = "ort")]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use nexus_inference::yolo::YoloOrtDetector;
use nexus_inference::Detector;
use nexus_types::{Frame, PixelFormat};

fn model_path() -> Option<PathBuf> {
    let p = std::env::var("NEXUS_TEST_YOLO_MODEL")
        .ok()
        .map(PathBuf::from)?;
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "[yolo_smoke] NEXUS_TEST_YOLO_MODEL={} not found, skipping",
            p.display()
        );
        None
    }
}

fn synth_frame(w: u32, h: u32) -> Frame {
    // Diagonal gradient — enough variation to exercise the resize path
    // without making the test outcome brittle to model behavior.
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
        captured_at: Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(data),
        trace_id: "smoke-trace".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_smoke_runs_on_synthetic_frame() {
    let Some(model) = model_path() else {
        return; // skip when model not staged locally
    };

    let det = YoloOrtDetector::open(
        &model,
        640,
        640,
        0.30,
        &[],
        nexus_inference::session_tuning::SessionTuning::default(),
    )
    .expect("yolo session must open");
    let frame = synth_frame(1280, 720);
    let dets = det.detect(&frame, &[]).await.expect("detect ok");

    eprintln!("[yolo_smoke] returned {} detections", dets.len());
    // Only structural assertions: bbox in-frame, confidence in [0, 1],
    // label in the v1 domain set.
    for d in &dets {
        assert!(d.confidence >= 0.0 && d.confidence <= 1.0, "{d:?}");
        assert!(d.bbox.x1 >= 0.0 && d.bbox.x2 <= frame.width as f32, "{d:?}");
        assert!(
            d.bbox.y1 >= 0.0 && d.bbox.y2 <= frame.height as f32,
            "{d:?}"
        );
        assert!(
            d.label.starts_with("person")
                || d.label.starts_with("vehicle")
                || d.label.starts_with("animal")
                || d.label.starts_with("carried"),
            "unexpected label: {}",
            d.label
        );
    }
}
