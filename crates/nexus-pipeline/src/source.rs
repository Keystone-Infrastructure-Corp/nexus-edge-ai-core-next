//! Frame sources — RTSP and a virtual generator for tests / dev boots.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "gstreamer")]
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
#[cfg(feature = "gstreamer")]
use nexus_types::CodecKind;
use nexus_types::{CameraId, Frame, PixelFormat};
use thiserror::Error;
/// True while `session_gen` is the generation the source is currently running.
///
/// Teardown is detached, so a superseded pipeline can still be PLAYING while
/// holding a clone of the live `tx`. Publishing from it interleaves two
/// independent `frame_id` sequences into one channel and stamps fresh
/// `captured_at` onto older pictures. See BUG-070.
#[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
fn is_current_session(generation: &AtomicU64, session_gen: u64) -> bool {
    generation.load(Ordering::Acquire) == session_gen
}
use tokio::sync::mpsc;
use uuid::Uuid;

/// Legacy default width of the RGB frame the `RtspSource`
/// produces after `videoscale` when no per-camera detector size is
/// available. As of the per-camera supervisor-frame work, the
/// active dims are passed in on `RtspSource::frame_width /
/// frame_height` (and on `PreRollIngester::new_with_rgb`'s
/// `rgb_w / rgb_h`), derived from the camera's resolved
/// `ModelConfig.input_width`. Every downstream consumer (detector,
/// tracker, motion_events bbox, frame cache, JPEG endpoint) sees
/// pixels in whatever coordinate space the camera is currently
/// running in. The bbox values written into `motion_events` are
/// in that same space — the clip's `frame_width / frame_height`
/// columns record which space, so the overlay UI scales bboxes
/// against `(<video>.videoWidth, .videoHeight)` (the CAMERA's
/// native H.264 resolution, NOT this).
pub const RTSP_SOURCE_FRAME_WIDTH: u32 = 960;
/// See [`RTSP_SOURCE_FRAME_WIDTH`].
pub const RTSP_SOURCE_FRAME_HEIGHT: u32 = 540;

/// Compute the supervisor (analysis) RGB frame size for a camera
/// analysing at `width` pixels wide.
///
/// `width` is the camera's supervisor width — the detector input width
/// by default, or a larger native-16:9 ladder rung when decoupled per
/// camera via `CameraBehavior::supervisor_width` (so a tile grid divides
/// the frame evenly into model-sized tiles). Returns a 16:9 frame whose
/// width equals `width` and whose height is `width * 9 / 16` rounded up
/// to an even integer (videoscale's caps negotiation prefers even dims;
/// YUV chroma planes can't represent odd dimensions cleanly). On the
/// exact-16:9 ∩ stride-32 ladder there is no rounding.
///
/// Examples:
///   * `supervisor_frame_for(512)`  -> `(512,  288)`
///   * `supervisor_frame_for(1024)` -> `(1024, 576)`
///   * `supervisor_frame_for(1536)` -> `(1536, 864)`
///
/// Rationale: cameras universally publish 16:9 streams (1080p, 720p, 4K
/// all 16:9), so producing a 16:9 supervisor frame is a plain rescale.
/// The detector input is native 16:9 too (M_NATIVE_ASPECT), so this frame
/// is fed to the model WITHOUT letterboxing OR stretching — no aspect
/// distortion, no invented rows. (Note the distinct, genuinely
/// letterboxed step for a non-16:9 *camera*: `videoscale add-borders=true`
/// pads it into this 16:9 supervisor frame upstream.)
pub const fn supervisor_frame_for(width: u32) -> (u32, u32) {
    // (w * 9 / 16) rounded up to the next even integer.
    let h_raw = width * 9 / 16;
    let h = (h_raw + 1) & !1;
    (width, h)
}

/// Hard ceiling on the gap between consecutive frames before a
/// live RTSP session is considered stalled and the supervisor
/// loop forces a reconnect. `rtspsrc` will sit forever in
/// PLAYING with no EOS / no Error when the upstream silently
/// stops sending RTP (common with: mediamtx publisher dying,
/// the YouTube/streamlink bridge wedging mid-segment, NAT
/// rebinding on TCP-tunnelled streams). 15s comfortably absorbs
/// transient hiccups at 1–15fps cameras (longest live gap we
/// expect is ~3s of stalled buffer at 0.3fps deep cameras),
/// while still recovering inside one preroll backoff cycle for
/// the operator. Same threshold is also used for the
/// "never-saw-the-first-frame" timeout — if the pipeline
/// reports PLAYING but no sample ever arrives in 15s the source
/// is almost certainly negotiating endlessly against a dead
/// publisher.
#[cfg(feature = "gstreamer")]
const RTSP_STALL_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Error)]
pub enum FrameSourceError {
    #[error("source closed")]
    Closed,
    #[error("backend: {0}")]
    Backend(String),
}

#[async_trait]
pub trait FrameSource: Send {
    /// Run until the source is closed or fails. Frames go out on `tx`.
    async fn run(self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError>;
}

// ---------------------------------------------------------------------------
// VirtualSource — black RGB frames at configured fps. No system dependency.
// ---------------------------------------------------------------------------

pub struct VirtualSource {
    pub camera_id: CameraId,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[async_trait]
impl FrameSource for VirtualSource {
    async fn run(self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        let interval_ms = if self.fps == 0 {
            200
        } else {
            1000 / self.fps as u64
        };
        let mut frame_id: u64 = 0;
        let buf = Arc::new(vec![0u8; (self.width * self.height * 3) as usize]);
        loop {
            frame_id += 1;
            let f = Frame {
                camera_id: self.camera_id,
                frame_id,
                captured_at: Utc::now(),
                width: self.width,
                height: self.height,
                format: PixelFormat::Rgb24,
                data: buf.clone(),
                trace_id: Uuid::now_v7().to_string(),
            };
            // try_send so the source never blocks on a slow consumer; the gate
            // / pool decide what to drop, not the source. Either branch sleeps
            // for the same interval, so just drop the result and sleep.
            let _ = tx.try_send(f);
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// FailingSource — a source that immediately returns a `Backend(msg)` error
// without producing any frames. Used by the supervisor as the dispatch
// target when a camera URL requires a backend the engine wasn't compiled
// with (today: rtsp:// without the `gstreamer` feature). Surfaces as a
// loud "frame source ended" warn in the supervisor instead of silently
// falling through to a 640x480 VirtualSource.
// ---------------------------------------------------------------------------

pub struct FailingSource {
    pub message: String,
}

#[async_trait]
impl FrameSource for FailingSource {
    async fn run(self: Box<Self>, _tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        Err(FrameSourceError::Backend(self.message))
    }
}

// ---------------------------------------------------------------------------
// RtspSource — real GStreamer RTSP source. Behind the `gstreamer` feature so
// the workspace builds bare on dev boxes.
//
// Pipeline:
//   rtspsrc location=URL latency=200 protocols=tcp+udp
//   ! decodebin force-sw-decoders=true
//   ! videorate ! videoscale ! videoconvert
//   ! video/x-raw,format=RGB,width=960,height=540,framerate=N/1
//   ! appsink name=sink emit-signals=false sync=false drop=true max-buffers=4
//
// `parse::launch` handles the dynamic pad-added linking on rtspsrc and
// decodebin for us. The appsink callback fires on a gstreamer streaming
// thread; we `try_send` so a slow downstream consumer drops frames at the
// edge instead of stalling the camera. The pool / gate decide what to drop,
// not the source.
//
// `force-sw-decoders=true` is REQUIRED on macOS: without it, decodebin
// autoplugs `vtdec` (Apple VideoToolbox), which produces GL textures and
// triggers a `GStreamer-GL-WARNING: An NSApplication needs to be running
// on the main thread`. Caps negotiation between vtdec and videoconvert
// then hangs at PAUSED→PLAYING and no samples ever reach the appsink.
// We don't run an NSApplication (we're a headless engine), so software
// decode is the only path that produces frames. avdec_h264/avdec_h265 from
// gst-libav handle every realistic camera codec at the FPS rates we use.
//
// Resolution is capped at 960×540 because every realistic downstream
// consumer is much smaller: the YOLO detector resizes to 640×640, the
// viewer renders into a card that's <800px wide, and the snapshot
// JPEG endpoint re-encodes per request (so smaller is faster everywhere).
// Source 1920×1080 RGB = 6.2 MB/frame; capped 960×540 = 1.5 MB/frame
// — 4× less channel bandwidth and JPEG encode time. videoscale's
// `add-borders` default is true since gst 1.6, so non-16:9 sources
// letterbox cleanly instead of distorting.
//
// Bus is pumped on a `spawn_blocking` task because gst-rs's `iter_timed`
// blocks the calling thread. EOS / Error end the session; the outer
// `run_with_backoff` then sleeps with exponential backoff (1s → 30s) and
// rebuilds the pipeline from scratch. Net: a flapping camera burns ≤30 s
// of wall clock between attempts and never wedges the engine.
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
pub struct RtspSource {
    pub camera_id: CameraId,
    pub url: String,
    pub max_fps: u32,
    /// RGB frame width the videoscale element produces. Derived
    /// at spawn time from the camera's resolved detector input
    /// size via [`supervisor_frame_for`]; the supervisor passes
    /// it down through `build_source`. Was hardcoded to
    /// [`RTSP_SOURCE_FRAME_WIDTH`] (960) before the per-camera
    /// work; callers without per-camera context can still pass
    /// the constant.
    pub frame_width: u32,
    /// Companion to [`Self::frame_width`].
    pub frame_height: u32,
    /// Operator-configured codec for this camera, if known. Used
    /// only for observability: the pipeline runs `decodebin` which
    /// is codec-agnostic and will handle whatever the camera
    /// actually publishes. When set, we compare against the
    /// `encoding-name` we observe on `rtspsrc`'s pad-added caps and
    /// warn on mismatch so operators can spot stale config (e.g.
    /// camera switched from H.264 to H.265 on the NVR side).
    /// `None` means autodetect / unknown — we still log what we saw.
    pub expected_codec: Option<CodecKind>,
}

// ---------------------------------------------------------------------------
// SharedRtspSource — frame source backed by a `PreRollIngester` that owns
// the only RTSP session for the camera. Used when the recorder is
// `gstreamer`, so the detector and recorder share one connection
// instead of opening two against the camera (the latter wedges on
// firmware that caps concurrent sessions at one per stream path —
// confirmed on the InSight 192.168.1.66 fixture).
//
// The ingester is built with [`PreRollIngester::new_with_rgb`], which
// adds a `tee` after the H.264 parser plus a second branch that
// decodes to RGB 960×540 and publishes every frame on a
// `broadcast::Sender<Frame>`. This source just subscribes to that
// broadcast and forwards into the supervisor's mpsc, mirroring the
// drop policy of [`RtspSource`] (try_send; the gate/pool decide what
// to drop, not the source).
//
// Reconnect is the ingester's problem — it has its own
// supervisor + exponential backoff. From this source's perspective a
// reconnect is just a brief gap in `recv()` and we keep looping; the
// broadcast::Receiver survives across ingester sessions because the
// `broadcast::Sender` lives in the ingester struct, not the pipeline.
// ---------------------------------------------------------------------------

/// Why analysis stopped using the camera's substream and reverted to
/// the main stream. Reported per camera so an operator sees which of
/// the three happened rather than a bare "unavailable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The second session never came up — refused, unauthorised, or
    /// unreachable.
    Refused,
    /// The session connected but produced no decoded frame inside the
    /// grace window.
    NoFrames,
    /// Frames arrived, then the rate collapsed below half of what the
    /// substream advertised.
    Unhealthy,
}

/// Verdict on the analysis session, recomputed on a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisVerdict {
    /// Too early to judge — inside the grace window with no frames yet.
    Measuring,
    /// Delivering frames at an acceptable rate.
    Healthy,
    /// Revert to the main stream for the stated reason.
    FallBack(FallbackReason),
}

/// What the source can actually observe about the analysis session.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisObservation {
    /// Frames received **in the current window**, not since the session
    /// started. A cumulative count would make this verdict useless: a
    /// substream that ran healthy for ten hours and then died needs
    /// another ten hours of silence before its lifetime average falls
    /// below the threshold.
    pub frames: u64,
    /// Wall time the current window has been open.
    pub elapsed: std::time::Duration,
    /// Whether the ingester currently has a live RTSP session. Tells a
    /// refused connection apart from one that connected and went quiet.
    pub session_live: bool,
    /// Frames per second the substream was configured to deliver.
    pub expected_fps: u32,
}

/// How long a fresh analysis session gets to produce its first frame.
/// Generous because the ingester's own reconnect backoff doubles from
/// 500 ms, so a camera that refuses the second session burns several
/// retries before anyone should conclude anything from the silence.
pub const ANALYSIS_FIRST_FRAME_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long frames must flow before their rate is judged. Below this a
/// slow start would read as a collapse.
pub const ANALYSIS_RATE_SETTLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Decide whether analysis should stay on the substream (SPEC-069).
///
/// Pure, so the policy is testable without a camera, a GPU or
/// GStreamer. The wiring around it is none of those things, and this
/// is the part that decides whether a site's analysis quietly stops.
#[must_use]
pub fn analysis_verdict(obs: &AnalysisObservation) -> AnalysisVerdict {
    if obs.frames == 0 {
        if obs.elapsed < ANALYSIS_FIRST_FRAME_GRACE {
            return AnalysisVerdict::Measuring;
        }
        return AnalysisVerdict::FallBack(if obs.session_live {
            FallbackReason::NoFrames
        } else {
            FallbackReason::Refused
        });
    }
    if obs.elapsed < ANALYSIS_RATE_SETTLE {
        return AnalysisVerdict::Healthy;
    }
    let expected = if obs.expected_fps == 0 {
        15
    } else {
        obs.expected_fps
    };
    if obs.frames as f64 / obs.elapsed.as_secs_f64() < f64::from(expected) / 2.0 {
        return AnalysisVerdict::FallBack(FallbackReason::Unhealthy);
    }
    AnalysisVerdict::Healthy
}

#[cfg(feature = "gstreamer")]
pub struct SharedRtspSource {
    pub camera_id: CameraId,
    /// The ingester whose RGB tap we subscribe to. Held as an
    /// `Arc` so the source can keep the ingester alive for the
    /// lifetime of the supervisor task even if the recorder's
    /// internal `ingesters` map drops its reference (e.g. a
    /// reconciler tearing down the camera mid-shutdown).
    pub ingester: std::sync::Arc<crate::preroll_ingester::PreRollIngester>,
    /// Second session on the camera's `analysis_url`, when one is
    /// configured (SPEC-069 Phase 2). When present its RGB tap is
    /// what the supervisor reads and [`Self::ingester`]'s tap is
    /// valved off; when it fails any of the triggers in
    /// [`analysis_verdict`] we open the main valve and read from
    /// there instead. The main session is never torn down or
    /// rebuilt by any of this — recording and HD live view are not
    /// collateral (SPEC-069 invariants I2\u2013I5).
    pub analysis: Option<std::sync::Arc<crate::preroll_ingester::PreRollIngester>>,
}

#[cfg(feature = "gstreamer")]
#[async_trait]
impl FrameSource for SharedRtspSource {
    async fn run(self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        use tokio::sync::broadcast::error::RecvError;
        // Which session are we reading? The analysis one while it is
        // healthy, the main one otherwise. Switching between them
        // never touches the main pipeline's NAL branch, so recording
        // and HD live view are unaffected either way.
        let mut reading_analysis = self.analysis.is_some();
        if self.analysis.is_some() {
            // Main stops decoding for analysis; the substream takes over.
            self.ingester.set_rgb_valve_closed(true);
            tracing::info!(
                camera_id = self.camera_id,
                "analysis reading the camera substream; main-stream rgb tap valved off"
            );
        }
        let mut rx = self.frames_from(reading_analysis)?;
        let expected_fps = self
            .analysis
            .as_ref()
            .and_then(|a| a.rgb_tap_fps())
            .unwrap_or(0);
        let started = std::time::Instant::now();
        let mut frames: u64 = 0;
        // Rate is judged over a rolling window, not the session's
        // lifetime — see `AnalysisObservation::frames`. `window_start`
        // and `window_frames` reset each time a window closes; `frames`
        // and `started` stay cumulative so the first-frame grace check
        // still measures from session start.
        let mut window_start = started;
        let mut window_frames: u64 = 0;
        let mut health_tick =
            tokio::time::interval(std::time::Duration::from_secs(ANALYSIS_HEALTH_TICK_SECS));
        health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            camera_id = self.camera_id,
            "shared rtsp source subscribed to ingester rgb tap"
        );
        loop {
            if tx.is_closed() {
                return Err(FrameSourceError::Closed);
            }
            tokio::select! {
                _ = health_tick.tick(), if reading_analysis => {
                    // Before the first frame, judge against the session's
                    // age (the grace window); afterwards, against the
                    // rolling window so a collapse is caught while it is
                    // happening rather than averaged away.
                    let (obs_frames, obs_elapsed) = if frames == 0 {
                        (0, started.elapsed())
                    } else {
                        (window_frames, window_start.elapsed())
                    };
                    let verdict = analysis_verdict(&AnalysisObservation {
                        frames: obs_frames,
                        elapsed: obs_elapsed,
                        session_live: self
                            .analysis
                            .as_ref()
                            .is_some_and(|a| a.is_buffering()),
                        expected_fps,
                    });
                    if obs_elapsed >= ANALYSIS_RATE_SETTLE {
                        window_start = std::time::Instant::now();
                        window_frames = 0;
                    }
                    if let AnalysisVerdict::FallBack(reason) = verdict {
                        tracing::warn!(
                            camera_id = self.camera_id,
                            ?reason,
                            frames,
                            elapsed_s = started.elapsed().as_secs(),
                            "analysis substream unusable; reverting to the main stream \
                             (recording and HD live view are unaffected)"
                        );
                        self.ingester.set_rgb_valve_closed(false);
                        // Stop the substream session outright. Leaving it
                        // reconnecting with no subscriber would turn one
                        // decode into two for the life of the camera —
                        // the precise cost this phase exists to remove.
                        if let Some(a) = self.analysis.as_ref() {
                            a.shutdown();
                        }
                        reading_analysis = false;
                        rx = self.frames_from(false)?;
                    }
                }
                recv = rx.recv() => match recv {
                    Ok(frame) => {
                        frames += 1;
                        window_frames += 1;
                        // Same drop policy as RtspSource: try_send so a
                        // slow downstream gate/pool drops at the edge
                        // instead of stalling the broadcast (which would
                        // cause Lagged on the NEXT recv).
                        let _ = tx.try_send(frame);
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            camera_id = self.camera_id,
                            dropped = n,
                            "shared rtsp source lagged \
                             (downstream too slow; gate/pool should be dropping at the edge)"
                        );
                    }
                    Err(RecvError::Closed) => {
                        // The Sender lives in the PreRollIngester
                        // struct. Closed means the ingester was
                        // dropped — typically only happens during
                        // engine shutdown. Return Closed so the
                        // supervisor sees the source ended.
                        return Err(FrameSourceError::Closed);
                    }
                },
            }
        }
    }
}

/// How often the analysis session's health is re-judged.
pub const ANALYSIS_HEALTH_TICK_SECS: u64 = 5;

#[cfg(feature = "gstreamer")]
impl SharedRtspSource {
    fn frames_from(
        &self,
        analysis: bool,
    ) -> Result<tokio::sync::broadcast::Receiver<Frame>, FrameSourceError> {
        let ing = if analysis {
            self.analysis.as_ref().unwrap_or(&self.ingester)
        } else {
            &self.ingester
        };
        ing.subscribe_frames().ok_or_else(|| {
            FrameSourceError::Backend(
                "shared rtsp source: ingester has no RGB tap (built via \
                 PreRollIngester::new instead of new_with_rgb)"
                    .into(),
            )
        })
    }
}

#[cfg(feature = "gstreamer")]
pub(crate) mod gst_init {
    use super::FrameSourceError;
    use std::sync::OnceLock;

    static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

    /// Idempotent `gstreamer::init()`. Both `RtspSource` and
    /// `GstClipRecorder` call this on every entry into a GStreamer
    /// code path; the OnceLock guarantees the underlying init only
    /// runs once per process.
    pub fn ensure() -> Result<(), FrameSourceError> {
        let res = GST_INIT.get_or_init(|| gstreamer::init().map_err(|e| e.to_string()));
        match res {
            Ok(()) => Ok(()),
            Err(e) => Err(FrameSourceError::Backend(format!("gst::init: {e}"))),
        }
    }
}

#[cfg(feature = "gstreamer")]
#[async_trait]
impl FrameSource for RtspSource {
    async fn run(self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        gst_init::ensure()?;
        let mut backoff_ms: u64 = 1_000;
        // Teardown is detached, so the pipeline this loop just abandoned may
        // still be PLAYING when the next session starts — and it holds a clone
        // of the same `tx`. Bumping a generation per session lets the appsink
        // tell the live session from a zombie (BUG-070).
        let generation = Arc::new(AtomicU64::new(0));
        loop {
            if tx.is_closed() {
                return Err(FrameSourceError::Closed);
            }
            let session_gen = generation.fetch_add(1, Ordering::AcqRel) + 1;
            match self.run_session(&tx, &generation, session_gen).await {
                Ok(()) => {
                    tracing::info!(camera_id = self.camera_id, "rtsp session EOS");
                }
                Err(e) => {
                    tracing::warn!(camera_id = self.camera_id, "rtsp session failed: {e}");
                }
            }
            if tx.is_closed() {
                return Err(FrameSourceError::Closed);
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }
}

#[cfg(feature = "gstreamer")]
impl RtspSource {
    async fn run_session(
        &self,
        tx: &mpsc::Sender<Frame>,
        generation: &Arc<AtomicU64>,
        session_gen: u64,
    ) -> Result<(), FrameSourceError> {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        use gstreamer_app::{AppSink, AppSinkCallbacks};
        use gstreamer_video::prelude::*;
        use gstreamer_video::{VideoFormat, VideoFrameRef, VideoInfo};
        use std::sync::atomic::{AtomicBool, Ordering};

        // The URL is operator-supplied via config; we drop embedded `"` to
        // keep `parse::launch` parsing safe but otherwise pass through (RFC
        // 3986 forbids unescaped quotes anyway).
        let url_safe = self.url.replace('"', "");
        let fr = if self.max_fps == 0 { 15 } else { self.max_fps };
        // `force-sw-decoders` is macOS-only. On macOS, decodebin
        // autoplugs `vtdec` which produces GL textures and deadlocks
        // a headless engine (see module docstring). On Linux every
        // realistic backend (`vah264dec` via libgstva, `vaapih264dec`
        // via legacy gstreamer-vaapi, or `nvh264dec`) is safe; forcing
        // software here would pin one CPU core per camera on any 4K+
        // H.264 stream and keep the iGPU at idle clock.
        //
        // This standalone source keeps `decodebin` (rank-based
        // autoplug already prefers `vah264dec` on a VA-capable Linux
        // box, so it is hardware-accelerated without an explicit
        // chain). The explicit-decoder selection driven by
        // `[runtime.decode] mode` (see `crate::decode`) applies to the
        // pre-roll RGB tap in `preroll_ingester.rs`, which builds its
        // pipeline by hand and so has no decodebin to autoplug. In
        // production the detector consumes that shared tap
        // (`SharedRtspSource`); this `RtspSource` is the fallback used
        // only when no shared tap exists.
        let force_sw = cfg!(target_os = "macos");
        // protocols=tcp forces TCP-only RTP transport. UDP would be
        // marginally lower latency but loses packets silently under
        // any link contention — see preroll_ingester.rs for the same
        // reasoning. latency=500 matches the recorder so both feeds
        // recover from the same hiccups at the same time.
        let desc = format!(
            "rtspsrc name=src location=\"{url_safe}\" latency=500 protocols=tcp \
             ! decodebin force-sw-decoders={force_sw} \
             ! {tail} \
             ! video/x-raw,format=RGB,width={w},height={h},framerate={fr}/1 \
             ! appsink name=sink emit-signals=false sync=false drop=true max-buffers=4",
            tail = crate::decode::CPU_TAIL,
            w = self.frame_width,
            h = self.frame_height,
        );

        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| FrameSourceError::Backend(format!("parse::launch: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| FrameSourceError::Backend("downcast Pipeline".into()))?;

        let sink = pipeline
            .by_name("sink")
            .ok_or_else(|| FrameSourceError::Backend("appsink 'sink' not found".into()))?
            .downcast::<AppSink>()
            .map_err(|_| FrameSourceError::Backend("downcast AppSink".into()))?;

        // Snoop the RTP encoding-name as soon as rtspsrc adds its
        // first src pad. Pure observability — we DO NOT switch
        // decoders here, decodebin handles whatever shows up. The
        // log line lets operators correlate "camera I configured as
        // H.264" against "what the camera actually publishes".
        if let Some(rtspsrc) = pipeline.by_name("src") {
            let camera_id_snoop = self.camera_id;
            let expected_snoop = self.expected_codec;
            let logged_codec = Arc::new(AtomicBool::new(false));
            rtspsrc.connect_pad_added(move |_elem, pad| {
                if logged_codec.swap(true, Ordering::Relaxed) {
                    return;
                }
                let Some(caps) = pad.current_caps().or_else(|| pad.allowed_caps()) else {
                    return;
                };
                let Some(s) = caps.structure(0) else {
                    return;
                };
                let encoding = s.get::<String>("encoding-name").ok();
                let observed = encoding
                    .as_deref()
                    .and_then(|e| match e.to_uppercase().as_str() {
                        "H264" => Some(CodecKind::H264),
                        "H265" | "HEVC" => Some(CodecKind::H265),
                        _ => None,
                    });
                match (expected_snoop, observed) {
                    (Some(exp), Some(obs)) if exp.base() != obs.base() => {
                        tracing::warn!(
                            camera_id = camera_id_snoop,
                            expected = %exp,
                            observed = %obs,
                            encoding_name = ?encoding,
                            "rtspsrc codec mismatch: stream advertises a different \
                             codec than the camera config; decodebin will still handle it, \
                             but operator should update the config to match"
                        );
                    }
                    (_, Some(obs)) => {
                        tracing::info!(
                            camera_id = camera_id_snoop,
                            expected = ?expected_snoop.map(|c| c.to_string()),
                            observed = %obs,
                            "rtspsrc negotiated codec"
                        );
                    }
                    (_, None) => {
                        tracing::info!(
                            camera_id = camera_id_snoop,
                            expected = ?expected_snoop.map(|c| c.to_string()),
                            encoding_name = ?encoding,
                            "rtspsrc pad-added: unrecognised encoding-name"
                        );
                    }
                }
            });
        }

        let camera_id = self.camera_id;
        let counter = Arc::new(parking_lot::Mutex::new(0u64));
        let tx_cb = tx.clone();
        let gen_cb = generation.clone();
        let counter_cb = counter.clone();
        let logged_first = Arc::new(AtomicBool::new(false));
        let logged_first_cb = logged_first.clone();
        // Wall-clock anchor for the stall watchdog. Reset on the
        // pipeline→PLAYING transition below so a session that
        // negotiates SDP but never produces a single sample still
        // gets killed inside `RTSP_STALL_TIMEOUT_SECS`; bumped on
        // every appsink callback so a normally-flowing session
        // never trips the watchdog.
        let last_frame_at: Arc<parking_lot::Mutex<Instant>> =
            Arc::new(parking_lot::Mutex::new(Instant::now()));
        let last_frame_at_cb = last_frame_at.clone();
        let last_frame_at_w = last_frame_at.clone();

        sink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let info = VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;

                    // Use VideoFrameRef so we read the *actual* per-buffer
                    // plane data and stride (which can come from a
                    // VideoMeta attached to the buffer and differ from
                    // caps-derived defaults). For RGB this is one plane;
                    // we copy row-by-row into a tightly packed Vec because
                    // downstream consumers (image::JpegEncoder for the
                    // snapshot endpoint, ndarray for the YOLO detector)
                    // expect width*height*3 with no row padding.
                    let frame_ref = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                        .map_err(|_| gst::FlowError::Error)?;

                    // One-shot diagnostic on the first sample of each
                    // session so we can see what was actually negotiated
                    // (vs what we asked for in the caps filter).
                    if !logged_first_cb.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            camera_id = camera_id,
                            caps = %caps,
                            format = ?info.format(),
                            width = info.width(),
                            height = info.height(),
                            stride0 = frame_ref.plane_stride().first().copied().unwrap_or(0),
                            buffer_size = buffer.size(),
                            expected_rgb_bytes = info.width() as usize * info.height() as usize * 3,
                            "rtsp appsink: first sample"
                        );
                    }

                    // Bail out loudly if the caps negotiation gave us
                    // anything other than RGB — we want a hard failure
                    // (and a backoff retry) instead of silently shipping
                    // YUV bytes mislabeled as RGB to the JPEG encoder.
                    if info.format() != VideoFormat::Rgb {
                        tracing::error!(
                            camera_id = camera_id,
                            format = ?info.format(),
                            "rtsp appsink received non-RGB sample; \
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
                        tracing::error!(
                            camera_id = camera_id,
                            stride,
                            row_bytes,
                            plane_len = plane.len(),
                            height,
                            "rtsp appsink buffer geometry inconsistent with caps"
                        );
                        return Err(gst::FlowError::Error);
                    }

                    let mut data = Vec::with_capacity(row_bytes * height);
                    if stride == row_bytes {
                        // Hot path: no padding, single bulk copy.
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
                    // Stall-watchdog heartbeat. Cheap (one mutex
                    // store on a contention-free lock) but it has
                    // to live in the callback because the bus
                    // thread can't observe sample flow directly.
                    *last_frame_at_cb.lock() = Instant::now();
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
                    // Never block streaming threads — the gate/pool drop policy
                    // is upstream of us. Only the current session may publish:
                    // a superseded pipeline still draining into the same `tx`
                    // would interleave its own `frame_id` sequence and stamp
                    // fresh `captured_at` on older pictures, which is what put
                    // the wall out of order and ran its OSD clock backwards.
                    if is_current_session(&gen_cb, session_gen) {
                        let _ = tx_cb.try_send(frame);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| FrameSourceError::Backend(format!("set Playing: {e}")))?;
        // Restart the watchdog clock from "just transitioned to
        // PLAYING"; without this a long preroll/SDP negotiation
        // (e.g. RTSPS handshake on a slow link) could eat into
        // the first-frame budget below.
        *last_frame_at.lock() = Instant::now();

        let bus = pipeline
            .bus()
            .ok_or_else(|| FrameSourceError::Backend("pipeline bus missing".into()))?;

        // Cooperative shutdown for the blocking bus thread. `iter_timed(NONE)`
        // would park the OS thread forever — on Ctrl-C the supervisor aborts
        // this task, but the spawn_blocking thread keeps the bus + a strong
        // pipeline ref alive, and the tokio runtime can never finish dropping.
        // Symptom: engine ignores Ctrl-C and needs SIGKILL. Fix: short
        // `timed_pop` poll that checks an AtomicBool every 100ms, and a
        // sibling future that flips the flag the moment the mpsc receiver is
        // dropped (which happens as soon as the supervisor task is aborted).
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_bus = shutdown.clone();
        let pipeline_for_bus = pipeline.clone();
        // Dedicated OS thread, NOT tokio::task::spawn_blocking: this loop
        // runs for the lifetime of the RTSP session. Pinning one of
        // tokio's bounded blocking-pool threads per camera starves every
        // other spawn_blocking call (detector replies, sqlx, clip-recorder
        // appsrc pushes) once the camera count crosses the cap. A plain
        // OS thread is free of the pool and costs ~8 KB stack.
        let (bus_done_tx, bus_done_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let thread_name = format!("nexus-gst-bus-cam{}", self.camera_id);
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                use gst::MessageView;
                let poll = gst::ClockTime::from_mseconds(100);
                let out = loop {
                    if shutdown_bus.load(Ordering::Relaxed) {
                        break Ok(());
                    }
                    let Some(msg) = bus.timed_pop(Some(poll)) else {
                        continue;
                    };
                    match msg.view() {
                        MessageView::Eos(..) => {
                            let _ = &pipeline_for_bus;
                            break Ok(());
                        }
                        MessageView::Error(e) => {
                            let _ = &pipeline_for_bus;
                            break Err(format!(
                                "{}: {}",
                                e.error(),
                                e.debug().unwrap_or_else(|| "<no debug>".into())
                            ));
                        }
                        _ => {}
                    }
                };
                let _ = bus_done_tx.send(out);
            })
            .map_err(|e| FrameSourceError::Backend(format!("spawn bus thread: {e}")))?;

        // `tx.closed()` resolves the moment the supervisor's Receiver is
        // dropped (typically within microseconds of `task.abort()`). Racing
        // it against the bus join means a Ctrl-C tear-down doesn't have to
        // wait for an RTSP timeout or an EOS that may never come.
        //
        // The stall watchdog covers the silent-failure case that
        // neither the bus nor the supervisor catches: rtspsrc
        // stays in PLAYING with the TCP connection alive but the
        // upstream stops pushing RTP — no EOS, no Error, the
        // appsink callback just goes quiet. Without this branch
        // `run_session` would block on the bus poll forever and
        // the outer reconnect loop in `RtspSource::run` would
        // never get a chance to retry. Polls the shared
        // `last_frame_at` once a second; returns a synthetic
        // backend error after `RTSP_STALL_TIMEOUT_SECS` so the
        // existing exponential-backoff reconnect kicks in.
        let stall_timeout = Duration::from_secs(RTSP_STALL_TIMEOUT_SECS);
        let cam_id_for_stall = self.camera_id;
        let stall_watchdog = async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            // Skip the immediate-fire tick; otherwise we'd see
            // elapsed < 1ms on the first poll and bail uselessly
            // if `Instant::now()` raced the PLAYING reset.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let elapsed = last_frame_at_w.lock().elapsed();
                if elapsed > stall_timeout {
                    tracing::warn!(
                        camera_id = cam_id_for_stall,
                        elapsed_ms = elapsed.as_millis() as u64,
                        threshold_ms = stall_timeout.as_millis() as u64,
                        "rtsp session stalled (no frames); forcing reconnect",
                    );
                    return FrameSourceError::Backend(format!(
                        "stall watchdog: no frames for {}s",
                        elapsed.as_secs()
                    ));
                }
            }
        };
        tokio::pin!(stall_watchdog);

        let bus_result = tokio::select! {
            r = bus_done_rx => {
                r.map_err(|e| FrameSourceError::Backend(format!("bus thread dropped: {e}")))?
                    .map_err(FrameSourceError::Backend)
            }
            _ = tx.closed() => {
                shutdown.store(true, Ordering::Relaxed);
                Err(FrameSourceError::Closed)
            }
            e = &mut stall_watchdog => {
                shutdown.store(true, Ordering::Relaxed);
                Err(e)
            }
        };

        // Null the pipeline regardless of which branch won. This unblocks
        // any in-flight bus dispatch on the (now-detached) blocking thread,
        // which will then observe `shutdown=true` on its next poll and exit
        // within ≤100 ms — no thread leak, no Drop hang. Detached because
        // we are on a tokio worker and `rtspsrc` parked in a read on a dead
        // camera makes the transition unbounded.
        crate::teardown::null_pipeline_detached(
            pipeline,
            "source::RtspSource::run",
            Some(camera_id),
        );
        bus_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(frames: u64, secs: u64, session_live: bool) -> AnalysisObservation {
        AnalysisObservation {
            frames,
            elapsed: std::time::Duration::from_secs(secs),
            session_live,
            expected_fps: 15,
        }
    }

    /// A camera that is simply slow to hand over its first substream
    /// frame must not be written off. The ingester's own reconnect
    /// backoff doubles from 500 ms, so several seconds of silence is
    /// ordinary rather than diagnostic.
    #[test]
    fn silence_inside_the_grace_window_is_not_a_verdict() {
        assert_eq!(
            analysis_verdict(&obs(0, 5, true)),
            AnalysisVerdict::Measuring
        );
    }

    /// A refused second session and one that connected then went quiet
    /// need different words in front of an operator: the first is a
    /// firmware session cap, the second is a camera problem.
    #[test]
    fn a_refused_session_is_distinguished_from_a_silent_one() {
        assert_eq!(
            analysis_verdict(&obs(0, 30, false)),
            AnalysisVerdict::FallBack(FallbackReason::Refused)
        );
        assert_eq!(
            analysis_verdict(&obs(0, 30, true)),
            AnalysisVerdict::FallBack(FallbackReason::NoFrames)
        );
    }

    /// Frames flowing at the advertised rate is the whole point.
    #[test]
    fn a_substream_delivering_its_rate_is_healthy() {
        assert_eq!(
            analysis_verdict(&obs(15 * 120, 120, true)),
            AnalysisVerdict::Healthy
        );
    }

    /// A substream that streams healthily for hours and then dies must
    /// be caught. Judged on a lifetime average it never would be — ten
    /// healthy hours need ten silent ones to drag the mean under the
    /// threshold — which is why the caller feeds this a rolling window.
    #[test]
    fn a_long_healthy_run_followed_by_silence_still_falls_back() {
        // One window's worth of total silence, which is what the caller
        // passes once the window has rolled.
        assert_eq!(
            analysis_verdict(&obs(0, 90, true)),
            AnalysisVerdict::FallBack(FallbackReason::NoFrames)
        );
    }

    /// Below half the advertised rate the substream is worse than the
    /// main stream it displaced, so revert.
    #[test]
    fn a_collapsed_rate_falls_back() {
        // 3 fps against an advertised 15.
        assert_eq!(
            analysis_verdict(&obs(3 * 120, 120, true)),
            AnalysisVerdict::FallBack(FallbackReason::Unhealthy)
        );
    }

    /// Rate is not judged before it has had time to settle, or every
    /// camera would fall back during its first seconds of streaming.
    #[test]
    fn rate_is_not_judged_before_it_settles() {
        assert_eq!(
            analysis_verdict(&obs(1, 10, true)),
            AnalysisVerdict::Healthy
        );
    }

    /// `max_fps = 0` means "unbounded" everywhere else in the engine and
    /// the ingester substitutes 15. If this function did not make the
    /// same substitution it would divide by zero and call every healthy
    /// camera unhealthy.
    #[test]
    fn an_unbounded_fps_falls_back_to_the_same_default_the_ingester_uses() {
        let o = AnalysisObservation {
            frames: 15 * 120,
            elapsed: std::time::Duration::from_secs(120),
            session_live: true,
            expected_fps: 0,
        };
        assert_eq!(analysis_verdict(&o), AnalysisVerdict::Healthy);
    }

    /// Regression lock on BUG-070. `set_state(Null)` is detached, so the
    /// pipeline a reconnect abandons may still be PLAYING — and it holds a
    /// clone of the same `tx` its replacement writes to. Two live producers
    /// on one channel interleave independent `frame_id` sequences and stamp
    /// fresh `captured_at` onto older pictures, which is what put the wall
    /// out of order and ran its burned-in OSD clock backwards.
    #[test]
    fn only_the_current_session_may_publish() {
        let generation = AtomicU64::new(0);

        let first = generation.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            is_current_session(&generation, first),
            "the session that just started owns the channel"
        );

        // The reconnect loop starts a replacement while the old pipeline is
        // still draining on a teardown thread.
        let second = generation.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            !is_current_session(&generation, first),
            "a superseded session must be fenced off the shared channel"
        );
        assert!(
            is_current_session(&generation, second),
            "the replacement session publishes"
        );
    }

    #[test]
    fn generations_never_repeat_across_reconnects() {
        let generation = AtomicU64::new(0);
        let seen: Vec<u64> = (0..64)
            .map(|_| generation.fetch_add(1, Ordering::AcqRel) + 1)
            .collect();

        // A repeated generation would un-fence a zombie whose value came
        // back around, so the counter must be strictly increasing.
        assert!(
            seen.windows(2).all(|w| w[1] > w[0]),
            "session generations must be strictly increasing"
        );

        let (current, superseded) = seen.split_last().expect("64 generations");
        assert!(
            superseded
                .iter()
                .all(|g| !is_current_session(&generation, *g)),
            "every superseded session stays fenced, however many reconnects ran"
        );
        assert!(
            is_current_session(&generation, *current),
            "the newest session is the one that publishes"
        );
    }
}
