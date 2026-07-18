//! GStreamer-backed clip recorder — M2.1 Stage B (PR B2 + B8 refactor).
//!
//! Per-clip pipeline lifecycle (post-B8 — single RTSP connection per
//! camera, fed by [`PreRollIngester`]):
//!
//! ```text
//!   appsrc name=src is-live=true format=time do-timestamp=false
//!     ! h264parse config-interval=0
//!     ! mp4mux fragment-duration=5000 streamable=true faststart=true
//!     ! filesink location=/var/lib/nexus/clips/cam1/...mp4
//! ```
//!
//! `config-interval=0` (trust the source) is critical: the upstream
//! [`PreRollIngester`] already runs its own h264parse and emits
//! source-natural byte-stream with SPS+PPS in front of every IDR
//! (true for every modern IP camera we've tested — InSight, Reolink,
//! mediamtx loopbacks). If this h264parse uses `config-interval=-1`
//! ("insert SPS+PPS before every IDR") it DOUBLES the parameter sets
//! to `[AUD, SPS, PPS, SPS, PPS, IDR]`. The duplicated SPS/PPS look
//! like the start of a NEW access unit to the parser's own framing
//! logic; the synthetic AU inherits no PTS from the source buffer,
//! and mp4mux silently rejects every PTS-less buffer with the
//! cryptic `error: Buffer has no PTS.` (gstqtmux.c:5927). The on-
//! disk file ends at 864 bytes (ftyp+moov stub) and the bus error
//! at EOS is the generic `Could not multiplex stream.` with no
//! further context. Diagnose with `GST_DEBUG=qtmux:5,h264parse:4`.
//!
//! Same-PTS sample coalescing ([`coalesce_same_pts`]): some IP
//! cameras (the InSight at `192.168.1.66` on our bench, both
//! `stream1` and `stream2`) emit access units the upstream
//! [`PreRollIngester`]'s h264parse splits into TWO appsink
//! emissions sharing one PTS — one tiny buffer carrying just
//! `[AUD, SEI]` (~35 bytes) and a second carrying the actual
//! `[slice ...]` payload. The recorder's h264parse sees these as
//! two separate AUs; it emits the first to mp4mux as a 35-byte
//! buffer WITH NO PTS (because the AU it represents is incomplete
//! until the slice arrives) which mp4mux flags as
//! `Sample with zero duration ... due to missing or backward
//! timestamps`. The very next valid buffer then trips the
//! `Buffer has no PTS` error against the cached previous buffer
//! and the whole clip dies at 864 bytes. We re-aggregate by
//! buffering any sample whose PTS equals the pending sample's PTS
//! and concatenating their byte-stream data into one push — both
//! in the snapshot pre-push pass and in the live pump. mp4mux
//! sees one PTS-bearing AU per frame and writes a healthy clip.
//! Live-pump pending buffers are flushed on a 200 ms inactivity
//! timer so a stalled stream can't strand the very last AU.
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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use nexus_config::AlertClipsConfig;
use nexus_store::{AlertClipId, ClipClose, ClipId, NewAlertClip, NewClip, Store};
use nexus_types::{CameraId, CodecKind};
use parking_lot::{Mutex as PlMutex, RwLock as PlRwLock};
use tokio::fs;
use tokio::sync::{broadcast, oneshot, Mutex, Notify};
use tracing::{debug, info, warn};

use crate::alert_clip::{
    alert_clip_inflight_path, alert_clip_rel_path, encode_alert_clip, trim_preroll, BoxFrame,
    BoxTimeline, BurnBox,
};
use crate::preroll::NalSample;
use crate::preroll_ingester::PreRollIngester;
use crate::recorder::{ClipFinal, ClipHandle, ClipMeta, ClipRecorder, OpenClip, RecorderError};
use crate::source::gst_init;

/// How long to wait for the per-clip pipeline to drain after we send
/// EOS at close time. 5s is generous; in practice mp4mux flushes in
/// well under a second once it sees EOS at the appsrc head.
const EOS_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Hardware-decode strategy applied to every per-camera RGB tap
    /// this recorder builds (`add_camera_ingester` /
    /// `resize_camera_rgb_tap`). Defaults to [`DecodeMode::Auto`];
    /// the engine overrides it from `[runtime.decode] mode` via
    /// [`Self::with_decode_mode`].
    decode_mode: crate::decode::DecodeMode,
    /// M-Alert-Clip configuration (defaults disabled). Set via
    /// [`Self::with_alert_clips`]; when disabled every alert-clip
    /// hook below is inert.
    alert_clips_cfg: AlertClipsConfig,
    /// Per-camera rolling box timeline the supervisor feeds via
    /// [`ClipRecorder::push_alert_boxes`]; the builder snapshots it
    /// to burn boxes into the decoded clip. `RwLock` over the map
    /// (rare inserts on first frame per camera), `Mutex` per timeline
    /// (one writer per gated frame, one reader per clip).
    alert_box_timelines: PlRwLock<HashMap<CameraId, Arc<PlMutex<BoxTimeline>>>>,
    /// Per-camera in-flight alert clip, for burst coalescing. Behind
    /// an `Arc` so the spawned builder task can remove its own entry
    /// on completion.
    alert_inflight: Arc<PlMutex<HashMap<CameraId, AlertInflight>>>,
    /// Optional wake handle shared with the cold replicator
    /// (M-Alert-Clip cloud delivery). The builder fires `notify_one()`
    /// the moment an alert clip is stamped `ready`, so the replicator
    /// ships it to the cloud immediately instead of waiting up to 5 min
    /// for its polling backstop. `None` (LAN-only / no cloud) leaves
    /// alert clips to the replicator's normal cadence.
    alert_cold_kick: Option<Arc<Notify>>,
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
    /// Wire codec captured from the per-camera ingester at
    /// `open()`. Stamped on the `motion_clips` row via
    /// [`CodecKind::base`] (collapses `_plus` vendor SVC labels
    /// to their base) and used by the live pump / coalesce path
    /// to dispatch slice-NAL detection (H.264 NAL header is 1 byte,
    /// H.265 NAL header is 2 bytes).
    codec: CodecKind,
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

/// Per-camera in-flight alert clip, for burst coalescing (M-Alert-Clip).
/// While one exists and its `deadline` hasn't passed, a new alert on the
/// same camera coalesces into it (bumping the deadline) instead of
/// starting a second clip.
struct AlertInflight {
    alert_clip_id: AlertClipId,
    /// Collection deadline as unix milliseconds. The builder collects
    /// the post window until `now >= deadline`; a coalescing alert
    /// bumps it via an atomic store so the single clip spans the burst.
    deadline: Arc<AtomicI64>,
}

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
            decode_mode: crate::decode::DecodeMode::default(),
            alert_clips_cfg: AlertClipsConfig::default(),
            alert_box_timelines: PlRwLock::new(HashMap::new()),
            alert_inflight: Arc::new(PlMutex::new(HashMap::new())),
            alert_cold_kick: None,
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

    /// Set the hardware-decode strategy for every per-camera RGB tap
    /// this recorder builds. Builder pattern so existing callsites
    /// that don't pass a mode keep the [`DecodeMode::Auto`] default.
    /// The engine wires this from `[runtime.decode] mode`.
    pub fn with_decode_mode(mut self, mode: crate::decode::DecodeMode) -> Self {
        self.decode_mode = mode;
        self
    }

    /// Enable alert-clip building with the given config (M-Alert-Clip).
    /// Builder pattern; the default is disabled, so callsites that
    /// don't pass a config keep today's behaviour (no alert clips).
    pub fn with_alert_clips(mut self, cfg: AlertClipsConfig) -> Self {
        self.alert_clips_cfg = cfg;
        self
    }

    /// Share the cold replicator's wake handle so a freshly-built alert
    /// clip kicks cloud replication immediately (M-Alert-Clip cloud
    /// delivery). Builder pattern; without it alert clips still ship on
    /// the replicator's normal cadence, just with more latency.
    pub fn with_alert_cold_kick(mut self, kick: Arc<Notify>) -> Self {
        self.alert_cold_kick = Some(kick);
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
    fn pipeline_desc(location: &Path, codec: CodecKind) -> String {
        let location_safe = location.to_string_lossy().replace('"', "");
        // Codec-specific parser + mp4mux stream-format. `avc` for
        // H.264, `hvc1` for H.265 — both accepted by mp4mux without
        // any other knob changes.
        let (parse, stream_format, caps_name) = match codec.base() {
            "h265" => ("h265parse", "hvc1", "video/x-h265"),
            _ => ("h264parse", "avc", "video/x-h264"),
        };
        // appsrc max-bytes=64 MiB ≈ 128 s headroom at 4 Mbps. The bigger
        // the queue, the longer push_buffer can stay non-blocking under
        // disk stalls, which keeps the upstream broadcast channel from
        // filling up and dropping samples (the most common cause of
        // visibly choppy clips).
        // {parse} config-interval=0 (trust the source). See module
        // docstring: -1 doubles SPS/PPS because the upstream ingester
        // already includes them per-keyframe, which makes mp4mux drop
        // every buffer with "Buffer has no PTS." and the output is
        // an 864-byte ftyp+moov stub.
        format!(
            "appsrc name=src is-live=false format=time do-timestamp=false \
                     stream-type=stream max-bytes=67108864 block=true \
             ! {parse} config-interval=0 \
             ! {caps_name},stream-format={stream_format},alignment=au \
             ! mp4mux fragment-duration=5000 streamable=true faststart=true \
             ! filesink location=\"{location_safe}\" sync=false"
        )
    }

    fn build_pipeline(
        location: &Path,
        codec: CodecKind,
    ) -> Result<(gst::Pipeline, AppSrc), RecorderError> {
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
        let desc = Self::pipeline_desc(location, codec);
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
        // Tell appsrc the caps explicitly — mp4mux + {parse} can't
        // negotiate without knowing the byte-stream codec. We don't
        // claim a framerate (mp4mux infers from PTS).
        let caps_name = match codec.base() {
            "h265" => "video/x-h265",
            _ => "video/x-h264",
        };
        let caps = gst::Caps::builder(caps_name)
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
        let codec = ingester.codec();

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

        let (pipeline, appsrc) = Self::build_pipeline(&path, codec)?;
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
        //
        // Pre-pass: coalesce any consecutive samples sharing a PTS
        // into one combined sample so the recorder's h264parse sees
        // a complete AU per push. See `coalesce_same_pts` for why
        // the InSight cameras need this.
        let snapshot_tail_pts = snapshot.iter().filter_map(|s| s.pts).next_back();
        let snapshot_for_blocking: Vec<NalSample> = {
            let mut pending: Option<NalSample> = None;
            let mut out: Vec<NalSample> = Vec::with_capacity(snapshot.len());
            for s in snapshot {
                if let Some(flushed) = coalesce_same_pts(&mut pending, s, codec) {
                    out.push(flushed);
                }
            }
            // Drain any tail. If it's a header-only stub (no slice
            // NAL), drop it — pushing it would just create the
            // 35-byte "missing picture" pathology we're guarding
            // against.
            if let Some(last) = pending.take() {
                if contains_slice_nal(&last.data, codec) {
                    out.push(last);
                } else {
                    debug!(
                        camera_id = args.camera_id,
                        size = last.data.len(),
                        "dropping snapshot tail: header-only stub (no slice NAL)"
                    );
                }
            }
            out
        };
        let appsrc_for_blocking = appsrc.clone();
        let (preroll_count, preroll_last_written_pts_ns) =
            match tokio::task::spawn_blocking(move || {
                let mut n = 0usize;
                let mut last_written: Option<u64> = None;
                for sample in &snapshot_for_blocking {
                    let written = push_sample(
                        &appsrc_for_blocking,
                        sample,
                        base_pts,
                        last_written,
                        FALLBACK_FRAME_INTERVAL_NS,
                    )
                    .map_err(|e| {
                        RecorderError::Io(std::io::Error::other(format!(
                            "push pre-roll sample: {e}"
                        )))
                    })?;
                    last_written = Some(written);
                    n += 1;
                }
                Ok::<(usize, Option<u64>), RecorderError>((n, last_written))
            })
            .await
            {
                Ok(Ok((n, last))) => (n, last),
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
            codec: codec.base().into(),
            container: CONTAINER.into(),
            // M2.2: "local" or "usb-<label>" depending on the hot
            // tier resolution above. Cold pointer is left null for
            // the replicator to fill in after the close-time hash.
            hot_handle: hot_handle.clone(),
            frame_width: args.frame_width,
            frame_height: args.frame_height,
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
            codec,
            pump_appsrc,
            live_rx,
            base_pts,
            last_pushed_pts,
            preroll_last_written_pts_ns,
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
                codec,
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
                            warn!(
                                error = %e.error(),
                                debug = %e.debug().unwrap_or_else(|| "<none>".into()),
                                src = ?e.src().map(|s| s.path_string()),
                                "gst recorder bus error during close drain"
                            );
                            return;
                        }
                        gst::MessageView::Warning(w) => {
                            warn!(
                                warning = %w.error(),
                                debug = %w.debug().unwrap_or_else(|| "<none>".into()),
                                src = ?w.src().map(|s| s.path_string()),
                                "gst recorder bus warning during close drain"
                            );
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
                codec: state.codec.base().into(),
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
            codec: state.codec.base().into(),
            container: CONTAINER.into(),
            discarded: false,
        })
    }

    async fn inflight_size_bytes(&self, handle: ClipHandle) -> Option<u64> {
        let path = {
            let open = self.open.lock().await;
            open.get(&handle.clip_id).map(|s| s.path.clone())?
        };
        tokio::fs::metadata(&path).await.ok().map(|m| m.len())
    }

    async fn shutdown(&self) {
        // Snapshot the (clip_id, camera_id) of every in-flight clip
        // under one lock, then release it so `close()` can re-lock
        // and `remove()` each one. Cloning the keys avoids holding
        // the lock across the per-clip EOS drain (which awaits the
        // bus for up to `EOS_DRAIN_TIMEOUT`).
        let handles: Vec<ClipHandle> = {
            let open = self.open.lock().await;
            open.iter()
                .map(|(clip_id, st)| ClipHandle {
                    clip_id: *clip_id,
                    camera_id: st.camera_id,
                })
                .collect()
        };
        if handles.is_empty() {
            return;
        }
        info!(
            count = handles.len(),
            "gst recorder: draining in-flight clips on shutdown (EOS + moov flush)"
        );
        let ended_at = Utc::now();
        for handle in handles {
            // Reuse the full `close()` finalise path (EOS → moov
            // flush → rename → row stamp). A concurrent supervisor
            // close of the same clip is safe: whichever calls
            // `open.remove()` first wins, the loser gets
            // `UnknownClip` which we swallow.
            match self.close(handle, ClipFinal { ended_at }).await {
                Ok(_) | Err(RecorderError::UnknownClip(_)) => {}
                Err(e) => warn!(
                    clip_id = handle.clip_id,
                    camera_id = handle.camera_id,
                    error = %e,
                    "gst recorder: failed to finalise clip during shutdown drain"
                ),
            }
        }
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
        max_fps: u32,
        rgb_w: u32,
        rgb_h: u32,
        codec: CodecKind,
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
                if existing.url() == url && existing.codec() == codec {
                    debug!(
                        camera_id,
                        "add_camera_ingester: ingester already running for this URL+codec — skipping"
                    );
                    return Ok(());
                }
            }
        }
        // Build with the shared RGB tap so the supervisor's
        // SharedRtspSource can consume detector frames off the same
        // RTSP session — required for cameras whose firmware caps
        // concurrent sessions at 1 per stream path (InSight et al).
        let new_ing = PreRollIngester::new_with_rgb(
            camera_id,
            url.to_string(),
            pre_roll_secs,
            codec,
            self.decode_mode,
            max_fps,
            rgb_w,
            rgb_h,
        )
        .map_err(|e| RecorderError::Io(std::io::Error::other(format!("ingester: {e}"))))?;
        // Insert under the exclusive lock; dropping the previous
        // `Arc<PreRollIngester>` here triggers its supervisor
        // shutdown via Drop, which cleans up the GStreamer pipeline
        // and reconnect task synchronously.
        let prev = self.ingesters.write().insert(camera_id, new_ing);
        if let Some(prev_ing) = prev {
            // Same reason as `remove_camera_ingester`: other
            // holders (a stale `SharedRtspSource` clone, an
            // in-flight clip's snapshot Arc) would otherwise keep
            // the previous ingester's supervisor reconnecting to
            // the old URL/codec for an unbounded amount of time
            // after we install the replacement.
            prev_ing.shutdown();
            info!(camera_id, %url, codec = %codec, "pre-roll ingester replaced (URL or codec changed)");
        } else {
            info!(camera_id, %url, pre_roll_secs, max_fps, codec = %codec, "pre-roll ingester started (hot-add)");
        }
        Ok(())
    }

    fn remove_camera_ingester(&self, camera_id: CameraId) {
        if let Some(ing) = self.ingesters.write().remove(&camera_id) {
            // Synchronously stop the supervisor + null the
            // GStreamer pipeline regardless of how many other
            // `Arc<PreRollIngester>` clones exist (a per-camera
            // supervisor's `SharedRtspSource`, an in-flight clip's
            // snapshot Arc). Without this, those clones would keep
            // the struct alive past the map removal, the supervisor
            // task we're about to lose track of would keep
            // reconnecting forever, and a misconfigured camera
            // that was added then deleted would leak GstPipelines
            // at the reconnect-backoff rate until process exit.
            ing.shutdown();
            info!(camera_id, "pre-roll ingester removed (hot-remove)");
        }
    }

    fn shared_frame_source(
        &self,
        camera_id: CameraId,
    ) -> Option<Box<dyn crate::source::FrameSource + Send>> {
        let read = self.ingesters.read();
        let ing = read.get(&camera_id)?;
        if !ing.has_rgb_tap() {
            // Ingester built without RGB tap (legacy path / test
            // construction). Returning None has the supervisor
            // open its own RtspSource — fine on cameras that allow
            // multiple sessions, but the whole point of this trait
            // method is to AVOID that on single-session firmwares.
            return None;
        }
        Some(Box::new(crate::source::SharedRtspSource {
            camera_id,
            ingester: ing.clone(),
        }))
    }

    fn resize_camera_rgb_tap(
        &self,
        camera_id: CameraId,
        new_rgb_w: u32,
        new_rgb_h: u32,
    ) -> Result<bool, RecorderError> {
        // Snapshot the existing ingester's identity + connection
        // params under a short read lock so we can drop it before
        // doing the slow PreRollIngester::new_with_rgb call.
        let snapshot = {
            let read = self.ingesters.read();
            read.get(&camera_id).map(|ing| {
                (
                    ing.url().to_string(),
                    ing.codec(),
                    ing.pre_roll_secs(),
                    ing.max_fps(),
                    ing.rgb_w(),
                    ing.rgb_h(),
                    ing.has_rgb_tap(),
                )
            })
        };
        let Some((url, codec, pre_roll_secs, max_fps, cur_w, cur_h, had_rgb)) = snapshot else {
            debug!(
                camera_id,
                new_rgb_w, new_rgb_h, "resize_camera_rgb_tap: no ingester registered"
            );
            return Ok(false);
        };
        if !had_rgb {
            // Ingester was built via `new` (no RGB tap). Resizing it
            // would change observable behaviour for callers that
            // built the legacy path on purpose; refuse instead.
            debug!(
                camera_id,
                "resize_camera_rgb_tap: existing ingester has no RGB tap, skipping"
            );
            return Ok(false);
        }
        if cur_w == new_rgb_w && cur_h == new_rgb_h {
            return Ok(false);
        }
        let new_ing = PreRollIngester::new_with_rgb(
            camera_id,
            url.clone(),
            pre_roll_secs,
            codec,
            self.decode_mode,
            max_fps,
            new_rgb_w,
            new_rgb_h,
        )
        .map_err(|e| RecorderError::Io(std::io::Error::other(format!("ingester: {e}"))))?;
        let prev = self.ingesters.write().insert(camera_id, new_ing);
        if let Some(prev_ing) = prev {
            // Same justification as the URL-change replace path in
            // `add_camera_ingester`: any stale `SharedRtspSource`
            // clones must NOT keep the previous supervisor
            // reconnecting against the old (now-superseded) RGB
            // dims indefinitely.
            prev_ing.shutdown();
        }
        info!(
            camera_id,
            %url,
            prev_w = cur_w,
            prev_h = cur_h,
            new_w = new_rgb_w,
            new_h = new_rgb_h,
            "pre-roll ingester RGB tap resized (crowd hysteresis)"
        );
        Ok(true)
    }

    fn push_alert_boxes(
        &self,
        camera_id: CameraId,
        ts: DateTime<Utc>,
        boxes: Vec<BurnBox>,
        sup_w: u32,
        sup_h: u32,
    ) {
        let cfg = &self.alert_clips_cfg;
        if !cfg.enabled {
            return;
        }
        // Keep a little more than the widest possible window so the
        // pre-roll is always covered even under coalescing.
        let retain = Duration::from_secs(u64::from(cfg.pre_secs + cfg.post_secs + 2));
        let existing = self.alert_box_timelines.read().get(&camera_id).cloned();
        let timeline = match existing {
            Some(t) => t,
            None => {
                let t = Arc::new(PlMutex::new(BoxTimeline::new(retain)));
                self.alert_box_timelines
                    .write()
                    .insert(camera_id, t.clone());
                t
            }
        };
        timeline.lock().push(BoxFrame {
            ts,
            boxes,
            sup_w,
            sup_h,
        });
    }

    async fn arm_alert_clip(
        &self,
        camera_id: CameraId,
        alert_ts: DateTime<Utc>,
    ) -> Option<AlertClipId> {
        let cfg = self.alert_clips_cfg.clone();
        if !cfg.enabled {
            return None;
        }
        let deadline_ms = alert_ts.timestamp_millis() + i64::from(cfg.post_secs) * 1000;

        // Coalesce into an in-flight builder whose window is still open
        // (one clip per motion burst). The parking_lot guard is dropped
        // at the end of this block — never held across an await.
        {
            let inflight = self.alert_inflight.lock();
            if let Some(entry) = inflight.get(&camera_id) {
                if Utc::now().timestamp_millis() < entry.deadline.load(Ordering::Acquire) {
                    let cur = entry.deadline.load(Ordering::Acquire);
                    entry
                        .deadline
                        .store(cur.max(deadline_ms), Ordering::Release);
                    return Some(entry.alert_clip_id);
                }
            }
        }

        // Start a fresh alert clip.
        let ingester = self.ingesters.read().get(&camera_id).cloned()?;
        let codec = ingester.codec();
        // Snapshot + trim the pre-roll ring; derive the window-start
        // wall-clock from the trimmed pre span so per-frame box lookup
        // lands on the right instant.
        let pre = trim_preroll(
            ingester.snapshot(),
            Duration::from_secs(u64::from(cfg.pre_secs)),
        );
        let pre_span = match (
            pre.first().and_then(|s| s.pts),
            pre.last().and_then(|s| s.pts),
        ) {
            (Some(a), Some(b)) if b >= a => b - a,
            _ => Duration::from_secs(u64::from(cfg.pre_secs)),
        };
        let window_start = alert_ts
            - chrono::Duration::from_std(pre_span)
                .unwrap_or_else(|_| chrono::Duration::seconds(i64::from(cfg.pre_secs)));
        let rel = alert_clip_rel_path(camera_id, window_start);
        let alert_clip_id = match self
            .store
            .insert_alert_clip(&NewAlertClip {
                camera_id,
                started_at: window_start,
                path: rel.to_string_lossy().into_owned(),
            })
            .await
        {
            Ok(id) => id,
            Err(e) => {
                warn!(camera_id, "alert clip row insert failed: {e}");
                return None;
            }
        };
        // Subscribe to the live tap NOW so the post window loses no
        // samples between arm and the spawned collector starting.
        let live_rx = ingester.subscribe();
        let timeline = self.alert_box_timelines.read().get(&camera_id).cloned();
        let deadline = Arc::new(AtomicI64::new(deadline_ms));
        self.alert_inflight.lock().insert(
            camera_id,
            AlertInflight {
                alert_clip_id,
                deadline: deadline.clone(),
            },
        );

        tokio::spawn(run_alert_clip_builder(
            self.store.clone(),
            self.clips_dir.clone(),
            self.alert_inflight.clone(),
            cfg,
            camera_id,
            codec,
            alert_clip_id,
            window_start,
            pre,
            live_rx,
            timeline,
            deadline,
            self.alert_cold_kick.clone(),
        ));
        Some(alert_clip_id)
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

/// Re-aggregate a single logical access unit that the upstream
/// ingester's h264parse split across two appsink emissions sharing
/// one PTS (the InSight 192.168.1.66 cameras do this — see the
/// module docstring for the full pathology).
///
/// State: caller holds a mutable `Option<NalSample>` slot. For each
/// incoming `NalSample`:
///
/// * If the slot is empty, store `incoming` there and return `None`.
/// * If the slot's `pts` matches `incoming.pts` (and both are
///   `Some`), concatenate `incoming.data` onto the pending sample's
///   data, OR-combine `is_keyframe`, prefer the earlier `dts`, and
///   return `None`. The merged sample stays in the slot.
/// * Otherwise the pending sample is now complete — swap it out and
///   return it; `incoming` takes its place in the slot.
///
/// Callers are responsible for draining the slot when no more
/// samples are coming (end of snapshot, stop signal, inactivity
/// timer). Returning `None` does **not** mean "drop this sample";
/// it means "still buffering".
fn coalesce_same_pts(
    pending: &mut Option<NalSample>,
    incoming: NalSample,
    codec: CodecKind,
) -> Option<NalSample> {
    match pending.take() {
        None => {
            *pending = Some(incoming);
            None
        }
        Some(prev) => {
            // Two reasons to merge:
            //  (a) Both samples carry the same Some(pts) — classic
            //      InSight split where h264parse emits an AU as two
            //      callbacks sharing one PTS.
            //  (b) `prev` does NOT contain a slice NAL (only AUD/
            //      SEI/SPS/PPS) — it's an incomplete access unit
            //      that MUST be glued onto the next sample
            //      regardless of PTS, because mp4mux cannot mux a
            //      header-only buffer (it lands as a 35–60 byte
            //      "missing picture in access unit" stub that
            //      Chrome's decoder drops along with the *next*
            //      real slice, smearing motion macroblocks across
            //      every P-frame until the next IDR). This catches
            //      firmware revisions where the second half of the
            //      split AU arrives with a synthesized-different
            //      PTS (e.g. ingester-side `last + 33 ms` fallback)
            //      OR with no PTS at all.
            //
            // Two None-PTS samples that BOTH contain a slice are
            // NOT considered a match — the PTS-synthesis path in
            // push_sample handles them individually, and merging
            // by accident would lose frames if a source ever
            // legitimately emits multiple PTS-less complete
            // frames in a row.
            let same_pts = prev.pts.is_some() && prev.pts == incoming.pts;
            let prev_incomplete = !contains_slice_nal(&prev.data, codec);
            if same_pts || prev_incomplete {
                let mut merged = prev;
                merged.data.extend_from_slice(&incoming.data);
                merged.is_keyframe = merged.is_keyframe || incoming.is_keyframe;
                // Prefer the earlier (smaller) dts so monotonic
                // expectations downstream still hold.
                merged.dts = match (merged.dts, incoming.dts) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                // If `prev` was a header-only stub and we just
                // glued the slice on, inherit incoming's PTS
                // when prev had none (otherwise mp4mux still
                // wouldn't have a usable timestamp).
                if merged.pts.is_none() {
                    merged.pts = incoming.pts;
                }
                *pending = Some(merged);
                None
            } else {
                *pending = Some(incoming);
                Some(prev)
            }
        }
    }
}

/// Scan an annex-B byte-stream for at least one VCL slice NAL.
///
/// For **H.264**, that's nal_unit_type 1 (non-IDR slice) or 5 (IDR
/// slice). The NAL unit header is 1 byte; type is bits [4:0] of the
/// byte immediately after the start code.
///
/// For **H.265**, that's any nal_unit_type 0..=31 (trailing /
/// leading / IRAP slice NALs are all in this range; non-VCL units
/// are 32..=63). The NAL unit header is 2 bytes; type is bits [6:1]
/// of the FIRST byte after the start code, i.e. `(byte >> 1) & 0x3F`.
///
/// Returns `true` if any slice NAL is present, `false` if the buffer
/// carries only non-VCL units (AUD, SEI, VPS/SPS/PPS, etc.). A buffer
/// without a slice is by definition NOT a complete access unit and
/// must not be muxed on its own — see [`coalesce_same_pts`] for the
/// rationale.
fn contains_slice_nal(data: &[u8], codec: CodecKind) -> bool {
    let is_h265 = codec.base() == "h265";
    let mut i = 0;
    while i + 3 < data.len() {
        let four = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        let three = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let header_off = if four {
            i + 4
        } else if three {
            i + 3
        } else {
            i += 1;
            continue;
        };
        if header_off < data.len() {
            let is_slice = if is_h265 {
                let nal_type = (data[header_off] >> 1) & 0x3f;
                nal_type <= 31
            } else {
                let nal_type = data[header_off] & 0x1f;
                nal_type == 1 || nal_type == 5
            };
            if is_slice {
                return true;
            }
        }
        i = header_off;
    }
    false
}

/// Push one [`NalSample`] into appsrc, rebasing PTS/DTS so the
/// recording's timeline starts at zero. `appsrc` is configured with
/// `block=true`, so this call may block on filesink/disk pressure;
/// callers MUST run it inside a `spawn_blocking` (the live pump
/// already does, the snapshot prepend in `open()` runs once on the
/// open path which is acceptable).
///
/// **PTS is ALWAYS set on the outgoing buffer**, even when
/// `sample.pts` is `None`. mp4mux/qtmux silently rejects every
/// buffer after the first that has no PTS — the on-disk file
/// stays as a 864-byte ftyp+moov stub, the close drain trips
/// `Could not multiplex stream.`, and no operator-visible error
/// is logged from inside the muxer itself. Some IP cameras (the
/// InSight at `192.168.1.66` on our test bench, for one) drop
/// PTS on individual delta frames; we'd lose every clip from
/// those cameras without this synthesis.
///
/// Fallback chain when `sample.pts` is `None`:
///
///   1. Use `sample.dts` rebased.
///   2. Use `last_written_pts_ns + fallback_interval_ns` (i.e.
///      assume the camera's nominal frame rate).
///   3. Use `0` (only fires for the very first sample of a clip
///      when both PTS and DTS are missing — vanishingly rare
///      because the pre-roll keyframe almost always carries a
///      PTS, and even if it doesn't `0` is a perfectly valid
///      start time for mp4mux).
///
/// Returns the PTS (in rebased nanoseconds) that was actually
/// written to the buffer so the caller can use it as the
/// synthesis anchor for the next sample.
pub(crate) fn push_sample(
    appsrc: &AppSrc,
    sample: &NalSample,
    base_pts: Duration,
    last_written_pts_ns: Option<u64>,
    fallback_interval_ns: u64,
) -> Result<u64, String> {
    let mut buf =
        gst::Buffer::with_size(sample.data.len()).map_err(|e| format!("alloc gst::Buffer: {e}"))?;
    let pts_ns: u64;
    {
        let buf_mut = buf.get_mut().ok_or("buffer not unique")?;
        let mut map = buf_mut
            .map_writable()
            .map_err(|e| format!("map_writable: {e}"))?;
        map.copy_from_slice(&sample.data);
        drop(map);

        // Resolve a PTS we can hand to mp4mux. Synthesis order
        // matches the docstring above.
        let raw_pts_ns: u64 = if let Some(pts) = sample.pts {
            pts.saturating_sub(base_pts).as_nanos() as u64
        } else if let Some(dts) = sample.dts {
            dts.saturating_sub(base_pts).as_nanos() as u64
        } else {
            last_written_pts_ns
                .map(|prev| prev.saturating_add(fallback_interval_ns))
                .unwrap_or(0)
        };
        // mp4mux requires monotonic-non-decreasing PTS. If a
        // jittery source backslides, nudge forward by 1 ns so
        // the muxer still accepts the buffer rather than
        // tripping the same silent-reject pathology we synth'd
        // around in the first place.
        pts_ns = match last_written_pts_ns {
            Some(prev) if raw_pts_ns < prev => prev.saturating_add(1),
            _ => raw_pts_ns,
        };
        buf_mut.set_pts(gst::ClockTime::from_nseconds(pts_ns));

        // DTS: prefer the source's value when present, else mirror
        // the (possibly synthesised) PTS. IP cameras don't emit
        // B-frames over RTSP so DTS == PTS is correct for the
        // fallback case.
        let dts_ns: u64 = if let Some(dts) = sample.dts {
            dts.saturating_sub(base_pts).as_nanos() as u64
        } else {
            pts_ns
        };
        buf_mut.set_dts(gst::ClockTime::from_nseconds(dts_ns));

        if !sample.is_keyframe {
            buf_mut.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
    }
    appsrc
        .push_buffer(buf)
        .map_err(|e| format!("appsrc push_buffer: {e:?}"))?;
    Ok(pts_ns)
}

/// Default inter-frame interval used by [`push_sample`] when a
/// buffer arrives with neither PTS nor DTS. 30 fps (≈33.3 ms) is
/// the safe default — every IP camera we ship for runs at 25 or
/// 30 fps and synthesising a 33 ms gap for a 25 fps stream just
/// stretches the wall-clock timeline by 33 %, which is still
/// playable and only affects samples whose PTS was missing in
/// the first place.
const FALLBACK_FRAME_INTERVAL_NS: u64 = 33_333_333;

/// Forward live broadcast samples into appsrc until the stop signal
/// fires. De-dups against the snapshot tail by skipping any sample
/// whose PTS is `<=` the last PTS we pushed during snapshot prepend.
/// Each push runs inside `spawn_blocking` because appsrc is
/// configured with `block=true` and the underlying push can stall
/// for tens of ms on filesink/disk pressure.
///
/// Same-PTS coalescing: an in-flight `pending` slot holds the most
/// recent NalSample until either (a) a new sample arrives with a
/// different PTS (flush `pending`, replace), (b) a new sample
/// arrives with the same PTS (concatenate into `pending`), or (c)
/// the 200 ms flush timer fires without a new arrival (flush
/// `pending` so a stalled stream doesn't strand the last AU). See
/// the module docstring + [`coalesce_same_pts`] for the pathology.
#[allow(clippy::too_many_arguments)]
async fn run_live_pump(
    camera_id: CameraId,
    clip_id: ClipId,
    codec: CodecKind,
    appsrc: AppSrc,
    mut live_rx: broadcast::Receiver<NalSample>,
    base_pts: Duration,
    mut last_pushed_pts: Option<Duration>,
    mut last_written_pts_ns: Option<u64>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut pending: Option<NalSample> = None;
    // Inactivity flush: if no new sample arrives within 200 ms,
    // drain `pending` so a stalled or low-FPS stream doesn't sit
    // on the last buffered AU forever. 200 ms is well over 6× a
    // 30 fps inter-frame interval — a healthy stream will always
    // displace `pending` on the next recv() before the timer fires.
    const LIVE_PUMP_FLUSH_AFTER: Duration = Duration::from_millis(200);

    // Local helper: push one sample, updating cursor state. Returns
    // `Ok(())` on success or `Err(())` if the push failed (caller
    // should bail out of the pump). Logging happens inside.
    async fn push_one(
        camera_id: CameraId,
        clip_id: ClipId,
        appsrc: &AppSrc,
        sample: NalSample,
        base_pts: Duration,
        last_pushed_pts: &mut Option<Duration>,
        last_written_pts_ns: &mut Option<u64>,
    ) -> Result<(), ()> {
        let push_appsrc = appsrc.clone();
        let pushed_pts = sample.pts;
        let anchor = *last_written_pts_ns;
        let result = tokio::task::spawn_blocking(move || {
            push_sample(
                &push_appsrc,
                &sample,
                base_pts,
                anchor,
                FALLBACK_FRAME_INTERVAL_NS,
            )
        })
        .await;
        match result {
            Ok(Ok(written_pts_ns)) => {
                *last_written_pts_ns = Some(written_pts_ns);
                if let Some(spts) = pushed_pts {
                    *last_pushed_pts = Some(spts);
                }
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(camera_id, clip_id, error = %e, "live pump push failed; ending pump");
                Err(())
            }
            Err(join_err) => {
                warn!(camera_id, clip_id, error = %join_err, "live pump spawn_blocking panicked; ending pump");
                Err(())
            }
        }
    }

    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                debug!(camera_id, clip_id, "live pump received stop signal");
                if let Some(last) = pending.take() {
                    if contains_slice_nal(&last.data, codec) {
                        let _ = push_one(
                            camera_id, clip_id, &appsrc, last, base_pts,
                            &mut last_pushed_pts, &mut last_written_pts_ns,
                        ).await;
                    } else {
                        debug!(
                            camera_id, clip_id,
                            size = last.data.len(),
                            "dropping pending tail at stop: header-only stub (no slice NAL)"
                        );
                    }
                }
                return;
            }
            _ = tokio::time::sleep(LIVE_PUMP_FLUSH_AFTER), if pending.is_some() => {
                // Inactivity flush — the pending AU has been
                // sitting here longer than a reasonable inter-
                // frame interval. If it carries a slice, push it
                // as-is. If it's a header-only stub waiting for
                // its slice that never came, drop it rather than
                // letting mp4mux land a "missing picture" 35-byte
                // stub (visible decoder error, smears motion
                // macroblocks in downstream playback until the
                // next IDR).
                if let Some(last) = pending.take() {
                    if !contains_slice_nal(&last.data, codec) {
                        debug!(
                            camera_id, clip_id,
                            size = last.data.len(),
                            "dropping pending tail at inactivity flush: header-only stub (no slice NAL)"
                        );
                    } else if push_one(
                        camera_id, clip_id, &appsrc, last, base_pts,
                        &mut last_pushed_pts, &mut last_written_pts_ns,
                    ).await.is_err() {
                        return;
                    }
                }
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
                    if let Some(to_push) = coalesce_same_pts(&mut pending, sample, codec) {
                        if push_one(
                            camera_id, clip_id, &appsrc, to_push, base_pts,
                            &mut last_pushed_pts, &mut last_written_pts_ns,
                        ).await.is_err() {
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

/// M-Alert-Clip builder task. Collects the post window off the live tap
/// until the (coalesce-extendable) deadline, splices it after the
/// pre-roll snapshot, and runs the blocking decode -> burn-in ->
/// re-encode on the blocking pool, then stamps the `alert_clips` row
/// ready (or failed). Spawned per burst by
/// [`ClipRecorder::arm_alert_clip`].
#[allow(clippy::too_many_arguments)]
async fn run_alert_clip_builder(
    store: Arc<Store>,
    clips_dir: PathBuf,
    inflight: Arc<PlMutex<HashMap<CameraId, AlertInflight>>>,
    cfg: AlertClipsConfig,
    camera_id: CameraId,
    codec: CodecKind,
    alert_clip_id: AlertClipId,
    window_start: DateTime<Utc>,
    pre: Vec<NalSample>,
    mut live_rx: broadcast::Receiver<NalSample>,
    timeline: Option<Arc<PlMutex<BoxTimeline>>>,
    deadline: Arc<AtomicI64>,
    cold_kick: Option<Arc<Notify>>,
) {
    // Collect the post window, deduping against the pre snapshot's tail
    // PTS (a sample can straddle the snapshot/live boundary).
    let pre_tail_pts = pre.iter().filter_map(|s| s.pts).next_back();
    // Hard cap on total collection so a continuous stream of coalescing
    // alerts can't extend the deadline forever and starve the encode.
    // Measured from now (~first arm) using the configured build timeout.
    let hard_cap_ms =
        Utc::now().timestamp_millis() + i64::from(cfg.build_timeout_secs.max(1)) * 1000;
    let mut post: Vec<NalSample> = Vec::new();
    loop {
        let now = Utc::now().timestamp_millis();
        let dl = deadline.load(Ordering::Acquire).min(hard_cap_ms);
        if now >= dl {
            break;
        }
        let remaining = Duration::from_millis((dl - now).max(0) as u64);
        match tokio::time::timeout(remaining, live_rx.recv()).await {
            Ok(Ok(sample)) => {
                if let (Some(sp), Some(tp)) = (sample.pts, pre_tail_pts) {
                    if sp <= tp {
                        continue;
                    }
                }
                post.push(sample);
            }
            // Lagged: the broadcast dropped some samples; the clip has a
            // small gap but is still useful. Keep collecting.
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            // Timeout: re-check the deadline (a coalescing alert may have
            // extended it) and loop.
            Err(_) => {}
        }
    }

    // Splice pre + post into one keyframe-first window.
    let mut window = pre;
    window.extend(post);

    // Snapshot the box timeline for the encode (frame-aligned raw boxes).
    let box_frames = timeline
        .as_ref()
        .map(|t| t.lock().snapshot())
        .unwrap_or_default();

    let rel = alert_clip_rel_path(camera_id, window_start);
    let final_path = clips_dir.join(&rel);
    let partial_path = alert_clip_inflight_path(&clips_dir, &rel);
    if let Some(parent) = partial_path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }

    let enc_partial = partial_path.clone();
    let max_w = cfg.max_encode_width;
    let result = tokio::task::spawn_blocking(move || {
        encode_alert_clip(
            &window,
            codec,
            &box_frames,
            window_start,
            &enc_partial,
            max_w,
        )
    })
    .await;

    match result {
        Ok(Ok(stats)) => {
            // Existence of the final path == "ready"; rename atomically.
            if let Err(e) = fs::rename(&partial_path, &final_path).await {
                warn!(camera_id, alert_clip_id, "alert clip rename failed: {e}");
                let _ = store.mark_alert_clip_failed(alert_clip_id).await;
            } else {
                // Hash the finalized MP4 so the cold replicator can PUT
                // it and stamp the cloud `clip_replicated` envelope's
                // `sha256_hex`. Alert clips are seconds long, so a
                // post-hoc read+hash is cheap. A hash failure is
                // non-fatal: the clip still serves local sinks; the
                // cold drain simply skips NULL-sha256 rows.
                let sha256 = match hash_file_sha256(&final_path).await {
                    Ok(h) => Some(h),
                    Err(e) => {
                        warn!(
                            camera_id,
                            alert_clip_id,
                            "alert clip sha256 failed; cloud replication will skip: {e}"
                        );
                        None
                    }
                };
                if let Err(e) = store
                    .mark_alert_clip_ready(
                        alert_clip_id,
                        stats.duration_ms,
                        stats.size_bytes,
                        sha256.as_deref(),
                    )
                    .await
                {
                    warn!(
                        camera_id,
                        alert_clip_id, "mark_alert_clip_ready failed: {e}"
                    );
                } else {
                    info!(
                        camera_id,
                        alert_clip_id,
                        duration_ms = stats.duration_ms,
                        size_bytes = stats.size_bytes,
                        "alert clip ready"
                    );
                    // Wake the cold replicator so it ships this alert
                    // clip to the cloud now (M-Alert-Clip cloud
                    // delivery) rather than on its 5-min backstop. Only
                    // meaningful when hashed (an un-hashed clip is
                    // skipped by the drain), so gate on `sha256`.
                    if sha256.is_some() {
                        if let Some(kick) = &cold_kick {
                            kick.notify_one();
                        }
                    }
                }
            }
        }
        Ok(Err(e)) => {
            warn!(camera_id, alert_clip_id, "alert clip encode failed: {e}");
            let _ = fs::remove_file(&partial_path).await;
            let _ = store.mark_alert_clip_failed(alert_clip_id).await;
        }
        Err(e) => {
            warn!(
                camera_id,
                alert_clip_id, "alert clip encode task join failed: {e}"
            );
            let _ = store.mark_alert_clip_failed(alert_clip_id).await;
        }
    }

    // Remove our in-flight entry — but only if it's still ours (a later
    // burst that reused the camera slot must not be evicted by us).
    let mut map = inflight.lock();
    if map.get(&camera_id).map(|e| e.alert_clip_id) == Some(alert_clip_id) {
        map.remove(&camera_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{CameraConfig, StoreConfig};
    use std::path::PathBuf;
    use url::Url;

    fn sample(pts_ms: Option<u64>, data: &[u8], is_kf: bool) -> NalSample {
        NalSample {
            pts: pts_ms.map(Duration::from_millis),
            dts: pts_ms.map(Duration::from_millis),
            is_keyframe: is_kf,
            data: data.to_vec(),
        }
    }

    /// Header-only AU: `[AUD, SEI]` byte-stream with no slice NAL.
    /// Mirrors the InSight 192.168.1.66 firmware that emits this as
    /// its own appsink callback before the slice arrives.
    fn header_only_au() -> Vec<u8> {
        // start code, NAL type 9 (AUD), AUD payload byte,
        // start code, NAL type 6 (SEI), SEI payload+RBSP trailing.
        vec![
            0, 0, 0, 1, 9, 0xF0, // AUD
            0, 0, 0, 1, 6, 5, 0x10, 0x20, 0x80, // SEI
        ]
    }

    /// Slice-bearing AU: a non-IDR slice NAL (type 1) with a few
    /// bytes of payload. Sufficient to trigger `contains_slice_nal`.
    fn slice_au(payload_size: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x41]; // start code + NAL type 1 (non-IDR slice)
        v.extend(std::iter::repeat_n(0xAB, payload_size));
        v
    }

    /// IDR slice (NAL type 5).
    fn idr_au(payload_size: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x65]; // start code + NAL type 5 (IDR slice)
        v.extend(std::iter::repeat_n(0xCD, payload_size));
        v
    }

    #[test]
    fn contains_slice_nal_detects_type_1_and_5() {
        assert!(!contains_slice_nal(&header_only_au(), CodecKind::H264));
        assert!(contains_slice_nal(&slice_au(10), CodecKind::H264));
        assert!(contains_slice_nal(&idr_au(10), CodecKind::H264));
        // Mixed: header + slice → has slice.
        let mut mixed = header_only_au();
        mixed.extend_from_slice(&slice_au(10));
        assert!(contains_slice_nal(&mixed, CodecKind::H264));
        // Empty / too short → no slice.
        assert!(!contains_slice_nal(&[], CodecKind::H264));
        assert!(!contains_slice_nal(&[0, 0, 0, 1], CodecKind::H264));
    }

    /// H.265 NAL header is 2 bytes, nal_unit_type = (byte0 >> 1) & 0x3F.
    /// VCL slice types are 0..=31; non-VCL (VPS=32, SPS=33, PPS=34,
    /// AUD=35, SEI_PREFIX=39, SEI_SUFFIX=40, etc.) live in 32..=63.
    #[test]
    fn contains_slice_nal_h265_detects_vcl_range() {
        // VCL slice type 1 (TRAIL_N) -> byte0 = (1 << 1) = 0x02.
        let vcl_trail = vec![0u8, 0, 0, 1, 0x02, 0x01, 0xAA, 0xBB];
        assert!(contains_slice_nal(&vcl_trail, CodecKind::H265));
        // VCL slice type 19 (IDR_W_RADL) -> byte0 = (19 << 1) = 0x26.
        let vcl_idr = vec![0u8, 0, 0, 1, 0x26, 0x01, 0xCC, 0xDD];
        assert!(contains_slice_nal(&vcl_idr, CodecKind::H265));
        // VPS (type 32) -> byte0 = (32 << 1) = 0x40. NOT a slice.
        let vps = vec![0u8, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
        assert!(!contains_slice_nal(&vps, CodecKind::H265));
        // SEI prefix (type 39) -> byte0 = (39 << 1) = 0x4E. NOT a slice.
        let sei = vec![0u8, 0, 0, 1, 0x4E, 0x01, 0x05, 0x00];
        assert!(!contains_slice_nal(&sei, CodecKind::H265));
        // Mixed VPS + slice → slice wins.
        let mut mixed = vps.clone();
        mixed.extend_from_slice(&vcl_idr);
        assert!(contains_slice_nal(&mixed, CodecKind::H265));
        // H.265 stream parsed as H.264 with byte 0x02 -> H.264
        // nal_type = 0x02 & 0x1f = 2 (DPA slice partition), NOT
        // 1 or 5 → false. Confirms the dispatch matters.
        assert!(!contains_slice_nal(&vcl_trail, CodecKind::H264));
    }

    #[test]
    fn coalesce_merges_same_pts_pair() {
        // Two samples sharing pts=66ms — the exact InSight 192.168.1.66
        // pathology: an [AUD, SEI] tiny buffer followed by [slice ...].
        let mut pending: Option<NalSample> = None;
        // First arrival: just buffered, nothing to flush yet.
        let header = sample(Some(66), &header_only_au(), false);
        assert!(coalesce_same_pts(&mut pending, header, CodecKind::H264).is_none());
        assert!(pending.is_some());
        // Second arrival with the same pts: merges into pending, still
        // nothing to flush.
        let slice = sample(Some(66), &slice_au(20), true);
        assert!(coalesce_same_pts(&mut pending, slice, CodecKind::H264).is_none());
        let merged = pending.as_ref().unwrap();
        assert_eq!(merged.pts, Some(Duration::from_millis(66)));
        assert!(contains_slice_nal(&merged.data, CodecKind::H264));
        assert!(merged.is_keyframe, "keyframe flag must OR-combine");
    }

    #[test]
    fn coalesce_different_pts_flushes_previous_when_both_have_slices() {
        let mut pending: Option<NalSample> = None;
        let first = sample(Some(33), &slice_au(10), false);
        assert!(coalesce_same_pts(&mut pending, first, CodecKind::H264).is_none());
        // Different PTS — previous gets flushed, new one buffered.
        // Both have slices so the header-only-merge path doesn't fire.
        let second = sample(Some(66), &slice_au(10), false);
        let flushed = coalesce_same_pts(&mut pending, second, CodecKind::H264)
            .expect("must flush previous slice on PTS change");
        assert_eq!(flushed.pts, Some(Duration::from_millis(33)));
        let still_pending = pending.as_ref().unwrap();
        assert_eq!(still_pending.pts, Some(Duration::from_millis(66)));
    }

    #[test]
    fn coalesce_none_pts_never_merges_when_both_have_slices() {
        // Two PTS-less complete frames in a row — must NOT merge
        // (would lose frames if a source ever legitimately emits two
        // PTS-less complete frames back to back). The synthesis path
        // in push_sample handles them individually.
        let mut pending: Option<NalSample> = None;
        let a = sample(None, &slice_au(10), false);
        assert!(coalesce_same_pts(&mut pending, a, CodecKind::H264).is_none());
        let b = sample(None, &slice_au(15), false);
        let flushed = coalesce_same_pts(&mut pending, b, CodecKind::H264)
            .expect("two None-PTS slice samples must NOT merge");
        assert_eq!(flushed.data.len(), 15); // first one was 10+5 header
        assert!(pending.is_some());
    }

    #[test]
    fn coalesce_header_only_then_slice_different_pts_still_merges() {
        // The regression case: ingester synthesizes
        // last_pts + 33ms on the slice half because gst h264parse
        // didn't restamp it, so the two halves arrive with DIFFERENT
        // Some(pts) — but `prev` is a header-only stub so the new
        // rule must still merge them.
        let mut pending: Option<NalSample> = None;
        let header = sample(Some(66), &header_only_au(), false);
        assert!(coalesce_same_pts(&mut pending, header, CodecKind::H264).is_none());
        let slice = sample(Some(99), &slice_au(20), false);
        assert!(
            coalesce_same_pts(&mut pending, slice, CodecKind::H264).is_none(),
            "header-only stub MUST be merged with following slice regardless of PTS"
        );
        let merged = pending.as_ref().unwrap();
        assert!(contains_slice_nal(&merged.data, CodecKind::H264));
        // PTS should still be the original Some(66) (prev's), not
        // overwritten by incoming's — earlier PTS is the source's
        // truth for this AU.
        assert_eq!(merged.pts, Some(Duration::from_millis(66)));
    }

    #[test]
    fn coalesce_header_only_then_slice_pts_none_inherits_pts() {
        // Variation: prev is header-only with Some(pts), slice
        // arrives PTS-less. Merged buffer should keep Some(pts).
        let mut pending: Option<NalSample> = None;
        let header = sample(Some(66), &header_only_au(), false);
        coalesce_same_pts(&mut pending, header, CodecKind::H264);
        let slice = sample(None, &slice_au(20), false);
        assert!(coalesce_same_pts(&mut pending, slice, CodecKind::H264).is_none());
        let merged = pending.as_ref().unwrap();
        assert_eq!(merged.pts, Some(Duration::from_millis(66)));
        assert!(contains_slice_nal(&merged.data, CodecKind::H264));
    }

    #[test]
    fn coalesce_pts_none_header_then_slice_inherits_incoming_pts() {
        // Variation: prev is header-only WITHOUT pts (rare but
        // possible), slice arrives with Some(pts). The merged buffer
        // must adopt incoming's pts so mp4mux can mux it.
        let mut pending: Option<NalSample> = None;
        let header = sample(None, &header_only_au(), false);
        coalesce_same_pts(&mut pending, header, CodecKind::H264);
        let slice = sample(Some(66), &slice_au(20), false);
        assert!(coalesce_same_pts(&mut pending, slice, CodecKind::H264).is_none());
        let merged = pending.as_ref().unwrap();
        assert_eq!(
            merged.pts,
            Some(Duration::from_millis(66)),
            "merged AU must inherit slice's PTS when header had none"
        );
    }

    #[test]
    fn coalesce_dts_keeps_earlier() {
        // When merging same-pts samples, dts should keep the earlier
        // (smaller) value so downstream monotonic expectations still
        // hold.
        let mut pending: Option<NalSample> = None;
        let mut a = sample(Some(66), &header_only_au(), false);
        a.dts = Some(Duration::from_millis(60));
        let mut b = sample(Some(66), &slice_au(10), false);
        b.dts = Some(Duration::from_millis(70));
        coalesce_same_pts(&mut pending, a, CodecKind::H264);
        coalesce_same_pts(&mut pending, b, CodecKind::H264);
        assert_eq!(
            pending.as_ref().unwrap().dts,
            Some(Duration::from_millis(60))
        );
    }

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
                    codec: None,
                },
                detector: nexus_config::CameraDetector::default(),
                behavior: nexus_config::CameraBehavior::default(),
                onvif: Default::default(),
                talk_down: Default::default(),
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
                frame_width: 960,
                frame_height: 540,
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
                frame_width: 960,
                frame_height: 540,
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
        let ingester = PreRollIngester::new(1, url, 5, CodecKind::H264).expect("build ingester");
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
                frame_width: 960,
                frame_height: 540,
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
        let desc = GstClipRecorder::pipeline_desc(
            Path::new("/var/lib/nexus/clips/1/2026-05-13/1234567890.partial.mp4"),
            CodecKind::H264,
        );
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
