//! Shared wire types for the Nexus edge engine.
//!
//! Every type in this crate is on the public boundary: it crosses the bus,
//! the database, the HTTP API, and the TypeScript UI. The `ts` feature
//! regenerates one `*.ts` file per exported type under `ui/src/api/types/`
//! via `ts-rs`. CI runs `cargo test -p nexus-types --features ts` then
//! `git diff --exit-code ui/src/api/types/` so any Rust-side schema drift
//! must be re-committed alongside the source change.
//!
//! The hand-curated `ui/src/api/types.ts` is the UI's *import entry point*
//! (the per-type generated files use `bigint` for `i64`/`u64`, while the UI
//! stays on `number`; the hand-written file is the explicit shim).
//!
//! The `export_to` paths look like `../../../ui/src/api/types/` because
//! ts-rs joins them onto `${CARGO_MANIFEST_DIR}/bindings/` (the default
//! `TS_RS_EXPORT_DIR`). From `crates/nexus-types/bindings/`, three `..`
//! hops reach the workspace root.

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
/// M3.1 — primary key for a stored visual reference crop used by the
/// YOLOE visual-prompt backend. Surfaces in [`CameraConfigUpdate`] (via
/// `VisualPromptRef`) and in admin REST payloads. Persisted by
/// `nexus-store::visual_prompts` (migration 0012).
pub type VisualPromptId = i64;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Axis-aligned bounding box in image pixels (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub struct Detection {
    pub label: String,
    pub confidence: f32,
    pub bbox: BBox,
    /// Optional per-detection attributes from the backend (e.g. open-vocab
    /// auxiliary scores). Kept opaque so backends can extend without
    /// schema migrations.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

// ---------------------------------------------------------------------------
// Identity (M6)
// ---------------------------------------------------------------------------

/// Three-role authorisation model. Lifted verbatim from M6 design:
/// `admin` (everything; only role allowed to mutate users, OIDC
/// config, storage backends, OAuth start/callback), `operator`
/// (camera/rule/zone/visual-prompt CRUD + replay + delivery
/// toggles), `viewer` (read-only — live frames, timeline, events,
/// dashboards).
///
/// The variants have a total order via [`Role::level`] so a
/// `require_role(operator)` extractor accepts both operators and
/// admins: `role.level() >= required.level()`. The order is
/// `Viewer < Operator < Admin` — picking integer levels (10, 50,
/// 100) leaves room for future tiers without renumbering.
///
/// Serde representation is `"admin" | "operator" | "viewer"` —
/// the same string we store in `users.role` and the
/// `audit_log.actor_label` denormalisation, and the same shape
/// the UI sees over JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full access including user / OIDC / storage / OAuth admin.
    Admin,
    /// Camera + rule + zone + visual-prompt CRUD + replay +
    /// per-rule delivery toggles. Cannot mutate users or
    /// security-critical config.
    Operator,
    /// Read-only — frames, timeline, events, dashboards.
    /// Newly-created users default to viewer (least privilege);
    /// admin promotes after the fact.
    #[default]
    Viewer,
}

impl Role {
    /// String form used in `users.role`, `audit_log` queries,
    /// JWT claims, and OIDC role mappings. Matches the serde
    /// representation by construction.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    /// Integer level used by the future `require_role` extractor.
    /// Higher = more permissions. Gaps are intentional — they
    /// leave headroom for tiers like `superadmin` (200) or
    /// `analyst` (30, between viewer and operator) without
    /// renumbering the existing three.
    pub fn level(self) -> u32 {
        match self {
            Role::Admin => 100,
            Role::Operator => 50,
            Role::Viewer => 10,
        }
    }

    /// Permission check: does `self` satisfy `required`?
    /// `admin >= operator >= viewer`.
    pub fn satisfies(self, required: Role) -> bool {
        self.level() >= required.level()
    }

    /// All three variants in increasing order of privilege.
    /// Used by the future role-enforcement matrix test.
    pub fn all() -> [Role; 3] {
        [Role::Viewer, Role::Operator, Role::Admin]
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = TypesError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Role::Admin),
            "operator" => Ok(Role::Operator),
            "viewer" => Ok(Role::Viewer),
            other => Err(TypesError::InvalidRole(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
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
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub context: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Pipeline status (for /api/health and the ops bus)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub struct PipelineStatus {
    pub camera_id: CameraId,
    pub state: PipelineState,
    pub frames_decoded: u64,
    pub frames_detected: u64,
    pub last_frame_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
#[serde(rename_all = "snake_case")]
pub enum PipelineState {
    Initializing,
    Running,
    Reconnecting,
    Stopped,
    Failed,
}

// ---------------------------------------------------------------------------
// Static-object map (parking-lot anchors)
// ---------------------------------------------------------------------------
//
// One anchor per known-static vehicle location for a camera. Mirrors
// the on-disk shape of `<state_dir>/static_objects/cam-<id>.json` (see
// `nexus-tracker::static_object::StaticAnchor`) so the wire payload and
// the persisted file share a single JSON-field convention. Centroid is
// in the per-camera supervisor frame (16:9 derived from the camera's
// detector input size — 640→640×360, 960→960×540, 1280→1280×720), same
// coordinate system as `TrackedObject.bbox`; the UI gets the actual
// dims from per-clip `source_width`/`source_height` on the tracks API
// so the same transform applies to overlays.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub struct StaticAnchor {
    /// Lowercased detector label (e.g. `"vehicle.car"`, `"truck"`).
    pub label: String,
    /// Centroid X in detector-frame pixels.
    pub center_x: f32,
    /// Centroid Y in detector-frame pixels.
    pub center_y: f32,
    /// Wall-clock (ms since UNIX epoch) of the most recent matching
    /// observation. `null` only for anchors loaded from a registry
    /// file written before the TTL sweep landed; the supervisor
    /// promotes those to a real timestamp on the first matching
    /// frame after restart. UI can render staleness from this value
    /// (e.g. "2m ago" tooltips) and the engine prunes anchors
    /// untouched for longer than `static_object.anchor_ttl_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub last_seen_unix_ms: Option<i64>,
}

/// Returned by `GET /api/cameras/:id/static-anchors`. `anchors`
/// is empty when the per-camera registry file is missing OR when the
/// camera has `behavior.parking_lot_mode = false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub struct StaticAnchorsResponse {
    pub camera_id: CameraId,
    pub anchors: Vec<StaticAnchor>,
}

// ---------------------------------------------------------------------------
// M7 alert delivery (sinks + delivery policy)
// ---------------------------------------------------------------------------
//
// Wire shapes shared by `nexus-sinks` (policy evaluation), `nexus-store`
// (persistence in `delivery_settings` + `rules.delivery_policy_json`),
// the Axum admin API, and the UI's `<WeeklyScheduleEditor>`. The two
// callers that mutate them — `PUT /api/v1/admin/delivery` and
// `PUT /api/v1/rules/:id/delivery` — round-trip the same JSON the UI
// edits, so any drift here is caught at the type level.

/// Weekly schedule grid: 7 days × 48 half-hour slots.
///
/// `grid[d][s]` is `true` when the slot allows delivery, `false`
/// when it suppresses. Day index follows
/// `chrono::Weekday::num_days_from_monday()` — index 0 is Monday,
/// index 6 is Sunday. Slot index is half-hours since 00:00 local
/// time: `s = hour * 2 + (minute >= 30 ? 1 : 0)`.
///
/// The grid is evaluated in the operator-configured IANA timezone
/// (`DeliverySettings::timezone`); DST transitions are handled by
/// `chrono-tz` so a 02:00 → 03:00 spring-forward day still
/// resolves cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverySchedule {
    /// `[day_of_week_0_is_monday][half_hour_slot_0_is_midnight]`.
    pub grid: Vec<Vec<bool>>,
}

impl DeliverySchedule {
    /// Slots per day (half-hour grid).
    pub const SLOTS_PER_DAY: usize = 48;
    /// Days per week.
    pub const DAYS: usize = 7;

    /// Construct an "always on" schedule (every slot true). Useful
    /// as a default when an operator hits *Override* but hasn't
    /// touched the grid yet.
    pub fn always() -> Self {
        Self {
            grid: vec![vec![true; Self::SLOTS_PER_DAY]; Self::DAYS],
        }
    }

    /// Construct an "always off" schedule (every slot false). The
    /// editor's *Off* preset.
    pub fn never() -> Self {
        Self {
            grid: vec![vec![false; Self::SLOTS_PER_DAY]; Self::DAYS],
        }
    }

    /// Validate the grid shape. Cheap structural check used by the
    /// admin handler and by `nexus-store::delivery_settings_put`
    /// before persisting. Operators sending a malformed grid get a
    /// 400, not a corrupt row.
    pub fn validate(&self) -> Result<(), TypesError> {
        if self.grid.len() != Self::DAYS {
            return Err(TypesError::InvalidSchedule(format!(
                "grid outer length must be {} (one per day), got {}",
                Self::DAYS,
                self.grid.len()
            )));
        }
        for (i, row) in self.grid.iter().enumerate() {
            if row.len() != Self::SLOTS_PER_DAY {
                return Err(TypesError::InvalidSchedule(format!(
                    "grid[{i}] length must be {} (half-hour slots), got {}",
                    Self::SLOTS_PER_DAY,
                    row.len()
                )));
            }
        }
        Ok(())
    }

    /// Resolve `(weekday_from_monday, half_hour_slot)` → allow.
    /// Returns `false` when indices are out of range — the policy
    /// treats malformed lookups as suppressed for safety.
    pub fn is_allowed(&self, day_from_monday: usize, slot: usize) -> bool {
        self.grid
            .get(day_from_monday)
            .and_then(|row| row.get(slot))
            .copied()
            .unwrap_or(false)
    }
}

/// Global delivery configuration — one row in `delivery_settings`.
///
/// `timezone` is an IANA name (`"America/Los_Angeles"`, `"UTC"`,
/// …). The default at install time is the host's timezone, falling
/// back to `"UTC"` if detection fails; operators can change it via
/// `PUT /api/v1/admin/delivery`. Bare-bones validation lives here;
/// the store layer adds the timezone-parses check via `chrono-tz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverySettings {
    /// Master kill switch. `false` suppresses every outbox row
    /// with `SuppressionReason::GlobalDisabled` — local recording
    /// continues regardless.
    pub enabled: bool,
    /// Optional weekly schedule. `None` means "no schedule" (every
    /// time is allowed, subject only to `enabled`).
    pub schedule: Option<DeliverySchedule>,
    /// IANA timezone name (e.g. `"America/Los_Angeles"`).
    pub timezone: String,
    /// Last mutation timestamp. Set by the store on each put.
    pub updated_at: DateTime<Utc>,
}

impl Default for DeliverySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
            timezone: "UTC".to_string(),
            updated_at: chrono::Utc::now(),
        }
    }
}

/// Per-rule delivery override — stored in `rules.delivery_policy_json`.
///
/// Cascade semantics (from `docs/M7_DELIVERY.md`):
///
/// - When the column is `NULL` (no policy set), the rule inherits
///   the global `DeliverySettings` verbatim.
/// - When `schedule` is `Some`, it **replaces** (does not
///   intersect) the global schedule — an "open" rule schedule
///   delivers even during a "closed" global slot.
/// - `enabled = false` at the rule level is "this rule's matches
///   never get delivered." Local recording continues; the global
///   schedule is irrelevant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDeliveryPolicy {
    pub enabled: bool,
    /// `None` = no per-rule schedule (inherit global schedule).
    /// `Some` = use this schedule, ignoring the global schedule.
    #[serde(default)]
    pub schedule: Option<DeliverySchedule>,
}

impl Default for RuleDeliveryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
        }
    }
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
    #[error("invalid delivery schedule: {0}")]
    InvalidSchedule(String),
    #[error("invalid role: {0:?} (expected admin|operator|viewer)")]
    InvalidRole(String),
    #[error("invalid codec: {0:?} (expected h264|h264_plus|h265|h265_plus)")]
    InvalidCodec(String),
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// Video codec carried by a camera's RTSP stream.
///
/// The `_plus` variants are operator-selected labels for vendor SVC
/// configurations (Hikvision H.264+/H.265+, Dahua Smart Codec, Uniview
/// U-Code). Autodetect (ONVIF / SDP) never produces a `_plus` variant —
/// SVC is wire-compatible with the base codec, so the depayloader,
/// parser, and decoder are identical. `.base()` collapses to the wire
/// label the cloud and the pipeline care about (`"h264"` or `"h265"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub enum CodecKind {
    H264,
    H264Plus,
    H265,
    H265Plus,
}

impl CodecKind {
    /// Wire label for cloud envelopes and GStreamer element prefixes
    /// (`rtph{base}depay`, `h{base}parse`, `avdec_h{base}`).
    pub fn base(self) -> &'static str {
        match self {
            CodecKind::H264 | CodecKind::H264Plus => "h264",
            CodecKind::H265 | CodecKind::H265Plus => "h265",
        }
    }

    /// All variants in display order; used by UI dropdowns and tests.
    pub fn all() -> [CodecKind; 4] {
        [
            CodecKind::H264,
            CodecKind::H264Plus,
            CodecKind::H265,
            CodecKind::H265Plus,
        ]
    }
}

impl std::fmt::Display for CodecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CodecKind::H264 => "h264",
            CodecKind::H264Plus => "h264_plus",
            CodecKind::H265 => "h265",
            CodecKind::H265Plus => "h265_plus",
        })
    }
}

impl std::str::FromStr for CodecKind {
    type Err = TypesError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "h264" => Ok(CodecKind::H264),
            "h264_plus" => Ok(CodecKind::H264Plus),
            "h265" => Ok(CodecKind::H265),
            "h265_plus" => Ok(CodecKind::H265Plus),
            other => Err(TypesError::InvalidCodec(other.to_string())),
        }
    }
}

/// Operator's choice on the camera-create form. `Auto` defers to the
/// edge admin API's RTSP DESCRIBE probe; a concrete `Kind` is honoured
/// as-is. Untagged so JSON `"auto"` and `"h264"` both round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodecSelector {
    /// String literal `"auto"` on the wire.
    Auto(AutoTag),
    /// One of the four `CodecKind` snake_case literals.
    Kind(CodecKind),
}

/// One-variant helper enum so [`CodecSelector::Auto`] only accepts the
/// exact string `"auto"` (and nothing else like `"AUTO"` or `"Auto"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoTag {
    Auto,
}

/// How the edge admin API resolved a camera's codec. Returned on
/// create/update responses so the UI can surface a warning banner when
/// autodetect failed and we fell back to a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/api/types/")
)]
pub enum CodecSource {
    /// Operator picked a specific codec.
    Operator,
    /// Resolved by RTSP DESCRIBE / ONVIF probe.
    Autodetect,
    /// Probe failed and we defaulted to H.264.
    DefaultFallback,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_perfect_overlap() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        assert!((a.iou(&a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn iou_no_overlap() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        let b = BBox {
            x1: 20.0,
            y1: 20.0,
            x2: 30.0,
            y2: 30.0,
        };
        assert_eq!(a.iou(&b), 0.0);
    }

    #[test]
    fn bbox_center_and_area() {
        let a = BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 4.0,
            y2: 6.0,
        };
        assert_eq!(a.area(), 24.0);
        assert_eq!(a.center(), (2.0, 3.0));
    }

    #[test]
    fn schedule_always_and_never() {
        let s = DeliverySchedule::always();
        s.validate().unwrap();
        assert!(s.is_allowed(0, 0));
        assert!(s.is_allowed(6, 47));

        let n = DeliverySchedule::never();
        n.validate().unwrap();
        assert!(!n.is_allowed(0, 0));
        assert!(!n.is_allowed(3, 24));
    }

    #[test]
    fn schedule_validate_rejects_bad_shape() {
        let too_few_days = DeliverySchedule {
            grid: vec![vec![true; 48]; 6],
        };
        assert!(too_few_days.validate().is_err());

        let too_few_slots = DeliverySchedule {
            grid: vec![vec![true; 47]; 7],
        };
        assert!(too_few_slots.validate().is_err());
    }

    #[test]
    fn schedule_is_allowed_oob_is_false() {
        // The policy treats malformed lookups as suppressed for safety.
        let s = DeliverySchedule::always();
        assert!(!s.is_allowed(7, 0));
        assert!(!s.is_allowed(0, 48));
    }

    #[test]
    fn schedule_round_trip_json() {
        let mut s = DeliverySchedule::always();
        s.grid[2][10] = false;
        s.grid[5][47] = false;
        let json = serde_json::to_string(&s).unwrap();
        let back: DeliverySchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn rule_policy_default_inherits() {
        let p = RuleDeliveryPolicy::default();
        assert!(p.enabled);
        assert!(p.schedule.is_none());
    }

    // -----------------------------------------------------------------------
    // M6 — Role enum
    // -----------------------------------------------------------------------

    #[test]
    fn role_string_round_trips_serde() {
        use std::str::FromStr;
        for r in Role::all() {
            // Display + as_str + serde lowercase all agree.
            let s = r.as_str();
            assert_eq!(format!("{r}"), s);
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{s}\""));
            // round-trip both directions
            assert_eq!(Role::from_str(s).unwrap(), r);
            let back: Role = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn role_from_str_rejects_unknown() {
        use std::str::FromStr;
        let err = Role::from_str("superadmin").unwrap_err();
        assert!(matches!(err, TypesError::InvalidRole(ref s) if s == "superadmin"));
    }

    #[test]
    fn role_satisfies_is_total_order() {
        // admin >= operator >= viewer.
        assert!(Role::Admin.satisfies(Role::Admin));
        assert!(Role::Admin.satisfies(Role::Operator));
        assert!(Role::Admin.satisfies(Role::Viewer));
        assert!(Role::Operator.satisfies(Role::Operator));
        assert!(Role::Operator.satisfies(Role::Viewer));
        assert!(!Role::Operator.satisfies(Role::Admin));
        assert!(Role::Viewer.satisfies(Role::Viewer));
        assert!(!Role::Viewer.satisfies(Role::Operator));
        assert!(!Role::Viewer.satisfies(Role::Admin));
    }

    #[test]
    fn role_levels_have_headroom() {
        // We rely on gaps so a future `analyst` (30) or
        // `superadmin` (200) doesn't force renumbering.
        let levels: Vec<u32> = Role::all().iter().map(|r| r.level()).collect();
        assert_eq!(levels, vec![10, 50, 100]);
        // Strictly increasing.
        for w in levels.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn role_all_in_ascending_order() {
        let a = Role::all();
        assert_eq!(a, [Role::Viewer, Role::Operator, Role::Admin]);
    }

    #[test]
    fn role_case_sensitive_from_str() {
        use std::str::FromStr;
        // Wire format is lowercase by spec; anything else is a
        // wire-protocol bug we want to surface loudly.
        assert!(Role::from_str("Admin").is_err());
        assert!(Role::from_str("ADMIN").is_err());
        assert!(Role::from_str("").is_err());
    }

    // -----------------------------------------------------------------------
    // Codec
    // -----------------------------------------------------------------------

    #[test]
    fn codec_round_trip_serde() {
        use std::str::FromStr;
        for c in CodecKind::all() {
            let s = c.to_string();
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"{s}\""));
            assert_eq!(CodecKind::from_str(&s).unwrap(), c);
            let back: CodecKind = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn codec_base_collapses_plus_variants() {
        assert_eq!(CodecKind::H264.base(), "h264");
        assert_eq!(CodecKind::H264Plus.base(), "h264");
        assert_eq!(CodecKind::H265.base(), "h265");
        assert_eq!(CodecKind::H265Plus.base(), "h265");
    }

    #[test]
    fn codec_from_str_rejects_unknown() {
        use std::str::FromStr;
        assert!(CodecKind::from_str("H264").is_err());
        assert!(CodecKind::from_str("hevc").is_err());
        assert!(CodecKind::from_str("av1").is_err());
        assert!(CodecKind::from_str("").is_err());
    }

    #[test]
    fn codec_selector_round_trips() {
        let auto: CodecSelector = serde_json::from_str("\"auto\"").unwrap();
        assert!(matches!(auto, CodecSelector::Auto(AutoTag::Auto)));
        assert_eq!(serde_json::to_string(&auto).unwrap(), "\"auto\"");

        let h265: CodecSelector = serde_json::from_str("\"h265\"").unwrap();
        assert!(matches!(h265, CodecSelector::Kind(CodecKind::H265)));
        assert_eq!(serde_json::to_string(&h265).unwrap(), "\"h265\"");

        assert!(serde_json::from_str::<CodecSelector>("\"unknown\"").is_err());
    }
}
