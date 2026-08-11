//! Engine-core inference smoke: for every detector kind on the native
//! 16:9 ladder, build the REAL engine detector via `*::from_config` (the
//! same loader `nexus_inference::build` uses — resolves the shape-matched
//! ONNX from the pack + reads the vocab from `models-manifest.json`) and
//! run `.detect()` on a synthetic frame. Unlike `build_detector_for_*`,
//! `from_config` returns `Result`, so a model that fails to load fails the
//! test loudly instead of silently degrading to `MockDetector`.
//!
//! Skips Hailo HEFs (card not yet available). Run:
//! ```bash
//! ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
//!   cargo test --locked -p nexus-inference --features ort,ep-cpu \
//!   pack_core_smoke -- --nocapture
//! ```
#![cfg(feature = "ort")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use nexus_config::{InferenceBackendKind, InferenceConfig, ModelConfig, PoolWorkerKind};
use nexus_inference::yolo::YoloOrtDetector;
use nexus_inference::yolo_world::YoloWorldDetector;
use nexus_inference::yoloe::YoloeDetector;
use nexus_inference::Detector;
use nexus_types::{Frame, PixelFormat};

fn pack_dir() -> PathBuf {
    if let Ok(p) = std::env::var("NEXUS_TEST_PACK") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../models")
        .canonicalize()
        .expect("workspace models/ dir must exist")
}

fn mk_cfg(kind: &str, w: u32, h: u32, pack: &Path) -> InferenceConfig {
    InferenceConfig {
        backend: InferenceBackendKind::InProcess,
        pool_worker_kind: PoolWorkerKind::Thread,
        workers: 1,
        restart_backoff_ms: 0,
        fail_soft: false,
        ep_priority: vec!["cpu".into()],
        model: ModelConfig {
            kind: kind.into(),
            pack_path: Some(pack.to_path_buf()),
            preset: format!("{w}x{h}"),
            input_width: w,
            input_height: h,
            score_threshold: 0.30,
            ..Default::default()
        },
        // Everything else at its default: an exhaustive literal here means any
        // new tuning knob breaks this test instead of being exercised by it.
        ..Default::default()
    }
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
        captured_at: Utc::now(),
        width: w,
        height: h,
        format: PixelFormat::Rgb24,
        data: Arc::new(data),
        trace_id: "core-smoke".into(),
    }
}

async fn load_and_run(kind: &str, w: u32, h: u32, pack: &Path) -> Result<usize, String> {
    let cfg = mk_cfg(kind, w, h, pack);
    let det: Arc<dyn Detector> = match kind {
        "yolo" => Arc::new(YoloOrtDetector::from_config(&cfg).map_err(|e| format!("load: {e}"))?),
        "yolo_world" => {
            Arc::new(YoloWorldDetector::from_config(&cfg).map_err(|e| format!("load: {e}"))?)
        }
        "yoloe" => Arc::new(YoloeDetector::from_config(&cfg).map_err(|e| format!("load: {e}"))?),
        _ => return Err(format!("unknown kind {kind}")),
    };
    let frame = synth_frame(1280, 720);
    let dets = det
        .detect(&frame, &[])
        .await
        .map_err(|e| format!("detect: {e}"))?;
    for d in &dets {
        if !(0.0..=1.0).contains(&d.confidence) {
            return Err(format!("confidence out of range: {d:?}"));
        }
        if d.bbox.x1 < 0.0
            || d.bbox.x2 > frame.width as f32
            || d.bbox.y1 < 0.0
            || d.bbox.y2 > frame.height as f32
        {
            return Err(format!("bbox out of frame: {d:?}"));
        }
    }
    Ok(dets.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pack_core_smoke_all_shapes() {
    let pack = pack_dir();
    eprintln!("[core-smoke] pack = {}", pack.display());
    let shapes = [(512u32, 288u32), (1024, 576), (1536, 864)];
    let kinds = ["yolo", "yolo_world", "yoloe"];

    let mut total = 0;
    let mut passed = 0;
    let mut failures = Vec::new();
    for kind in kinds {
        for (w, h) in shapes {
            total += 1;
            match load_and_run(kind, w, h, &pack).await {
                Ok(n) => {
                    passed += 1;
                    eprintln!("[core-smoke] PASS  {kind:12} {w}x{h}  -> {n} detections");
                }
                Err(e) => {
                    eprintln!("[core-smoke] FAIL  {kind:12} {w}x{h}  -> {e}");
                    failures.push(format!("{kind} {w}x{h}: {e}"));
                }
            }
        }
    }
    eprintln!("[core-smoke] {passed}/{total} detector configs loaded + ran on the engine core");
    assert!(
        failures.is_empty(),
        "engine-core inference failed for: {failures:?}"
    );
}
