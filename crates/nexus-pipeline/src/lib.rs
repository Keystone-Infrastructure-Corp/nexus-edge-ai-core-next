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
pub mod source;
pub mod supervisor;

pub use cache::{LatestEntry, LatestFrameCache};
pub use gate::MotionGate;
pub use source::{FrameSource, FrameSourceError, VirtualSource};
pub use supervisor::{spawn_camera, CameraHandle};

#[cfg(feature = "gstreamer")]
pub use source::RtspSource;
