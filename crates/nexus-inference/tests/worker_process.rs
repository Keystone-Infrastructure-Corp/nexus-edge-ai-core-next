//! Integration tests for [`WorkerProcessBackend`].
//!
//! Uses cargo's `CARGO_BIN_EXE_nexus-inference-worker` env var to find the
//! freshly-built worker binary so this test doesn't depend on `cargo
//! install` or the worker being next to the test runner.

use std::path::PathBuf;
use std::sync::Arc;

use nexus_inference::backends::{
    BackendState, DetectorBackend, InProcessBackend, WorkerProcessBackend,
};
use nexus_inference::detectors::{Detector, MockDetector};
use nexus_inference::pool::DetectorPool;
use nexus_types::{Frame, PixelFormat};

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nexus-inference-worker"))
}

fn test_frame() -> Frame {
    Frame {
        camera_id: 7,
        frame_id: 1,
        captured_at: chrono::Utc::now(),
        captured_mono: std::time::Instant::now(),
        width: 16,
        height: 16,
        format: PixelFormat::Rgb24,
        data: Arc::new(vec![0u8; 16 * 16 * 3]),
        trace_id: "test-trace".into(),
    }
}

#[tokio::test]
async fn worker_process_round_trips_detection() {
    let backend = WorkerProcessBackend::start_with_program(0, &worker_bin(), "mock", &[])
        .expect("spawn worker");
    assert_eq!(backend.state(), BackendState::Ready);
    assert_eq!(backend.name(), "worker_process");

    let detections = backend
        .detect(&test_frame(), &[])
        .await
        .expect("detect over wire");
    assert!(
        !detections.is_empty(),
        "MockDetector always emits one person detection"
    );
}

#[tokio::test]
async fn worker_process_handles_multiple_in_flight_requests() {
    let backend = Arc::new(
        WorkerProcessBackend::start_with_program(0, &worker_bin(), "mock", &[])
            .expect("spawn worker"),
    );
    let mut joins = Vec::new();
    for i in 0..16u64 {
        let backend = backend.clone();
        joins.push(tokio::spawn(async move {
            let mut f = test_frame();
            f.frame_id = i;
            backend.detect(&f, &[]).await
        }));
    }
    for j in joins {
        let res = j.await.expect("task join").expect("detect ok");
        assert_eq!(res.len(), 1);
    }
}

#[tokio::test]
async fn pool_routes_through_worker_process_backends() {
    let mut workers: Vec<Arc<dyn DetectorBackend>> = Vec::new();
    for slot in 0..2 {
        let b = WorkerProcessBackend::start_with_program(slot, &worker_bin(), "mock", &[])
            .expect("spawn worker");
        workers.push(Arc::new(b));
    }
    let pool = DetectorPool::new(workers, None);
    // The pool implements Detector; this is exactly what the engine pipeline calls.
    for i in 0..4u64 {
        let mut f = test_frame();
        f.frame_id = i;
        let detections = pool.detect(&f, &[]).await.expect("pool detect");
        assert_eq!(detections.len(), 1);
    }

    // Snapshot must include both workers in the Ready state.
    let snap = pool.snapshot();
    assert_eq!(snap.len(), 2);
    for s in snap {
        assert_eq!(s.state, BackendState::Ready);
        assert_eq!(s.name, "worker_process");
    }
}

/// M2 fail-soft: SIGKILL the worker child, prove the pool routes around
/// the dead slot via the InProcessBackend fallback. This is the chaos
/// test that gates the M2 closeout — without it we'd be guessing the
/// fallback path actually fires under hard child death.
///
/// macOS + Linux only (Windows has no SIGKILL semantic via `kill -9`);
/// CI runs on `ubuntu-latest`, dev runs on macOS. The test is a no-op on
/// Windows.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_fails_soft_when_worker_killed() {
    use std::time::Duration;

    let worker = WorkerProcessBackend::start_with_program(0, &worker_bin(), "mock", &[])
        .expect("spawn worker");
    let pid = worker.child_pid().expect("worker pid");
    let worker: Arc<dyn DetectorBackend> = Arc::new(worker);

    let fallback: Arc<dyn DetectorBackend> =
        Arc::new(InProcessBackend::new(99, Arc::new(MockDetector::new())));
    let pool = DetectorPool::new(vec![worker.clone()], Some(fallback));

    // Sanity: pre-kill, the worker handles the call.
    let pre = pool
        .detect(&test_frame(), &[])
        .await
        .expect("pre-kill detect");
    assert_eq!(pre.len(), 1);

    // SIGKILL via shell — avoids a libc/nix dependency just for this test.
    let status = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("kill -9 invocation");
    assert!(status.success(), "kill -9 {pid} failed: {status}");

    // Wait for the reader task to notice the EOF and flip state to
    // Failed. 2s budget is comfortable; the reader normally trips on the
    // very next read() syscall.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while worker.state() != BackendState::Failed {
        if std::time::Instant::now() > deadline {
            panic!(
                "worker did not transition to Failed after kill -9; state = {:?}",
                worker.state()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Post-kill: the pool MUST keep serving via the fallback.
    for i in 0..4u64 {
        let mut f = test_frame();
        f.frame_id = i + 100;
        let dets = pool
            .detect(&f, &[])
            .await
            .expect("pool detect after kill (fallback path)");
        assert_eq!(dets.len(), 1, "fallback MockDetector emits one detection");
    }

    // Snapshot reflects reality: worker Failed, fallback present and Ready.
    let snap = pool.snapshot();
    assert_eq!(snap.len(), 2);
    let worker_snap = snap.iter().find(|s| s.slot == 0).expect("worker slot 0");
    assert_eq!(worker_snap.state, BackendState::Failed);
    let fb_snap = snap
        .iter()
        .find(|s| s.name.contains("(fallback)"))
        .expect("fallback snapshot");
    assert_eq!(fb_snap.state, BackendState::Ready);
}
