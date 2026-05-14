//! GStreamer-backed clip recorder — M2.1 Stage B (PR B2 + B8 refactor).
//!
//! Per-clip pipeline lifecycle (post-B8 — single RTSP connection per
//! camera, fed by [`PreRollIngester`]):
//!
//! ```text
//!   appsrc name=src is-live=true format=time do-timestamp=false
//!     ! h264parse config-interval=-1
//!     ! mp4mux fragment-duration=5000 streamable=true faststart=true
//!     ! filesink location=/var/lib/nexus/clips/cam1/...mp4
//! ```
//!
//! At [`ClipRecorder::open`]:
//!
//!   1. Build the appsrc-headed mp4 writer and start it Playing.
//!   2. Snapshot the camera's pre-roll ring buffer
//!      ([`PreRollIngester::snapshot`]) and push every sample into
//!      appsrc. The first sample is always a keyframe (the ring
//!      buffer guarantees GOP alignment); we use it as the t=0
//!      anchor and rebase all PTS/DTS to start at zero so mp4mux
//!      writes a sane track timeline.
//!   3. Subscribe to the ingester's live broadcast channel; spawn an
//!      async pump that forwards every new live sample into appsrc
//!      until close() fires (or the recorder is dropped).
//!
//! At [`ClipRecorder::close`]:
//!
//!   1. Signal the live pump to stop and wait for it to drain.
//!   2. Send EOS into appsrc → flows through h264parse → mp4mux
//!      finalises moov → filesink closes the file.
//!   3. Wait on bus for EOS message (or 5s timeout) and null the
//!      pipeline.
//!   4. stat() the resulting file, stamp `ended_at` / `duration_ms`
//!      / `size_bytes` on the `motion_clips` row.
//!
//! Hard invariants:
//!
//! * **One RTSP connection per camera, lifetime = process lifetime.**
//!   The per-clip rtspsrc of pre-B8 is gone; all NAL traffic comes
//!   in via the always-on [`PreRollIngester`]. This avoids the
//!   re-handshake stutter at every clip open and means cheap
//!   cameras (4-connection limits) don't get hammered.
//! * **No re-encode.** appsrc receives byte-stream H.264 from the
//!   ingester; mp4mux just wraps it.
//! * **Pre-roll prepended.** Every clip starts with the ring
//!   buffer's snapshot — typically ~5s of footage from BEFORE the
//!   triggering motion event. `pre_roll_secs=0` produces an empty
//!   snapshot and the recording starts at the first live sample,
//!   matching pre-B8 behaviour.
//! * **Fragmented mp4** so a kill-mid-write leaves a partially-
//!   readable file.
//! * **Codec is `h264`, container is `mp4`** stamped on the row.
//! * **Refused on panic.** Same semantics as `StubClipRecorder`: the
//!   watermark sampler flips `set_panic(true)` when the disk is hot
//!   and `open()` returns `RecorderError::Refused`.
//!
//! Per-camera ingester resolution: the recorder is constructed with
//! a snapshot of `HashMap<CameraId, Arc<PreRollIngester>>` from the
//! engine boot. A camera added at runtime that isn't in the snapshot
//! triggers a `Refused` with a one-shot warn log; the operator
//! restarts the engine to pick up the new camera. (Same hot-reload
//! limitation as everything else upstream.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use nexus_store::{ClipClose, ClipId, NewClip, Store};
use nexus_types::CameraId;
use parking_lot::Mutex as PlMutex;
use tokio::fs;
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{debug, info, warn};

use crate::preroll::NalSample;
use crate::preroll_ingester::PreRollIngester;
use crate::recorder::{ClipFinal, ClipHandle, ClipMeta, ClipRecorder, OpenClip, RecorderError};
use crate::source::gst_init;

/// How long to wait for the per-clip pipeline to drain after we send
/// EOS at close time. 5s is generous; in practice mp4mux flushes in
/// well under a second once it sees EOS at the appsrc head.
const EOS_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Codec stamped on every row this recorder writes.
const CODEC: &str = "h264";
/// Container stamped on every row this recorder writes.
const CONTAINER: &str = "mp4";

pub struct GstClipRecorder {
    store: Arc<Store>,
    clips_dir: PathBuf,
    /// Per-camera always-on RTSP feed. The recorder is a passive
    /// consumer — it neither opens nor manages the underlying
    /// connection. A None lookup at `open()` triggers a Refused so
    /// the supervisor doesn't end up writing a `motion_clips` row
    /// that points at a never-opened file.
    ingesters: HashMap<CameraId, Arc<PreRollIngester>>,
    panic: PlMutex<bool>,
    /// Per-clip GStreamer + pump state. Held under a tokio Mutex
    /// because the close path awaits on the pump shutdown and the
    /// bus drain.
    open: Mutex<HashMap<ClipId, OpenState>>,
}

struct OpenState {
    camera_id: CameraId,
    started_at: DateTime<Utc>,
    path: PathBuf,
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    /// Signals the live-pump task to stop forwarding broadcast
    /// samples into appsrc. Sent at the start of `close()` so any
    /// already-in-flight sample finishes before we send EOS.
    pump_stop: Option<oneshot::Sender<()>>,
    /// Handle to the live-pump task. Awaited at `close()` so we know
    /// no further `push_buffer()` calls are racing the EOS we send.
    pump_handle: Option<tokio::task::JoinHandle<()>>,
}

// gst::Pipeline / AppSrc are Send + Sync by GObject contract, so
// OpenState is auto-Send + auto-Sync.

impl GstClipRecorder {
    pub fn new(
        store: Arc<Store>,
        clips_dir: impl AsRef<Path>,
        ingesters: HashMap<CameraId, Arc<PreRollIngester>>,
    ) -> Result<Self, RecorderError> {
        gst_init::ensure()
            .map_err(|e| RecorderError::Io(std::io::Error::other(format!("gst init: {e}"))))?;
        Ok(Self {
            store,
            clips_dir: clips_dir.as_ref().to_path_buf(),
            ingesters,
            panic: PlMutex::new(false),
            open: Mutex::new(HashMap::new()),
        })
    }

    /// Path the recorder will write for `(camera_id, started_at)`.
    /// Same shape as `StubClipRecorder::clip_path` so the rest of the
    /// system (eviction loop, retention sweeper, orphan-file scan)
    /// works against either recorder unchanged.
    pub fn clip_path(&self, camera_id: CameraId, started_at: DateTime<Utc>) -> PathBuf {
        let ts = started_at.format("%Y%m%dT%H%M%S");
        let ms = started_at.timestamp_subsec_millis();
        self.clips_dir
            .join(format!("cam{camera_id}"))
            .join(format!("{ts}_{ms:03}.mp4"))
    }

    fn build_pipeline(location: &Path) -> Result<(gst::Pipeline, AppSrc), RecorderError> {
        // location came from clips_dir + a deterministic timestamp
        // template; strip embedded `"` before splicing into launch
        // string so a pathological path can't break the parser.
        let location_safe = location.to_string_lossy().replace('"', "");
        // appsrc properties:
        //   is-live=false  : we're bulk-feeding from an external
        //                    source, NOT driving the pipeline clock.
        //                    is-live=true would have appsrc reject
        //                    samples whose PTS doesn't align with
        //                    the pipeline's running-time — our PTS
        //                    are rebased from the camera's clock and
        //                    would race the pipeline clock at
        //                    startup, dropping silently.
        //   format=time     : we provide PTS in nanoseconds.
        //   do-timestamp=false : honour our PTS verbatim instead of
        //                        overwriting with the pipeline clock.
        //   block=true      : if the internal queue fills up, block
        //                     the pusher rather than returning Eos.
        //                     Our pump is async on tokio so blocking
        //                     just yields to the runtime.
        //   max-bytes=16M   : ~32s of headroom at 4 Mbps; the live
        //                     pump shouldn't ever hit this in normal
        //                     operation but it bounds memory if
        //                     filesink is slow (full disk).
        let desc = format!(
            "appsrc name=src is-live=false format=time do-timestamp=false \
                     stream-type=stream max-bytes=16777216 block=true \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=avc,alignment=au \
             ! mp4mux fragment-duration=5000 streamable=true faststart=true \
             ! filesink location=\"{location_safe}\" sync=false"
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| RecorderError::Io(std::io::Error::other(format!("parse::launch: {e}"))))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| {
                RecorderError::Io(std::io::Error::other("downcast Pipeline".to_string()))
            })?;
        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| {
                RecorderError::Io(std::io::Error::other("appsrc 'src' not found".to_string()))
            })?
            .downcast::<AppSrc>()
            .map_err(|_| RecorderError::Io(std::io::Error::other("downcast AppSrc".to_string())))?;
        // Tell appsrc the caps explicitly — mp4mux + h264parse can't
        // negotiate without knowing this is byte-stream H.264. We
        // don't claim a framerate (mp4mux infers from PTS).
        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        appsrc.set_caps(Some(&caps));
        appsrc.set_stream_type(AppStreamType::Stream);
        Ok((pipeline, appsrc))
    }
}

#[async_trait]
impl ClipRecorder for GstClipRecorder {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        if *self.panic.lock() {
            return Err(RecorderError::Refused);
        }
        let ingester = match self.ingesters.get(&args.camera_id) {
            Some(i) => i.clone(),
            None => {
                warn!(
                    camera_id = args.camera_id,
                    "GstClipRecorder: no PreRollIngester for this camera; refusing open. \
                     Restart the engine to pick up cameras added after boot."
                );
                return Err(RecorderError::Refused);
            }
        };

        let path = self.clip_path(args.camera_id, args.started_at);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let (pipeline, appsrc) = Self::build_pipeline(&path)?;
        // set_state(Playing) returns Async; appsrc starts accepting
        // pushes immediately. We push the pre-roll snapshot before
        // even waiting for the state change to complete — the queue
        // inside appsrc absorbs them.
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| RecorderError::Io(std::io::Error::other(format!("set Playing: {e}"))))?;

        // Snapshot the ring buffer + subscribe to live BEFORE either
        // path can race the other. The subscribe happens-before any
        // sample produced after `subscribe()` returns, so we won't
        // double-count samples that straddle the snapshot boundary
        // (the snapshot is a copy of what's *already* buffered, the
        // broadcast carries everything from this moment forward).
        // There is a small overlap: a sample currently in the ring
        // buffer might also arrive on the broadcast a few moments
        // later. That's fine — we de-dup by PTS in the pump (skip
        // anything <= last_pushed_pts).
        let snapshot = ingester.snapshot();
        let live_rx = ingester.subscribe();

        // Determine the rebase anchor. mp4mux wants PTS to start at
        // (or near) zero for a sane track timeline. The first sample
        // is guaranteed to be a keyframe (ring buffer invariant) but
        // may have no PTS during stream startup; fall back to
        // Duration::ZERO in that case.
        let base_pts = snapshot
            .iter()
            .find_map(|s| s.pts)
            .unwrap_or(Duration::ZERO);

        // Push the snapshot synchronously so close()-immediately-after-open()
        // still produces a file with the pre-roll content. Wrapped
        // in spawn_blocking because appsrc.push_buffer() with
        // block=true can stall briefly on filesink/disk pressure
        // (and we MUST NOT block the tokio worker on a GStreamer
        // synchronous call).
        let snapshot_tail_pts = snapshot.iter().filter_map(|s| s.pts).next_back();
        let snapshot_for_blocking = snapshot;
        let appsrc_for_blocking = appsrc.clone();
        let preroll_count = match tokio::task::spawn_blocking(move || {
            let mut n = 0usize;
            for sample in &snapshot_for_blocking {
                push_sample(&appsrc_for_blocking, sample, base_pts).map_err(|e| {
                    RecorderError::Io(std::io::Error::other(format!("push pre-roll sample: {e}")))
                })?;
                n += 1;
            }
            Ok::<usize, RecorderError>(n)
        })
        .await
        {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(e);
            }
            Err(join_err) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(RecorderError::Io(std::io::Error::other(format!(
                    "preroll spawn_blocking: {join_err}"
                ))));
            }
        };
        let last_pushed_pts: Option<Duration> = snapshot_tail_pts;

        let rel = path
            .strip_prefix(&self.clips_dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        let new = NewClip {
            camera_id: args.camera_id,
            started_at: args.started_at,
            path: rel,
            codec: CODEC.into(),
            container: CONTAINER.into(),
            backend_id: "local".into(),
        };
        let clip_id = match self.store.open_clip(&new).await {
            Ok(id) => id,
            Err(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(e.into());
            }
        };

        // Live pump: forward every new broadcast sample into appsrc
        // until close() signals stop. De-dup against the snapshot's
        // tail by PTS. Lagged broadcast errors are logged but the
        // pump keeps running — a brief glitch is preferable to
        // killing the recording outright.
        let (pump_stop_tx, pump_stop_rx) = oneshot::channel();
        let pump_appsrc = appsrc.clone();
        let pump_handle = tokio::spawn(run_live_pump(
            args.camera_id,
            clip_id,
            pump_appsrc,
            live_rx,
            base_pts,
            last_pushed_pts,
            pump_stop_rx,
        ));

        self.open.lock().await.insert(
            clip_id,
            OpenState {
                camera_id: args.camera_id,
                started_at: args.started_at,
                path,
                pipeline,
                appsrc,
                pump_stop: Some(pump_stop_tx),
                pump_handle: Some(pump_handle),
            },
        );
        info!(
            camera_id = args.camera_id,
            clip_id,
            preroll_samples = preroll_count,
            "gst recorder opened clip (pre-roll prepended)"
        );
        Ok(ClipHandle {
            clip_id,
            camera_id: args.camera_id,
        })
    }

    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        let mut state = self
            .open
            .lock()
            .await
            .remove(&handle.clip_id)
            .ok_or(RecorderError::UnknownClip(handle.clip_id))?;

        // Step 1: signal the live pump to stop and wait for it. Once
        // the pump returns we know no more push_buffer() calls are
        // racing the EOS we're about to send.
        if let Some(stop_tx) = state.pump_stop.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = state.pump_handle.take() {
            // 2s should be plenty — the pump only awaits a
            // broadcast::recv() with a oneshot select; stop fires
            // immediately. If it somehow hangs we abort and proceed
            // anyway; appsrc is robust against parallel push during
            // EOS in practice.
            match tokio::time::timeout(Duration::from_secs(2), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(error = %e, "live pump task panicked during close")
                }
                Err(_) => {
                    warn!("live pump task did not exit within 2s; proceeding to EOS anyway")
                }
            }
        }

        // Step 2: send EOS into appsrc — mp4mux finalises the moov
        // atom, filesink closes the file. Drain the bus for either
        // EOS or Error up to EOS_DRAIN_TIMEOUT.
        let pipeline = state.pipeline;
        let appsrc = state.appsrc;
        let pipeline_for_blocking = pipeline.clone();
        let drain = tokio::task::spawn_blocking(move || {
            // end_of_stream() returns FlowError::Eos on the next
            // push, which is the signal mp4mux uses to write moov.
            let _ = appsrc.end_of_stream();
            let bus = match pipeline_for_blocking.bus() {
                Some(b) => b,
                None => return,
            };
            let deadline = std::time::Instant::now() + EOS_DRAIN_TIMEOUT;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return;
                }
                let timeout = gst::ClockTime::from_nseconds(remaining.as_nanos() as u64);
                match bus.timed_pop(Some(timeout)) {
                    None => return,
                    Some(msg) => match msg.view() {
                        gst::MessageView::Eos(..) => return,
                        gst::MessageView::Error(e) => {
                            warn!(error = %e.error(), "gst recorder bus error during close drain");
                            return;
                        }
                        _ => {}
                    },
                }
            }
        });
        let _ = drain.await;
        let _ = pipeline.set_state(gst::State::Null);

        let duration_ms = (args.ended_at - state.started_at).num_milliseconds().max(0);
        let size_bytes = match fs::metadata(&state.path).await {
            Ok(meta) => meta.len() as i64,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %state.path.display(),
                    "gst recorder could not stat clip; recording size_bytes=0"
                );
                0
            }
        };

        self.store
            .close_clip(
                handle.clip_id,
                &ClipClose {
                    ended_at: args.ended_at,
                    duration_ms,
                    size_bytes,
                },
            )
            .await?;

        debug!(
            camera_id = state.camera_id,
            clip_id = handle.clip_id,
            duration_ms,
            size_bytes,
            "gst recorder closed clip"
        );
        Ok(ClipMeta {
            clip_id: handle.clip_id,
            camera_id: state.camera_id,
            path: state.path,
            duration_ms,
            size_bytes,
            codec: CODEC.into(),
            container: CONTAINER.into(),
        })
    }

    fn set_panic(&self, panic: bool) {
        let mut guard = self.panic.lock();
        if *guard != panic {
            info!(panic, "gst recorder panic flag changed");
        }
        *guard = panic;
    }

    fn is_panic(&self) -> bool {
        *self.panic.lock()
    }

    fn kind(&self) -> &'static str {
        "gstreamer"
    }
}

/// Push one [`NalSample`] into appsrc, rebasing PTS/DTS so the
/// recording's timeline starts at zero. `appsrc` is configured with
/// `block=true`, so this call may block on filesink/disk pressure;
/// callers MUST run it inside a `spawn_blocking` (the live pump
/// already does, the snapshot prepend in `open()` runs once on the
/// open path which is acceptable).
fn push_sample(appsrc: &AppSrc, sample: &NalSample, base_pts: Duration) -> Result<(), String> {
    let mut buf =
        gst::Buffer::with_size(sample.data.len()).map_err(|e| format!("alloc gst::Buffer: {e}"))?;
    {
        let buf_mut = buf.get_mut().ok_or("buffer not unique")?;
        let mut map = buf_mut
            .map_writable()
            .map_err(|e| format!("map_writable: {e}"))?;
        map.copy_from_slice(&sample.data);
        drop(map);
        if let Some(pts) = sample.pts {
            let rebased = pts.saturating_sub(base_pts);
            buf_mut.set_pts(gst::ClockTime::from_nseconds(rebased.as_nanos() as u64));
        }
        if let Some(dts) = sample.dts {
            let rebased = dts.saturating_sub(base_pts);
            buf_mut.set_dts(gst::ClockTime::from_nseconds(rebased.as_nanos() as u64));
        }
        if !sample.is_keyframe {
            buf_mut.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
    }
    appsrc
        .push_buffer(buf)
        .map_err(|e| format!("appsrc push_buffer: {e:?}"))?;
    Ok(())
}

/// Forward live broadcast samples into appsrc until the stop signal
/// fires. De-dups against the snapshot tail by skipping any sample
/// whose PTS is `<=` the last PTS we pushed during snapshot prepend.
/// Each push runs inside `spawn_blocking` because appsrc is
/// configured with `block=true` and the underlying push can stall
/// for tens of ms on filesink/disk pressure.
async fn run_live_pump(
    camera_id: CameraId,
    clip_id: ClipId,
    appsrc: AppSrc,
    mut live_rx: broadcast::Receiver<NalSample>,
    base_pts: Duration,
    mut last_pushed_pts: Option<Duration>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                debug!(camera_id, clip_id, "live pump received stop signal");
                return;
            }
            recv = live_rx.recv() => match recv {
                Ok(sample) => {
                    // De-dup: skip anything whose PTS is at-or-before
                    // the last sample we pushed from the snapshot.
                    if let (Some(spts), Some(last)) = (sample.pts, last_pushed_pts) {
                        if spts <= last {
                            continue;
                        }
                    }
                    let push_appsrc = appsrc.clone();
                    let pushed_pts = sample.pts;
                    let result = tokio::task::spawn_blocking(move || {
                        push_sample(&push_appsrc, &sample, base_pts)
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {
                            if let Some(spts) = pushed_pts {
                                last_pushed_pts = Some(spts);
                            }
                        }
                        Ok(Err(e)) => {
                            warn!(camera_id, clip_id, error = %e, "live pump push failed; ending pump");
                            return;
                        }
                        Err(join_err) => {
                            warn!(camera_id, clip_id, error = %join_err, "live pump spawn_blocking panicked; ending pump");
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(camera_id, clip_id, dropped = n, "live pump lagged; samples dropped");
                    // Fall through and keep recv()ing from the new
                    // tail — short glitch in the recording but the
                    // clip continues.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(camera_id, clip_id, "live broadcast closed; ending pump");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{CameraConfig, StoreConfig};
    use std::path::PathBuf;
    use url::Url;

    async fn fixture() -> (Arc<Store>, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db_path.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "front".into(),
                url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                enabled: true,
                prompts: vec![],
                model_override: None,
                zones: vec![],
                max_fps: 0,
                parking_lot_mode: false,
            })
            .await
            .unwrap();
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();
        (store, dir, clips_dir)
    }

    #[tokio::test]
    async fn clip_path_is_stable_and_per_camera() {
        let (store, _dir, clips_dir) = fixture().await;
        let rec = GstClipRecorder::new(store, &clips_dir, HashMap::new()).unwrap();
        let t = chrono::DateTime::parse_from_rfc3339("2026-05-13T12:34:56.789Z")
            .unwrap()
            .with_timezone(&Utc);
        let p = rec.clip_path(1, t);
        assert!(p.starts_with(&clips_dir));
        assert!(
            p.to_string_lossy().contains("cam1"),
            "path missing cam1 component"
        );
        assert!(
            p.to_string_lossy().ends_with("20260513T123456_789.mp4"),
            "path tail wrong: {p:?}"
        );
    }

    #[tokio::test]
    async fn panic_flag_refuses_open_but_allows_close() {
        let (store, _dir, clips_dir) = fixture().await;
        // Even with no ingester registered the panic gate fires
        // first — panic is the highest-priority refusal.
        let rec = GstClipRecorder::new(store, &clips_dir, HashMap::new()).unwrap();
        rec.set_panic(true);
        assert!(rec.is_panic());
        let res = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: Utc::now(),
            })
            .await;
        assert!(matches!(res, Err(RecorderError::Refused)));
    }

    #[tokio::test]
    async fn open_with_no_ingester_for_camera_refuses() {
        let (store, _dir, clips_dir) = fixture().await;
        // Empty ingester map -> camera 1 lookup misses -> Refused.
        let rec = GstClipRecorder::new(store, &clips_dir, HashMap::new()).unwrap();
        let res = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: Utc::now(),
            })
            .await;
        assert!(matches!(res, Err(RecorderError::Refused)));
    }

    #[tokio::test]
    async fn kind_reports_gstreamer() {
        let (store, _dir, clips_dir) = fixture().await;
        let rec = GstClipRecorder::new(store, &clips_dir, HashMap::new()).unwrap();
        assert_eq!(rec.kind(), "gstreamer");
    }

    // -----------------------------------------------------------
    // Live RTSP acceptance test — drives the recorder against a
    // real camera. Skipped by default; opt in by setting
    // NEXUS_RTSP_TEST_URL=rtsp://user:pass@host/path. Asserts:
    //
    //   1. open() succeeds (pipeline reaches Playing).
    //   2. After ~5s of recording, close() returns ClipMeta with
    //      size_bytes > 0 and duration_ms ~5000ms.
    //   3. The on-disk file is a non-empty mp4 whose header bytes
    //      look like ISO BMFF ('ftyp' at offset 4..8).
    //
    // Run:
    //   NEXUS_RTSP_TEST_URL='rtsp://admin:Testing1928!@192.168.1.66/stream1' \
    //     cargo test -p nexus-pipeline --features gstreamer -- --ignored \
    //       live_rtsp_smoke --nocapture
    // -----------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires NEXUS_RTSP_TEST_URL pointing at a live RTSP camera"]
    async fn live_rtsp_smoke() {
        let Ok(url) = std::env::var("NEXUS_RTSP_TEST_URL") else {
            eprintln!(
                "skip: set NEXUS_RTSP_TEST_URL=rtsp://user:pass@host/path to run this acceptance test"
            );
            return;
        };
        let (store, _dir, clips_dir) = fixture().await;
        // Spin up the always-on ingester and wait for it to land at
        // least one keyframe so the pre-roll snapshot is non-empty
        // (otherwise the recording starts only at the first live
        // sample after open(), which is what pre-B8 did).
        let ingester = PreRollIngester::new(1, url, 5).expect("build ingester");
        for _ in 0..50 {
            if ingester.is_buffering() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            ingester.is_buffering(),
            "ingester never buffered any keyframe — camera unreachable?"
        );
        let mut ingesters: HashMap<i64, Arc<PreRollIngester>> = HashMap::new();
        ingesters.insert(1, ingester);
        let rec = GstClipRecorder::new(store.clone(), &clips_dir, ingesters).unwrap();

        let started_at = Utc::now();
        let handle = rec
            .open(OpenClip {
                camera_id: 1,
                started_at,
            })
            .await
            .expect("open() should succeed against a reachable camera");

        // Let the live pump forward ~5s of additional samples.
        // mp4mux fragments every 5000ms so this guarantees at least
        // one finalised moof/mdat pair lands in the file.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let ended_at = Utc::now();
        let meta = rec
            .close(handle, ClipFinal { ended_at })
            .await
            .expect("close() should return ClipMeta after EOS drain");

        assert!(
            meta.size_bytes > 0,
            "live RTSP clip is empty (0 bytes); pipeline drained nothing"
        );
        assert!(
            meta.duration_ms >= 4500 && meta.duration_ms <= 6000,
            "duration_ms {} outside expected ~5000ms window",
            meta.duration_ms
        );
        assert_eq!(meta.codec, "h264");
        assert_eq!(meta.container, "mp4");

        let bytes = tokio::fs::read(&meta.path).await.expect("clip readable");
        assert!(
            bytes.len() >= 8,
            "clip file too short: {} bytes",
            bytes.len()
        );
        // ISO BMFF: bytes 4..8 are the 'ftyp' atom marker.
        assert_eq!(
            &bytes[4..8],
            b"ftyp",
            "clip is not an ISO BMFF mp4 (missing ftyp marker); first 16 bytes = {:02x?}",
            &bytes[..16.min(bytes.len())]
        );

        eprintln!(
            "live RTSP smoke OK (with pre-roll): {} bytes, {} ms, path = {}",
            meta.size_bytes,
            meta.duration_ms,
            meta.path.display()
        );
    }
}
