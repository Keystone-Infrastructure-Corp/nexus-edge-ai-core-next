//! Per-camera streaming DAG.
//!
//! See [`docs/ARCHITECTURE.md`](../../../../docs/ARCHITECTURE.md). The
//! pipeline is the only crate that knows about all the others; everything
//! upstream stays decoupled.
//!
//! ```text
//!   FrameSource ──→ MotionGate ──→ DetectorPool ──→ Tracker ──→ RuleEvaluator
//!        │                                             │              │
//!        │ frame                               objects │              ▼
//!        └────────→ LatestFrameCache (L7) ◀────────────┘      EventStore + Bus
//! ```
//!
//! The cache is fed at **two** rates (BUG-136). Its frame is tapped straight
//! off the source, so the live-view wall never inherits inference
//! throughput; its objects come from the far end of the DAG and therefore
//! run at whatever rate the gate passes.

#![forbid(unsafe_code)]

pub mod alert_clip;
pub mod cache;
pub mod crowd_hysteresis;
pub mod decode;
pub mod entity_sighting;
pub mod gate;
pub mod overlay;
pub mod post_roll;
pub mod preroll;
pub mod recorder;
pub mod sink_router;
pub mod skip_policy;
pub mod source;
pub mod static_clear;
pub mod stats;
pub mod supervisor;
pub mod tile;
pub mod tile_executor;

#[cfg(feature = "gstreamer")]
pub mod gst_clip_recorder;

#[cfg(feature = "gstreamer")]
pub mod preroll_ingester;

#[cfg(feature = "gstreamer")]
pub mod teardown;

#[cfg(feature = "gstreamer")]
pub mod thumbnail;

#[cfg(feature = "gstreamer-webrtc")]
pub mod moq_publish;

#[cfg(feature = "gstreamer-webrtc")]
pub mod webrtc;

pub use cache::{LatestEntry, LatestFrameCache};
pub use decode::{select_decode_chain, DecodeBackend, DecodeChain, DecodeMode, FactoryProbe};
pub use entity_sighting::{
    EntityLocalPersist, EntityLocalSeed, EntityLocalUpdate, NoopEntityLocalPersist,
    NoopSightingHook, SightingHook, SightingScheduler, SightingSnapshot,
};
pub use gate::MotionGate;
pub use preroll::{NalRingBuffer, NalSample};
pub use recorder::{
    ClipFinal, ClipHandle, ClipMeta, ClipRecorder, OpenClip, RecorderError, StubClipRecorder,
};
pub use sink_router::{
    AlertClipScheduleGate, NoopAlertClipScheduleGate, NoopSinkRouter, SinkRouter,
};
pub use source::{supervisor_frame_for, RTSP_SOURCE_FRAME_HEIGHT, RTSP_SOURCE_FRAME_WIDTH};
pub use source::{FailingSource, FrameSource, FrameSourceError, VirtualSource};
pub use static_clear::StaticAnchorClearRegistry;
pub use stats::{
    AnalysisStreamRegistry, AnalysisStreamStatus, CameraFrameStats, DecodeHealth,
    DecodeHealthRegistry, FrameStatsRegistry,
};
pub use supervisor::{spawn_camera, CameraHandle};

#[cfg(feature = "gstreamer")]
pub use gst_clip_recorder::{GstClipRecorder, IngesterRegistry};

#[cfg(feature = "gstreamer")]
pub use decode::install_shared_display_context;
#[cfg(feature = "gstreamer")]
pub use decode::GstFactoryProbe;

#[cfg(feature = "gstreamer")]
pub use preroll_ingester::PreRollIngester;

#[cfg(feature = "gstreamer")]
pub use teardown::{null_pipeline_detached, TeardownStats};

#[cfg(feature = "gstreamer")]
pub use source::RtspSource;

#[cfg(feature = "gstreamer")]
pub use source::SharedRtspSource;

#[cfg(feature = "gstreamer-webrtc")]
pub use moq_publish::{MoqError, MoqSession};

#[cfg(feature = "gstreamer-webrtc")]
pub use webrtc::{IceServerCfg, WebRtcError, WebRtcEvent, WebRtcMode, WebRtcSession};
