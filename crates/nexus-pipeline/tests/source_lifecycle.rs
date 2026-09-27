//! BUG-223 — a camera's frame source must end with its supervisor.
//!
//! `stop_camera` and every reconciler restart end a supervisor with
//! `task.abort()`. The source runs in a task of its own, and a bare
//! `JoinHandle` detaches on drop, so whether the source then ends depended on
//! the source noticing its receiver had gone. `VirtualSource` never looks,
//! and `SharedRtspSource` parks on an ingester that a stopped camera has shut
//! down: it holds the `Arc` that keeps that ingester's broadcast sender alive,
//! so the receive it waits on can never close. Each restart left one behind.
//!
//! The RTSP test is the other half: ending the source must still hang up the
//! camera, not just stop polling it.
//!
//! No ORT; `virtual://` + an in-memory bus + sqlite on a tempdir, mirroring
//! `live_frame_freshness.rs`. The two GStreamer tests need no camera.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use nexus_bus::{BroadcastBus, Bus};
use nexus_config::{CameraConfig, ClipsConfig, RulesConfig, StoreConfig, TrackerConfig};
use nexus_inference::{Detector, InferenceError};
use nexus_pipeline::cache::LatestFrameCache;
use nexus_pipeline::recorder::{ClipFinal, ClipHandle, ClipMeta, OpenClip, RecorderError};
use nexus_pipeline::supervisor::spawn_camera;
use nexus_pipeline::{
    CameraHandle, ClipRecorder, FrameSource, FrameSourceError, FrameStatsRegistry, VirtualSource,
};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_types::{CameraId, Detection, Frame};
use parking_lot::Mutex;
use url::Url;

/// How long a stopped supervisor's source may outlive it. An abort takes
/// effect at the source's next await, so this only bounds a broken run.
const SOURCE_ENDS_WITHIN: Duration = Duration::from_secs(2);

struct NoDetections;

#[async_trait]
impl Detector for NoDetections {
    async fn detect(&self, _f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
        Ok(vec![])
    }

    fn name(&self) -> &'static str {
        "no_detections"
    }
}

/// Hands the supervisor one prepared source, then none, so a later build
/// falls through to the URL like the stub recorder.
struct HandsOutOnce(Mutex<Option<Box<dyn FrameSource + Send>>>);

#[async_trait]
impl ClipRecorder for HandsOutOnce {
    async fn open(&self, _args: OpenClip) -> Result<ClipHandle, RecorderError> {
        Err(RecorderError::Refused)
    }
    async fn close(&self, _h: ClipHandle, _args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        Err(RecorderError::Refused)
    }
    fn set_panic(&self, _panic: bool) {}
    fn is_panic(&self) -> bool {
        false
    }
    fn kind(&self) -> &'static str {
        "hands_out_once"
    }
    fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
        self.0.lock().take()
    }
}

/// Runs `inner` unchanged while holding `alive`, so the `Arc`'s weak count
/// says whether the source's task still exists.
struct Alive {
    inner: Box<dyn FrameSource + Send>,
    alive: Arc<()>,
}

#[async_trait]
impl FrameSource for Alive {
    async fn run(
        self: Box<Self>,
        tx: tokio::sync::mpsc::Sender<Frame>,
    ) -> Result<(), FrameSourceError> {
        let Alive { inner, alive } = *self;
        let _alive = alive;
        inner.run(tx).await
    }
}

/// Spawn camera 1 on `url`, taking its first frame source from `source`.
async fn spawn(
    dir: &std::path::Path,
    url: &str,
    source: Option<Box<dyn FrameSource + Send>>,
    stats: Arc<FrameStatsRegistry>,
) -> CameraHandle {
    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
    let tracker_cfg = TrackerConfig::default();
    let store = Arc::new(
        Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", dir.join("nexus.db").display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .expect("open store"),
    );
    let cam = CameraConfig {
        id: 1,
        name: "source-lifecycle".into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse(url).unwrap(),
            analysis_url: None,
            enabled: true,
            max_fps: 10,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec!["person".into()],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: Default::default(),
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    };
    store.upsert_camera(&cam).await.expect("seed cameras row");
    spawn_camera(
        cam,
        Arc::new(NoDetections),
        None,
        Arc::from(nexus_tracker::build_tracker(&tracker_cfg)),
        tracker_cfg.annotator.clone(),
        tracker_cfg.static_object.clone(),
        ClipsConfig::default(),
        dir.to_path_buf(),
        Arc::new(RuleEvaluator::new(&RulesConfig::default(), &[]).expect("empty rule set")),
        store,
        Arc::new(HandsOutOnce(Mutex::new(source))),
        bus,
        Arc::new(LatestFrameCache::new()),
        stats,
        nexus_pipeline::StaticAnchorClearRegistry::new(),
        512,
        288,
        Arc::new(nexus_pipeline::NoopSightingHook),
        nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
        Vec::new(),
        Arc::new(nexus_pipeline::NoopEntityLocalPersist),
        None,
        Arc::new(nexus_pipeline::NoopSinkRouter),
        Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
    )
}

/// Poll `done` for up to `within`; `false` if it never held.
async fn holds_within(within: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while !done() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

async fn assert_dropped<T>(weak: &Weak<T>, what: &str) {
    assert!(
        holds_within(SOURCE_ENDS_WITHIN, || weak.upgrade().is_none()).await,
        "{what} outlived its stopped supervisor by more than {SOURCE_ENDS_WITHIN:?}: \
         the source task was detached, not ended, so every camera restart leaks one"
    );
}

/// `VirtualSource` drops a send it cannot make and sleeps, forever: nothing
/// in it ever looks at whether its receiver still exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_supervisor_takes_its_frame_source_with_it() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let alive = Arc::new(());
    let weak = Arc::downgrade(&alive);
    let source = Box::new(Alive {
        inner: Box::new(VirtualSource {
            camera_id: 1,
            width: 512,
            height: 288,
            fps: 10,
        }),
        alive,
    });
    let stats = Arc::new(FrameStatsRegistry::new());
    let handle = spawn(dir.path(), "virtual://local", Some(source), stats.clone()).await;
    assert!(
        holds_within(Duration::from_secs(10), || stats.snapshot(1).is_some()).await,
        "precondition: the source never delivered a frame"
    );

    handle.task.abort();

    assert_dropped(&weak, "the virtual frame source").await;
}

/// A stopped camera's ingester is shut down, so it sends nothing again, and
/// the source parked on it holds the `Arc` that keeps its broadcast sender
/// alive: the receive can never close. That `Arc` is also the ingester's
/// pre-roll ring and its last broadcast frames, kept per restart.
#[cfg(feature = "gstreamer")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_supervisor_releases_the_ingester_its_source_reads() {
    use nexus_pipeline::{DecodeMode, PreRollIngester, SharedRtspSource};

    let dir = tempfile::tempdir().expect("tmpdir");
    // Nothing listens on the discard port: the session never connects and the
    // tap never yields, like a camera that went offline before it was stopped.
    let ingester = PreRollIngester::new_with_rgb(
        1,
        "rtsp://127.0.0.1:9/offline",
        0,
        nexus_types::CodecKind::H264,
        DecodeMode::default(),
        10,
        512,
        288,
        None,
    )
    .expect("ingester");
    let weak = Arc::downgrade(&ingester);
    let source = Box::new(SharedRtspSource {
        camera_id: 1,
        ingester: ingester.clone(),
        analysis: None,
        analysis_stream: None,
    });
    let handle = spawn(
        dir.path(),
        "rtsp://127.0.0.1:9/offline",
        Some(source),
        Arc::new(FrameStatsRegistry::new()),
    )
    .await;
    let source_built = || Arc::strong_count(&ingester) == 2;
    assert!(
        holds_within(Duration::from_secs(10), source_built).await,
        "precondition: the supervisor never built its source"
    );
    // Let the source reach its receive.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // What `stop_camera` does: abort the supervisor, then shut the ingester.
    handle.task.abort();
    ingester.shutdown();
    drop(ingester);

    assert_dropped(&weak, "the shared source's ingester").await;
}

/// `RtspSource` owns a GStreamer pipeline and a bus thread for as long as a
/// session runs. However the source ends, the camera's RTSP connection has to
/// close with it: a pipeline left PLAYING keeps the session, and cameras whose
/// firmware allows one session per stream refuse the restarted supervisor.
///
/// The camera is a listener that accepts and never answers, so `rtspsrc`
/// waits on it for its own 20 s TCP timeout. A hang-up well inside that is
/// the source's teardown, not the timeout.
#[cfg(feature = "gstreamer")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_supervisor_hangs_up_its_rtsp_camera() {
    use tokio::io::AsyncReadExt;

    let dir = tempfile::tempdir().expect("tmpdir");
    let camera = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!(
        "rtsp://127.0.0.1:{}/stream",
        camera.local_addr().unwrap().port()
    );
    // No shared source: the supervisor builds an `RtspSource` from the URL.
    let handle = spawn(dir.path(), &url, None, Arc::new(FrameStatsRegistry::new())).await;
    let (mut conn, _) = tokio::time::timeout(Duration::from_secs(10), camera.accept())
        .await
        .expect("precondition: the source never connected to the camera")
        .expect("accept");
    let mut request = [0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut request))
        .await
        .expect("precondition: the source never sent its RTSP request")
        .expect("read");
    assert!(
        request[..n].starts_with(b"OPTIONS "),
        "precondition: expected an RTSP request, got {:?}",
        String::from_utf8_lossy(&request[..n])
    );

    handle.task.abort();

    let hung_up = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match conn.read(&mut request).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    })
    .await;
    assert!(
        hung_up.is_ok(),
        "the camera's RTSP connection stayed open 5 s after its supervisor was stopped: \
         the source's pipeline was left PLAYING"
    );
}
