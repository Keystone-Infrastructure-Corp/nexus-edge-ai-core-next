//! Static-object filter (v1 EventFilter::staticVehicle*)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticObjectConfig {
    /// Frames a vehicle track must dwell below
    /// `significant_movement_pixels` (EMA-smoothed) before promoting
    /// to "static" and being suppressed from the rule eval slice.
    /// Unset (default): derived from `dwell_secs` at the camera's
    /// measured inference rate. Set to pin a frame count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_frames: Option<u32>,
    /// Wall-clock dwell used when `dwell_frames` is unset. Default: 5 s,
    /// so on a ~1.9 fps Hailo-8 camera a parked vehicle promotes (~10
    /// frames) before ByteTrack retires its track (#335).
    #[serde(default = "default_static_object_dwell_secs")]
    pub dwell_secs: f32,
    /// Also promote stationary `person` tracks to anchors, so a phantom
    /// stationary person (e.g. a misdetected gym machine) stops looping
    /// alerts on a `parking_lot_mode` camera. Opt-in per site: a
    /// stationary person is suppressed until they move. Default: false.
    #[serde(default)]
    pub anchor_persons: bool,
    /// Px-EMA threshold above which a static track is considered
    /// "moving again". v1 default: 36.
    #[serde(default = "default_static_object_significant_movement_pixels")]
    pub significant_movement_pixels: u32,
    /// Consecutive moving frames required to demote a previously
    /// promoted track and erase its persistent anchor. v1 default: 3.
    #[serde(default = "default_static_object_significant_movement_frames")]
    pub significant_movement_frames: u32,
    /// EMA blend factor for the per-track movement signal. New value
    /// weighs `alpha`, prior smoothed value weighs `1 - alpha`. v1
    /// default: 0.35.
    #[serde(default = "default_static_object_movement_ema_alpha")]
    pub movement_ema_alpha: f32,
    /// Pixel radius for matching a fresh observation to an existing
    /// persistent anchor. v1 default: 40.
    #[serde(default = "default_static_object_match_distance_pixels")]
    pub match_distance_pixels: u32,
    /// Pixel jump above which the per-track FSM state is wiped on the
    /// assumption that the upstream tracker has recycled this
    /// `track_id` onto a different physical object. Without this the
    /// new vehicle inherits the previous track's `static_promoted`
    /// flag and gets suppressed despite never having been parked.
    /// Set to `0` to disable the guard. Default: 60.
    #[serde(default = "default_static_object_track_id_reuse_reset_pixels")]
    pub track_id_reuse_reset_pixels: u32,
    /// When true, write/load the per-camera anchor registry to disk
    /// under `runtime.state_dir`. v1 default: true.
    #[serde(default = "crate::inference::default_true")]
    pub persistence_enabled: bool,
    /// Time-to-live for a persisted anchor with no matching observation.
    /// Each frame that produces a vehicle track within `match_distance_pixels`
    /// of an anchor refreshes its `last_seen_unix_ms`; once an anchor goes
    /// untouched for `anchor_ttl_secs` (measured against the frame's own
    /// `captured_at`, so it works equally well across long offline periods),
    /// the filter prunes it from the registry. Fixes the “stale anchor
    /// keeps haunting the live viewer after the parked car drove off-screen”
    /// failure mode that demotion-on-resumed-motion can't cover. v1 default:
    /// 3600 (one hour). Set to `0` to disable the sweep entirely.
    #[serde(default = "default_static_object_anchor_ttl_secs")]
    pub anchor_ttl_secs: u32,
}

impl Default for StaticObjectConfig {
    fn default() -> Self {
        Self {
            dwell_frames: None,
            dwell_secs: default_static_object_dwell_secs(),
            anchor_persons: false,
            significant_movement_pixels: default_static_object_significant_movement_pixels(),
            significant_movement_frames: default_static_object_significant_movement_frames(),
            movement_ema_alpha: default_static_object_movement_ema_alpha(),
            match_distance_pixels: default_static_object_match_distance_pixels(),
            track_id_reuse_reset_pixels: default_static_object_track_id_reuse_reset_pixels(),
            persistence_enabled: true,
            anchor_ttl_secs: default_static_object_anchor_ttl_secs(),
        }
    }
}

fn default_static_object_dwell_secs() -> f32 {
    5.0
}
fn default_static_object_significant_movement_pixels() -> u32 {
    36
}
fn default_static_object_significant_movement_frames() -> u32 {
    3
}
fn default_static_object_movement_ema_alpha() -> f32 {
    0.35
}
fn default_static_object_match_distance_pixels() -> u32 {
    40
}
fn default_static_object_track_id_reuse_reset_pixels() -> u32 {
    60
}
fn default_static_object_anchor_ttl_secs() -> u32 {
    3600
}
