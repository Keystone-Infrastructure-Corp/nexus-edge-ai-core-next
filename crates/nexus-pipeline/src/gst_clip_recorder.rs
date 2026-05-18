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
//! engine boot, but the map is wrapped in a `parking_lot::RwLock` so
//! the engine's `config.changed` reconciler can add / remove
//! per-camera ingesters live via [`Self::add_camera_ingester`] and
//! [`Self::remove_camera_ingester`]. A camera that wasn't reconciled
//! in (e.g. ingester build error) still triggers a `Refused` with a
//! one-shot warn log at `open()` time.

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
use parking_lot::{Mutex as PlMutex, RwLock as PlRwLock};
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
    ///
    /// Wrapped in `parking_lot::RwLock` so the engine's
    /// `config.changed` reconciler can hot-add / hot-remove
    /// ingesters without a restart. Reads (one per `open()`) take
    /// the shared lock; reconciler writes take the exclusive lock.
    /// Contention is negligible: clip opens fire at most a few
    /// times per minute per camera and the reconciler only writes
    /// on admin actions.
    ingesters: PlRwLock<HashMap<CameraId, Arc<PreRollIngester>>>,
    panic: PlMutex<bool>,
    /// Per-clip GStreamer + pump state. Held under a tokio Mutex
    /// because the close path awaits on the pump shutdown and the
    /// bus drain.
    open: Mutex<HashMap<ClipId, OpenState>>,
    /// M2.2: optional bus for publishing `CLIP_CLOSED` so the cold
    /// replicator can pick up new clips event-driven instead of
    /// waiting on its 5-min polling backstop. None in tests that
    /// don't care about cold-mirror; the close path skips publish
    /// when None.
    bus: Option<Arc<dyn nexus_bus::Bus>>,
    /// M2.2 Phase 3: optional USB resolver + preferred label. See
    /// [`crate::recorder::effective_clips_dir`] for the routing
    /// truth table. None on resolver disables USB tiering and
    /// every clip lands on the local hot tier. The label handle is
    /// shared with the admin API — see
    /// [`crate::recorder::PreferredUsbLabel`] for the rationale.
    usb_resolver: Option<Arc<dyn crate::recorder::UsbResolver>>,
    preferred_usb_label: crate::recorder::PreferredUsbLabel,
}

struct OpenState {
    camera_id: CameraId,
    started_at: DateTime<Utc>,
    path: PathBuf,
    /// Hot-tier directory the in-flight + final files live under.
    /// Cached at `open()` so `close()` writes the renamed file in
    /// the same dir even if a USB volume detached mid-recording
    /// (in-flight clips finish where they started).
    effective_dir: PathBuf,
    /// `"local"` or `"usb-<label>"`. Stamped on the row at open and
    /// repeated on the `CLIP_CLOSED` bus event.
    hot_handle: String,
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
            ingesters: PlRwLock::new(ingesters),
            panic: PlMutex::new(false),
            open: Mutex::new(HashMap::new()),
            bus: None,
            usb_resolver: None,
            preferred_usb_label: crate::recorder::PreferredUsbLabel::default(),
        })
    }

    /// Attach a bus so the recorder publishes `topic::CLIP_CLOSED`
    /// on every successful normal-path close. Builder pattern so
    /// existing callsites that don't yet pass a bus keep working.
    pub fn with_bus(mut self, bus: Arc<dyn nexus_bus::Bus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// M2.2 Phase 3: attach a USB resolver + preferred label so
    /// new clips can be routed to a hot-tier USB volume. Both
    /// arguments are required together. Builder pattern so
    /// existing callsites that don't yet care about USB keep
    /// working unchanged.
    ///
    /// `preferred_label` accepts anything that converts into a
    /// [`crate::recorder::PreferredUsbLabel`] — the engine passes a
    /// shared handle; tests pass a bare `Some("X".to_string())`.
    pub fn with_usb(
        mut self,
        resolver: Arc<dyn crate::recorder::UsbResolver>,
        preferred_label: impl Into<crate::recorder::PreferredUsbLabel>,
    ) -> Self {
        self.usb_resolver = Some(resolver);
        self.preferred_usb_label = preferred_label.into();
        self
    }

    /// In-flight path the recorder will write for `(camera_id,
    /// started_at)`. Per M2.1 spec layout — see
    /// [`crate::recorder::inflight_clip_path`]. The file is renamed
    /// to its final `{start_ms}_{duration_ms}.mp4` shape on close.
    pub fn clip_path(&self, camera_id: CameraId, started_at: DateTime<Utc>) -> PathBuf {
        crate::recorder::inflight_clip_path(&self.clips_dir, camera_id, started_at)
    }

    /// Returns the parse-launch description of the recorder
    /// pipeline. Extracted so tests can assert the M2.1 spec
    /// invariant that the recorder is a strict codec passthrough
    /// (no encoder, no raw-video conversion). Keep this aligned
    /// with [`Self::build_pipeline`].
    fn pipeline_desc(location: &Path) -> String {
        let location_safe = location.to_string_lossy().replace('"', "");
        // appsrc max-bytes=64 MiB ≈ 128 s headroom at 4 Mbps. The bigger
        // the queue, the longer push_buffer can stay non-blocking under
        // disk stalls, which keeps the upstream broadcast channel from
        // filling up and dropping samples (the most common cause of
        // visibly choppy clips).
        format!(
            "appsrc name=src is-live=false format=time do-timestamp=false \
                     stream-type=stream max-bytes=67108864 block=true \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=avc,alignment=au \
             ! mp4mux fragment-duration=5000 streamable=true faststart=true \
             ! filesink location=\"{location_safe}\" sync=false"
        )
    }

    fn build_pipeline(location: &Path) -> Result<(gst::Pipeline, AppSrc), RecorderError> {
        // location came from clips_dir + a deterministic timestamp
        // template; strip embedded `"` before splicing into launch
        // string so a pathological path can't break the parser.
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
        //   max-bytes=64M   : ~128s of headroom at 4 Mbps; the live
        //                     pump shouldn't ever hit this in normal
        //                     operation but it bounds memory if
        //                     filesink is slow (full disk). Sized
        //                     deliberately large because any push
        //                     stall propagates back to the broadcast
        //                     channel and starts dropping frames.
        let desc = Self::pipeline_desc(location);
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
        let ingester = match self.ingesters.read().get(&args.camera_id) {
            Some(i) => i.clone(),
            None => {
                warn!(
                    camera_id = args.camera_id,
                    "GstClipRecorder: no PreRollIngester for this camera; refusing open. \
                     The engine's config.changed reconciler should have built one — \
                     check the engine log for `failed to start pre-roll ingester` warnings."
                );
                return Err(RecorderError::Refused);
            }
        };

        // Resolve USB hot-tier routing once at open(). The choice
        // is captured into `OpenState` so close() finishes the clip
        // in the same dir even if the volume detaches mid-recording.
        // `preferred_usb_label.get()` reads the *current* admin
        // setting; an in-flight clip ignores subsequent PUTs.
        let preferred = self.preferred_usb_label.get();
        let (effective_dir, hot_handle) = crate::recorder::effective_clips_dir(
            &self.clips_dir,
            preferred.as_deref(),
            self.usb_resolver
                .as_deref()
                .map(|r| r as &dyn crate::recorder::UsbResolver),
        );

        let path =
            crate::recorder::inflight_clip_path(&effective_dir, args.camera_id, args.started_at);
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

        let rel = crate::recorder::clip_rel_path(&self.clips_dir, &path);
        let new = NewClip {
            camera_id: args.camera_id,
            started_at: args.started_at,
            hot_path: rel,
            codec: CODEC.into(),
            container: CONTAINER.into(),
            // M2.2: "local" or "usb-<label>" depending on the hot
            // tier resolution above. Cold pointer is left null for
            // the replicator to fill in after the close-time hash.
            hot_handle: hot_handle.clone(),
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
                effective_dir,
                hot_handle,
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

        // M2.1 spec: discard sub-3s clips. The pipeline + filesink
        // are already torn down so we can safely unlink the file.
        if duration_ms < crate::recorder::MIN_CLIP_DURATION_MS {
            warn!(
                camera_id = state.camera_id,
                clip_id = handle.clip_id,
                duration_ms,
                min_ms = crate::recorder::MIN_CLIP_DURATION_MS,
                "gst recorder: clip too short -- discarding (delete file + cascade-delete metadata)"
            );
            if let Err(e) = fs::remove_file(&state.path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        clip_id = handle.clip_id,
                        path = %state.path.display(),
                        error = %e,
                        "gst recorder: failed to unlink discarded short clip"
                    );
                }
            }
            self.store
                .cascade_delete_clip_metadata(handle.clip_id)
                .await?;
            return Ok(ClipMeta {
                clip_id: handle.clip_id,
                camera_id: state.camera_id,
                path: state.path,
                duration_ms,
                size_bytes: 0,
                codec: CODEC.into(),
                container: CONTAINER.into(),
                discarded: true,
            });
        }

        // Normal close path: rename in-flight file to spec layout
        // `{start_ms}_{duration_ms}.mp4`, stat for size, stamp the row.
        let final_abs = crate::recorder::final_clip_path(
            &state.effective_dir,
            state.camera_id,
            state.started_at,
            duration_ms,
        );
        if let Err(e) = fs::rename(&state.path, &final_abs).await {
            warn!(
                clip_id = handle.clip_id,
                from = %state.path.display(),
                to   = %final_abs.display(),
                error = %e,
                "gst recorder: rename to final path failed; row will keep in-flight path"
            );
        }
        let final_used = if final_abs.exists() {
            final_abs
        } else {
            state.path.clone()
        };
        let rel = crate::recorder::clip_rel_path(&self.clips_dir, &final_used);
        let size_bytes = match fs::metadata(&final_used).await {
            Ok(meta) => meta.len() as i64,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %final_used.display(),
                    "gst recorder could not stat clip; recording size_bytes=0"
                );
                0
            }
        };

        // M2.2: hash the closed mp4 so the cold replicator knows what
        // bytes it's about to copy. Done in a blocking task to keep
        // the tokio worker pool free; mp4 clips are typically <50 MB
        // so the wall-clock cost is sub-100 ms on modern SSDs. A
        // hash failure is logged and stored as NULL — the replicator
        // skips NULL-sha256 rows on the next tick rather than copy
        // an unverified blob.
        let sha256 = match hash_file_sha256(&final_used).await {
            Ok(hex) => Some(hex),
            Err(e) => {
                warn!(
                    error = %e,
                    path = %final_used.display(),
                    "gst recorder: sha256 of closed clip failed; cold replicator will skip"
                );
                None
            }
        };

        // Snapshot fields we want to publish AFTER moving them into
        // ClipClose below.
        let rel_for_event = rel.clone();
        let sha256_for_event = sha256.clone();

        self.store
            .close_clip(
                handle.clip_id,
                &ClipClose {
                    ended_at: args.ended_at,
                    duration_ms,
                    size_bytes,
                    hot_path: Some(rel),
                    sha256,
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

        // M2.2: notify the cold replicator. Best-effort — a missing
        // bus, a serialize failure, or zero subscribers MUST NOT
        // fail the close (the row is already on disk and the 5-min
        // polling backstop will pick it up regardless).
        if let Some(bus) = &self.bus {
            let payload = serde_json::json!({
                "clip_id": handle.clip_id,
                "camera_id": state.camera_id,
                "hot_handle": state.hot_handle,
                "hot_path": rel_for_event,
                "size_bytes": size_bytes,
                "sha256": sha256_for_event,
            });
            use nexus_bus::BusExt;
            if let Err(e) = bus.publish(nexus_bus::topic::CLIP_CLOSED, &payload).await {
                debug!(
                    clip_id = handle.clip_id,
                    error = %e,
                    "publish CLIP_CLOSED failed (replicator will catch up via polling backstop)"
                );
            }
        }

        Ok(ClipMeta {
            clip_id: handle.clip_id,
            camera_id: state.camera_id,
            path: final_used,
            duration_ms,
            size_bytes,
            codec: CODEC.into(),
            container: CONTAINER.into(),
            discarded: false,
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

    fn add_camera_ingester(
        &self,
        camera_id: CameraId,
        url: &str,
        pre_roll_secs: u32,
    ) -> Result<(), RecorderError> {
        // Idempotent + URL-aware: if we already have an ingester for
        // this camera with the same URL, do nothing. If the URL
        // changed (e.g. operator re-pointed the camera), tear down
        // the old one before building the new — running two RTSP
        // sessions against one camera tends to make the camera-side
        // session counter pop.
        {
            let read = self.ingesters.read();
            if let Some(existing) = read.get(&camera_id) {
                if existing.url() == url {
                    debug!(
                        camera_id,
                        "add_camera_ingester: ingester already running for this URL — skipping"
                    );
                    return Ok(());
                }
            }
        }
        let new_ing = PreRollIngester::new(camera_id, url.to_string(), pre_roll_secs)
            .map_err(|e| RecorderError::Io(std::io::Error::other(format!("ingester: {e}"))))?;
        // Insert under the exclusive lock; dropping the previous
        // `Arc<PreRollIngester>` here triggers its supervisor
        // shutdown via Drop, which cleans up the GStreamer pipeline
        // and reconnect task synchronously.
        let prev = self.ingesters.write().insert(camera_id, new_ing);
        if prev.is_some() {
            info!(camera_id, %url, "pre-roll ingester replaced (URL changed)");
        } else {
            info!(camera_id, %url, pre_roll_secs, "pre-roll ingester started (hot-add)");
        }
        Ok(())
    }

    fn remove_camera_ingester(&self, camera_id: CameraId) {
        if self.ingesters.write().remove(&camera_id).is_some() {
            info!(camera_id, "pre-roll ingester removed (hot-remove)");
        }
    }
}

/// Compute the lower-case hex sha256 of `path`. Reads the file in
/// 1 MiB chunks on a blocking task so the tokio worker pool stays
/// free while a 50 MB clip hashes (~50 ms on NVMe). Surfaced as an
/// `io::Error` on any failure so the caller can swallow it (and
/// store sha256=NULL) without leaking a Box<dyn Error>.
async fn hash_file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
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
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    enabled: true,
                    max_fps: 0,
                },
                detector: nexus_config::CameraDetector::default(),
                behavior: nexus_config::CameraBehavior::default(),
                zones: vec![],
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
        let p1 = rec.clip_path(1, t);
        let p2 = rec.clip_path(2, t);
        assert!(p1.starts_with(&clips_dir));
        // M2.1 spec layout: {clips_dir}/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}.partial.mp4
        let expected_ms = t.timestamp_millis();
        let s1 = p1.to_string_lossy().to_string();
        let s2 = p2.to_string_lossy().to_string();
        assert!(s1.contains("/1/"), "path missing camera_id component: {s1}");
        assert!(s2.contains("/2/"), "path missing camera_id component: {s2}");
        assert!(
            s1.contains("/2026-05-13/"),
            "path missing UTC date dir: {s1}"
        );
        assert!(
            s1.ends_with(&format!("{expected_ms}.partial.mp4")),
            "path tail wrong: {s1}"
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

    /// M2.1 audit: the recorder MUST be a strict codec passthrough.
    /// We assert that the parse-launch description contains
    /// `h264parse` (mux-side parser only, no decode/re-encode) and
    /// does NOT contain any of the common H.264 encoders or any
    /// raw-video element. If this fires, somebody has sneaked an
    /// encoder into the pipeline and the recorder is no longer
    /// passthrough — that's a CPU-cost regression and a quality
    /// regression and needs a deliberate decision.
    #[test]
    fn pipeline_string_is_codec_passthrough() {
        let desc = GstClipRecorder::pipeline_desc(Path::new(
            "/var/lib/nexus/clips/1/2026-05-13/1234567890.partial.mp4",
        ));
        assert!(desc.contains("appsrc"), "missing appsrc: {desc}");
        assert!(desc.contains("h264parse"), "missing h264parse: {desc}");
        assert!(desc.contains("mp4mux"), "missing mp4mux: {desc}");
        assert!(desc.contains("filesink"), "missing filesink: {desc}");
        for forbidden in [
            "x264enc",
            "avenc_h264",
            "openh264enc",
            "vaapih264enc",
            "vtenc_h264",
            "videoconvert",
            "video/x-raw",
            "decodebin",
            "avdec_h264",
        ] {
            assert!(
                !desc.contains(forbidden),
                "recorder pipeline must be codec-passthrough but contains `{forbidden}`: {desc}"
            );
        }
    }
}
