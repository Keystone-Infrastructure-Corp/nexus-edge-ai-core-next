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
    /// Per-camera override for `tracker.static_object.anchor_ttl_secs`.
    /// When `Some`, the supervisor replaces the global TTL with this
    /// value when constructing the camera's `StaticObjectFilter`.
    /// `None` means "inherit the engine default". Restart required:
    /// the value is read once at supervisor start and not hot-reloaded
    /// — the reconciler only respawns on URL change today.
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
    pub fn validate_finite(&self) -> Result<(), String> {
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
        }
        match &self.detector.model_override {
            Some(model) => model
                .validate_finite()
                .map_err(|why| format!("model_override {why}")),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
