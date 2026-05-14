//! Always-on H.264 pre-roll ingester — M2.1 Stage B PR B8.
//!
//! Per-camera GStreamer pipeline, started at engine boot, that holds
//! the only RTSP connection for the camera and:
//!
//!   1. Maintains a 5s rolling ring buffer of byte-stream H.264 NAL
//!      samples (see [`crate::preroll::NalRingBuffer`]). When motion
//!      fires, [`GstClipRecorder`] snapshots this buffer and prepends
//!      it to the new clip so the recording starts ~5s before motion
//!      onset (NVR pre-roll convention).
//!
//!   2. Fans every live sample out over a tokio broadcast channel so
//!      the active recorder can keep appending to the same clip
//!      without opening a second TCP connection to the camera.
//!
//! Pipeline:
//!
//! ```text
//!   rtspsrc location=URL latency=200 protocols=tcp+udp
//!     ! rtph264depay
//!     ! h264parse config-interval=-1
//!     ! video/x-h264,stream-format=byte-stream,alignment=au
//!     ! appsink name=tap emit-signals=true sync=false
//!         max-buffers=200 drop=false
//! ```
//!
//! `stream-format=byte-stream,alignment=au` (Annex-B, access-unit-aligned)
//! is what mp4mux's `appsrc` feed expects when we splice the snapshot
//! at clip-open. `config-interval=-1` re-emits SPS/PPS in front of
//! every keyframe — critical because a snapshot starting on the
//! Nth keyframe must carry decoder config to be playable.
//!
//! Re-connect strategy: the ingester runs an async supervisor that
//! tears the pipeline down and rebuilds it on bus error or EOS, with
//! exponential backoff capped at 30s. The ring buffer survives
//! reconnect (we keep what we last buffered) but is NOT rewound — a
//! camera that drops for 60s leaves a 60s pre-roll gap on the next
//! recording, which is still better than zero pre-roll.
//!
//! Memory cost: roughly `bitrate_bytes_per_sec * pre_roll_secs`.
//! ~2 MB per camera at 4 Mbps 1080p, ~5 MB at 4K. Bounded by the
//! ring buffer itself; the broadcast channel is capped (see
//! [`BROADCAST_CAPACITY`]) to keep a slow recorder from blocking
//! the streaming thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use nexus_types::CameraId;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::preroll::{NalRingBuffer, NalSample};
use crate::source::gst_init;

/// How many in-flight live samples the broadcast channel buffers
/// per subscriber. At ~30fps a recording at most 1-2 seconds behind
/// before we should be panicking anyway.
const BROADCAST_CAPACITY: usize = 64;

/// Max backoff between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum IngesterError {
    #[error("gstreamer init: {0}")]
    GstInit(String),
    #[error("gstreamer pipeline: {0}")]
    Pipeline(String),
    #[error("appsink wiring: {0}")]
    AppSink(String),
}

pub struct PreRollIngester {
    camera_id: CameraId,
    url: String,
    /// `pre_roll_secs == 0` is a valid disable knob — we still run
    /// the always-on pipeline (so the broadcast channel is alive
    /// for recording) but the ring buffer never accumulates.
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    /// Active GStreamer pipeline, populated by the supervisor each
    /// time it (re)builds a session. Drop sets it to NULL
    /// synchronously so the GObject ref cycle teardown doesn't
    /// trip GStreamer's "disposed in PLAYING state" critical and
    /// SIGSEGV.
    active_pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    /// Polled by the supervisor between session attempts; flipped
    /// to true by Drop to break the reconnect loop.
    shutdown: Arc<AtomicBool>,
    /// Background task driving the GStreamer pipeline. Aborted in
    /// Drop AFTER the active pipeline has been transitioned to NULL.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl PreRollIngester {
    /// Build, start the always-on pipeline, and return immediately.
    /// Pipeline state changes happen on a background task — callers
    /// that need to know "is the camera actually online?" should
    /// read [`PreRollIngester::is_buffering`] after a brief grace
    /// period.
    pub fn new(
        camera_id: CameraId,
        url: impl Into<String>,
        pre_roll_secs: u32,
    ) -> Result<Arc<Self>, IngesterError> {
        gst_init::ensure().map_err(|e| IngesterError::GstInit(e.to_string()))?;
        let url = url.into();
        let ring = Arc::new(Mutex::new(NalRingBuffer::new(Duration::from_secs(
            pre_roll_secs as u64,
        ))));
        let (live_tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let active_pipeline = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let task_url = url.clone();
        let task_ring = ring.clone();
        let task_tx = live_tx.clone();
        let task_pipeline = active_pipeline.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            run_supervisor(
                camera_id,
                task_url,
                task_ring,
                task_tx,
                task_pipeline,
                task_shutdown,
            )
            .await;
        });

        Ok(Arc::new(Self {
            camera_id,
            url,
            ring,
            live_tx,
            active_pipeline,
            shutdown,
            task: Mutex::new(Some(task)),
        }))
    }

    pub fn camera_id(&self) -> CameraId {
        self.camera_id
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Subscribe to every live H.264 NAL sample arriving from this
    /// camera. The first sample a fresh subscriber sees is the
    /// next one ingested; backlog before the subscribe is NOT
    /// replayed. Recorders that need pre-roll context call
    /// [`PreRollIngester::snapshot`] separately and prepend the
    /// snapshot to the live stream.
    pub fn subscribe(&self) -> broadcast::Receiver<NalSample> {
        self.live_tx.subscribe()
    }

    /// Take a copy of every NAL currently in the pre-roll ring
    /// buffer. Returned vec starts on a keyframe (or is empty if
    /// no keyframe has arrived yet). The buffer continues filling
    /// independently — taking a snapshot does NOT drain it.
    pub fn snapshot(&self) -> Vec<NalSample> {
        self.ring.lock().snapshot()
    }

    /// True iff the ring buffer has at least one keyframe and one
    /// sample. Used by the recorder + tests to wait for the
    /// camera to become healthy enough to record.
    pub fn is_buffering(&self) -> bool {
        let g = self.ring.lock();
        g.gop_count() >= 1 && g.sample_count() >= 1
    }
}

async fn run_supervisor(
    camera_id: CameraId,
    url: String,
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    active_pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    shutdown: Arc<AtomicBool>,
) {
    info!(
        camera_id,
        url, "preroll ingester supervisor starting (always-on)"
    );
    let mut backoff = Duration::from_millis(500);
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match run_session(
            camera_id,
            &url,
            ring.clone(),
            live_tx.clone(),
            active_pipeline.clone(),
            shutdown.clone(),
        )
        .await
        {
            Ok(()) => {
                info!(camera_id, "preroll ingester session ended cleanly (EOS)");
                backoff = Duration::from_millis(500);
            }
            Err(e) => {
                warn!(
                    camera_id,
                    error = %e,
                    backoff_ms = backoff.as_millis(),
                    "preroll ingester session failed; reconnecting after backoff"
                );
            }
        }
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff.saturating_mul(2)).min(MAX_BACKOFF);
    }
}

async fn run_session(
    camera_id: CameraId,
    url: &str,
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    active_pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), IngesterError> {
    let url_safe = url.replace('"', "");
    let desc = format!(
        "rtspsrc location=\"{url_safe}\" latency=200 protocols=tcp+udp \
         ! rtph264depay \
         ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=tap emit-signals=true sync=false \
             max-buffers=200 drop=false"
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| IngesterError::Pipeline(format!("parse::launch: {e}")))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| IngesterError::Pipeline("downcast Pipeline".into()))?;

    let sink = pipeline
        .by_name("tap")
        .ok_or_else(|| IngesterError::AppSink("appsink 'tap' not found".into()))?
        .downcast::<AppSink>()
        .map_err(|_| IngesterError::AppSink("downcast AppSink".into()))?;

    let cb_ring = ring.clone();
    let cb_tx = live_tx.clone();
    // Some IP cameras drop PTS on individual H.264 frames after the
    // first keyframe (we've seen this on the 192.168.1.66 fixture).
    // qtmux/mp4mux refuses to mux any buffer without PTS and silently
    // drops the rest of the stream, leaving an 864-byte file with
    // only ftyp+moov stub. Fall back to DTS, and as a last resort
    // synthesise a monotonic PTS based on the previous PTS + an
    // assumed 33ms frame duration (~30fps). This keeps the recording
    // continuous even on cameras with flaky timestamps.
    let last_pts = std::sync::Arc::new(parking_lot::Mutex::new(None::<Duration>));
    sink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let raw_pts = buffer.pts().map(|t| Duration::from_nanos(t.nseconds()));
                let raw_dts = buffer.dts().map(|t| Duration::from_nanos(t.nseconds()));
                let pts = {
                    let mut last = last_pts.lock();
                    let resolved = raw_pts
                        .or(raw_dts)
                        .or_else(|| last.map(|prev| prev + Duration::from_millis(33)));
                    if let Some(v) = resolved {
                        *last = Some(v);
                    }
                    resolved
                };
                let dts = raw_dts.or(pts);
                // GST_BUFFER_FLAG_DELTA_UNIT is set on every non-key
                // sample. Absence of the flag => keyframe.
                let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                let nal = NalSample {
                    pts,
                    dts,
                    is_keyframe,
                    data: map.as_slice().to_vec(),
                };
                // Push into ring first so a slow broadcast doesn't
                // delay the buffer's persistence path. The ring is
                // bounded by duration so pushes are O(1) amortised.
                cb_ring.lock().push(nal.clone());
                // Broadcast to live subscribers. Errors here just
                // mean no one is listening (typical: no clip open),
                // which is fine — the ring carries us either way.
                let _ = cb_tx.send(nal);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| IngesterError::Pipeline(format!("set Playing: {e}")))?;

    // Register the live pipeline with the ingester struct so Drop
    // can null it synchronously (the bus iterator below blocks the
    // tokio task; it can't react to a Rust-side shutdown signal
    // without external state-change kicks). Must happen AFTER
    // set_state(Playing) so a racing Drop doesn't transition a
    // never-Playing pipeline to NULL (which would be a no-op and
    // leave us hung in the bus iter), and BEFORE the long-blocking
    // bus iter below so a Drop happening during the first second
    // of the session still finds the pipeline.
    *active_pipeline.lock() = Some(pipeline.clone());

    // Drive the bus on a blocking task so we observe Errors / EOS
    // and propagate them up to the supervisor for reconnect. We
    // use a short polling timeout instead of iter_timed(NONE) so
    // the loop can re-check the shutdown flag between bus pops —
    // otherwise Drop's pipeline.set_state(NULL) wouldn't cause
    // iter_timed to return (it only returns on actual messages),
    // and the spawn_blocking thread would hold a strong ref on
    // the pipeline + keep the process alive past main exit.
    let bus = pipeline
        .bus()
        .ok_or_else(|| IngesterError::Pipeline("pipeline bus missing".into()))?;
    let pipeline_for_bus = pipeline.clone();
    let bus_shutdown = shutdown;
    let result: Result<(), IngesterError> = tokio::task::spawn_blocking(move || loop {
        if bus_shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let timeout = gst::ClockTime::from_mseconds(250);
        match bus.timed_pop(Some(timeout)) {
            None => continue,
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(..) => {
                    debug!(camera_id, "preroll ingester pipeline EOS");
                    return Ok(());
                }
                gst::MessageView::Error(e) => {
                    let err = format!(
                        "{} (debug: {})",
                        e.error(),
                        e.debug().unwrap_or_else(|| "<none>".into())
                    );
                    return Err(IngesterError::Pipeline(err));
                }
                _ => {}
            },
        }
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(IngesterError::Pipeline(format!(
            "bus task join: {join_err}"
        )))
    });

    // Pipeline is going down — deregister BEFORE nulling so Drop
    // doesn't race with us.
    *active_pipeline.lock() = None;
    let _ = pipeline_for_bus.set_state(gst::State::Null);
    if let Err(e) = result {
        error!(camera_id, error = %e, "preroll ingester session error");
        return Err(e);
    }
    Ok(())
}

impl Drop for PreRollIngester {
    fn drop(&mut self) {
        // Order matters:
        //   1. Set shutdown flag so the supervisor doesn't reconnect
        //      after we null its pipeline.
        //   2. Take the active pipeline out of the mutex and
        //      transition it to NULL synchronously. This drains the
        //      bus iter and unblocks the supervisor's blocking
        //      task.
        //   3. Abort the supervisor task. (Aborting first leaves
        //      the pipeline in PLAYING which causes GStreamer to
        //      emit a CRITICAL and on macOS SIGSEGV during dispose.)
        self.shutdown.store(true, Ordering::Release);
        if let Some(pipeline) = self.active_pipeline.lock().take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        if let Some(handle) = self.task.lock().take() {
            handle.abort();
        }
        debug!(camera_id = self.camera_id, "preroll ingester dropped");
    }
}
