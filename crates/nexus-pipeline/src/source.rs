//! Frame sources — RTSP and a virtual generator for tests / dev boots.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use nexus_types::{CameraId, Frame, PixelFormat};
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

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
//   ! videoconvert ! videoscale ! videorate
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
        loop {
            if tx.is_closed() {
                return Err(FrameSourceError::Closed);
            }
            match self.run_session(&tx).await {
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
    async fn run_session(&self, tx: &mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        use gstreamer_app::{AppSink, AppSinkCallbacks};
        use gstreamer_video::VideoInfo;

        // The URL is operator-supplied via config; we drop embedded `"` to
        // keep `parse::launch` parsing safe but otherwise pass through (RFC
        // 3986 forbids unescaped quotes anyway).
        let url_safe = self.url.replace('"', "");
        let fr = if self.max_fps == 0 { 15 } else { self.max_fps };
        let desc = format!(
            "rtspsrc location=\"{url_safe}\" latency=200 protocols=tcp+udp \
             ! decodebin force-sw-decoders=true \
             ! videoconvert ! videoscale ! videorate \
             ! video/x-raw,format=RGB,width=960,height=540,framerate={fr}/1 \
             ! appsink name=sink emit-signals=false sync=false drop=true max-buffers=4"
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

        let camera_id = self.camera_id;
        let counter = Arc::new(parking_lot::Mutex::new(0u64));
        let tx_cb = tx.clone();
        let counter_cb = counter.clone();

        sink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let info = VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    // GStreamer pads each row to a SIMD-aligned stride
                    // (typically 16- or 64-byte boundaries). Downstream
                    // consumers — image::JpegEncoder for the snapshot
                    // endpoint, ndarray for the YOLO detector — all
                    // expect a tightly packed `width * height * 3` buffer.
                    // Copy row-by-row using `info.stride()[0]` so we
                    // never ship a misaligned frame. Symptom of the bug
                    // when this was missing: greyscale-looking output
                    // (RGB triplets smear across rows) plus a truncated
                    // bottom edge (data runs out before the last rows).
                    let map_slice = map.as_slice();
                    let stride = info.stride()[0] as usize;
                    let width = info.width() as usize;
                    let height = info.height() as usize;
                    let row_bytes = width * 3;
                    let mut data = Vec::with_capacity(row_bytes * height);
                    if stride == row_bytes {
                        // Hot path: no padding, single bulk copy.
                        let end = row_bytes * height;
                        if map_slice.len() < end {
                            return Err(gst::FlowError::Error);
                        }
                        data.extend_from_slice(&map_slice[..end]);
                    } else {
                        if map_slice.len() < stride * height {
                            return Err(gst::FlowError::Error);
                        }
                        for y in 0..height {
                            let start = y * stride;
                            data.extend_from_slice(&map_slice[start..start + row_bytes]);
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
                    // Never block streaming threads — the gate/pool drop policy
                    // is upstream of us.
                    let _ = tx_cb.try_send(frame);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| FrameSourceError::Backend(format!("set Playing: {e}")))?;

        let bus = pipeline
            .bus()
            .ok_or_else(|| FrameSourceError::Backend("pipeline bus missing".into()))?;
        let pipeline_for_bus = pipeline.clone();
        let bus_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            use gst::MessageView;
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    MessageView::Eos(..) => {
                        return Ok(());
                    }
                    MessageView::Error(e) => {
                        // Keep pipeline alive past the iterator drop so the
                        // outer caller can null it explicitly.
                        let _ = &pipeline_for_bus;
                        return Err(format!(
                            "{}: {}",
                            e.error(),
                            e.debug().unwrap_or_else(|| "<no debug>".into())
                        ));
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| FrameSourceError::Backend(format!("bus join: {e}")))?;

        let _ = pipeline.set_state(gst::State::Null);
        bus_result.map_err(FrameSourceError::Backend)
    }
}
