//! SPEC-075 F2 — a stopped camera's decode health is retired, not only
//! cleared. `stop_camera` shuts the camera's ingesters down and then clears,
//! but each pipeline goes to NULL on a detached thread, so its probes can
//! still fire after the clear and would bring the camera back into the census
//! as a live decode chain.
//!
//! Retiring is only safe if everything that starts a decode chain claims the
//! camera back, and nothing that merely moves analysis between sessions
//! retires it — either mistake leaves a running camera's decode health dark
//! for the life of the process. These drive the recorder through both, with a
//! direct probe write standing in for the pipeline's: no camera is needed.
#![cfg(feature = "gstreamer")]

use std::collections::HashMap;
use std::sync::Arc;

use nexus_config::StoreConfig;
use nexus_pipeline::{ClipRecorder, DecodeHealthRegistry, GstClipRecorder};
use nexus_store::Store;
use nexus_types::CodecKind;

/// Nothing listens on the discard port, so no session ever connects.
const OFFLINE: &str = "rtsp://127.0.0.1:9/offline";

async fn recorder(dir: &std::path::Path) -> (GstClipRecorder, Arc<DecodeHealthRegistry>) {
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite://{}?mode=rwc", dir.join("n.db").display()),
            ..Default::default()
        })
        .await
        .expect("open store"),
    );
    let health = Arc::new(DecodeHealthRegistry::new());
    let rec = GstClipRecorder::new(store, dir, HashMap::new())
        .expect("recorder")
        .with_decode_health(health.clone());
    (rec, health)
}

fn decoded(health: &DecodeHealthRegistry) -> Option<u64> {
    health.snapshot(7).map(|h| h.decoder_output_frames)
}

#[tokio::test]
async fn a_restarted_cameras_new_ingester_records_its_decode_health() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let (rec, health) = recorder(dir.path()).await;
    rec.add_camera_ingester(7, OFFLINE, 0, 15, 512, 288, CodecKind::H264)
        .expect("boot ingester");

    // `stop_camera`, then the stopped pipeline's late probe.
    rec.remove_camera_ingester(7);
    health.clear(7);
    health.observe_decoder_output(7);
    assert_eq!(
        decoded(&health),
        None,
        "precondition: the camera is retired"
    );

    // `start_camera`, then the new ingester's first probe.
    rec.add_camera_ingester(7, OFFLINE, 0, 15, 512, 288, CodecKind::H264)
        .expect("hot-add");
    health.observe_decoder_output(7);
    rec.remove_camera_ingester(7);

    assert_eq!(
        decoded(&health),
        Some(1),
        "the restarted camera's decode chain went unrecorded: its new ingester \
         never claimed the camera back from the stop"
    );
}

#[tokio::test]
async fn moving_analysis_between_sessions_keeps_recording_decode_health() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let (rec, health) = recorder(dir.path()).await;

    rec.set_camera_analysis_ingester(7, Some(OFFLINE), 15, 512, 288, CodecKind::H264)
        .expect("attach");
    health.observe_decoder_output(7);
    assert_eq!(
        decoded(&health),
        Some(1),
        "attaching a substream retired the camera: none of its probes record"
    );

    rec.set_camera_analysis_ingester(7, None, 15, 512, 288, CodecKind::H264)
        .expect("detach");
    health.observe_decoder_output(7);
    assert_eq!(
        decoded(&health),
        Some(1),
        "falling back to the main stream retired the camera: none of its probes record"
    );
}
