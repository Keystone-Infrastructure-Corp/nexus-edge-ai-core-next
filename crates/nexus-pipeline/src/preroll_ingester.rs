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
//!   rtspsrc location=URL latency=500 protocols=tcp
//!     ! rtph264depay
//!     ! h264parse config-interval=0
//!     ! video/x-h264,stream-format=byte-stream,alignment=au
//!     ! appsink name=tap emit-signals=true sync=false
//!         max-buffers=200 drop=false
//! ```
//!
//! `stream-format=byte-stream,alignment=au` (Annex-B, access-unit-aligned)
//! is what mp4mux's `appsrc` feed expects when we splice the snapshot
//! at clip-open.
//!
//! `config-interval=0` (do NOT insert SPS/PPS) is deliberate. We
//! used to set `-1` (insert SPS/PPS before every IDR), but that
//! interacts badly with cameras whose H.264 stream already carries
//! SPS/PPS in every keyframe access unit (most modern IP cameras —
//! confirmed on the InSight 192.168.1.66 fixture). With `-1`,
//! h264parse on the ingester emits `[AUD, SPS, PPS, SPS, PPS, IDR]`.
//! Downstream, the recorder's `h264parse → mp4mux` chain interprets
//! the second SPS/PPS pair as the start of a *new* access unit;
//! that synthetic AU inherits no PTS from the source buffer, and
//! qtmux silently rejects every PTS-less buffer with the cryptic
//! `"Could not multiplex stream."` on EOS — leaving a 864-byte
//! ftyp+moov stub on disk. With `config-interval=0` we pass the
//! camera's byte-stream through unchanged: cameras that already
//! include SPS/PPS per keyframe work end-to-end, and clips for
//! cameras that DON'T (some Axis/Hikvision models in legacy modes)
//! only become un-decodable when the snapshot starts mid-GOP — a
//! known limitation we can revisit by caching the most-recent
//! SPS/PPS NALs and prepending them to AUs that lack them.
//! See also `gst_clip_recorder::push_sample` for the per-buffer
//! PTS synthesis that complements this fix.
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

use chrono::Utc;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_video::prelude::*;
use gstreamer_video::{VideoFormat, VideoFrameRef, VideoInfo};
use nexus_types::{CameraId, CodecKind, Frame, PixelFormat};
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::preroll::{NalRingBuffer, NalSample};
use crate::source::gst_init;

/// How many in-flight live samples the broadcast channel buffers
/// per subscriber. Tokio's broadcast drops the OLDEST sample when
/// full (no backpressure on the sender), so any slow consumer past
/// this capacity sees `RecvError::Lagged(n)` and the matching frames
/// never reach the recorder — clip plays back choppy with chunks
/// missing. 512 buffers ≈ 17s at 30fps; an average H.264 frame at
/// 720p is ~10–50 KB, so worst-case ~25 MB per camera. Cheaper than
/// losing frames in the recording.
const BROADCAST_CAPACITY: usize = 512;

/// How many in-flight RGB frames the per-camera frame broadcast
/// holds per subscriber. The supervisor downstream of the
/// broadcast::Receiver has its own mpsc::Sender(8) and a motion
/// gate that drops the bulk of frames, so this only has to absorb
/// jitter on the tokio task wakeup — not the entire detector
/// backlog. 16 buffers ≈ 1s at 15fps. Smaller is better: each RGB
/// frame at 960×540 is 1.5 MB, so 16 ≈ 24 MB peak per camera. A
/// slow detector sees `RecvError::Lagged(n)` and the shared source
/// emits a one-line warn but continues — the gate/pool drop policy
/// is upstream of us so missed frames are routine.
const FRAME_BROADCAST_CAPACITY: usize = 16;

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

/// Optional shared RGB frame tap. When `Some`, the ingester's
/// GStreamer pipeline grows a `tee` after the H.264 parser plus a
/// second branch that decodes to RGB at the camera's `max_fps` and
/// publishes every decoded frame on a tokio broadcast channel. The
/// supervisor's [`crate::source::SharedRtspSource`] subscribes to
/// this channel and forwards into the per-camera frame mpsc —
/// collapsing the two RTSP sessions (one for the detector RGB feed,
/// one for the recorder NAL feed) that the old `RtspSource` +
/// `PreRollIngester` pair would otherwise open. The collapse is
/// REQUIRED for cameras whose firmware caps concurrent RTSP
/// sessions at one per stream path (confirmed on the InSight
/// 192.168.1.66 fixture); on cameras that tolerate two sessions it
/// just halves the upstream bandwidth + per-camera CPU.
struct FrameTap {
    tx: broadcast::Sender<Frame>,
    max_fps: u32,
    /// RGB width the second branch's `videoscale` produces.
    /// Derived at construction time from the camera's resolved
    /// detector input size and threaded into the `format!` for
    /// the pipeline string. Was hardcoded to
    /// [`RTSP_SOURCE_FRAME_WIDTH`] (960) before the per-camera
    /// supervisor-frame work.
    width: u32,
    /// Companion to [`Self::width`].
    height: u32,
}

pub struct PreRollIngester {
    camera_id: CameraId,
    url: String,
    /// Wire codec carried by the upstream RTSP feed. Selects
    /// `rtph264depay`/`h264parse`/`avdec_h264` vs the H.265
    /// equivalents in the generated pipeline string. The vendor
    /// `_plus` variants (Hikvision H.264+/H.265+, Dahua Smart
    /// Codec) collapse to their base via [`CodecKind::base`] —
    /// GStreamer's stock parsers handle the SVC bitstream as
    /// plain H.264/H.265.
    codec: CodecKind,
    /// `pre_roll_secs == 0` is a valid disable knob — we still run
    /// the always-on pipeline (so the broadcast channel is alive
    /// for recording) but the ring buffer never accumulates.
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    /// Set iff the pipeline was built with the RGB tap. See
    /// [`FrameTap`] for the why.
    frame_tap: Option<FrameTap>,
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
    ///
    /// Builds the H.264-NAL-only pipeline (no RGB tap). The
    /// detector must still open its own `RtspSource` against the
    /// same URL — fine on cameras that allow multiple concurrent
    /// RTSP sessions per path, broken on single-session cameras
    /// (see [`Self::new_with_rgb`] for the collapse).
    pub fn new(
        camera_id: CameraId,
        url: impl Into<String>,
        pre_roll_secs: u32,
        codec: CodecKind,
    ) -> Result<Arc<Self>, IngesterError> {
        Self::build(camera_id, url, pre_roll_secs, codec, None)
    }

    /// Variant of [`Self::new`] that also exposes a decoded RGB
    /// frame stream off the same RTSP session, sized to
    /// `rgb_w × rgb_h` at `max_fps` (0 ⇒ 15). Pipeline grows a
    /// `tee` after `h264parse` plus a second branch
    /// `queue → avdec_h264 → videoconvert → videoscale → videorate
    /// → appsink RGB`; the detector consumes frames via
    /// [`Self::subscribe_frames`] without opening a second
    /// connection to the camera. Use this constructor whenever the
    /// recorder is `gstreamer` so single-session cameras (e.g.
    /// InSight firmware caps at 1 session per stream path) work
    /// end-to-end.
    ///
    /// `rgb_w / rgb_h` are derived at the engine spawn site from
    /// the camera's resolved `ModelConfig.input_width` via
    /// [`crate::source::supervisor_frame_for`]; pass
    /// `(RTSP_SOURCE_FRAME_WIDTH, RTSP_SOURCE_FRAME_HEIGHT)` to
    /// reproduce the pre-per-camera (960×540) behaviour.
    pub fn new_with_rgb(
        camera_id: CameraId,
        url: impl Into<String>,
        pre_roll_secs: u32,
        codec: CodecKind,
        max_fps: u32,
        rgb_w: u32,
        rgb_h: u32,
    ) -> Result<Arc<Self>, IngesterError> {
        Self::build(
            camera_id,
            url,
            pre_roll_secs,
            codec,
            Some((max_fps, rgb_w, rgb_h)),
        )
    }

    fn build(
        camera_id: CameraId,
        url: impl Into<String>,
        pre_roll_secs: u32,
        codec: CodecKind,
        rgb_params: Option<(u32, u32, u32)>,
    ) -> Result<Arc<Self>, IngesterError> {
        gst_init::ensure().map_err(|e| IngesterError::GstInit(e.to_string()))?;
        let url = url.into();
        let ring = Arc::new(Mutex::new(NalRingBuffer::new(Duration::from_secs(
            pre_roll_secs as u64,
        ))));
        let (live_tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let frame_tap = rgb_params.map(|(fps, w, h)| {
            let (tx, _rx) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
            FrameTap {
                tx,
                max_fps: if fps == 0 { 15 } else { fps },
                width: w,
                height: h,
            }
        });
        let active_pipeline = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let task_url = url.clone();
        let task_ring = ring.clone();
        let task_tx = live_tx.clone();
        let task_frame_tap = frame_tap
            .as_ref()
            .map(|t| (t.tx.clone(), t.max_fps, t.width, t.height));
        let task_pipeline = active_pipeline.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            run_supervisor(
                camera_id,
                task_url,
                codec,
                task_ring,
                task_tx,
                task_frame_tap,
                task_pipeline,
                task_shutdown,
            )
            .await;
        });

        Ok(Arc::new(Self {
            camera_id,
            url,
            codec,
            ring,
            live_tx,
            frame_tap,
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

    /// Wire codec the ingester's GStreamer pipeline is parsing.
    /// Used by the recorder at `open()` to capture into
    /// `OpenState.codec` so the per-clip mp4mux chain spins up
    /// the matching parser without an extra config lookup.
    pub fn codec(&self) -> CodecKind {
        self.codec
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

    /// Subscribe to the decoded RGB frame stream off this
    /// ingester's RTSP session. Returns `None` if the ingester was
    /// built via [`Self::new`] (no RGB tap). Returns `Some` if
    /// built via [`Self::new_with_rgb`]. The first frame a fresh
    /// subscriber sees is the next one decoded — no replay of past
    /// frames. Drop the receiver to detach; the GStreamer pipeline
    /// keeps decoding regardless (the cost is paid even with zero
    /// subscribers, but every realistic deployment has exactly one:
    /// the per-camera supervisor task).
    pub fn subscribe_frames(&self) -> Option<broadcast::Receiver<Frame>> {
        self.frame_tap.as_ref().map(|t| t.tx.subscribe())
    }

    /// True iff this ingester was built with the shared RGB tap
    /// enabled. Used by the recorder's `shared_frame_source` to
    /// decide whether to hand the supervisor a
    /// [`crate::source::SharedRtspSource`] or have it open its own
    /// `RtspSource`.
    pub fn has_rgb_tap(&self) -> bool {
        self.frame_tap.is_some()
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
    codec: CodecKind,
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    frame_tap: Option<(broadcast::Sender<Frame>, u32, u32, u32)>,
    active_pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    shutdown: Arc<AtomicBool>,
) {
    info!(
        camera_id,
        url,
        codec = %codec,
        rgb_tap = frame_tap.is_some(),
        "preroll ingester supervisor starting (always-on)"
    );
    let mut backoff = Duration::from_millis(500);
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match run_session(
            camera_id,
            &url,
            codec,
            ring.clone(),
            live_tx.clone(),
            frame_tap.clone(),
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
    codec: CodecKind,
    ring: Arc<Mutex<NalRingBuffer>>,
    live_tx: broadcast::Sender<NalSample>,
    frame_tap: Option<(broadcast::Sender<Frame>, u32, u32, u32)>,
    active_pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), IngesterError> {
    let url_safe = url.replace('"', "");
    // Codec-specific element names: rtp{depay}, {parse}, video/x-{base},
    // and the {decoder} used by the optional RGB tap branch. The base
    // collapse (`_plus` -> base) is intentional \u2014 Hikvision H.264+ /
    // Dahua Smart Codec are SVC-tagged but the bitstream still parses
    // through the stock H.264/H.265 elements.
    let (rtp_depay, parse, base_caps, decoder) = match codec.base() {
        "h265" => ("rtph265depay", "h265parse", "video/x-h265", "avdec_h265"),
        _ => ("rtph264depay", "h264parse", "video/x-h264", "avdec_h264"),
    };
    // protocols=tcp (NOT tcp+udp) so rtspsrc never falls back to UDP.
    // UDP packet loss on a contended link (WiFi / busy switch / bursty
    // CPU on the receiver) shows up as 2\u20134 s gaps in the recorded clip
    // where the camera OSD clock visibly jumps. TCP gives guaranteed
    // in-order delivery; the camera buffers send-side rather than
    // silently dropping. Latency bumped to 500 ms to absorb the
    // resulting in-band re-tx jitter.
    // {parse} config-interval=0 (trust the source). See module
    // docstring for the multi-paragraph explanation of why -1
    // catastrophically breaks recording on cameras that already
    // include SPS/PPS (or H.265 VPS/SPS/PPS) in every keyframe access
    // unit.
    //
    // When `frame_tap` is `Some`, the pipeline grows a `tee` after
    // the parser caps filter, with two queued branches:
    //   * `tap`  \u2014 the existing byte-stream appsink that feeds the
    //              ring buffer + recorder broadcast.
    //   * `rgb`  \u2014 `{decoder} \u2192 videoconvert \u2192 videoscale \u2192
    //              videorate \u2192 appsink RGB` at the camera's
    //              per-camera supervisor-frame resolution. The
    //              detector subscribes via
    //              [`PreRollIngester::subscribe_frames`] and never
    //              opens its own RTSP connection. Queues sit at
    //              both branch heads (mandatory for `tee`); `tap` queue
    //              is lossless and `rgb` queue is `leaky=downstream`
    //              so a slow detector drops the oldest decoded frame
    //              instead of stalling the shared upstream parser.
    let desc = match &frame_tap {
        None => format!(
            "rtspsrc location=\"{url_safe}\" latency=500 protocols=tcp \
             ! {rtp_depay} \
             ! {parse} config-interval=0 \
             ! {base_caps},stream-format=byte-stream,alignment=au \
             ! appsink name=tap emit-signals=true sync=false \
                 max-buffers=200 drop=false"
        ),
        Some((_, max_fps, rgb_w, rgb_h)) => format!(
            "rtspsrc location=\"{url_safe}\" latency=500 protocols=tcp \
             ! {rtp_depay} \
             ! {parse} config-interval=0 \
             ! {base_caps},stream-format=byte-stream,alignment=au \
             ! tee name=t \
             t. ! queue max-size-buffers=200 max-size-bytes=0 max-size-time=0 \
                ! appsink name=tap emit-signals=true sync=false \
                    max-buffers=200 drop=false \
             t. ! queue leaky=downstream max-size-buffers=8 max-size-bytes=0 max-size-time=0 \
                ! {decoder} \
                ! videoconvert ! videoscale ! videorate \
                ! video/x-raw,format=RGB,width={w},height={h},framerate={fr}/1 \
                ! appsink name=rgb emit-signals=true sync=false drop=true max-buffers=4",
            w = rgb_w,
            h = rgb_h,
            fr = max_fps,
        ),
    };
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

    // RGB tap (shared with the detector) — only wired when
    // [`PreRollIngester::new_with_rgb`] was used. Reads decoded
    // RGB samples off the second `tee` branch and publishes them
    // on the per-camera `Frame` broadcast. Code mirrors the
    // RtspSource appsink callback: VideoFrameRef for stride-safe
    // reads, hard error on non-RGB caps, row-by-row tight pack
    // because every downstream consumer (image::JpegEncoder,
    // ndarray) wants width*height*3 with no padding.
    if let Some((frame_tx, _max_fps, _rgb_w, _rgb_h)) = &frame_tap {
        let rgb_sink = pipeline
            .by_name("rgb")
            .ok_or_else(|| IngesterError::AppSink("appsink 'rgb' not found".into()))?
            .downcast::<AppSink>()
            .map_err(|_| IngesterError::AppSink("downcast rgb AppSink".into()))?;
        let frame_tx_cb = frame_tx.clone();
        let counter = Arc::new(parking_lot::Mutex::new(0u64));
        let counter_cb = counter.clone();
        let logged_first = Arc::new(AtomicBool::new(false));
        let logged_first_cb = logged_first.clone();
        rgb_sink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let info = VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                    let frame_ref = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                        .map_err(|_| gst::FlowError::Error)?;

                    if !logged_first_cb.swap(true, Ordering::Relaxed) {
                        info!(
                            camera_id,
                            caps = %caps,
                            format = ?info.format(),
                            width = info.width(),
                            height = info.height(),
                            stride0 = frame_ref.plane_stride().first().copied().unwrap_or(0),
                            buffer_size = buffer.size(),
                            "preroll ingester rgb tap: first sample"
                        );
                    }

                    if info.format() != VideoFormat::Rgb {
                        error!(
                            camera_id,
                            format = ?info.format(),
                            "rgb appsink received non-RGB sample; \
                             check capsfilter and videoconvert in the pipeline"
                        );
                        return Err(gst::FlowError::NotNegotiated);
                    }

                    let plane = frame_ref.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                    let stride = frame_ref.plane_stride().first().copied().unwrap_or(0) as usize;
                    let width = info.width() as usize;
                    let height = info.height() as usize;
                    let row_bytes = width * 3;

                    if stride < row_bytes || plane.len() < stride * height {
                        error!(
                            camera_id,
                            stride,
                            row_bytes,
                            plane_len = plane.len(),
                            height,
                            "rgb appsink buffer geometry inconsistent with caps"
                        );
                        return Err(gst::FlowError::Error);
                    }

                    let mut data = Vec::with_capacity(row_bytes * height);
                    if stride == row_bytes {
                        data.extend_from_slice(&plane[..row_bytes * height]);
                    } else {
                        for y in 0..height {
                            let start = y * stride;
                            data.extend_from_slice(&plane[start..start + row_bytes]);
                        }
                    }

                    let frame_id = {
                        let mut g = counter_cb.lock();
                        *g = g.saturating_add(1);
                        *g
                    };
                    let frame = Frame {
                        camera_id,
                        frame_id,
                        captured_at: Utc::now(),
                        width: info.width(),
                        height: info.height(),
                        format: PixelFormat::Rgb24,
                        data: Arc::new(data),
                        trace_id: Uuid::now_v7().to_string(),
                    };
                    // `broadcast::send` returns Err iff no
                    // subscribers — fine, we're a shared bus and
                    // the supervisor may not have called
                    // `subscribe_frames` yet at very first session
                    // start. Slow subscribers see `Lagged(n)` on
                    // their next recv (handled in
                    // `SharedRtspSource`), NOT a send error here.
                    let _ = frame_tx_cb.send(frame);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

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
