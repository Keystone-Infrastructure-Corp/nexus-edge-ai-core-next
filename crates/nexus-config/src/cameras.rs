//! Cameras

use crate::inference::ModelConfig;
use nexus_types::{CameraId, CodecKind, VisualPromptId};
use serde::{Deserialize, Serialize};
use url::Url;

/// Ingest plumbing for a camera — the bits the supervisor and the
/// source backend need to actually pull frames. Grouped so adding
/// a new ingest knob (e.g. transport hints, auth) lands in one
/// place and helpers that only need ingest can take `&Ingest`
/// instead of `&CameraConfig`.
///
/// Serialised flat into `CameraConfig` via `#[serde(flatten)]`,
/// so the wire shape — every TOML in `config/`, every payload the
/// admin UI sends — is unchanged. The nested Rust type is purely
/// an organisational refactor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraIngest {
    pub url: Url,
    /// Lower-resolution stream the *analysis* tap decodes, when the
    /// camera offers one. `None` means analysis shares the main
    /// session, which is the pre-SPEC-069 behaviour and stays the
    /// fallback whenever a second session cannot be established.
    ///
    /// Never an alternate source for [`Self::url`]: recording, the
    /// pre-roll ring and HD live view read the main stream and only
    /// the main stream (SPEC-069 invariants I1–I5). Carries
    /// credentials exactly like `url` and is redacted in every place
    /// `url` is — the roster envelope, the support bundle, the audit
    /// log (REPO_BOUNDARY R5b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_url: Option<Url>,
    #[serde(default = "crate::inference::default_true")]
    pub enabled: bool,
    /// Per-camera FPS cap. 0 = unbounded.
    #[serde(default)]
    pub max_fps: u32,
    /// Video codec carried by the RTSP stream. `None` means
    /// "unknown — let the pipeline default to H.264 and warn at
    /// spawn". Populated by the admin API's autodetect (RTSP
    /// DESCRIBE / ONVIF Media) at camera-create time, or
    /// hand-picked by the operator. The `_plus` variants are
    /// vendor SVC labels (Hikvision H.264+/H.265+, Dahua Smart
    /// Codec, Uniview U-Code); autodetect never emits them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<CodecKind>,
}

/// ONVIF device-control endpoint + credentials for a camera.
///
/// Powers Phase 7.6 device control (PTZ / imaging / device / etc.):
/// the cloud console issues live ONVIF commands *through the edge
/// tunnel*, but the credentials themselves are **edge-resident only**
/// — AGENTS.md Rule 6 / REPO_BOUNDARY R5b. They ride the existing
/// `cameras.config_json` blob (no SQLite schema change, no migration)
/// and MUST NEVER be serialized into the `camera_roster` envelope that
/// crosses the tunnel (the roster builder hand-picks metadata fields,
/// so this stays trivially redacted; a unit test asserts it).
///
/// Stored plaintext inside `config_json`, matching the existing
/// convention for RTSP credentials embedded in [`CameraIngest::url`].
///
/// `endpoint` is the verbatim WS-Discovery `XAddrs` device-service URL
/// (e.g. `http://192.168.1.64/onvif/device_service`); when a camera is
/// added from discovery it is populated from
/// [`crate`]'s discovered-device `onvif_xaddrs` so an ONVIF camera
/// needs no re-entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CameraOnvif {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl CameraOnvif {
    /// True when nothing is configured, so the blob serializes away
    /// cleanly via `skip_serializing_if` — keeps the `config_json`
    /// shape byte-identical for the (overwhelming) non-ONVIF majority.
    pub fn is_empty(&self) -> bool {
        self.endpoint.is_none() && self.username.is_none() && self.password.is_none()
    }
}

/// ONVIF talk-down (two-way audio / speaker) capability for a camera,
/// discovered via `GetAudioOutputs` and the RTSP backchannel SDP
/// (Phase 7.6.7). Edge-resident only — like [`CameraOnvif`] it never
/// crosses the tunnel into the `camera_roster` envelope, and it
/// serializes away when empty so non-speaker cameras keep an unchanged
/// `config_json`.
///
/// Carries no credential: the RTSP backchannel reuses the ingest URL's
/// userinfo, and the ONVIF probe reuses [`CameraOnvif`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CameraTalkDown {
    /// True when the camera advertises an audio output (speaker) the
    /// operator can talk down through.
    #[serde(default)]
    pub speaker_present: bool,
    /// Backchannel audio codec the camera expects (e.g. `PCMU`,
    /// `PCMA`, `G726`, `AAC`) as reported by the backchannel SDP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backchannel_codec: Option<String>,
    /// RTSP backchannel control URL the talk-down session streams audio
    /// to (the `a=control:` of the `sendonly` audio media line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backchannel_url: Option<String>,
}

impl CameraTalkDown {
    /// True when nothing is configured, so the block serializes away
    /// via `skip_serializing_if` and the `config_json` stays
    /// byte-identical for the non-speaker majority.
    pub fn is_empty(&self) -> bool {
        !self.speaker_present && self.backchannel_codec.is_none() && self.backchannel_url.is_none()
    }
}

/// Detector-side knobs — open-vocab prompts and model overrides.
/// Anything that changes WHAT the inference layer is asked to
/// look for, vs. CameraIngest which controls how frames get there.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CameraDetector {
    /// Open-vocab text prompts, or labels-of-interest for ensemble.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// M3.1 — visual-prompt references attached to this camera.
    /// Each entry pairs a stored reference-crop id (resolved against
    /// `runtime.visual_prompts_dir` + the `visual_prompts` table) with
    /// the human-facing label the detector should emit for matches
    /// (e.g. `"amazon_van"`). Only the YOLOE visual-mode detector
    /// reads this field; other backends ignore it.
    #[serde(default)]
    pub visual_prompts: Vec<VisualPromptRef>,
    /// Per-camera overrides for the inference model (kind, pack, thresholds).
    #[serde(default)]
    pub model_override: Option<ModelConfig>,
}

/// M3.1 — wire-shape reference to a stored visual prompt. Embedded in
/// [`CameraDetector::visual_prompts`] and fan-pushed inside
/// [`CameraConfigUpdate`]. The detector resolves `id` against the
/// `visual_prompts` table (migration 0012) to load the embedding,
/// then emits detections under `label` for every matching crop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VisualPromptRef {
    pub id: VisualPromptId,
    pub label: String,
}

/// Tracker / rules-pipeline behavior overrides — everything that
/// changes how the downstream pipeline reacts to detections, not
/// how detections get produced.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CameraBehavior {
    /// When true, this camera enables the static-object filter
    /// (`tracker.static_object.*`). Vehicles that promote to "static"
    /// are dropped from the rule-eval slice and persisted to the
    /// per-camera registry at `runtime.state_dir`. Default: false.
    #[serde(default)]
    pub parking_lot_mode: bool,
    /// SPEC-077 — the explicit parking stillness policy. `None` (default) is
    /// the legacy static filter and is never serialized, so an old core sees
    /// no new key. Requires `parking_lot_mode` and a [`ZoneKind::Parking`]
    /// zone ([`CameraConfig::validate_finite`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parking_policy_v2: Option<ParkingPolicyV2>,
    /// Per-camera override for `tracker.static_object.anchor_ttl_secs`.
    /// When `Some`, the supervisor replaces the global TTL with this
    /// value when constructing the camera's `StaticObjectFilter`.
    /// `None` means "inherit the engine default". Read once at supervisor
    /// start; an edit applies when the reconciler restarts the camera's
    /// supervisor for it (BUG-285).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_ttl_secs: Option<u32>,
    /// M_NATIVE_ASPECT — analysis (supervisor) frame width, decoupled
    /// from the detector input width. `None` (default) derives the
    /// supervisor width from the resolved detector input width. When set
    /// it MUST be a native-16:9 ladder rung (512 | 1024 | 1536)
    /// ≥ the detector input width: the engine analyses at this width and
    /// tiles it into model-sized tiles, giving exact 1:1 tiles with zero
    /// resampling (e.g. supervisor 1536×864 + model 512×288 → a 3×3 grid
    /// of 512×288 tiles). A value below the detector width is clamped up
    /// to it. Restart required (read once at supervisor start; the
    /// reconciler respawns when the supervisor dims change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_width: Option<u32>,
    /// M_PERF_CROWD Phase E1 — adaptive detector cadence under crowd.
    /// Threshold (number of currently-tracked objects, EMA-smoothed)
    /// at or above which `DetectorSkipPolicy` becomes active. When
    /// active, the detector runs only every `detector_skip_every_n_frames`
    /// gate-allowed frame; intervening frames advance the tracker with
    /// an empty detection slice so ByteTrack's `predict()` still ages
    /// tracks. Set together with `detector_skip_every_n_frames` to
    /// enable; either being `None` disables the policy. Defaults `None`
    /// → no skipping, preserves current behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_skip_crowded_threshold: Option<u32>,
    /// M_PERF_CROWD Phase E1 — companion to
    /// `detector_skip_crowded_threshold`. When the policy is active,
    /// detector runs on `frame_counter % n == 0` and is skipped
    /// otherwise. Typical value: 2 (halves detector cadence under
    /// crowd). Values `< 2` are coerced to "always run". `None`
    /// disables the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_skip_every_n_frames: Option<u32>,
    /// M_PERF_CROWD Phase E3 — adaptive detector input downscale
    /// under crowd. Threshold (number of currently-tracked objects,
    /// EMA-smoothed) at or above which the supervisor begins counting
    /// toward downscale. Same EMA shape as E1's
    /// `detector_skip_crowded_threshold` but tracked independently so
    /// operators can tune each knob without coupling. `None` disables
    /// the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_downscale_crowded_threshold: Option<u32>,
    /// M_PERF_CROWD Phase E3 — sustained-crowd hysteresis window.
    /// Crowd EMA must sit at or above
    /// `detector_downscale_crowded_threshold` continuously for this
    /// many seconds before the supervisor swaps to the low-res
    /// detector. The same window in reverse (EMA below threshold)
    /// triggers the swap back to the high-res detector. Set together
    /// with `detector_downscale_to_width` and
    /// `detector_downscale_to_height` to enable; any being `None`
    /// disables the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_downscale_sustained_secs: Option<u32>,
    /// M_PERF_CROWD Phase E3 — target detector input width while
    /// downscaled. The router pre-builds a second inference layer at
    /// `(camera's effective kind, detector_downscale_to_width,
    /// detector_downscale_to_height)` at startup. When the camera's
    /// hysteresis flips to downscaled, the supervisor picks this
    /// pre-built layer instead of the camera's normal detector.
    /// Typical pairing: high-res 960 → low-res 640, or 1280 →
    /// 960.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_downscale_to_width: Option<u32>,
    /// M_PERF_CROWD Phase E3 — companion to
    /// `detector_downscale_to_width`. See that field's doc for the
    /// composite semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_downscale_to_height: Option<u32>,
    /// M_PERF_CROWD Phase E2 — adaptive supervisor frame downscale
    /// under crowd. Threshold (number of currently-tracked objects,
    /// EMA-smoothed) at or above which the per-camera supervisor
    /// begins counting toward a *frame-level* downscale (sibling of
    /// E3's detector-level downscale). Tracked independently from E1
    /// / E3 so operators can tune each lever on its own. `None`
    /// disables the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_downscale_crowded_threshold: Option<u32>,
    /// M_PERF_CROWD Phase E2 — sustained-crowd window before the
    /// supervisor requests a fresh `PreRollIngester` RGB tap at
    /// `supervisor_downscale_to_width`. Typical value: 60.
    /// `supervisor_upscale_clear_secs` controls the (typically
    /// longer) clear-side window; when that field is `None` the
    /// hysteresis is symmetric and falls back to this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_downscale_sustained_secs: Option<u32>,
    /// M_PERF_CROWD Phase E2 — asymmetric clear-side hysteresis
    /// window. Crowd EMA must sit *below*
    /// `supervisor_downscale_crowded_threshold` continuously for this
    /// many seconds before the supervisor rebuilds the RGB tap back
    /// at high res. Typical value: 300 (5 min), longer than the
    /// 60s down-trigger because each rebuild closes any open clip
    /// and re-spawns the source. `None` reuses
    /// `supervisor_downscale_sustained_secs` (symmetric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_upscale_clear_secs: Option<u32>,
    /// M_PERF_CROWD Phase E2 — target *square* width for the RGB tap
    /// while in the downscaled state. Height is derived via
    /// [`nexus_pipeline::supervisor_frame_for`] (16:9, even). When
    /// the camera flips to downscaled the supervisor calls the
    /// recorder's `resize_camera_rgb_tap` with this width; the
    /// underlying `PreRollIngester` is rebuilt at the new dims and
    /// the supervisor re-spawns its `FrameSource` against the new
    /// shared RGB stream. Set together with the threshold +
    /// sustained-secs knobs above to enable. `None` disables.
    /// Typical pairing: 960 → 640 or 1280 → 960.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_downscale_to_width: Option<u32>,
    /// M_TILE_REINFER (G1) — opt-in cascaded re-inference on a small
    /// set of cropped sub-regions when the stage-1 detection set
    /// passes [`tile_trigger`]. `None` / `Some(false)` keeps the
    /// current single-shot pipeline. When `Some(true)`, the
    /// supervisor calls `nexus_pipeline::tile_executor::run_tile_inference`
    /// after stage-1 and merges the results before tracker / rules.
    /// Banned at config-load when the effective model is
    /// `kind == "ensemble"`: per-member tile budgeting is out of
    /// scope for v1 and the wedge plan documents the exclusion.
    /// Disabled at runtime on any tick where E1's detector-skip
    /// policy fires (no point re-inferring on a skipped frame).
    /// See `docs/edge-core/M_TILE_REINFER.md` (cloud).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_enabled: Option<bool>,
    /// M_TILE_REINFER (G1) — crowd EMA threshold at or above which
    /// the cascade engages. Set together with [`tile_enabled`]; both
    /// being `Some` is required to activate. `None` disables the
    /// trigger entirely (defensive — even if `tile_enabled` is
    /// `Some(true)`, an absent trigger keeps the cascade off so
    /// half-configured cameras don't quietly burn cycles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_trigger: Option<u32>,
    /// M_TILE_REINFER (G1) — per-frame cap on stage-2 invocations.
    /// `None` defaults to `3` at the call site (a 2×2 grid yields
    /// at most 4 cells; capping at 3 keeps the worst-case latency
    /// budget within one extra detector cost). Operators raise this
    /// to allow more thorough coverage on higher-power boxes, or lower
    /// it to throttle compute on lower-power ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_max_per_frame: Option<u32>,
    /// M_TILE_REINFER (G1) — grid preset (square-only to preserve
    /// 16:9 per cell). `None` defaults to `G2x2` at the call site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_grid: Option<TileGridConfig>,
    /// Discovery crops: row-major cell indices of [`tile_grid`] (`0` =
    /// top-left) that get a stage-2 pass on a schedule even when stage-1
    /// found nothing there. The density cascade above can only re-examine
    /// cells stage-1 already detected in, so it never finds a distant
    /// object stage-1 missed outright. Needs `tile_enabled = true`; works
    /// with or without `tile_trigger`. Only adds detail when
    /// [`supervisor_width`] is above the detector input width. Due cells
    /// are taken first out of `tile_max_per_frame`. Empty = off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tile_discovery_cells: Vec<u32>,
    /// Seconds after which a discovery cell is due again. `None` = 5.
    /// The real interval is this plus `ceil(cells / tile_max_per_frame)`
    /// analysed frames, so size it against the camera's analysed fps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_discovery_revisit_secs: Option<u32>,
    /// SPEC-089 — how recording activity is decided. `None` (default) is
    /// legacy and is never serialized, so an old core sees no new key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_policy: Option<RecordingPolicyConfig>,
    /// SPEC-089 — opaque token of the saved `recording_policy`, owned by the
    /// engine: the store assigns one when it creates a camera or first opens
    /// one saved without it, and a camera write that changes the policy
    /// assigns a new one. A write's own value is never kept. Empty, and never
    /// serialized, only before the store has assigned it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recording_policy_revision: String,
}

/// SPEC-077 `parking_policy_v2` of a camera ([`CameraBehavior::parking_policy_v2`]).
/// Every value is required: there is no v2 default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingPolicyV2 {
    /// Observed stillness a vehicle needs before it is promoted, in capture seconds.
    pub dwell_secs: f64,
    /// Highest net speed that still counts as standing, in box diagonals per second.
    pub max_speed_box_diagonals_per_sec: f64,
    /// Highest drift from the first observed centre, in box diagonals.
    pub max_displacement_box_diagonals: f64,
    /// Longest gap between observations that keeps a dwell going, in seconds.
    pub max_observation_gap_secs: f64,
}

/// M_TILE_REINFER (G1) — wire-shape tile grid preset.
///
/// Square grids only so each cell preserves the parent's 16:9
/// aspect ratio. Mirrored runtime-side as
/// `nexus_pipeline::tile::TileGridConfig`; the pipeline crate
/// provides the `From` adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileGridConfig {
    /// 2 rows × 2 cols = 4 tiles, each at half supervisor resolution.
    G2x2,
    /// 3 rows × 3 cols = 9 tiles, each at one-third supervisor resolution.
    G3x3,
}

/// SPEC-089 per-camera recording activity mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPolicyMode {
    /// Live track count plus alerts decides recording, as before SPEC-089.
    #[default]
    Legacy,
    /// Legacy writes footage; the fresh-evidence policy runs beside it, measured only.
    Shadow,
    /// The fresh-evidence policy decides recording.
    Observed,
}

/// SPEC-089 `recording_policy` of a camera ([`CameraBehavior::recording_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingPolicyConfig {
    #[serde(default)]
    pub mode: RecordingPolicyMode,
    /// No successful analysis for this long puts recording in degraded coverage.
    #[serde(
        default = "default_analysis_stale_after_ms",
        deserialize_with = "bounded_analysis_stale_after_ms"
    )]
    pub analysis_stale_after_ms: u64,
}

impl RecordingPolicyConfig {
    /// What a camera runs for `policy`: absent is legacy with the defaults, so
    /// writes compare policies through this.
    pub fn or_legacy(policy: Option<Self>) -> Self {
        policy.unwrap_or(Self {
            mode: RecordingPolicyMode::Legacy,
            analysis_stale_after_ms: default_analysis_stale_after_ms(),
        })
    }
}

fn default_analysis_stale_after_ms() -> u64 {
    10_000
}

/// Refused when decoded, so every camera write path rejects it. Above a
/// minute a dead detector could hide that long behind "quiet".
fn bounded_analysis_stale_after_ms<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<u64, D::Error> {
    let ms = u64::deserialize(d)?;
    if (1..=60_000).contains(&ms) {
        Ok(ms)
    } else {
        Err(serde::de::Error::custom(format!(
            "analysis_stale_after_ms must be 1..=60000, got {ms}"
        )))
    }
}

/// One configured camera. Wire shape (TOML + JSON) is flat — every
/// field of the nested groups (`ingest`, `detector`, `behavior`)
/// appears at the top level thanks to `#[serde(flatten)]`. The
/// nesting is purely a code-organisation refactor; existing
/// TOML and admin-API payloads remain bit-for-bit compatible.
///
/// Note: `#[serde(deny_unknown_fields)]` is intentionally omitted.
/// Serde does not support `deny_unknown_fields` together with
/// `#[serde(flatten)]` (the flattened keys can't be distinguished
/// from "unknown" at deserialise time). Operators who typo a
/// camera field will see the behaviour silently default instead of
/// hitting a load-time error; the trade-off is acceptable because
/// the structural ergonomics inside the engine matter more here
/// than catching field-name typos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    // Defaults to 0 so admin-API POST bodies (and any caller that
    // expects server-assigned ids) can omit the field. The
    // `create_camera` handler in `nexus-engine::api` force-zeros
    // this on insert anyway, so a missing id deserialises to the
    // same value the handler would assign — no behaviour change
    // for existing callers that send `id: 0` explicitly.
    #[serde(default)]
    pub id: CameraId,
    pub name: String,
    #[serde(flatten)]
    pub ingest: CameraIngest,
    #[serde(flatten)]
    pub detector: CameraDetector,
    #[serde(flatten)]
    pub behavior: CameraBehavior,
    /// ONVIF device-control endpoint + credentials (Phase 7.6).
    /// Edge-resident only — never crosses the tunnel. Defaults to
    /// empty and is skipped on serialize when empty, so existing
    /// non-ONVIF cameras keep an unchanged `config_json` shape.
    #[serde(default, skip_serializing_if = "CameraOnvif::is_empty")]
    pub onvif: CameraOnvif,
    /// ONVIF talk-down (speaker) capability (Phase 7.6.7). Edge-resident
    /// only — never crosses the tunnel. Skipped on serialize when empty.
    #[serde(default, skip_serializing_if = "CameraTalkDown::is_empty")]
    pub talk_down: CameraTalkDown,
    /// Polygon zones used by motion gate / dwell rules.
    #[serde(default)]
    pub zones: Vec<ZoneConfig>,
}

impl CameraConfig {
    /// Refuse a float that is not a finite number: `inf`, `nan`, or a number
    /// beyond f32's range, which parses to infinity. JSON has no literal for
    /// one, so `serde_json` stores it as `null`, and no later read can decode
    /// a camera row holding that: the engine cannot apply the camera's stored
    /// configuration, and every read of the whole camera list, such as the
    /// console's, fails. Every write that takes a camera from outside checks
    /// this first: the config file at load, and the admin API's and fleet
    /// apply's camera writes.
    ///
    /// The same writes also refuse a zone polygon the supervisor cannot apply
    /// as saved (SPEC-088): a vertex outside the normalized frame, or fewer
    /// than three vertices. It is refused, never clamped or reordered.
    ///
    /// SPEC-077: a `parking_policy_v2` needs `parking_lot_mode` and a parking
    /// zone, each of its values must be finite and above zero, and a parking
    /// zone needs the policy, so a save never silently falls back to legacy.
    pub fn validate_finite(&self) -> Result<(), String> {
        let parking_zone = self.zones.iter().any(|z| z.kind == ZoneKind::Parking);
        match &self.behavior.parking_policy_v2 {
            None if parking_zone => {
                return Err("a parking zone requires parking_policy_v2 in the same save".into())
            }
            None => {}
            Some(_) if !self.behavior.parking_lot_mode => {
                return Err("parking_policy_v2 requires parking_lot_mode = true".into())
            }
            Some(_) if !parking_zone => {
                return Err("parking_policy_v2 requires at least one parking zone".into())
            }
            Some(policy) => {
                let ParkingPolicyV2 {
                    dwell_secs,
                    max_speed_box_diagonals_per_sec,
                    max_displacement_box_diagonals,
                    max_observation_gap_secs,
                } = *policy;
                for (field, value) in [
                    ("dwell_secs", dwell_secs),
                    (
                        "max_speed_box_diagonals_per_sec",
                        max_speed_box_diagonals_per_sec,
                    ),
                    (
                        "max_displacement_box_diagonals",
                        max_displacement_box_diagonals,
                    ),
                    ("max_observation_gap_secs", max_observation_gap_secs),
                ] {
                    if !(value.is_finite() && value > 0.0) {
                        return Err(format!(
                            "parking_policy_v2.{field} must be a finite number above 0, got {value}"
                        ));
                    }
                }
            }
        }
        for zone in &self.zones {
            if zone
                .polygon
                .iter()
                .any(|(x, y)| !x.is_finite() || !y.is_finite())
            {
                return Err(format!(
                    "zone '{}' has a polygon vertex that is not a finite number",
                    zone.id
                ));
            }
            if zone
                .polygon
                .iter()
                .any(|(x, y)| !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y))
            {
                return Err(format!(
                    "zone '{}' has a polygon vertex outside the frame (0..1)",
                    zone.id
                ));
            }
            if zone.polygon.len() < 3 {
                return Err(format!(
                    "zone '{}' needs at least 3 polygon vertices, got {}",
                    zone.id,
                    zone.polygon.len()
                ));
            }
        }
        match &self.detector.model_override {
            Some(model) => model
                .validate_finite()
                .map_err(|why| format!("model_override {why}")),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    pub id: String,
    pub name: String,
    /// Polygon vertices in normalized (0..1) coordinates.
    pub polygon: Vec<(f32, f32)>,
    #[serde(default)]
    pub kind: ZoneKind,
    /// M_PERF_CROWD Phase B1 — per-zone minimum bbox area, in pixels of
    /// the supervisor analysis frame. When `Some(N)`, tracked objects
    /// whose centre lies inside this polygon are dropped if their bbox
    /// area is below `N`. Layered on top of the global
    /// [`ModelConfig::min_bbox_area_px`] (applied at the detector
    /// wrapper). Typical use: keep the global threshold low so a
    /// doorway zone with no override still admits tiny boxes, while
    /// non-doorway zones tighten the threshold to suppress distant
    /// noise. `None` = inherit the global threshold (no extra filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bbox_area_px_override: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ZoneKind {
    #[default]
    Inclusion,
    Exclusion,
    Dwell,
    /// SPEC-077 — parking stalls for `parking_policy_v2`. Never filters a
    /// detection, as an exclusion or as an inclusion.
    Parking,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_equality_includes_area_override() {
        let a = ZoneConfig {
            id: "z1".into(),
            name: "Excluded".into(),
            polygon: vec![(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        };
        let mut b = a.clone();
        b.min_bbox_area_px_override = Some(40);
        assert_ne!(a, b);
    }

    /// SPEC-089 — the flattened `recording_policy` key parses where every
    /// camera write decodes it, and a camera without one keeps its exact
    /// pre-SPEC-089 JSON, so an old core sees no new key.
    #[test]
    fn recording_policy_is_an_optional_top_level_camera_key() {
        let old = r#"{"id":7,"name":"Lot","url":"rtsp://10.0.0.5/stream","enabled":true,"max_fps":0,"prompts":[],"visual_prompts":[],"model_override":null,"parking_lot_mode":true,"zones":[]}"#;
        let camera: CameraConfig = serde_json::from_str(old).unwrap();
        assert_eq!(camera.behavior.recording_policy, None);
        assert_eq!(serde_json::to_string(&camera).unwrap(), old);

        let shadow = old.replace(
            r#""parking_lot_mode":true"#,
            r#""parking_lot_mode":true,"recording_policy":{"mode":"shadow"}"#,
        );
        let camera: CameraConfig = serde_json::from_str(&shadow).unwrap();
        let policy = camera.behavior.recording_policy.expect("parsed");
        assert_eq!(policy.mode, RecordingPolicyMode::Shadow);
        assert_eq!(policy.analysis_stale_after_ms, 10_000);
        let round_trip: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&camera).unwrap()).unwrap();
        assert_eq!(
            round_trip["recording_policy"],
            serde_json::json!({"mode": "shadow", "analysis_stale_after_ms": 10_000})
        );

        // The bound on the nested value still applies through the flatten.
        let invalid = old.replace(
            r#""zones":[]"#,
            r#""zones":[],"recording_policy":{"analysis_stale_after_ms":0}"#,
        );
        assert!(serde_json::from_str::<CameraConfig>(&invalid).is_err());
        let toml_camera: CameraConfig = toml::from_str(
            "name = \"c1\"\nurl = \"rtsp://example/cam\"\nrecording_policy = { mode = \"shadow\" }",
        )
        .unwrap();
        assert_eq!(
            toml_camera.behavior.recording_policy.map(|p| p.mode),
            Some(RecordingPolicyMode::Shadow)
        );
    }

    /// SPEC-089 — `recording_policy_revision` is a flattened top-level key a
    /// camera without one never serializes, and a policy compares through
    /// `or_legacy`, so absent and an explicit default legacy are one policy.
    #[test]
    fn the_recording_policy_revision_is_an_optional_top_level_key() {
        let old = r#"{"id":7,"name":"Lot","url":"rtsp://10.0.0.5/stream","enabled":true,"max_fps":0,"prompts":[],"visual_prompts":[],"model_override":null,"parking_lot_mode":true,"zones":[]}"#;
        let mut camera: CameraConfig = serde_json::from_str(old).unwrap();
        assert_eq!(camera.behavior.recording_policy_revision, "");
        assert_eq!(serde_json::to_string(&camera).unwrap(), old);

        camera.behavior.recording_policy_revision = "0a1b2c".into();
        let json = serde_json::to_value(&camera).unwrap();
        assert_eq!(json["recording_policy_revision"], "0a1b2c");
        let back: CameraConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.behavior.recording_policy_revision, "0a1b2c");

        let policy = |mode, analysis_stale_after_ms| RecordingPolicyConfig {
            mode,
            analysis_stale_after_ms,
        };
        let legacy = policy(RecordingPolicyMode::Legacy, 10_000);
        assert_eq!(RecordingPolicyConfig::or_legacy(None), legacy);
        assert_eq!(RecordingPolicyConfig::or_legacy(Some(legacy)), legacy);
        for other in [
            policy(RecordingPolicyMode::Legacy, 5_000),
            policy(RecordingPolicyMode::Shadow, 10_000),
            policy(RecordingPolicyMode::Observed, 10_000),
        ] {
            assert_ne!(RecordingPolicyConfig::or_legacy(Some(other)), legacy);
        }
    }

    /// Every outside camera write (config load, the admin API's PUT and POST)
    /// calls `validate_finite`, so a polygon it refuses never reaches the
    /// store or a supervisor.
    #[test]
    fn a_zone_polygon_the_supervisor_cannot_apply_is_refused() {
        let camera = |polygon: Vec<(f32, f32)>| -> CameraConfig {
            let mut cam: CameraConfig =
                toml::from_str("name = \"c1\"\nurl = \"rtsp://example/cam\"").unwrap();
            cam.zones = vec![ZoneConfig {
                id: "z1".into(),
                name: "door".into(),
                polygon,
                kind: ZoneKind::Exclusion,
                min_bbox_area_px_override: None,
            }];
            cam
        };
        assert_eq!(
            camera(vec![(0.1, 0.1), (0.9, 0.1), (0.5, 0.9)]).validate_finite(),
            Ok(())
        );
        assert_eq!(
            camera(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]).validate_finite(),
            Ok(()),
            "the frame's own corners are inside it"
        );
        for (what, polygon, refusal) in [
            (
                "x beyond the frame",
                vec![(1.01, 0.1), (0.9, 0.1), (0.5, 0.9)],
                "outside the frame",
            ),
            (
                "negative y",
                vec![(0.1, -0.01), (0.9, 0.1), (0.5, 0.9)],
                "outside the frame",
            ),
            (
                "NaN",
                vec![(f32::NAN, 0.1), (0.9, 0.1), (0.5, 0.9)],
                "finite",
            ),
            ("two vertices", vec![(0.1, 0.1), (0.9, 0.9)], "at least 3"),
            ("no vertices", vec![], "at least 3"),
        ] {
            let why = camera(polygon).validate_finite().expect_err(what);
            assert!(why.contains(refusal), "{what}: {why}");
        }
    }

    /// SPEC-077 — candidate values from the plan's calibration grid, never a default.
    fn parking_policy() -> ParkingPolicyV2 {
        ParkingPolicyV2 {
            dwell_secs: 10.0,
            max_speed_box_diagonals_per_sec: 0.05,
            max_displacement_box_diagonals: 0.10,
            max_observation_gap_secs: 3.0,
        }
    }

    fn parking_zone() -> ZoneConfig {
        ZoneConfig {
            id: "stalls".into(),
            name: "Stalls".into(),
            polygon: vec![(0.1, 0.1), (0.9, 0.1), (0.5, 0.9)],
            kind: ZoneKind::Parking,
            min_bbox_area_px_override: None,
        }
    }

    /// A camera in parking-lot mode with a parking zone and `policy`.
    fn parking_camera(policy: Option<ParkingPolicyV2>) -> CameraConfig {
        let mut cam: CameraConfig =
            toml::from_str("name = \"c1\"\nurl = \"rtsp://example/cam\"").unwrap();
        cam.behavior.parking_lot_mode = true;
        cam.behavior.parking_policy_v2 = policy;
        cam.zones = vec![parking_zone()];
        cam
    }

    /// SPEC-077 — a camera with no v2 policy keeps its exact JSON, zones
    /// included, so a consumer that predates v2 sees no new key and no new
    /// zone kind.
    #[test]
    fn parking_policy_absent_keeps_legacy_camera_json_unchanged() {
        let old = r#"{"id":7,"name":"Lot","url":"rtsp://10.0.0.5/stream","enabled":true,"max_fps":0,"prompts":[],"visual_prompts":[],"model_override":null,"parking_lot_mode":true,"zones":[{"id":"z1","name":"Lane","polygon":[[0.0,0.0],[1.0,0.0],[0.0,1.0]],"kind":"exclusion"},{"id":"z2","name":"Door","polygon":[[0.0,0.0],[0.5,0.0],[0.0,0.5]],"kind":"inclusion"},{"id":"z3","name":"Bench","polygon":[[0.5,0.5],[1.0,0.5],[0.5,1.0]],"kind":"dwell"}]}"#;
        let camera: CameraConfig = serde_json::from_str(old).unwrap();
        assert_eq!(camera.behavior.parking_policy_v2, None);
        assert_eq!(camera.validate_finite(), Ok(()));
        assert_eq!(serde_json::to_string(&camera).unwrap(), old);
    }

    /// SPEC-077 — a parking zone and the v2 policy parse from JSON and TOML
    /// and round-trip as written.
    #[test]
    fn parking_policy_v2_round_trips_with_a_parking_zone() {
        let json = serde_json::json!({
            "id": 7, "name": "Lot", "url": "rtsp://10.0.0.5/stream", "enabled": true,
            "max_fps": 0, "prompts": [], "visual_prompts": [], "model_override": null,
            "parking_lot_mode": true,
            "parking_policy_v2": {
                "dwell_secs": 10.0,
                "max_speed_box_diagonals_per_sec": 0.05,
                "max_displacement_box_diagonals": 0.1,
                "max_observation_gap_secs": 3.0
            },
            "zones": [{
                "id": "stalls", "name": "Stalls",
                "polygon": [[0.25, 0.25], [0.75, 0.25], [0.5, 0.75]],
                "kind": "parking"
            }]
        });
        let camera: CameraConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(camera.behavior.parking_policy_v2, Some(parking_policy()));
        assert_eq!(camera.zones[0].kind, ZoneKind::Parking);
        assert_eq!(camera.validate_finite(), Ok(()));
        assert_eq!(serde_json::to_value(&camera).unwrap(), json);

        let toml_camera: CameraConfig = toml::from_str(
            "name = \"c1\"\nurl = \"rtsp://example/cam\"\nparking_lot_mode = true\n\
             parking_policy_v2 = { dwell_secs = 10, max_speed_box_diagonals_per_sec = 0.05, \
             max_displacement_box_diagonals = 0.1, max_observation_gap_secs = 3 }\n\
             [[zones]]\nid = \"stalls\"\nname = \"Stalls\"\n\
             polygon = [[0.1, 0.1], [0.9, 0.1], [0.5, 0.9]]\nkind = \"parking\"",
        )
        .unwrap();
        assert_eq!(
            toml_camera.behavior.parking_policy_v2,
            Some(parking_policy())
        );
        assert_eq!(toml_camera.zones, vec![parking_zone()]);
        assert_eq!(toml_camera.validate_finite(), Ok(()));
    }

    /// SPEC-077 — every v2 value must be a finite number above zero, and the
    /// object has exactly its four keys: nothing is defaulted or ignored.
    #[test]
    fn parking_policy_v2_refuses_malformed_values() {
        type Set = fn(&mut ParkingPolicyV2, f64);
        let fields: [(&str, Set); 4] = [
            ("dwell_secs", |p, v| p.dwell_secs = v),
            ("max_speed_box_diagonals_per_sec", |p, v| {
                p.max_speed_box_diagonals_per_sec = v
            }),
            ("max_displacement_box_diagonals", |p, v| {
                p.max_displacement_box_diagonals = v
            }),
            ("max_observation_gap_secs", |p, v| {
                p.max_observation_gap_secs = v
            }),
        ];
        for (field, set) in fields {
            for bad in [f64::NAN, -1.0, 0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY] {
                let mut policy = parking_policy();
                set(&mut policy, bad);
                let why = parking_camera(Some(policy))
                    .validate_finite()
                    .expect_err(&format!("{field} = {bad}"));
                assert!(why.contains(field), "{field} = {bad}: {why}");
            }
        }

        let toml_nan: CameraConfig = toml::from_str(
            "name = \"c1\"\nurl = \"rtsp://example/cam\"\nparking_lot_mode = true\n\
             parking_policy_v2 = { dwell_secs = nan, max_speed_box_diagonals_per_sec = 0.05, \
             max_displacement_box_diagonals = 0.1, max_observation_gap_secs = 3 }",
        )
        .unwrap();
        assert!(toml_nan.validate_finite().is_err(), "TOML nan is refused");

        let full = serde_json::json!({
            "dwell_secs": 10.0,
            "max_speed_box_diagonals_per_sec": 0.05,
            "max_displacement_box_diagonals": 0.1,
            "max_observation_gap_secs": 3.0
        });
        for key in full.as_object().unwrap().keys() {
            let mut missing = full.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<ParkingPolicyV2>(missing).is_err(),
                "a v2 policy without {key} has no default to fall back on"
            );
        }
        let mut extra = full.clone();
        extra["significant_movement_pixels"] = serde_json::json!(36);
        assert!(serde_json::from_value::<ParkingPolicyV2>(extra).is_err());
        let mut text = full;
        text["dwell_secs"] = serde_json::json!("10");
        assert!(serde_json::from_value::<ParkingPolicyV2>(text).is_err());
    }

    /// SPEC-077 — v2 is only ever explicit: it needs parking-lot mode and a
    /// parking zone, and a parking zone needs it. Without both, a camera
    /// keeps its legacy behaviour.
    #[test]
    fn parking_policy_v2_needs_parking_lot_mode_and_a_parking_zone() {
        assert_eq!(
            parking_camera(Some(parking_policy())).validate_finite(),
            Ok(())
        );

        let mut mode_off = parking_camera(Some(parking_policy()));
        mode_off.behavior.parking_lot_mode = false;
        let why = mode_off
            .validate_finite()
            .expect_err("v2 without parking_lot_mode");
        assert!(why.contains("parking_lot_mode"), "{why}");

        for (what, zones) in [
            ("no zones", vec![]),
            (
                "only an exclusion zone",
                vec![ZoneConfig {
                    kind: ZoneKind::Exclusion,
                    ..parking_zone()
                }],
            ),
        ] {
            let mut no_parking_zone = parking_camera(Some(parking_policy()));
            no_parking_zone.zones = zones;
            let why = no_parking_zone.validate_finite().expect_err(what);
            assert!(why.contains("parking zone"), "{what}: {why}");
        }

        let why = parking_camera(None)
            .validate_finite()
            .expect_err("a parking zone without v2");
        assert!(why.contains("parking_policy_v2"), "{why}");
        let mut legacy_mode_off = parking_camera(None);
        legacy_mode_off.behavior.parking_lot_mode = false;
        assert!(
            legacy_mode_off.validate_finite().is_err(),
            "a parking zone is refused without v2 whatever the mode"
        );

        let mut legacy = parking_camera(None);
        legacy.zones.clear();
        assert_eq!(
            legacy.validate_finite(),
            Ok(()),
            "parking-lot mode with no parking zone and no v2 is the legacy camera"
        );
    }
}
