//! Shared wire types for the Nexus edge engine.
//!
//! Every type in this crate is on the public boundary: it crosses the bus,
//! the database, the HTTP API, and the TypeScript UI. The `ts` feature
//! generates `ui/src/api/types.ts` from these definitions via `ts-rs`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[cfg(feature = "ts")]
use ts_rs::TS;

// ---------------------------------------------------------------------------
// ID aliases — keep them light so they cross any boundary cheaply.
// ---------------------------------------------------------------------------

pub type CameraId = i64;
pub type FrameId = u64;
pub type TrackId = u64;
pub type RuleId = String;
pub type EventId = Uuid;
pub type TraceId = String;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Axis-aligned bounding box in image pixels (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct BBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl BBox {
    pub fn width(&self) -> f32 {
        (self.x2 - self.x1).max(0.0)
    }

    pub fn height(&self) -> f32 {
        (self.y2 - self.y1).max(0.0)
    }

    pub fn area(&self) -> f32 {
        self.width() * self.height()
    }

    pub fn center(&self) -> (f32, f32) {
        ((self.x1 + self.x2) * 0.5, (self.y1 + self.y2) * 0.5)
    }

    pub fn iou(&self, other: &BBox) -> f32 {
        let ix1 = self.x1.max(other.x1);
        let iy1 = self.y1.max(other.y1);
        let ix2 = self.x2.min(other.x2);
        let iy2 = self.y2.min(other.y2);
        let iw = (ix2 - ix1).max(0.0);
        let ih = (iy2 - iy1).max(0.0);
        let inter = iw * ih;
        let union = self.area() + other.area() - inter;
        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Rgb24,
    Bgr24,
    Nv12,
    I420,
}

/// A decoded frame. Pixel buffer is held behind `Arc` so cloning is a
/// pointer bump — that's the hot-path commitment.
#[derive(Debug, Clone)]
pub struct Frame {
    pub camera_id: CameraId,
    pub frame_id: FrameId,
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Arc<Vec<u8>>,
    pub trace_id: TraceId,
}

impl Frame {
    pub fn stride(&self) -> usize {
        match self.format {
            PixelFormat::Rgb24 | PixelFormat::Bgr24 => self.width as usize * 3,
            PixelFormat::Nv12 | PixelFormat::I420 => self.width as usize,
        }
    }
}

/// Lightweight frame summary for the bus and the UI. Never carries pixels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct FrameMetadata {
    pub camera_id: CameraId,
    pub frame_id: FrameId,
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub trace_id: TraceId,
    pub objects: Vec<TrackedObject>,
}

// ---------------------------------------------------------------------------
// Detection + tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct Detection {
    pub label: String,
    pub confidence: f32,
    pub bbox: BBox,
    /// Optional per-detection attributes from the backend (e.g. open-vocab
    /// auxiliary scores). Kept opaque so backends can extend without
    /// schema migrations.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct TrackedObject {
    pub track_id: TrackId,
    pub label: String,
    pub confidence: f32,
    pub bbox: BBox,
    /// Frames since this track was first seen.
    pub age_frames: u32,
    /// Wall-clock age of the track in milliseconds.
    pub age_ms: u64,
    /// Tracker + annotator outputs (motion.speed_class, dwell.zone_state, …).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct Artifacts {
    /// Path (or URL) of an annotated snapshot at the moment of the alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Path of a short video clip surrounding the alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    /// Cloud handoff receipt, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_receipt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct AlertEvent {
    pub event_id: EventId,
    pub camera_id: CameraId,
    pub rule_id: RuleId,
    pub track_id: Option<TrackId>,
    pub label: String,
    pub severity: Severity,
    pub bbox: Option<BBox>,
    pub frame_id: FrameId,
    pub captured_at: DateTime<Utc>,
    pub trace_id: TraceId,
    #[serde(default)]
    pub artifacts: Artifacts,
    #[serde(default)]
    pub context: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Pipeline status (for /api/health and the ops bus)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
pub struct PipelineStatus {
    pub camera_id: CameraId,
    pub state: PipelineState,
    pub frames_decoded: u64,
    pub frames_detected: u64,
    pub last_frame_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "../ui/src/api/types/"))]
#[serde(rename_all = "snake_case")]
pub enum PipelineState {
    Initializing,
    Running,
    Reconnecting,
    Stopped,
    Failed,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TypesError {
    #[error("invalid bbox: {0}")]
    InvalidBBox(String),
    #[error("invalid frame buffer length: expected {expected}, got {got}")]
    FrameBufferLen { expected: usize, got: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_perfect_overlap() {
        let a = BBox { x1: 0.0, y1: 0.0, x2: 10.0, y2: 10.0 };
        assert!((a.iou(&a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn iou_no_overlap() {
        let a = BBox { x1: 0.0, y1: 0.0, x2: 10.0, y2: 10.0 };
        let b = BBox { x1: 20.0, y1: 20.0, x2: 30.0, y2: 30.0 };
        assert_eq!(a.iou(&b), 0.0);
    }

    #[test]
    fn bbox_center_and_area() {
        let a = BBox { x1: 0.0, y1: 0.0, x2: 4.0, y2: 6.0 };
        assert_eq!(a.area(), 24.0);
        assert_eq!(a.center(), (2.0, 3.0));
    }
}
