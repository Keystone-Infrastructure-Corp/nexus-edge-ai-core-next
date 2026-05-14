//! Per-camera streaming DAG.
//!
//! See [`docs/ARCHITECTURE.md`](../../../../docs/ARCHITECTURE.md). The
//! pipeline is the only crate that knows about all the others; everything
//! upstream stays decoupled.
//!
//! ```text
//!   FrameSource → MotionGate → DetectorPool → Tracker → RuleEvaluator
//!                                  │                          │
//!                                  ▼                          ▼
//!                         LatestFrameCache (L7)        EventStore + Bus
//! ```

#![forbid(unsafe_code)]

pub mod cache;
pub mod gate;
pub mod post_roll;
pub mod preroll;
pub mod recorder;
pub mod source;
pub mod supervisor;

#[cfg(feature = "gstreamer")]
pub mod gst_clip_recorder;

#[cfg(feature = "gstreamer")]
pub mod preroll_ingester;

#[cfg(feature = "gstreamer")]
pub mod thumbnail;

pub use cache::{LatestEntry, LatestFrameCache};
pub use gate::MotionGate;
pub use preroll::{NalRingBuffer, NalSample};
pub use recorder::{
    ClipFinal, ClipHandle, ClipMeta, ClipRecorder, OpenClip, RecorderError, StubClipRecorder,
};
pub use source::{FrameSource, FrameSourceError, VirtualSource};
pub use supervisor::{spawn_camera, CameraHandle};

#[cfg(feature = "gstreamer")]
pub use gst_clip_recorder::GstClipRecorder;

#[cfg(feature = "gstreamer")]
pub use preroll_ingester::PreRollIngester;

#[cfg(feature = "gstreamer")]
pub use source::RtspSource;
