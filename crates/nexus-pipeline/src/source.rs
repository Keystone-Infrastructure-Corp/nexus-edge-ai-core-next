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
        let interval_ms = if self.fps == 0 { 200 } else { 1000 / self.fps as u64 };
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
            // / pool decide what to drop, not the source.
            if tx.try_send(f).is_err() {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RtspSource — real GStreamer RTSP source. Wired for M1.
// Behind the `gstreamer` feature so the workspace builds bare on dev boxes.
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
pub struct RtspSource {
    pub camera_id: CameraId,
    pub url: String,
    pub max_fps: u32,
}

#[cfg(feature = "gstreamer")]
#[async_trait]
impl FrameSource for RtspSource {
    async fn run(self: Box<Self>, _tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
        // M1 implementation:
        //   gst::init();
        //   pipeline:
        //     rtspsrc location=URL latency=200 protocols=tcp+udp
        //     ! rtph264depay ! decodebin ! videoconvert
        //     ! video/x-raw,format=RGB
        //     ! appsink name=sink emit-signals=true sync=false drop=true max-buffers=4
        //   appsink.set_callbacks → for each sample, build Frame, tx.try_send().
        //
        // Backoff supervisor: on EOS / error, re-create the pipeline with
        // exponential backoff (1s → 30s).
        Err(FrameSourceError::Backend(
            "gstreamer RtspSource is wired in M1 — virtual source is the M0 default".into(),
        ))
    }
}
