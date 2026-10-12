//! Tracker

use crate::static_filter::StaticObjectConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    #[serde(default)]
    pub backend: TrackerBackendKind,
    #[serde(default = "default_track_ttl_ms")]
    pub track_ttl_ms: u64,
    #[serde(default = "default_iou_threshold")]
    pub iou_threshold: f32,
    /// ByteTrack-specific tuning. Ignored when `backend != Bytetrack`.
    /// All fields default to v1 (`event_filter.cpp`) values so a config
    /// that simply flips `backend = "bytetrack"` runs at v1 parity
    /// without further keys.
    #[serde(default)]
    pub bytetrack: ByteTrackConfig,
    /// Track annotator tuning (motion/dwell/zone/group attributes).
    /// All fields default to v1 (`track_annotator.hpp`) values.
    #[serde(default)]
    pub annotator: AnnotatorConfig,
    /// Static-object filter tuning (parked-vehicle suppression).
    /// All fields default to v1 (`event_filter.cpp`) values. Activated
    /// per-camera via `cameras[*].parking_lot_mode = true`.
    #[serde(default)]
    pub static_object: StaticObjectConfig,
}

// Hand-written so `Default` agrees with the `#[serde(default = "...")]`
// fallbacks above. The derive would zero everything (track_ttl_ms = 0,
// iou_threshold = 0.0), which silently breaks the IoU tracker because every
// active track expires immediately on the next update.
//
// This is the canonical example of the pattern; the same fix is applied to
// every other Config substruct in this file that uses
// `#[serde(default = "fn")]`. New substructs MUST follow the same rule:
// either no per-field default fns (so derive is correct) or a hand-written
// `impl Default` that calls the same fns serde uses.
impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            backend: TrackerBackendKind::default(),
            track_ttl_ms: default_track_ttl_ms(),
            iou_threshold: default_iou_threshold(),
            bytetrack: ByteTrackConfig::default(),
            annotator: AnnotatorConfig::default(),
            static_object: StaticObjectConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerBackendKind {
    IouNaive,
    #[default]
    Bytetrack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteTrackConfig {
    /// Detections at or above this confidence enter the first-pass
    /// association. v1 default: 0.5.
    #[serde(default = "default_bytetrack_high_confidence")]
    pub high_confidence: f32,
    /// Detections in `[low_confidence, high_confidence)` enter the
    /// second-pass recovery match. v1 default: 0.1.
    #[serde(default = "default_bytetrack_low_confidence")]
    pub low_confidence: f32,
    /// Minimum IoU for a (track, detection) to be considered the same
    /// object during association. v1 default: 0.3.
    #[serde(default = "default_bytetrack_match_iou_threshold")]
    pub match_iou_threshold: f32,
    /// Frames a confirmed/lost track may go without a match before being
    /// retired. v1 default: 30.
    #[serde(default = "default_bytetrack_max_lost_frames")]
    pub max_lost_frames: u32,
    /// Hit streak required for a tentative track to be promoted to
    /// confirmed. v1 default: 1 (promote on first hit — keeps event
    /// suppression off when detections are intermittent).
    #[serde(default = "default_bytetrack_confirm_frames")]
    pub confirm_frames: u32,
    /// Frames a tentative (still-unconfirmed) track may go without a
    /// match before being culled. v1 default: 3.
    #[serde(default = "default_bytetrack_tentative_max_missed_frames")]
    pub tentative_max_missed_frames: u32,
    /// EMA blend factor for the smoothed display bbox. New box weighs
    /// `alpha`, prior smoothed box weighs `1 - alpha`. v1 default: 0.6.
    #[serde(default = "default_bytetrack_display_smoothing_alpha")]
    pub display_smoothing_alpha: f32,
    /// Fallback for a track no detection overlapped: link the nearest
    /// same-label detection whose centre lies within this many box lengths
    /// (the box's larger side) per second since the track's last match.
    /// Keeps a moving vehicle on one track when the analysis rate is too low
    /// for its consecutive boxes to overlap (#362). `0` disables it.
    #[serde(default = "default_bytetrack_motion_match_box_lengths_per_sec")]
    pub motion_match_box_lengths_per_sec: f32,
    /// Spatial-bucket cell size (px) for the `associate_pass` neighbour
    /// search (Phase M_PERF_CROWD C3/C1). `None` or `Some(0)` preserves
    /// the original O(N²) sweep; `Some(n)` builds a grid over the
    /// detection centres and walks only the 3×3 neighbourhood per
    /// track. Safe when `n >= max_velocity_per_frame + half_max_bbox_dim`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spatial_bucket_size_px: Option<u32>,
}

impl Default for ByteTrackConfig {
    fn default() -> Self {
        Self {
            high_confidence: default_bytetrack_high_confidence(),
            low_confidence: default_bytetrack_low_confidence(),
            match_iou_threshold: default_bytetrack_match_iou_threshold(),
            max_lost_frames: default_bytetrack_max_lost_frames(),
            confirm_frames: default_bytetrack_confirm_frames(),
            tentative_max_missed_frames: default_bytetrack_tentative_max_missed_frames(),
            display_smoothing_alpha: default_bytetrack_display_smoothing_alpha(),
            motion_match_box_lengths_per_sec: default_bytetrack_motion_match_box_lengths_per_sec(),
            spatial_bucket_size_px: None,
        }
    }
}

fn default_track_ttl_ms() -> u64 {
    2_000
}
fn default_iou_threshold() -> f32 {
    0.3
}
fn default_bytetrack_high_confidence() -> f32 {
    0.5
}
fn default_bytetrack_low_confidence() -> f32 {
    0.1
}
fn default_bytetrack_match_iou_threshold() -> f32 {
    0.3
}
fn default_bytetrack_max_lost_frames() -> u32 {
    30
}
fn default_bytetrack_confirm_frames() -> u32 {
    1
}
fn default_bytetrack_tentative_max_missed_frames() -> u32 {
    3
}
fn default_bytetrack_display_smoothing_alpha() -> f32 {
    0.6
}
fn default_bytetrack_motion_match_box_lengths_per_sec() -> f32 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotatorConfig {
    /// Speed (px/sec) at or above which a non-vehicle track is classified
    /// `walking`. Below = `stationary`. v1 default: 30.0.
    #[serde(default = "default_annotator_speed_walking_px_per_sec")]
    pub speed_walking_px_per_sec: f32,
    /// Speed (px/sec) at or above which a non-vehicle track becomes
    /// `running`. v1 default: 120.0.
    #[serde(default = "default_annotator_speed_running_px_per_sec")]
    pub speed_running_px_per_sec: f32,
    /// Speed (px/sec) at or above which a `vehicle.*` label becomes
    /// `vehicle_speed`. v1 default: 250.0.
    #[serde(default = "default_annotator_speed_vehicle_px_per_sec")]
    pub speed_vehicle_px_per_sec: f32,
    /// Px/frame EMA threshold below which a vehicle track accumulates
    /// "parked" frames. v1 default: 1.5.
    #[serde(default = "default_annotator_parked_ema_threshold_px")]
    pub parked_ema_threshold_px: f32,
    /// Frames a vehicle track must stay below `parked_ema_threshold_px`
    /// before `motion.parked_vehicle = "yes"`. Unset (default): derived
    /// from `parked_min_secs` at the camera's measured inference rate.
    /// Set to pin a frame count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_min_frames_to_flag: Option<u32>,
    /// Wall-clock parked time used when `parked_min_frames_to_flag` is
    /// unset. Default: 1 s.
    #[serde(default = "default_annotator_parked_min_secs")]
    pub parked_min_secs: f32,
    /// Direction (px/sec EMA magnitude) below which `motion.direction`
    /// is reported as `"none"`. v1 default: 8.0.
    #[serde(default = "default_annotator_direction_min_px_per_sec")]
    pub direction_min_px_per_sec: f32,
    /// EMA factor for the per-track movement signal (px/frame). Higher
    /// = more reactive, lower = more smoothing. v1 default: 0.30.
    #[serde(default = "default_annotator_movement_ema_alpha")]
    pub movement_ema_alpha: f32,
    /// EMA factor for the per-track direction (dx, dy) signal. v1
    /// default: 0.50 (more reactive than the speed EMA).
    #[serde(default = "default_annotator_direction_ema_alpha")]
    pub direction_ema_alpha: f32,
    /// Group-size search radius as a multiple of this track's bbox
    /// half-perimeter. Same-label tracks within the radius are counted.
    /// v1 default: 2.5.
    #[serde(default = "default_annotator_group_radius_box_multiplier")]
    pub group_radius_box_multiplier: f32,
    /// Frames an annotator may keep stale per-track state after the
    /// track was last observed. Generous on purpose so it outlives
    /// lost-track recovery. v1 default: 600 (~20 s @ 30 fps).
    #[serde(default = "default_annotator_stale_state_frames")]
    pub stale_state_frames: u32,
    /// Phase M_PERF_CROWD C2 opt-in: cell size (px) for spatial
    /// bucketing the group-size pre-pass. `None` (default) → naive
    /// `Vec<(f32, f32)>` per-label scan (preserves bit-identical
    /// historical behaviour). `Some(n)` with `n > 0` builds a per-label
    /// `HashMap<(GridX, GridY), Vec<(f32, f32)>>` and the per-track
    /// loop iterates only the cells the bbox `radius` overlaps. Bucket
    /// size must satisfy `n ≥ max plausible radius` for any track at
    /// runtime; otherwise the cell walk may miss neighbours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_spatial_bucket_size_px: Option<u32>,
    /// Phase 8.1 — non-vehicle object classes (lowercased detector
    /// labels) the static-object filter should also promote to
    /// persistent anchors, so equipment like `ladder`, `wheelbarrow`,
    /// `scissor lift` joins `vehicle` in the registry. Empty (default)
    /// preserves vehicle-only static-anchor behaviour. Drives the
    /// `motion.removed_anchor_ids` / `motion.carrying_anchor_label`
    /// attributes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub static_anchor_classes: Vec<String>,
    /// Phase 8.1 — detector labels (lowercased) treated as "tools" for
    /// the `motion.tool_in_proximity_*` attributes (e.g. `crowbar`,
    /// `hammer`, `bolt cutter`). Empty (default) disables tool
    /// proximity stamping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_proximity_labels: Vec<String>,
    /// Phase 8.1 — proximity search radius (multiple of the moving
    /// track's bbox half-perimeter) for `motion.near_static_vehicle_*`.
    /// Default 1.5.
    #[serde(default = "default_annotator_proximity_radius_box_multiplier")]
    pub proximity_radius_box_multiplier: f32,
    /// Phase 8.1 — proximity radius (multiple of the person's bbox
    /// half-perimeter) for `motion.tool_in_proximity_*`. Default 1.0.
    #[serde(default = "default_annotator_tool_proximity_radius_box_multiplier")]
    pub tool_proximity_radius_box_multiplier: f32,
}

impl Default for AnnotatorConfig {
    fn default() -> Self {
        Self {
            speed_walking_px_per_sec: default_annotator_speed_walking_px_per_sec(),
            speed_running_px_per_sec: default_annotator_speed_running_px_per_sec(),
            speed_vehicle_px_per_sec: default_annotator_speed_vehicle_px_per_sec(),
            parked_ema_threshold_px: default_annotator_parked_ema_threshold_px(),
            parked_min_frames_to_flag: None,
            parked_min_secs: default_annotator_parked_min_secs(),
            direction_min_px_per_sec: default_annotator_direction_min_px_per_sec(),
            movement_ema_alpha: default_annotator_movement_ema_alpha(),
            direction_ema_alpha: default_annotator_direction_ema_alpha(),
            group_radius_box_multiplier: default_annotator_group_radius_box_multiplier(),
            stale_state_frames: default_annotator_stale_state_frames(),
            group_spatial_bucket_size_px: None,
            static_anchor_classes: Vec::new(),
            tool_proximity_labels: Vec::new(),
            proximity_radius_box_multiplier: default_annotator_proximity_radius_box_multiplier(),
            tool_proximity_radius_box_multiplier:
                default_annotator_tool_proximity_radius_box_multiplier(),
        }
    }
}

fn default_annotator_speed_walking_px_per_sec() -> f32 {
    30.0
}
fn default_annotator_speed_running_px_per_sec() -> f32 {
    120.0
}
fn default_annotator_speed_vehicle_px_per_sec() -> f32 {
    250.0
}
fn default_annotator_parked_ema_threshold_px() -> f32 {
    1.5
}
fn default_annotator_parked_min_secs() -> f32 {
    1.0
}
fn default_annotator_direction_min_px_per_sec() -> f32 {
    8.0
}
fn default_annotator_movement_ema_alpha() -> f32 {
    0.30
}
fn default_annotator_direction_ema_alpha() -> f32 {
    0.50
}
fn default_annotator_group_radius_box_multiplier() -> f32 {
    2.5
}
fn default_annotator_stale_state_frames() -> u32 {
    600
}
fn default_annotator_proximity_radius_box_multiplier() -> f32 {
    1.5
}
fn default_annotator_tool_proximity_radius_box_multiplier() -> f32 {
    1.0
}
