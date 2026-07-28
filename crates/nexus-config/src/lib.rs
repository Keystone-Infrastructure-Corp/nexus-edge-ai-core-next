//! TOML-backed configuration for the Nexus edge engine.
//!
//! Every backend-selectable layer exposes a `backend` field so operators can
//! pin the implementation. Scale knobs (`workers`, `capacity`, `worker_threads`)
//! live alongside the backend choice — the config file is the only place the
//! deployment topology is declared.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nexus_types::{CameraId, CodecKind, VisualPromptId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;
use url::Url;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("validation: {0}")]
    Validation(String),
}

/// Compatibility shims applied to a parsed config so the engine can
/// emit operator-visible warnings on upgrade paths. Returned by
/// [`Config::load_with_compat`]. Reserved for future upgrade-path
/// shims; currently has no fields. Kept as a typed handle so callers
/// don't break when new shims are added.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompatNotice {
    // No fields. The original `auth_grandfathered` flag was retired
    // alongside the `AuthMode::None` / `AuthMode::DevToken` variants;
    // legacy values now produce a hard ConfigError at load time so
    // there is nothing to surface as a soft warning anymore.
    #[doc(hidden)]
    _private: (),
}

/// Detect `auth.mode = "none"` or `auth.mode = "dev_token"` in
/// the raw TOML source and return a hard ConfigError. Those
/// variants were removed in M-Admin Phase 0 — operators must
/// switch to `local`, `oidc`, or `hybrid` explicitly rather
/// than landing on a silently-different auth posture on upgrade.
///
/// Scans line-by-line so a `#`-commented example mention of the
/// legacy value (e.g. in `nexus.example.toml`) doesn't trip the
/// check.
fn reject_legacy_auth_mode(txt: &str) -> Result<(), ConfigError> {
    for raw in txt.lines() {
        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let trimmed = line.trim();
        if trimmed == r#"mode = "none""# || trimmed == r#"mode = "dev_token""# {
            let legacy = if trimmed.contains("none") {
                "none"
            } else {
                "dev_token"
            };
            return Err(ConfigError::Validation(format!(
                "auth.mode = \"{legacy}\" is no longer supported (removed in M-Admin Phase 0). \
                 Set auth.mode to one of \"local\", \"oidc\", or \"hybrid\". \
                 See config/nexus.example.toml and docs/ARCHITECTURE.md §11."
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub inference: InferenceConfig,
    #[serde(default)]
    pub tracker: TrackerConfig,
    #[serde(default)]
    pub rules: RulesConfig,
    #[serde(default)]
    pub bus: BusConfig,
    #[serde(default)]
    pub cameras: Vec<CameraConfig>,
    /// M7 alert-delivery sinks. Each entry maps 1:1 onto a
    /// registered `nexus_sinks::AlertSink`. Empty list (the
    /// default) means “engine records every alert locally but
    /// never ships anything off the box” — the dispatcher still
    /// runs and the outbox stays empty.
    #[serde(default)]
    pub sinks: Vec<SinkConfig>,
    /// Phase 5.6 — cross-camera re-identification. Disabled by
    /// default. When enabled, the per-camera supervisor mints a
    /// per-stable-track UUIDv7 and emits an `entity_sighting` wire
    /// envelope through the cloud tunnel every `emit_interval_s`
    /// seconds (plus once on first-stable). See `WEDGE_PLAN.md` and
    /// `nexus_pipeline::SightingScheduler` for the per-track FSM.
    #[serde(default)]
    pub reid: ReidConfig,
    /// Phase 7.6.6 — generic LAN device proxy (REPO_BOUNDARY R5c).
    /// Ships OFF; opt-in per deployment.
    #[serde(default)]
    pub lan_proxy: LanProxyConfig,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let txt = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&txt)?;
        cfg.normalize_shapes();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Same as [`Config::load`] but reports compatibility shims
    /// applied to the parsed config so the engine can surface them
    /// at boot. Currently retained as a typed boundary for future
    /// upgrade-path warnings; no shims are active today.
    ///
    /// Legacy `auth.mode = "none"` / `"dev_token"` values from
    /// pre-M-Admin-Phase-0 configs are rejected here with a clear
    /// error so operators upgrade explicitly rather than landing
    /// on a silently-different auth posture.
    pub fn load_with_compat(path: impl AsRef<Path>) -> Result<(Self, CompatNotice), ConfigError> {
        let txt = std::fs::read_to_string(path)?;
        reject_legacy_auth_mode(&txt)?;
        let mut cfg: Config = toml::from_str(&txt)?;
        cfg.normalize_shapes();
        cfg.validate()?;
        Ok((cfg, CompatNotice::default()))
    }

    /// Remap legacy square (or otherwise off-ladder) detector input
    /// shapes to the native 16:9 ladder, in place. Applied at load after
    /// deserialize and before [`Config::validate`]. Never fails — each
    /// remap emits a `warn!`, and the fleet-config hash bumps once on
    /// upgrade (intended; see docs/edge-core/M_NATIVE_ASPECT.md §5).
    pub fn normalize_shapes(&mut self) {
        self.inference.model.remap_legacy_shapes();
        for cam in &mut self.cameras {
            if let Some(m) = cam.detector.model_override.as_mut() {
                m.remap_legacy_shapes();
            }
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.inference.workers == 0
            && matches!(self.inference.backend, InferenceBackendKind::Pool)
        {
            return Err(ConfigError::Validation(
                "inference.backend = 'pool' requires inference.workers >= 1".into(),
            ));
        }
        for cam in &self.cameras {
            if cam.id <= 0 {
                return Err(ConfigError::Validation(format!(
                    "camera id must be > 0, got {}",
                    cam.id
                )));
            }
            if cam.ingest.url.scheme() != "rtsp"
                && cam.ingest.url.scheme() != "rtsps"
                && cam.ingest.url.scheme() != "file"
                && cam.ingest.url.scheme() != "virtual"
            {
                return Err(ConfigError::Validation(format!(
                    "camera {} url has unsupported scheme '{}'",
                    cam.id,
                    cam.ingest.url.scheme()
                )));
            }
            // M_TILE_REINFER (G1) — ban tile cascade on ensemble
            // detectors. Per-member tile budgeting is out of scope
            // for v1; see docs/edge-core/M_TILE_REINFER.md (cloud).
            if cam.behavior.tile_enabled == Some(true) {
                let effective_kind = cam
                    .detector
                    .model_override
                    .as_ref()
                    .map(|m| m.kind.as_str())
                    .unwrap_or_else(|| self.inference.model.kind.as_str());
                if effective_kind == "ensemble" {
                    return Err(ConfigError::Validation(format!(
                        "camera {} sets tile_enabled = true but its effective model.kind is 'ensemble' \
                         — G1 tile re-inference is incompatible with ensemble detectors \
                         (see docs/edge-core/M_TILE_REINFER.md)",
                        cam.id
                    )));
                }
            }
        }
        // M7 — sink ids must be unique. The dispatcher keys every
        // `alert_sink_outbox` row by `<kind>:<name>`; duplicates
        // would make outbox rows ambiguous and the registry would
        // silently drop one of the duplicates on `replace()`.
        let mut seen = HashSet::new();
        for sink in &self.sinks {
            let key = (sink.kind(), sink.name());
            if !seen.insert(key) {
                return Err(ConfigError::Validation(format!(
                    "duplicate sink id '{}:{}' (each <kind>:<name>) pair must be unique)",
                    sink.kind(),
                    sink.name()
                )));
            }
            sink.validate()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// 0 = num_cpus.
    #[serde(default)]
    pub worker_threads: usize,
    #[serde(default = "default_blocking_threads")]
    pub blocking_threads: usize,
    /// Writable directory for per-camera persisted state
    /// (static-object registries, etc.). Created on demand.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// M3.1 — directory holding stored visual-prompt reference crops
    /// (one file per `VisualPromptId`, original PNG/JPEG). The detector
    /// encodes them once into per-prompt embedding vectors persisted in
    /// the SQLite `visual_prompts` table; this directory is the source
    /// of truth for the original pixels (re-encoding on model change,
    /// thumbnail rendering in the admin UI). Created on demand.
    #[serde(default = "default_visual_prompts_dir")]
    pub visual_prompts_dir: PathBuf,
    /// M2.1 motion-clip recording + safety-floor configuration.
    #[serde(default)]
    pub clips: ClipsConfig,
    /// Hardware-decode strategy for the RTSP ingest path. Defaults to
    /// `Auto` (probe for a VA-capable GPU, fall back to software), so
    /// configs that predate this knob auto-enable hardware decode on a
    /// capable box with zero migration.
    #[serde(default)]
    pub decode: RuntimeDecodeConfig,
    /// M6 auth-side runtime knobs (lockout FSM thresholds, audit
    /// retention). All have safe defaults so existing configs that
    /// predate M6 boot unchanged.
    #[serde(default)]
    pub auth: RuntimeAuthConfig,
    /// M6 audit-log retention. Daily sweeper deletes rows older
    /// than `retention_days`. Defaults to 365 days.
    #[serde(default)]
    pub audit: RuntimeAuditConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            blocking_threads: default_blocking_threads(),
            state_dir: default_state_dir(),
            visual_prompts_dir: default_visual_prompts_dir(),
            clips: ClipsConfig::default(),
            decode: RuntimeDecodeConfig::default(),
            auth: RuntimeAuthConfig::default(),
            audit: RuntimeAuditConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Hardware decode
// ---------------------------------------------------------------------------

/// Decoder-selection strategy for the RTSP ingest path (the RGB tap that
/// feeds the detector). Serialised as `[runtime.decode] mode = "..."`
/// and optionally overridden per camera via [`CameraIngest::decode`].
///
/// The actual element selection (which `vah26Xdec` / `avdec_h26X` /
/// `msdkh26Xdec` chain to launch, with fail-open fallback) lives in
/// `nexus_pipeline::decode`. This enum is only the operator-facing knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeMode {
    /// Probe for the best available hardware backend (libva `va`, then
    /// NVIDIA `nvdec`) and fall back to software when neither is
    /// registered. Default.
    #[default]
    Auto,
    /// Force the libva `va` backend (`vah26Xdec` + `vapostproc`). Falls
    /// back to software with a warning if those elements are missing.
    Va,
    /// Force the Intel Media-SDK backend (`msdkh26Xdec` + `msdkvpp`).
    /// Falls back to VA, then software.
    Msdk,
    /// Force the NVIDIA NVDEC backend (`nvh26Xdec`, from the `nvcodec`
    /// plugin). Falls back to software with a warning if the plugin is
    /// missing or the driver's NVDEC userspace is unreachable.
    Nvdec,
    /// Force the software backend (`avdec_h26X`). Always available.
    Software,
}

/// `[runtime.decode]` section. Currently a single `mode` knob; kept as a
/// struct so future decode tuning (e.g. an explicit libva driver name or
/// a VRAM budget) is an additive field rather than a breaking reshape.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDecodeConfig {
    /// Global decode strategy. Per-camera [`CameraIngest::decode`]
    /// overrides this when set.
    #[serde(default)]
    pub mode: DecodeMode,
}

fn default_blocking_threads() -> usize {
    // 64 leaves enough headroom for camera-scaled spawn_blocking
    // callers (clip-recorder appsrc pushes, drain tasks, detector
    // reply recvs, sqlx sqlite workers) on a 32-camera box.
    //
    // The historical default of 8 wedged 10-camera deployments
    // when the GStreamer bus pumps were on this pool (see
    // preroll_ingester::run_session). Those have since moved to
    // dedicated std::threads, but the pool still gates many other
    // short-lived blocking ops — keeping it generous is cheap
    // (each tokio blocking thread is on-demand, ~8 KB stack).
    64
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/state")
}

fn default_visual_prompts_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/visual_prompts")
}

// ---------------------------------------------------------------------------
// Runtime auth + audit (M6)
// ---------------------------------------------------------------------------

/// Runtime-tunable knobs for the M6 local-users lockout FSM.
/// Operators override these in `nexus.toml` under
/// `[runtime.auth.lockout]`. All defaults match the M6 design
/// (5 fails in 15 minutes → 15-minute lockout).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthConfig {
    #[serde(default)]
    pub lockout: LockoutConfig,
}

/// Failed-login lockout policy. The FSM lives in
/// `nexus-engine::auth::lockout`. These knobs let operators tune
/// the thresholds without recompiling — useful for sites with
/// monitoring tools that already do brute-force protection
/// upstream and want a looser per-user lockout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockoutConfig {
    /// Number of consecutive failed-login attempts inside
    /// `window_secs` that trip the lockout. Default: 5.
    #[serde(default = "default_lockout_max_attempts")]
    pub max_attempts: u32,
    /// Sliding window for the attempt counter (seconds).
    /// Default: 900 (15 min).
    #[serde(default = "default_lockout_window_secs")]
    pub window_secs: u32,
    /// Lockout duration once the threshold is tripped (seconds).
    /// Default: 900 (15 min). Admins can clear early via
    /// `POST /api/v1/admin/users/:id/unlock`.
    #[serde(default = "default_lockout_secs")]
    pub lockout_secs: u32,
}

impl Default for LockoutConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_lockout_max_attempts(),
            window_secs: default_lockout_window_secs(),
            lockout_secs: default_lockout_secs(),
        }
    }
}

fn default_lockout_max_attempts() -> u32 {
    5
}

fn default_lockout_window_secs() -> u32 {
    900
}

fn default_lockout_secs() -> u32 {
    900
}

/// M6 audit-log retention. Daily sweeper deletes audit_log rows
/// older than `retention_days`. Reuses the M2.1 retention sweeper
/// plumbing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuditConfig {
    /// How long audit_log rows live before the daily sweeper
    /// deletes them. Default: 365 days. Set to 0 to disable the
    /// sweeper entirely (retain forever — used by operators who
    /// ship audit to an external SIEM and don't want local
    /// expiry).
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u32,
}

impl Default for RuntimeAuditConfig {
    fn default() -> Self {
        Self {
            retention_days: default_audit_retention_days(),
        }
    }
}

fn default_audit_retention_days() -> u32 {
    365
}

// ---------------------------------------------------------------------------
// Clips (M2.1 motion timeline + clip recording + safety floor)
// ---------------------------------------------------------------------------

/// Pick which clip-recorder implementation the engine wires up at
/// boot. `Stub` writes 0-byte placeholder files; `Gstreamer` writes
/// real H.264-pass-through fragmented mp4 via
/// `nexus_pipeline::GstClipRecorder` (only available when the
/// `gstreamer` feature is on for `nexus-pipeline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecorderKind {
    #[default]
    Stub,
    Gstreamer,
}

/// Recording, retention, and disk-safety knobs for the motion timeline.
///
/// **Hand-written `impl Default`.** The codebase rule (see DEV_NOTES.md
/// "Cargo / Rust") is: never combine `#[derive(Default)]` with
/// `#[serde(default = "fn")]`. The serde defaults below fire for
/// missing keys during deserialise; this `impl Default` keeps
/// `T::default()` callers (tests, builders) producing the same values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipsConfig {
    /// Which recorder implementation to wire up at boot.
    #[serde(default)]
    pub recorder: RecorderKind,
    /// Where the recorder writes mp4 files. Created on demand.
    #[serde(default = "default_clips_dir")]
    pub clips_dir: PathBuf,
    /// How long an unevicted clip lives before the daily retention
    /// sweeper deletes it. The watermark sampler can evict sooner if
    /// disk is tight.
    #[serde(default = "default_motion_clips_retention_days")]
    pub motion_clips_retention_days: u32,
    /// Cap on `track.updated` motion-event row writes per active track
    /// per second. `track.born` and `track.died` are always emitted.
    /// Default 1.0 ≈ one row per track per second.
    #[serde(default = "default_motion_events_sample_hz")]
    pub motion_events_sample_hz: f32,
    /// Below this percentage of free space on `clips_dir`'s filesystem
    /// the watermark sampler starts evicting one round per check.
    #[serde(default = "default_low_watermark_pct")]
    pub low_watermark_pct: u8,
    /// Below this percentage the recorder refuses to open new clips
    /// and the eviction loop runs hard until free space recovers to
    /// `low_watermark_pct + 5`.
    #[serde(default = "default_panic_watermark_pct")]
    pub panic_watermark_pct: u8,
    /// How often the watermark sampler runs.
    #[serde(default = "default_watermark_sample_interval_secs")]
    pub watermark_sample_interval_secs: u32,
    /// How long the supervisor waits after the last live track
    /// disappears before closing the open clip. A new motion event
    /// arriving inside the grace window cancels the pending close,
    /// so a single clip spans the brief gap between two intermittent
    /// tracks. 0 disables post-roll entirely (the clip closes the
    /// moment `live_track_count` hits zero, matching pre-B3 behaviour).
    #[serde(default = "default_post_roll_secs")]
    pub post_roll_secs: u32,
    /// Pre-roll buffer length in seconds — how much encoded H.264
    /// the always-on ingester keeps in RAM ahead of motion. When a
    /// new clip opens, the ring buffer's snapshot is prepended to
    /// the file so the operator sees the moment leading up to
    /// motion onset, not just the moment after.
    ///
    /// 0 disables pre-roll entirely; the recorder behaves exactly
    /// as it did before B8 (clips start at the first sample taken
    /// AFTER the open call). Default 5s matches the M2.1 spec; the
    /// per-camera RAM cost is roughly `bitrate * pre_roll_secs`,
    /// e.g. ~2 MB for a 4 Mbps 1080p camera.
    #[serde(default = "default_pre_roll_secs")]
    pub pre_roll_secs: u32,
    /// M2.2 Phase 3: when set, the recorder routes new clips to the
    /// USB volume with this label (e.g. `"NEXUS_VAULT"`) if the
    /// `usb_watch` task currently sees it attached. When the label
    /// is unset, missing, or the volume is unmounted, the recorder
    /// falls back to writing under `clips_dir` (`hot_handle = "local"`).
    /// In-flight clips never migrate mid-recording — attach/detach
    /// only takes effect on the next `open()` call.
    #[serde(default)]
    pub preferred_usb_label: Option<String>,
    /// Hard ceiling on a single clip's on-disk size in bytes. A
    /// healthy camera never approaches this — a 30-60 s 1080p clip is
    /// a few MB. The cap exists to bound the damage when a camera
    /// emits a corrupt H.264 stream whose byte-rate explodes (seen in
    /// the field: 640x360 clips ballooning to ~2 GiB at 160-740 Mbps).
    /// The recorder stats the in-flight file periodically and rotates
    /// when it crosses this size (Phase 3b enforcement); the cold
    /// replicator derives its own pre-emptive quarantine ceiling from
    /// this value so a runaway clip never head-of-line-blocks the
    /// upload queue. Default 256 MiB.
    #[serde(default = "default_max_clip_bytes")]
    pub max_clip_bytes: u64,
    /// M-Alert-Clip: short, burned-in alert clips delivered to sinks
    /// promptly, independent of the 5-minute motion clip. All-defaulted;
    /// disabled by default. See docs/edge-core/M_ALERT_CLIP.md.
    #[serde(default)]
    pub alert_clips: AlertClipsConfig,
    /// Guarantee every alert/event has an underlying full-resolution
    /// motion clip. The cheap `MotionGate` and the motion tracker are
    /// tuned for sustained movement, so a small / distant / briefly
    /// moving object can trip a rule (fire an alert) on a keyframe pass
    /// without the tracker ever declaring `Born` — leaving the event
    /// with no surrounding native-resolution video. When this is set,
    /// an alert firing on a frame with no clip open force-opens a
    /// native-resolution motion clip (identical to the motion path,
    /// including pre-roll) and each subsequent alert frame keeps it
    /// open through the same `post_roll_secs` grace window. This is the
    /// full-res, longer-capture motion clip — NOT the reduced-resolution
    /// burned-in alert clip, which remains governed by `alert_clips`.
    /// Default `true`.
    #[serde(default = "default_record_motion_clip_on_alert")]
    pub record_motion_clip_on_alert: bool,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            recorder: RecorderKind::default(),
            clips_dir: default_clips_dir(),
            motion_clips_retention_days: default_motion_clips_retention_days(),
            motion_events_sample_hz: default_motion_events_sample_hz(),
            low_watermark_pct: default_low_watermark_pct(),
            panic_watermark_pct: default_panic_watermark_pct(),
            watermark_sample_interval_secs: default_watermark_sample_interval_secs(),
            post_roll_secs: default_post_roll_secs(),
            pre_roll_secs: default_pre_roll_secs(),
            preferred_usb_label: None,
            max_clip_bytes: default_max_clip_bytes(),
            alert_clips: AlertClipsConfig::default(),
            record_motion_clip_on_alert: default_record_motion_clip_on_alert(),
        }
    }
}

fn default_clips_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/clips")
}

fn default_motion_clips_retention_days() -> u32 {
    30
}

fn default_motion_events_sample_hz() -> f32 {
    1.0
}

fn default_low_watermark_pct() -> u8 {
    15
}

fn default_panic_watermark_pct() -> u8 {
    5
}

fn default_watermark_sample_interval_secs() -> u32 {
    30
}

fn default_post_roll_secs() -> u32 {
    10
}

fn default_pre_roll_secs() -> u32 {
    5
}

fn default_max_clip_bytes() -> u64 {
    // 256 MiB. A healthy 30-60 s clip is a few MB; this only ever
    // trips on corrupt byte-exploding streams.
    256 * 1024 * 1024
}

fn default_record_motion_clip_on_alert() -> bool {
    true
}

/// M-Alert-Clip: configuration for the short, burned-in "alert clip"
/// that covers only the alert timeframe and is delivered to sinks
/// promptly, independent of the 5-minute motion clip. Enabled by
/// default; operators disable it per-org / per-core from the cloud
/// console's delivery settings (which flips the runtime
/// `DeliverySettings.attach_alert_clip` gate) without editing this
/// file. See docs/edge-core/M_ALERT_CLIP.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertClipsConfig {
    /// Capability switch. Default `true`: the edge builds alert clips
    /// and clip-attaching sinks resolve them (the operator-facing on/off
    /// lives in delivery settings, `DeliverySettings.attach_alert_clip`,
    /// AND-gated with this). Set `false` here to hard-disable the
    /// capability on a box regardless of the cloud toggle.
    #[serde(default = "default_alert_clips_enabled")]
    pub enabled: bool,
    /// Seconds of footage before the alert timestamp to include.
    /// Effectively bounded by the ingester's `pre_roll_secs` (the
    /// window can't include more than is buffered). Default 3.
    #[serde(default = "default_alert_clip_pre_secs")]
    pub pre_secs: u32,
    /// Seconds of footage after the alert timestamp to keep collecting
    /// before the clip is finalized. Sets the delivery-latency floor:
    /// the clip is ready ~`post_secs` after the alert. Default 5.
    #[serde(default = "default_alert_clip_post_secs")]
    pub post_secs: u32,
    /// Downscale cap (pixels of width) applied to the native frame
    /// before the bbox burn-in re-encode, to bound per-alert CPU.
    /// `0` disables the cap (encode at native width). Default 1280.
    #[serde(default = "default_alert_clip_max_encode_width")]
    pub max_encode_width: u32,
    /// How long the sink dispatcher waits for the alert clip to
    /// finalize before delivering the alarm clip-less. Should exceed
    /// `post_secs` plus worst-case encode time. Default 30.
    #[serde(default = "default_alert_clip_build_timeout_secs")]
    pub build_timeout_secs: u32,
}

impl Default for AlertClipsConfig {
    fn default() -> Self {
        Self {
            enabled: default_alert_clips_enabled(),
            pre_secs: default_alert_clip_pre_secs(),
            post_secs: default_alert_clip_post_secs(),
            max_encode_width: default_alert_clip_max_encode_width(),
            build_timeout_secs: default_alert_clip_build_timeout_secs(),
        }
    }
}

fn default_alert_clips_enabled() -> bool {
    true
}

fn default_alert_clip_pre_secs() -> u32 {
    3
}

fn default_alert_clip_post_secs() -> u32 {
    5
}

fn default_alert_clip_max_encode_width() -> u32 {
    1280
}

fn default_alert_clip_build_timeout_secs() -> u32 {
    30
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_api_bind")]
    pub api_bind: String,
    /// Optional second listener that serves the same router
    /// (API + SPA) on a different `host:port`. Intended use is
    /// `0.0.0.0:80` so operators can reach the admin console at
    /// `http://<host>/` without typing the engine port, while
    /// `api_bind` (default `0.0.0.0:8089`) stays available for
    /// programmatic API consumers. Binding port `<1024` on a
    /// non-root user requires `CAP_NET_BIND_SERVICE` — Docker
    /// already has it, the systemd unit in `docs/INSTALL.md §7.7`
    /// sets `AmbientCapabilities=CAP_NET_BIND_SERVICE`.
    #[serde(default)]
    pub ui_bind: Option<String>,
    /// Optional TLS listener. When set (typically `0.0.0.0:443`),
    /// the engine terminates TLS in-process using rustls and serves
    /// the same router as `api_bind`/`ui_bind`. Requires
    /// `tls_cert_path` + `tls_key_path` to also be set; if the cert
    /// files are missing at boot the listener is skipped with a
    /// warning (the engine still serves plain HTTP).
    #[serde(default)]
    pub https_bind: Option<String>,
    /// Path to the PEM-encoded TLS server certificate chain. The
    /// installer's `nexus-engine tls init` subcommand writes a
    /// self-signed leaf here on first boot; once cloud enrollment
    /// is wired (M-HTTPS Phase 3) the cloud-issued leaf overwrites
    /// it. Owner `root:nexus`, mode `0644`.
    #[serde(default)]
    pub tls_cert_path: Option<PathBuf>,
    /// Path to the PEM-encoded TLS private key matching
    /// `tls_cert_path`. Owner `root:nexus`, mode `0640`.
    #[serde(default)]
    pub tls_key_path: Option<PathBuf>,
    /// When `https_bind` is set and this is true (the default),
    /// the plain-HTTP `ui_bind` listener stops serving the
    /// application router and instead returns a 308 redirect
    /// to `https://<Host>{path}`. When false, both HTTP and
    /// HTTPS serve the application (useful for staged rollouts
    /// or operators who haven't trusted the self-signed cert
    /// yet). Ignored when `https_bind` is `None`.
    #[serde(default = "default_redirect_http_to_https")]
    pub redirect_http_to_https: bool,
    /// Strict-Transport-Security `max-age` (seconds) to advertise
    /// on every HTTPS response. Omit (the default) until the cert
    /// chain is trusted by the operator's browser — caching HSTS
    /// against a self-signed leaf can trap a workstation that
    /// later refuses to override the warning.
    #[serde(default)]
    pub hsts_max_age_seconds: Option<u64>,
    /// Filesystem path served as the SPA root. The Dockerfile installs
    /// the built UI here; locally `npm run build` puts it under `ui/dist`.
    #[serde(default = "default_ui_root")]
    pub ui_root: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_bind: default_api_bind(),
            ui_bind: None,
            https_bind: None,
            tls_cert_path: None,
            tls_key_path: None,
            redirect_http_to_https: default_redirect_http_to_https(),
            hsts_max_age_seconds: None,
            ui_root: default_ui_root(),
        }
    }
}

fn default_api_bind() -> String {
    "0.0.0.0:8089".to_string()
}

fn default_redirect_http_to_https() -> bool {
    true
}

fn default_ui_root() -> PathBuf {
    PathBuf::from("/usr/share/nexus/ui")
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    #[serde(default = "default_sqlite_url")]
    pub url: String,
    #[serde(default)]
    pub seed_from_config: bool,
    /// If true, attach a DuckDB analytics view via `ATTACH ... AS analytics`.
    #[serde(default)]
    pub duckdb_attach: bool,
    #[serde(default = "default_duckdb_path")]
    pub duckdb_path: PathBuf,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            url: default_sqlite_url(),
            seed_from_config: true,
            duckdb_attach: false,
            duckdb_path: default_duckdb_path(),
        }
    }
}

fn default_sqlite_url() -> String {
    "sqlite:///var/lib/nexus/nexus.db?mode=rwc".to_string()
}

fn default_duckdb_path() -> PathBuf {
    PathBuf::from("/var/lib/nexus/analytics.duckdb")
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub json_logs: bool,
    #[serde(default)]
    pub otlp: Option<OtlpConfig>,
}

// Hand-written so `Default` agrees with serde. The derive would give
// `log_level = ""`, which silently drops every log line because tracing's
// EnvFilter treats an empty directive as "deny everything". See
// /memories/repo/nexus-config-default-debt.md for the broader pattern.
impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            json_logs: false,
            otlp: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    pub endpoint: String,
    #[serde(default)]
    pub service_name: Option<String>,
    /// Tail-sampling rate for non-alert traces (0.0–1.0).
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
}

fn default_log_level() -> String {
    // Production default: quiet. `nexus=debug` here made every emit-config'd
    // box log per-frame DEBUG (measurable journald + CPU overhead at 16
    // cameras), and since `nexus-probe emit-config` serializes this default
    // into /etc/nexus/nexus.toml, that verbosity shipped to every install.
    // Mirrors the systemd unit's `RUST_LOG=info,nexus=info`. Dev configs that
    // want DEBUG set it explicitly (see config/*.toml).
    "info,nexus=info".to_string()
}

fn default_sample_ratio() -> f64 {
    0.01
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub mode: AuthMode,
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    /// Path to the admin-auth JSON file holding the shared HS256
    /// signing secret (M2.2 Phase 2 step 12). File shape:
    /// `{"secret": "..."}`. When set, every write against
    /// `/api/v1/admin/*` requires a valid HS256 JWT signed with
    /// that secret; when unset the engine falls back to "loopback
    /// bind only" + the `NEXUS_ADMIN_BEARER_ALLOW_REMOTE=1` escape
    /// hatch. See `nexus-engine::admin_auth` for the verifier.
    #[serde(default)]
    pub admin_secret_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// M6 local-users backend. Per-user argon2id passwords, lockout
    /// FSM, first-boot bootstrap admin. Rejects [`AuthConfig::oidc`].
    ///
    /// Default for fresh installs. M-Admin Phase 0 closeout
    /// retired the legacy `None` and `DevToken` variants — every
    /// edge deployment now lands on a real per-user credential.
    #[default]
    Local,
    /// M6 OIDC backend. Auth-code + PKCE against an external IdP
    /// (Authentik, Keycloak, Azure AD, Okta, Google Workspace).
    /// Requires [`AuthConfig::oidc`].
    Oidc,
    /// M6 hybrid — local users AND OIDC at once. The only mode
    /// that allows both sources. Required for the "break-glass
    /// local admin during IdP outage" pattern. Requires
    /// [`AuthConfig::oidc`].
    Hybrid,
}

impl AuthMode {
    /// Does this mode permit local username/password login?
    /// True for `Local` and `Hybrid`.
    pub fn allows_local(self) -> bool {
        matches!(self, AuthMode::Local | AuthMode::Hybrid)
    }

    /// Does this mode permit OIDC sign-in? True for `Oidc` and
    /// `Hybrid`.
    pub fn allows_oidc(self) -> bool {
        matches!(self, AuthMode::Oidc | AuthMode::Hybrid)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// OIDC issuer URL (e.g. `https://auth.example.com/application/o/nexus/`).
    /// Used as the base for discovery at
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// Expected `aud` claim. Typically the OIDC client ID issued by
    /// the IdP for this Nexus deployment.
    pub audience: String,
    /// Optional explicit JWKS URI; if absent, discovery resolves it
    /// from the issuer's well-known metadata.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// OIDC client ID for the auth-code + PKCE flow. Required by
    /// the M6 OIDC backend; the M5-era validator-only path ignores
    /// it.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Display name shown on the `/login` page's "Sign in with X"
    /// button (e.g. `"Authentik"`, `"Microsoft"`). Falls back to
    /// `"single sign-on"` if absent.
    #[serde(default)]
    pub display_name: Option<String>,
    /// OAuth scopes to request. Defaults to `["openid", "profile",
    /// "email", "groups"]` — `groups` is what every M6-supported
    /// IdP uses to carry role information, but the role mapper
    /// also looks at `roles` and a configurable custom claim.
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    /// Claim path lookup order for role mapping. First claim that
    /// exists wins. Defaults to `["groups", "roles",
    /// "https://nexus.local/role"]`.
    #[serde(default = "default_oidc_role_claims")]
    pub role_claims: Vec<String>,
    /// Per-role mapping rules. Each entry pairs a Nexus role with
    /// a list of values that, if found in the resolved role claim,
    /// promote the user to that role. The highest-privilege match
    /// wins (admin > operator > viewer).
    ///
    /// Example TOML:
    /// ```toml
    /// [auth.oidc.role_map]
    /// admin = ["nexus-admins"]
    /// operator = ["nexus-operators", "security-team"]
    /// ```
    #[serde(default)]
    pub role_map: OidcRoleMap,
    /// When true, an OIDC user whose claims don't match any
    /// `role_map` entry is rejected with 403 instead of receiving
    /// the default viewer role. Stricter installs (regulated
    /// industries) typically flip this on.
    #[serde(default)]
    pub deny_unmapped: bool,
    /// Full absolute callback URL handed to the IdP on both the
    /// `/authorize` redirect and the `/token` exchange. MUST byte-
    /// match what is registered with the IdP. When absent, the
    /// engine falls back to the relative path
    /// `/api/v1/auth/oidc/callback` which Authentik / Keycloak /
    /// Okta / Google all accept.
    ///
    /// **Microsoft Entra ID requires this field** — Entra rejects
    /// relative paths and demands the full `https://<host>/...`
    /// URL exactly as registered in the App registration's
    /// Authentication blade. Localhost over `http://` is allowed
    /// for development; everything else must be HTTPS.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Path to a file (mode 0600 recommended) holding the OIDC
    /// client secret. Loaded once at boot and held in RAM; the
    /// file is never re-read. When set, the engine sends
    /// `client_secret=<contents>` in the token-endpoint exchange
    /// alongside PKCE (canonical OAuth 2.0 confidential web-app
    /// flow). Required by every IdP that registers the app as a
    /// confidential client and configures a secret (Entra "Web"
    /// platform with a client secret, Okta "Web" application,
    /// Authentik "Confidential" client type, etc.). When absent,
    /// the engine sends PKCE only — works for public clients
    /// (Entra "Mobile and desktop" / "Single-page application"
    /// platforms) or for confidential clients registered without
    /// a secret.
    #[serde(default)]
    pub client_secret_file: Option<PathBuf>,
    /// Name of an environment variable holding the OIDC client
    /// secret. Resolved once at boot. Mutually exclusive with
    /// `client_secret_file` — setting both is a config error,
    /// not a silent precedence rule. Pick the one that matches
    /// your deploy target:
    ///
    /// * **Docker Compose / systemd**: prefer `client_secret_file`
    ///   pointing at the Docker-secret mount (`/run/secrets/...`)
    ///   or systemd `$CREDENTIALS_DIRECTORY/...` path. Files keep
    ///   the secret out of `/proc/<pid>/environ`.
    /// * **Kubernetes / Nomad / PaaS (Fly, Render, etc.)**: prefer
    ///   `client_secret_env = "NEXUS_OIDC_CLIENT_SECRET"` and wire
    ///   the platform's Secret object to inject that env var. No
    ///   file mounts required.
    ///
    /// The env var must be non-empty at engine start; whitespace
    /// is trimmed. The engine never re-reads the env after boot.
    #[serde(default)]
    pub client_secret_env: Option<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string(),
        "groups".to_string(),
    ]
}

fn default_oidc_role_claims() -> Vec<String> {
    vec![
        "groups".to_string(),
        "roles".to_string(),
        "https://nexus.local/role".to_string(),
    ]
}

/// Per-role allow-lists for OIDC claim values. A user is granted
/// the highest-privilege role whose list contains any value found
/// in any of the configured `role_claims`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcRoleMap {
    #[serde(default)]
    pub admin: Vec<String>,
    #[serde(default)]
    pub operator: Vec<String>,
    #[serde(default)]
    pub viewer: Vec<String>,
}

impl OidcRoleMap {
    /// Returns true if at least one mapping is configured.
    /// Required for `Local`/`Hybrid` validation so an OIDC-disabled
    /// install can ship an empty map without tripping a "you
    /// forgot to map any group" warning.
    pub fn is_empty(&self) -> bool {
        self.admin.is_empty() && self.operator.is_empty() && self.viewer.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Inference
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceConfig {
    /// Single-process or pool of N workers.
    #[serde(default)]
    pub backend: InferenceBackendKind,
    /// Pool-worker isolation strategy. Ignored when `backend != pool`.
    #[serde(default)]
    pub pool_worker_kind: PoolWorkerKind,
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default = "default_restart_backoff_ms")]
    pub restart_backoff_ms: u64,
    /// On all-workers-down, fall through to in-process backend.
    #[serde(default = "default_true")]
    pub fail_soft: bool,
    /// Ordered list of EPs to try at session-init time.
    #[serde(default = "default_ep_priority")]
    pub ep_priority: Vec<String>,
    /// Concrete model (open-vocab, ensemble, …).
    #[serde(default)]
    pub model: ModelConfig,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            backend: InferenceBackendKind::default(),
            pool_worker_kind: PoolWorkerKind::default(),
            workers: default_workers(),
            restart_backoff_ms: default_restart_backoff_ms(),
            fail_soft: true,
            ep_priority: default_ep_priority(),
            model: ModelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InferenceBackendKind {
    /// Single in-process detector.
    #[default]
    InProcess,
    /// `DetectorPool` of N backends + fail-soft fallback.
    Pool,
}

/// Isolation strategy for backends inside a `DetectorPool`.
///
/// `Thread` is the dev / single-host default: each worker is an OS thread
/// with its own current-thread tokio runtime. Cheap to spin up, shares
/// address space with the engine.
///
/// `Process` spawns the `nexus-inference-worker` binary as a child and
/// drives it over a length-prefixed bincode pipe. This is the production
/// stance — a panicking model or driver bug only takes the child down,
/// the engine + pool route around the dead slot, and the fail-soft
/// fallback keeps the pipeline live until M2's in-place restart lands.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolWorkerKind {
    #[default]
    Thread,
    Process,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// `"yolo"` (closed-vocab YOLOv26-nano, default) | `"open_vocab"` /
    /// `"yolo_world"` | `"yoloe"` (M3.1 open-vocab text + visual prompts) |
    /// `"yoloe_visual"` | `"yoloe_promptfree"` (M3.3 open-set auto-class)
    /// | `"classifier_ensemble"` | `"ensemble"` (M3.2 same-camera multi-
    /// detector fan-out — see `members` below) | `"mock"`.
    ///
    /// `yolo` matches the v1 ship — `models/yolo26n_<W>x<H>.onnx` on the
    /// native 16:9 ladder (512x288 … 2048x1152).
    #[serde(default = "default_model_kind")]
    pub kind: String,
    /// Optional model-pack directory containing `models-manifest.json`.
    /// When set, the engine loads the shape-matched artifact
    /// (`<model>_<W>x<H>.*`) from it, keyed on `input_width` /
    /// `input_height`.
    ///
    /// `skip_serializing_if` keeps `None` out of the serialized form so the
    /// fleet-config hash (`nexus-engine` `fleet_hash`) matches the cloud's
    /// `normalize_detector_config` projection, which drops an absent
    /// `pack_path` rather than emitting `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_path: Option<PathBuf>,
    /// Pack preset label — the canonical `"<W>x<H>"` shape string (e.g.
    /// "512x288"). A display/label mirror of `input_width`×`input_height`;
    /// the resolver keys on those two fields, not this string. Legacy
    /// bare-width presets ("640") are normalized at load.
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_input_width")]
    pub input_width: u32,
    #[serde(default = "default_input_height")]
    pub input_height: u32,
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
    /// M3.2 — same-camera detector ensemble. Meaningful when
    /// `kind == "ensemble"`: each entry is itself a `ModelConfig`
    /// (so members can be `yolo`, `yolo_world`, `yoloe`,
    /// `yoloe_visual`, or even another nested `ensemble`). Per-member
    /// fields like `pack_path`, `preset`, `input_width`,
    /// `input_height`, `score_threshold` apply to that member only;
    /// the parent's values are ignored when `kind == "ensemble"`.
    /// Omitted / empty under any other `kind` (kept opt-in via
    /// `serde(default)` so existing configs round-trip unchanged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ModelConfig>,
    /// Per-frame cap on detections returned by **any** detector kind.
    /// `None` keeps every detection that survives the inner pipeline;
    /// `Some(k)` sorts by confidence desc and truncates to the K most-
    /// confident objects. Wired at construction time via
    /// [`crate::caps::TopKDetector`] — see
    /// `crates/nexus-inference/src/caps.rs`.
    ///
    /// History: this field originated as the M3.3 yoloe_promptfree-only
    /// cap and was promoted to a universal knob in M_PERF_CROWD Phase B1
    /// without renaming so existing configs round-trip unchanged. The
    /// open-vocab `yoloe_promptfree` kind also applies it internally
    /// (its baseline behaviour); the outer wrapper is idempotent in
    /// that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// M_PERF_CROWD Phase B1 — drop any detection whose bbox area
    /// (`(x2 − x1) × (y2 − y1)` in supervisor-frame pixels) is below
    /// this threshold. Primary far-field noise knob for closed-vocab
    /// `yolo` on wide-angle lenses. `None` disables (current
    /// behaviour). Per-zone tighter overrides land via
    /// [`ZoneConfig::min_bbox_area_px_override`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bbox_area_px: Option<u32>,
    /// M_PERF_CROWD Phase C3 — opt-in spatial bucketing for the
    /// shared class-aware NMS pass in `nexus_inference::nms`. Used by
    /// `yoloe`, `yoloe_visual`, `yolo_world`, and `ensemble`. Closes
    /// the O(N²) suppression scan to O(N) by only checking the 3×3
    /// grid neighbourhood. Output is bit-identical to the naive path
    /// when `bucket_size_px ≥ max bbox dim in supervisor frame`.
    /// `None` (default) preserves the pre-C3 naive pass exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nms_spatial_bucket_size_px: Option<u32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            kind: default_model_kind(),
            pack_path: None,
            preset: default_preset(),
            input_width: default_input_width(),
            input_height: default_input_height(),
            score_threshold: default_score_threshold(),
            members: Vec::new(),
            top_k: None,
            min_bbox_area_px: None,
            nms_spatial_bucket_size_px: None,
        }
    }
}

impl ModelConfig {
    /// Remap a legacy square (or off-ladder) input shape to the native
    /// 16:9 ladder, in place, recursively for ensemble members. Emits a
    /// `warn!` per remap; never fails. Also normalizes `preset` to the
    /// canonical `"<W>x<H>"` form the (w,h)-keyed resolver expects, so
    /// legacy bare-width presets ("640") stop shadowing the real shape.
    pub fn remap_legacy_shapes(&mut self) {
        if let Some((w, h)) = remap_to_ladder(self.input_width, self.input_height) {
            let from = format!("{}x{}", self.input_width, self.input_height);
            let to = format!("{w}x{h}");
            warn!(
                %from,
                %to,
                kind = %self.kind,
                "config: remapped legacy detector input shape to the native 16:9 ladder"
            );
            self.input_width = w;
            self.input_height = h;
        }
        let canonical = format!("{}x{}", self.input_width, self.input_height);
        if self.preset != canonical {
            self.preset = canonical;
        }
        for member in &mut self.members {
            member.remap_legacy_shapes();
        }
    }
}

fn default_workers() -> usize {
    1
}
fn default_restart_backoff_ms() -> u64 {
    2_000
}
fn default_true() -> bool {
    true
}
/// Default EP order matches the documented hardware matrix:
///   Intel iGPU/dGPU/NPU → openvino
///   NVIDIA              → tensorrt → cuda (M5)
///   anything else       → cpu
/// A generated `/etc/nexus/nexus.toml` (`nexus-probe emit-config`) pins the
/// right short list for the detected box (e.g. an NPU box adds "npu" between
/// openvino and cpu; an NVIDIA box leads with "tensorrt"). `coreml` is
/// dev-only and excluded from production defaults — opt in explicitly in your
/// config if you need it.
fn default_ep_priority() -> Vec<String> {
    vec![
        "openvino".into(),
        "tensorrt".into(),
        "cuda".into(),
        "cpu".into(),
    ]
}
fn default_model_kind() -> String {
    "yolo".into()
}
fn default_preset() -> String {
    "512x288".into()
}
fn default_input_width() -> u32 {
    512
}
fn default_input_height() -> u32 {
    288
}
fn default_score_threshold() -> f32 {
    0.30
}

/// The native-16:9 shape ladder — exact 16:9 ∩ stride-32 (W=512k, H=288k).
/// Every shipped detector input shape is one of these rungs.
pub const SHAPE_LADDER: [(u32, u32); 4] = [(512, 288), (1024, 576), (1536, 864), (2048, 1152)];

/// Map a legacy square (or otherwise off-ladder) `(w, h)` to the nearest
/// native-16:9 ladder rung. The three shipped legacy squares map by
/// intent (640→512×288, 960→1024×576, 1280→1536×864); anything else
/// snaps to the rung closest in pixel count. Returns `None` when the
/// shape is already an exact ladder rung.
fn remap_to_ladder(w: u32, h: u32) -> Option<(u32, u32)> {
    if SHAPE_LADDER.contains(&(w, h)) {
        return None;
    }
    Some(match (w, h) {
        (640, 640) => (512, 288),
        (960, 960) => (1024, 576),
        (1280, 1280) => (1536, 864),
        _ => {
            let target = u64::from(w) * u64::from(h);
            *SHAPE_LADDER
                .iter()
                .min_by_key(|(lw, lh)| (u64::from(*lw) * u64::from(*lh)).abs_diff(target))
                .expect("SHAPE_LADDER is non-empty")
        }
    })
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

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
    /// before `motion.parked_vehicle = "yes"`. v1 default: 30 (~1 s @ 30 fps).
    #[serde(default = "default_annotator_parked_min_frames_to_flag")]
    pub parked_min_frames_to_flag: u32,
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
            parked_min_frames_to_flag: default_annotator_parked_min_frames_to_flag(),
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
fn default_annotator_parked_min_frames_to_flag() -> u32 {
    30
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

// ---------------------------------------------------------------------------
// Static-object filter (v1 EventFilter::staticVehicle*)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticObjectConfig {
    /// Frames a vehicle track must dwell below
    /// `significant_movement_pixels` (EMA-smoothed) before promoting
    /// to "static" and being suppressed from the rule eval slice.
    /// v1 default: 150 (~5 s @ 30 fps).
    #[serde(default = "default_static_object_dwell_frames")]
    pub dwell_frames: u32,
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
    #[serde(default = "default_true")]
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
            dwell_frames: default_static_object_dwell_frames(),
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

fn default_static_object_dwell_frames() -> u32 {
    150
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

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesConfig {
    #[serde(default)]
    pub backend: RulesBackendKind,
    /// Inline rules from TOML — useful for smoke tests; production rules live in the DB.
    #[serde(default)]
    pub inline: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RulesBackendKind {
    #[default]
    Cel,
}

/// The CEL predicate plus its severity tag — i.e. "what is this
/// rule actually checking, and how loudly does it alert". Grouped
/// so a refactor that adds a sibling predicate field (alternate
/// expression language, alternate severity ramp) lands in one
/// place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulePredicate {
    /// CEL expression evaluated against the per-frame `object` /
    /// `camera` / `now` context.
    pub when: String,
    pub severity: String,
}

/// Scope filters — which cameras + zones the rule applies to.
/// Both gates short-circuit at the start of the evaluator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleGates {
    #[serde(default)]
    pub camera_filter: Option<Vec<CameraId>>,
    /// Zone-id allow-list. When `Some` and non-empty, an object only
    /// matches the rule if its bbox centre falls inside at least one
    /// zone whose `id` appears in this list AND that zone is defined
    /// on the camera producing the event. `None` or empty = no zone
    /// gate (rule fires anywhere in the frame).
    ///
    /// The pipeline looks up the zones on the camera at evaluation
    /// time so a rule transparently follows zone-polygon edits — the
    /// rule config only stores ids, never the polygons themselves.
    #[serde(default)]
    pub zones: Option<Vec<String>>,
}

/// Debounce + cooldown — the three knobs that suppress runaway
/// alerts on noisy detectors. All three default to the
/// production-tested values from the original flat config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDebounce {
    #[serde(default = "default_min_track_age_ms")]
    pub min_track_age_ms: u64,
    #[serde(default = "default_consecutive_frames")]
    pub consecutive_frames: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

impl Default for RuleDebounce {
    fn default() -> Self {
        Self {
            min_track_age_ms: default_min_track_age_ms(),
            consecutive_frames: default_consecutive_frames(),
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

/// One configured alerting rule. Wire shape is flat — `predicate`,
/// `gates`, and `debounce` are `#[serde(flatten)]`'d so every
/// existing TOML rule and every payload the admin UI sends remains
/// bit-for-bit compatible. The nested Rust groups are purely a
/// code-organisation refactor: the supervisor / preview pipeline
/// can take `&RulePredicate` when it only needs the CEL, and
/// readers can tell at a glance which fields belong to the
/// scope-gate vs. the debounce ladder vs. the predicate itself.
///
/// Note: `#[serde(deny_unknown_fields)]` is intentionally omitted
/// — it's incompatible with `#[serde(flatten)]` (same trade-off as
/// `CameraConfig`; see its doc-comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub predicate: RulePredicate,
    #[serde(flatten)]
    pub gates: RuleGates,
    #[serde(flatten)]
    pub debounce: RuleDebounce,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// M7 per-rule sink routing. The `"<kind>:<name>"` ids of the
    /// alert-delivery sinks this rule's matches are enqueued to.
    /// Empty (the default) routes to **every** configured sink, so
    /// existing rules keep delivering everywhere. A non-empty list
    /// restricts delivery to the named sinks (filtered at dispatch
    /// time to those that actually exist, so a stale id never
    /// produces an undeliverable outbox row). This is distinct from
    /// the per-rule delivery *policy* (`rules.delivery_policy_json`,
    /// which gates whether/when delivery happens): routing decides
    /// *which* sinks, the policy decides *if* and *when*.
    ///
    /// Lives inside `rules.config_json` (no schema migration). Old
    /// payloads without the field deserialize to an empty list via
    /// `#[serde(default)]`, preserving the route-to-all behaviour.
    #[serde(default)]
    pub sinks: Vec<String>,
    /// Phase 8 — when `true`, alerts from this rule are sent to the cloud
    /// with `verification_state = candidate` so the cloud VLM
    /// behavior-verifier adjudicates them (advancing to `verified` /
    /// `dismissed` / `review`), instead of the default `verified` that
    /// shows in the console immediately. Only meaningful when the org is
    /// on a tier with cloud verification; otherwise the alert simply stays
    /// `candidate`. Lives inside `rules.config_json` (no schema
    /// migration); old payloads without the field deserialize to `false`.
    #[serde(default)]
    pub verify: bool,
}

fn default_min_track_age_ms() -> u64 {
    500
}
fn default_consecutive_frames() -> u32 {
    2
}
fn default_cooldown_ms() -> u64 {
    30_000
}

// ---------------------------------------------------------------------------
// Bus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusConfig {
    #[serde(default)]
    pub backend: BusBackendKind,
    #[serde(default = "default_bus_capacity")]
    pub capacity: usize,
    #[serde(default)]
    pub nats_url: Option<String>,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            backend: BusBackendKind::default(),
            capacity: default_bus_capacity(),
            nats_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BusBackendKind {
    #[default]
    Broadcast,
    Nats,
}

fn default_bus_capacity() -> usize {
    1024
}

// ---------------------------------------------------------------------------
// Sinks (M7 alert delivery)
// ---------------------------------------------------------------------------

/// One configured alert-delivery sink. Tagged by `kind` so the
/// engine knows which `nexus_sinks::AlertSink` to build at boot;
/// `name` is operator-chosen and is the half of the `<kind>:<name>`
/// SinkId every `alert_sink_outbox` row references.
///
/// Wire shape:
///
/// ```toml
/// [[sinks]]
/// kind = "webhook"
/// name = "primary"
/// url  = "https://example.com/nexus"
/// hmac_secret = "shared-secret"  # optional
/// timeout_secs = 10              # optional, default 10
///
/// [sinks.headers]                # optional
/// "X-Tenant" = "acme"
/// ```
///
/// Renaming a sink (changing `name` while keeping `kind`) is
/// forbidden in M7 because outbox rows reference the historical
/// id by string; the engine rejects validation if two entries
/// share `(kind, name)`. Operators MUST delete + re-add to rename.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SinkConfig {
    /// Generic HTTP webhook with optional HMAC-SHA256 signature.
    /// v1 parity port of `webhook_retry_queue.cpp`.
    Webhook(WebhookSinkConfig),
    /// SureView Ops central-monitoring sink. Triggers a SureView
    /// alarm point via a single JSON POST to the `/receiver`
    /// endpoint ("HTTP Alarms"); video stays on the SureView device.
    ///
    /// The serde discriminator is pinned to `"sureview"` (NOT the
    /// snake_case default `"sure_view"`) so the JSON / TOML `kind`
    /// tag matches [`SinkConfig::kind`] and the `<kind>:<name>`
    /// SinkId the dispatcher + outbox use.
    #[serde(rename = "sureview")]
    SureView(SureViewSinkConfig),
    /// SureView Ops central-monitoring sink over **SMTP / Email
    /// Alarms**. Sends one email per alert to the alarm point's
    /// unique receiver address; the operator-facing message rides the
    /// `Subject`, optional GPS map-plot coordinates ride the body, and
    /// the clip / snapshot can ride as attachments. No account API key
    /// is required — the destination address identifies the alarm
    /// point (per the SureView *SMTP Alarms* reference).
    ///
    /// The serde discriminator is pinned to `"sureview_email"` (the
    /// snake_case default already matches, but it is spelled
    /// explicitly to keep the wire tag stable alongside the
    /// `<kind>:<name>` SinkId).
    #[serde(rename = "sureview_email")]
    SureViewEmail(SureViewEmailSinkConfig),
    /// Generic SMTP email sink — one message per alert to an
    /// operator-chosen recipient list, through the site's own relay
    /// (Microsoft 365, Google Workspace, Exchange connector, on-prem
    /// MTA). Unlike [`SinkConfig::SureViewEmail`] this is not tied to
    /// any monitoring vendor's alarm-point semantics: it carries a
    /// real `to` / `cc` list, an operator-filterable subject prefix,
    /// and an HTML body alongside the plain-text alternative.
    Email(EmailSinkConfig),
}

impl SinkConfig {
    /// Discriminator — matches `nexus_sinks::AlertSink::kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            SinkConfig::Webhook(_) => "webhook",
            SinkConfig::SureView(_) => "sureview",
            SinkConfig::SureViewEmail(_) => "sureview_email",
            SinkConfig::Email(_) => "email",
        }
    }

    /// Operator-chosen identifier — the `<name>` half of the
    /// `<kind>:<name>` SinkId every outbox row references.
    pub fn name(&self) -> &str {
        match self {
            SinkConfig::Webhook(cfg) => &cfg.name,
            SinkConfig::SureView(cfg) => &cfg.name,
            SinkConfig::SureViewEmail(cfg) => &cfg.name,
            SinkConfig::Email(cfg) => &cfg.name,
        }
    }

    /// Per-kind validation invoked from `Config::validate`. Cheap
    /// structural checks only — the sink crate does protocol-level
    /// validation lazily on first `deliver()` call.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            SinkConfig::Webhook(cfg) => cfg.validate(),
            SinkConfig::SureView(cfg) => cfg.validate(),
            SinkConfig::SureViewEmail(cfg) => cfg.validate(),
            SinkConfig::Email(cfg) => cfg.validate(),
        }
    }

    /// Sentinel the admin GET surface substitutes for any secret
    /// field so a configured secret never leaves the box. A PUT
    /// that echoes this value back means "keep the stored secret
    /// unchanged" — see [`SinkConfig::restore_redacted_secrets_from`].
    pub const REDACTED_SECRET: &'static str = "__nexus_secret_redacted__";

    /// Replace every secret field with [`Self::REDACTED_SECRET`] in
    /// place. Called before a sink config is serialised into an
    /// admin GET response so the live secret never leaves the edge.
    /// Covers SureView `api_key` and webhook `hmac_secret`. Custom
    /// webhook `headers` are NOT redacted — operators must not place
    /// secrets there when configuring from the cloud.
    pub fn redact_secrets(&mut self) {
        match self {
            SinkConfig::Webhook(w) => {
                if w.hmac_secret.is_some() {
                    w.hmac_secret = Some(Self::REDACTED_SECRET.to_string());
                }
            }
            SinkConfig::SureView(s) => {
                s.api_key = Self::REDACTED_SECRET.to_string();
            }
            SinkConfig::SureViewEmail(s) => {
                if s.password.is_some() {
                    s.password = Some(Self::REDACTED_SECRET.to_string());
                }
            }
            SinkConfig::Email(s) => {
                if s.password.is_some() {
                    s.password = Some(Self::REDACTED_SECRET.to_string());
                }
            }
        }
    }

    /// For an incoming PUT body, restore any secret left as the
    /// redaction sentinel from the previously-stored config. Lets
    /// the cloud console round-trip a sink edit without ever
    /// handling the live secret: it echoes [`Self::REDACTED_SECRET`]
    /// for an unchanged field and the edge re-fills it from the db.
    /// `existing` is the prior stored config for the same `sink_id`;
    /// a kind mismatch is a no-op (the caller validates kind).
    pub fn restore_redacted_secrets_from(&mut self, existing: &SinkConfig) {
        match (self, existing) {
            (SinkConfig::Webhook(new), SinkConfig::Webhook(old))
                if new.hmac_secret.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.hmac_secret = old.hmac_secret.clone();
            }
            (SinkConfig::SureView(new), SinkConfig::SureView(old))
                if new.api_key == Self::REDACTED_SECRET =>
            {
                new.api_key = old.api_key.clone();
            }
            (SinkConfig::SureViewEmail(new), SinkConfig::SureViewEmail(old))
                if new.password.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.password = old.password.clone();
            }
            (SinkConfig::Email(new), SinkConfig::Email(old))
                if new.password.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.password = old.password.clone();
            }
            _ => {}
        }
    }

    /// `true` iff any secret field still holds the redaction
    /// sentinel. After [`Self::restore_redacted_secrets_from`] this
    /// signals a brand-new sink whose secret the operator never
    /// supplied (the sentinel had nothing to restore from) — the
    /// admin handler rejects such a PUT with a 400.
    pub fn has_redacted_secret(&self) -> bool {
        match self {
            SinkConfig::Webhook(w) => w.hmac_secret.as_deref() == Some(Self::REDACTED_SECRET),
            SinkConfig::SureView(s) => s.api_key == Self::REDACTED_SECRET,
            SinkConfig::SureViewEmail(s) => s.password.as_deref() == Some(Self::REDACTED_SECRET),
            SinkConfig::Email(s) => s.password.as_deref() == Some(Self::REDACTED_SECRET),
        }
    }
}

/// HTTP webhook sink configuration. JSON POST of the `AlertEvent`
/// payload, optional shared-secret HMAC-SHA256 signature shipped
/// in the `X-Nexus-Signature: sha256=<hex>` header (GitHub style),
/// optional custom headers fan-out.
///
/// Retry + backoff lives in the dispatcher
/// (`nexus_sinks::dispatcher`), not the sink — the sink does at
/// most one HTTP attempt per `deliver()` call and classifies the
/// outcome as `Transient` (5xx, 408, 429, network) or `Permanent`
/// (other 4xx).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// Target HTTP(S) endpoint. The webhook sink POSTs the alert
    /// JSON to this URL on every delivery attempt.
    pub url: Url,
    /// Optional custom request headers. Common use: tenant tags,
    /// auth bearer tokens (set the `Authorization` header here).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional shared secret. When set, the sink computes
    /// `hex(hmac_sha256(secret, body))` and ships it in the
    /// `X-Nexus-Signature: sha256=<hex>` header.
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Per-attempt HTTP timeout in seconds. The dispatcher's
    /// retry backoff (500ms → 60s, 8 attempts) wraps this.
    #[serde(default = "default_webhook_timeout_secs")]
    pub timeout_secs: u64,
}

impl WebhookSinkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "webhook sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "webhook sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if self.url.scheme() != "http" && self.url.scheme() != "https" {
            return Err(ConfigError::Validation(format!(
                "webhook sink '{}' url scheme '{}' is not http(s)",
                self.name,
                self.url.scheme()
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "webhook sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

fn default_webhook_timeout_secs() -> u64 {
    10
}

/// SureView Ops "HTTP Alarms" sink configuration. Triggers a SureView
/// alarm point with a single JSON POST to the regional `/receiver`
/// endpoint, per the SureView Ops *HTTP Alarms* reference
/// (<https://help.sureviewops.com/hc/en-us/articles/13213264758557-Http-Alarms>).
///
/// SureView Ops is an alarm receiver: the POST only *triggers* an
/// alarm point identified by its **System Identifier** (configured in
/// SureView's *Alarm Setup → HTTP Alarms* tab). Video is NOT carried
/// in the payload — the operator pulls live/recorded video from the
/// camera the customer has set up as a SureView device — so this sink
/// has no media-upload step.
///
/// The account API Key is a per-customer secret — it lives in
/// `nexus.toml` on the edge box, never in this repo — and is sent
/// base64-encoded in the `Authorization` header (per the docs).
/// Retry/backoff is the dispatcher's job; the sink does one HTTP POST
/// per `deliver()` call and classifies the outcome as `Transient`
/// (5xx, 408, 429, network) or `Permanent` (other 4xx, e.g. a bad
/// API key).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SureViewSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SureView Ops SaaS hosting region. Resolves to the documented
    /// receiver endpoint unless `endpoint` overrides it. Defaults to
    /// `us`.
    #[serde(default)]
    pub region: SureViewRegion,
    /// Explicit receiver endpoint override. Set this only for on-prem
    /// SureView installs or integration testing; production SaaS
    /// deployments should set `region` instead.
    #[serde(default)]
    pub endpoint: Option<Url>,
    /// Per-customer account API Key from SureView's *Alarm Setup →
    /// HTTP Alarms* tab. Secret — sourced from `nexus.toml` on the
    /// box, never committed. Sent base64-encoded in `Authorization`.
    pub api_key: String,
    /// Default SureView alarm-point **System Identifier** to trigger.
    /// Used for any camera without a `system_identifiers` override.
    pub system_identifier: String,
    /// Optional per-camera override of `system_identifier`, keyed by
    /// the nexus camera id (as a string). Lets each camera trigger
    /// its own SureView alarm point / zone.
    #[serde(default)]
    pub system_identifiers: std::collections::HashMap<String, String>,
    /// Optional static `"Latitude,Longitude"` reported as the SureView
    /// `location` field on every alarm from this sink.
    #[serde(default)]
    pub location: Option<String>,
    /// Per-attempt HTTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff (500ms → 60s, 8 attempts) wraps it.
    #[serde(default = "default_sureview_timeout_secs")]
    pub timeout_secs: u64,
}

/// SureView Ops SaaS hosting region — selects the documented
/// `/receiver` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SureViewRegion {
    /// `https://us.sureviewops.com/receiver`
    #[default]
    Us,
    /// `https://eu.sureviewops.com/receiver`
    Eu,
}

impl SureViewRegion {
    /// The documented regional receiver URL.
    pub fn receiver_url(self) -> &'static str {
        match self {
            SureViewRegion::Us => "https://us.sureviewops.com/receiver",
            SureViewRegion::Eu => "https://eu.sureviewops.com/receiver",
        }
    }

    /// The documented regional SMTP relay host for *SMTP / Email
    /// Alarms*. AMER routes through `us-smtp`; EMEA / APAC through
    /// `eu-smtp`.
    pub fn smtp_host(self) -> &'static str {
        match self {
            SureViewRegion::Us => "us-smtp.sureviewops.com",
            SureViewRegion::Eu => "eu-smtp.sureviewops.com",
        }
    }
}

impl SureViewSinkConfig {
    /// The receiver URL this sink POSTs to — the explicit `endpoint`
    /// override when set, else the region's documented receiver URL.
    pub fn resolved_endpoint(&self) -> Result<Url, ConfigError> {
        match &self.endpoint {
            Some(u) => Ok(u.clone()),
            None => Url::parse(self.region.receiver_url())
                .map_err(|e| ConfigError::Validation(format!("sureview region url: {e}"))),
        }
    }

    /// Resolve the SureView System Identifier for a given camera —
    /// the per-camera override if present, else the default.
    pub fn system_identifier_for(&self, camera_id: CameraId) -> &str {
        self.system_identifiers
            .get(&camera_id.to_string())
            .map(String::as_str)
            .unwrap_or(&self.system_identifier)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "sureview sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "sureview sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if let Some(endpoint) = &self.endpoint {
            if endpoint.scheme() != "http" && endpoint.scheme() != "https" {
                return Err(ConfigError::Validation(format!(
                    "sureview sink '{}' endpoint scheme '{}' is not http(s)",
                    self.name,
                    endpoint.scheme()
                )));
            }
        }
        if self.api_key.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' api_key must be non-empty",
                self.name
            )));
        }
        if self.system_identifier.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' system_identifier must be non-empty",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

fn default_sureview_timeout_secs() -> u64 {
    15
}

/// SureView Ops "SMTP / Email Alarms" sink configuration. Triggers a
/// SureView alarm point by sending one email per alert to the alarm
/// point's unique receiver address, per the SureView Ops *SMTP Alarms*
/// reference
/// (<https://help.sureviewops.com/hc/en-us/articles/13211794487837-Smtp-alarms-Email-Alarms>).
///
/// Unlike the HTTP Alarms sink there is **no account API key** — the
/// destination email address itself identifies the alarm point.
/// SureView reads the email as follows:
///   * `To` — the alarm point's unique address (e.g.
///     `8nrawg1sxc@us.sureviewops.com`). Per-camera overrides live in
///     [`Self::alarm_emails`].
///   * `From` — any syntactically valid address (SureView ignores it).
///   * `Subject` — the operator-facing alarm message.
///   * body — optional; a decimal `"latitude,longitude"` pair auto-
///     plots the alarm on the SureView map ([`Self::location`]).
///   * attachments — optional clip (MP4) / snapshot (JPG).
///
/// SureView's relay requires no authentication, but some networks
/// front it with an authenticating relay, so optional
/// [`Self::username`] / [`Self::password`] (a redacted secret) are
/// supported. Retry / backoff is the dispatcher's job; the sink sends
/// at most one message per `deliver()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SureViewEmailSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SureView Ops SaaS hosting region. Selects the documented SMTP
    /// relay host unless `smtp_host` overrides it. Defaults to `us`.
    #[serde(default)]
    pub region: SureViewRegion,
    /// Explicit SMTP relay host override. Set this only for on-prem
    /// SureView installs or integration testing; production SaaS
    /// deployments should set `region` instead.
    #[serde(default)]
    pub smtp_host: Option<String>,
    /// SMTP submission port. Defaults to 587 (SureView also accepts
    /// 25).
    #[serde(default = "default_sureview_smtp_port")]
    pub smtp_port: u16,
    /// Negotiate STARTTLS on the connection. Defaults to `true`
    /// (SureView supports but does not require TLS); set `false` only
    /// for a plaintext test relay.
    #[serde(default = "default_true")]
    pub starttls: bool,
    /// Envelope / header `From` address. SureView ignores it, but SMTP
    /// requires a syntactically valid sender. Defaults to
    /// `nexus-edge@localhost`.
    #[serde(default = "default_sureview_from")]
    pub from_address: String,
    /// Default SureView alarm-point receiver address (the `To`). Used
    /// for any camera without an `alarm_emails` override.
    pub alarm_email: String,
    /// Optional per-camera override of `alarm_email`, keyed by the
    /// nexus camera id (as a string). Lets each camera trigger its own
    /// SureView alarm point.
    #[serde(default)]
    pub alarm_emails: std::collections::HashMap<String, String>,
    /// Optional static `"Latitude,Longitude"` placed in the email body
    /// so SureView auto-plots the alarm on its map.
    #[serde(default)]
    pub location: Option<String>,
    /// Attach the alert's annotated snapshot (JPG) when one is present
    /// on the event. Best-effort: a missing / unreadable file is
    /// logged and the alarm is still sent. Defaults to `false`.
    #[serde(default)]
    pub attach_snapshot: bool,
    /// Attach the alert's motion clip (MP4) when one is present on the
    /// event. Best-effort and size-capped; a missing / oversized file
    /// is logged and the alarm is still sent. Defaults to `false`.
    #[serde(default)]
    pub attach_clip: bool,
    /// Optional SMTP AUTH username for a fronting authenticating relay.
    /// Leave unset for SureView's unauthenticated relay.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional SMTP AUTH password (paired with `username`). Secret —
    /// sourced from `nexus.toml` on the box, never committed; redacted
    /// on the admin GET surface.
    #[serde(default)]
    pub password: Option<String>,
    /// Per-attempt SMTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff wraps it.
    #[serde(default = "default_sureview_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_sureview_smtp_port() -> u16 {
    587
}

fn default_sureview_from() -> String {
    "nexus-edge@localhost".to_string()
}

impl SureViewEmailSinkConfig {
    /// The SMTP relay host this sink connects to — the explicit
    /// `smtp_host` override when set, else the region's documented
    /// relay host.
    pub fn resolved_smtp_host(&self) -> &str {
        match &self.smtp_host {
            Some(h) => h.as_str(),
            None => self.region.smtp_host(),
        }
    }

    /// Resolve the SureView alarm-point address for a given camera —
    /// the per-camera override if present, else the default.
    pub fn alarm_email_for(&self, camera_id: CameraId) -> &str {
        self.alarm_emails
            .get(&camera_id.to_string())
            .map(String::as_str)
            .unwrap_or(&self.alarm_email)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "sureview_email sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if !self.from_address.contains('@') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' from_address '{}' is not a valid email",
                self.name, self.from_address
            )));
        }
        if !self.alarm_email.contains('@') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' alarm_email '{}' is not a valid email",
                self.name, self.alarm_email
            )));
        }
        for (cam, addr) in &self.alarm_emails {
            if !addr.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "sureview_email sink '{}' alarm_emails['{cam}'] '{addr}' is not a valid email",
                    self.name
                )));
            }
        }
        if self.resolved_smtp_host().is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' smtp_host must be non-empty",
                self.name
            )));
        }
        if self.smtp_port == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' smtp_port must be > 0",
                self.name
            )));
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' username and password must be set together",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

/// Generic SMTP email sink configuration — "email this alert to
/// these people".
///
/// Distinct from [`SureViewEmailSinkConfig`], which is hard-wired to
/// SureView's alarm-point model (the destination address *is* the
/// alarm point, so there is one recipient, a fixed subject shape, and
/// no HTML part). This sink is the operator-facing one: an explicit
/// recipient list, a subject an operator can filter their mailbox on,
/// and a readable HTML body alongside the plain-text alternative.
///
/// The relay is whatever the site already uses — Microsoft 365,
/// Google Workspace, a corporate Exchange connector, or an on-prem
/// MTA. Credentials stay on the box: they are entered through the
/// admin API, persisted in the edge-resident `alert_sinks` table, and
/// redacted by [`SinkConfig::redact_secrets`] before any GET response
/// leaves the appliance.
///
/// ```toml
/// [[sinks]]
/// kind = "email"
/// name = "site-ops"
/// smtp_host = "smtp.example.com"
/// from_address = "nexus@example.com"
/// from_name = "Nexus Edge AI"
/// to = ["ops@example.com", "security@example.com"]
/// subject_prefix = "[North Yard]"
/// username = "nexus@example.com"
/// password = "app-password"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SMTP relay hostname. Required — unlike the SureView sink
    /// there is no vendor default to fall back on.
    pub smtp_host: String,
    /// SMTP submission port. Defaults to 587 (STARTTLS submission).
    #[serde(default = "default_email_smtp_port")]
    pub smtp_port: u16,
    /// Negotiate STARTTLS on the connection. Defaults to `true`;
    /// set `false` only for a plaintext relay on a trusted LAN.
    #[serde(default = "default_true")]
    pub starttls: bool,
    /// Envelope / header `From` address. Most relays require this to
    /// be an address they are authorised to send as.
    pub from_address: String,
    /// Optional display name shown beside `from_address` in a mail
    /// client (`Nexus Edge AI <nexus@example.com>`).
    #[serde(default)]
    pub from_name: Option<String>,
    /// Recipients. Must be non-empty — a sink with nobody to mail is
    /// a silent no-op, so it is rejected at validation instead.
    pub to: Vec<String>,
    /// Optional carbon-copy recipients.
    #[serde(default)]
    pub cc: Vec<String>,
    /// Optional `Reply-To`. Useful when `from_address` is a no-reply
    /// mailbox but replies should reach a monitored inbox.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Optional literal prefix for the subject line, e.g.
    /// `"[North Yard]"`. Gives operators a stable string to build
    /// mailbox rules on when one relay serves several sites.
    #[serde(default)]
    pub subject_prefix: Option<String>,
    /// Attach the alert's annotated snapshot (JPG) when the event
    /// carries one. Defaults to `true` — a still frame is the whole
    /// point of an alert email. Best-effort: a missing or unreadable
    /// file is logged and the mail still goes out.
    #[serde(default = "default_true")]
    pub attach_snapshot: bool,
    /// Attach the alert's motion clip (MP4) when the event carries
    /// one. Defaults to `false` because clips routinely exceed relay
    /// message-size limits; best-effort and size-capped when enabled.
    #[serde(default)]
    pub attach_clip: bool,
    /// Optional SMTP AUTH username. Most hosted relays require it.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional SMTP AUTH password (paired with `username`). Secret —
    /// redacted on the admin GET surface, never leaves the box.
    #[serde(default)]
    pub password: Option<String>,
    /// Per-attempt SMTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff wraps it.
    #[serde(default = "default_email_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_email_smtp_port() -> u16 {
    587
}

fn default_email_timeout_secs() -> u64 {
    15
}

impl EmailSinkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "email sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "email sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if self.smtp_host.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' smtp_host must be non-empty",
                self.name
            )));
        }
        if self.smtp_port == 0 {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' smtp_port must be > 0",
                self.name
            )));
        }
        if !self.from_address.contains('@') {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' from_address '{}' is not a valid email",
                self.name, self.from_address
            )));
        }
        // A sink with no recipients would accept every outbox row and
        // deliver nothing, which is indistinguishable from working.
        if self.to.is_empty() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' must have at least one 'to' recipient",
                self.name
            )));
        }
        for addr in self.to.iter().chain(self.cc.iter()) {
            if !addr.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "email sink '{}' recipient '{addr}' is not a valid email",
                    self.name
                )));
            }
        }
        if let Some(reply_to) = &self.reply_to {
            if !reply_to.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "email sink '{}' reply_to '{reply_to}' is not a valid email",
                    self.name
                )));
            }
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' username and password must be set together",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cameras
// ---------------------------------------------------------------------------

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
    #[serde(default = "default_true")]
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
    /// it MUST be a native-16:9 ladder rung (512 | 1024 | 1536 | 2048)
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

// ---------------------------------------------------------------------------
// CameraConfigUpdate — what gets fan-pushed to detector slots on hot reload
// ---------------------------------------------------------------------------

/// Diff sent into every detector slot when a camera changes. Each slot
/// applies it idempotently — if the diff matches its current state the
/// push is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfigUpdate {
    pub camera_id: CameraId,
    pub prompts: Vec<String>,
    /// M3.1 — visual-prompt attachments for this camera. Empty for
    /// every backend except the YOLOE visual-mode detector. Defaults
    /// to empty so older fan-push payloads that predate the field
    /// still deserialise cleanly.
    #[serde(default)]
    pub visual_prompts: Vec<VisualPromptRef>,
    pub model: ModelConfig,
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Phase 7.6.6 — generic LAN device proxy (REPO_BOUNDARY R5c)
// ---------------------------------------------------------------------------

/// `[lan_proxy]` block. **Disabled by default.** When `enabled = true`,
/// the audited, SSRF-bounded `POST /api/v1/admin/proxy` admin route
/// lets the operator console reach a non-ONVIF device on the edge's own
/// LAN (camera web UIs, NVRs). The whole feature is an explicit,
/// narrowly-scoped exception to the credential-boundary rule, so it is
/// opt-in per deployment (R5c §5: ships off). Every other R5c
/// constraint (SSRF guard, discovery allowlist, per-call audit, no
/// credential persistence) is enforced unconditionally in
/// `crate::lan_proxy` whenever this is on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LanProxyConfig {
    /// Master switch. Defaults `false`.
    #[serde(default)]
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Phase 5.6 — cross-camera re-identification
// ---------------------------------------------------------------------------

/// `[reid]` block. Disabled by default. When `enabled = true`, the
/// per-camera supervisor runs the configured [`nexus_reid::Extractor`]
/// on each stable track once on first-stable and again every
/// `emit_interval_s` of wall-clock, publishing `entity_sighting`
/// envelopes through the cloud tunnel. See
/// `crates/nexus-pipeline/src/entity_sighting.rs` for the per-track
/// FSM and `WEDGE_PLAN.md §4` for the wire contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReidConfig {
    /// Master switch. `false` keeps the supervisor's per-frame
    /// scheduler tick alive (it's cheap) but installs the
    /// [`nexus_pipeline::NoopSightingHook`] so nothing reaches the
    /// cloud.
    #[serde(default)]
    pub enabled: bool,
    /// Optional ONNX model path. When `Some(_)` AND the engine is
    /// built with `--features ort`, the engine loads
    /// `nexus_reid::DinoV2Extractor`. When `None` (or `ort` feature
    /// off), the engine falls back to
    /// `nexus_reid::MockExtractor::with_config(model_id, dim)` —
    /// useful for end-to-end wire tests against a real cloud without
    /// shipping ONNX weights to the dev box.
    #[serde(default)]
    pub model_path: Option<PathBuf>,
    /// Model id string. MUST match the cloud's wire allowlist —
    /// "dinov2-s-v1" (384-dim, default) or "osnet-x1.0-v1" (512-dim).
    /// Anything else is rejected by the edge-gateway at ingest time.
    #[serde(default = "default_reid_model_id")]
    pub model_id: String,
    /// Embedding dimension. Must match `model_id`'s declared dim
    /// (384 for dinov2-s-v1, 512 for osnet-x1.0-v1).
    #[serde(default = "default_reid_dim")]
    pub dim: usize,
    /// Periodic re-emit cadence in seconds. After the first
    /// stable-track emit, the scheduler waits this long before
    /// firing again. Default 5s — bandwidth-friendly at ~7-10
    /// concurrent tracks per camera.
    #[serde(default = "default_reid_emit_interval_s")]
    pub emit_interval_s: u64,
    /// Concurrent-track count above which the scheduler switches
    /// the periodic re-emit branch to [`crowded_emit_interval_s`].
    /// `0` disables the adaptive cadence (always use
    /// `emit_interval_s`). M_PERF_CROWD B2 — defaults to 15.
    /// First-emit unaffected: freshly-stable entities still emit
    /// promptly so the cloud linker can stitch them.
    #[serde(default = "default_reid_crowded_track_threshold")]
    pub crowded_track_threshold: u32,
    /// Periodic re-emit cadence in seconds used while the per-camera
    /// tracked-object count exceeds [`crowded_track_threshold`].
    /// M_PERF_CROWD B2 — defaults to 15s, giving ~3× bandwidth
    /// reduction at 30+ concurrent tracks (30 × every-5s →
    /// 30 × every-15s).
    #[serde(default = "default_reid_crowded_emit_interval_s")]
    pub crowded_emit_interval_s: u64,
    /// Minimum tracker `age_frames` before the first emit fires.
    /// Filters out single-frame false positives that the tracker
    /// would otherwise let through. Default 5 frames (~165 ms at
    /// 30 fps; ~1 s at 5 fps).
    #[serde(default = "default_reid_min_track_age_frames")]
    pub min_track_age_frames: u32,
    /// M_PERF_CROWD B4 — worker-side bbox-width floor (pixels in
    /// supervisor-frame coordinates, pre-crop-resize) for re-id
    /// extraction. Snapshots whose source bbox width is below this
    /// value are dropped before the batched ORT call so we don't
    /// burn compute on crops too small to embed reliably.
    ///
    /// Threshold tuning:
    /// * **Reduce (20-24)** if same-person sightings from different
    ///   cameras/poses aren't linking — relaxation captures more
    ///   pose variations so the cross-camera kNN has more chances
    ///   to find common neighbors (trades some noise for recall).
    /// * **Increase (48-64)** if the same person is appearing as
    ///   multiple entities due to embedding drift across angles
    ///   — stricter filtering reduces noisy small-crop embeddings.
    ///
    /// DINOv2-S patch_size=14 reaches stability around 30-40px per
    /// side; below 20px drift becomes significant. Default 40
    /// (paired with a 96px height floor) keeps only crops large
    /// enough to embed cleanly — daytime-crowd validation showed
    /// sub-40px crops were the dominant source of cross-camera
    /// embedding drift. `0` disables the filter.
    #[serde(default = "default_reid_min_crop_w_px")]
    pub min_crop_w_px: u32,
    /// M_PERF_CROWD B4 — worker-side bbox-height floor (pixels) for
    /// re-id extraction. See [`min_crop_w_px`] for tuning guidance.
    /// Default 96. `0` disables the filter.
    #[serde(default = "default_reid_min_crop_h_px")]
    pub min_crop_h_px: u32,
    /// EP priority list for the ORT session. Ignored when
    /// `model_path` is `None`. Default mirrors `[inference].ep_priority`.
    #[serde(default = "default_ep_priority")]
    pub ep_priority: Vec<String>,
}

impl Default for ReidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: None,
            model_id: default_reid_model_id(),
            dim: default_reid_dim(),
            emit_interval_s: default_reid_emit_interval_s(),
            crowded_track_threshold: default_reid_crowded_track_threshold(),
            crowded_emit_interval_s: default_reid_crowded_emit_interval_s(),
            min_track_age_frames: default_reid_min_track_age_frames(),
            min_crop_w_px: default_reid_min_crop_w_px(),
            min_crop_h_px: default_reid_min_crop_h_px(),
            ep_priority: default_ep_priority(),
        }
    }
}

fn default_reid_model_id() -> String {
    "dinov2-s-v1".into()
}
fn default_reid_dim() -> usize {
    384
}
fn default_reid_emit_interval_s() -> u64 {
    5
}
fn default_reid_crowded_track_threshold() -> u32 {
    15
}
fn default_reid_crowded_emit_interval_s() -> u64 {
    15
}
fn default_reid_min_track_age_frames() -> u32 {
    5
}
fn default_reid_min_crop_w_px() -> u32 {
    40
}
fn default_reid_min_crop_h_px() -> u32 {
    96
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid generic-email sink, mutated per assertion below.
    fn email_sink_cfg() -> EmailSinkConfig {
        EmailSinkConfig {
            name: "site-ops".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            starttls: true,
            from_address: "nexus@example.com".to_string(),
            from_name: None,
            to: vec!["ops@example.com".to_string()],
            cc: Vec::new(),
            reply_to: None,
            subject_prefix: None,
            attach_snapshot: true,
            attach_clip: false,
            username: None,
            password: None,
            timeout_secs: 15,
        }
    }

    /// The `kind = "email"` discriminator is the `<kind>` half of every
    /// `alert_sink_outbox.sink_id` — pin it so a serde rename can never
    /// silently orphan historical outbox rows.
    #[test]
    fn email_sink_kind_tag_is_stable() {
        let cfg = SinkConfig::Email(email_sink_cfg());
        assert_eq!(cfg.kind(), "email");
        assert_eq!(cfg.name(), "site-ops");
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["kind"], "email");
    }

    /// Minimal TOML (only the required fields) must parse, with every
    /// optional field falling back to its documented default.
    #[test]
    fn email_sink_parses_from_minimal_toml() {
        let toml = r#"
kind = "email"
name = "site-ops"
smtp_host = "smtp.example.com"
from_address = "nexus@example.com"
to = ["ops@example.com"]
"#;
        let cfg: SinkConfig = toml::from_str(toml).unwrap();
        let SinkConfig::Email(e) = &cfg else {
            panic!("expected an email sink, got {cfg:?}");
        };
        assert_eq!(e.smtp_port, 587);
        assert!(e.starttls);
        assert!(e.attach_snapshot, "snapshots ride along by default");
        assert!(!e.attach_clip, "clips are opt-in (relay size limits)");
        assert_eq!(e.timeout_secs, 15);
        cfg.validate().unwrap();
    }

    /// A sink with no recipients would accept every outbox row and
    /// deliver nothing — indistinguishable from working. Reject it.
    #[test]
    fn email_sink_rejects_empty_recipient_list() {
        let mut e = email_sink_cfg();
        e.to.clear();
        assert!(SinkConfig::Email(e).validate().is_err());
    }

    #[test]
    fn email_sink_rejects_malformed_addresses() {
        for mutate in [
            (|e: &mut EmailSinkConfig| e.from_address = "nope".into()) as fn(&mut EmailSinkConfig),
            |e| e.to = vec!["nope".into()],
            |e| e.cc = vec!["nope".into()],
            |e| e.reply_to = Some("nope".into()),
        ] {
            let mut e = email_sink_cfg();
            mutate(&mut e);
            assert!(
                SinkConfig::Email(e.clone()).validate().is_err(),
                "expected rejection for {e:?}"
            );
        }
    }

    /// Half-configured SMTP AUTH silently authenticates as nobody, so
    /// the pair must be all-or-nothing.
    #[test]
    fn email_sink_rejects_half_configured_auth() {
        let mut e = email_sink_cfg();
        e.username = Some("nexus@example.com".into());
        assert!(SinkConfig::Email(e).validate().is_err());
    }

    /// The relay password must never leave the appliance, and echoing
    /// the sentinel back on a PUT must restore the stored value rather
    /// than overwrite it with the sentinel.
    #[test]
    fn email_sink_password_round_trips_through_redaction() {
        let mut stored = email_sink_cfg();
        stored.username = Some("nexus@example.com".into());
        stored.password = Some("s3cret".into());
        let stored = SinkConfig::Email(stored);

        let mut leaving = stored.clone();
        leaving.redact_secrets();
        assert!(leaving.has_redacted_secret());
        assert!(
            !serde_json::to_string(&leaving).unwrap().contains("s3cret"),
            "live password escaped the box"
        );

        // The console echoes the sentinel back for an unchanged secret.
        let mut incoming = leaving;
        incoming.restore_redacted_secrets_from(&stored);
        assert!(!incoming.has_redacted_secret());
        let SinkConfig::Email(e) = incoming else {
            unreachable!()
        };
        assert_eq!(e.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn defaults_validate() {
        let cfg = Config {
            cameras: vec![],
            ..Default::default()
        };
        cfg.validate().unwrap();
    }

    // --- M_NATIVE_ASPECT Phase 5: legacy shape remap ------------------------

    #[test]
    fn default_model_shape_is_on_the_ladder() {
        let m = ModelConfig::default();
        assert_eq!((m.input_width, m.input_height), (512, 288));
        assert_eq!(m.preset, "512x288");
        assert!(SHAPE_LADDER.contains(&(m.input_width, m.input_height)));
    }

    #[test]
    fn remap_legacy_squares_to_ladder() {
        for (sq, want) in [
            ((640u32, 640u32), (512u32, 288u32)),
            ((960, 960), (1024, 576)),
            ((1280, 1280), (1536, 864)),
        ] {
            let mut m = ModelConfig {
                input_width: sq.0,
                input_height: sq.1,
                preset: sq.0.to_string(),
                ..Default::default()
            };
            m.remap_legacy_shapes();
            assert_eq!((m.input_width, m.input_height), want, "{sq:?}");
            assert_eq!(m.preset, format!("{}x{}", want.0, want.1));
        }
    }

    #[test]
    fn remap_off_ladder_square_snaps_to_nearest_rung() {
        // 320² (102_400 px) is closest to the 512×288 rung (147_456 px).
        let mut m = ModelConfig {
            input_width: 320,
            input_height: 320,
            preset: "320".into(),
            ..Default::default()
        };
        m.remap_legacy_shapes();
        assert_eq!((m.input_width, m.input_height), (512, 288));
        assert_eq!(m.preset, "512x288");
    }

    #[test]
    fn remap_is_idempotent_for_ladder_shapes() {
        let mut m = ModelConfig {
            input_width: 1536,
            input_height: 864,
            preset: "1536x864".into(),
            ..Default::default()
        };
        m.remap_legacy_shapes();
        assert_eq!((m.input_width, m.input_height), (1536, 864));
        assert_eq!(m.preset, "1536x864");
    }

    #[test]
    fn remap_recurses_into_ensemble_members() {
        let mut parent = ModelConfig {
            kind: "ensemble".into(),
            members: vec![ModelConfig {
                input_width: 960,
                input_height: 960,
                preset: "960".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        parent.remap_legacy_shapes();
        assert_eq!(
            (
                parent.members[0].input_width,
                parent.members[0].input_height
            ),
            (1024, 576)
        );
        assert_eq!(parent.members[0].preset, "1024x576");
    }

    /// Phase 7.6.6 / REPO_BOUNDARY R5c §5 — the generic LAN device proxy
    /// is opt-in: with `[lan_proxy]` absent (or defaulted) the feature is
    /// off. This pins the default so the edge never proxies LAN traffic
    /// unless an operator explicitly enables it.
    #[test]
    fn lan_proxy_is_off_by_default() {
        assert!(!LanProxyConfig::default().enabled);
        let cfg = Config {
            cameras: vec![],
            ..Default::default()
        };
        assert!(!cfg.lan_proxy.enabled);
    }

    /// M-Alert-Clip — alert clips are ON by default; operators disable
    /// them per-org / per-core from the cloud console's delivery settings
    /// (`DeliverySettings.attach_alert_clip`), not by editing config.
    /// Pins the defaults so a regression can't silently change the
    /// capability switch or the window/timeout tunables.
    #[test]
    fn alert_clips_are_on_by_default() {
        let d = AlertClipsConfig::default();
        assert!(d.enabled);
        assert_eq!(d.pre_secs, 3);
        assert_eq!(d.post_secs, 5);
        assert_eq!(d.max_encode_width, 1280);
        assert_eq!(d.build_timeout_secs, 30);
        // The ClipsConfig default embeds the enabled AlertClipsConfig.
        assert!(ClipsConfig::default().alert_clips.enabled);
        // A `[clips]` section without `[clips.alert_clips]` deserialises to
        // the enabled default.
        let clips: ClipsConfig = toml::from_str("").unwrap();
        assert!(clips.alert_clips.enabled);
        assert_eq!(clips.alert_clips.post_secs, 5);
        // An explicit `enabled = false` still hard-disables the capability.
        let off: ClipsConfig = toml::from_str("[alert_clips]\nenabled = false\n").unwrap();
        assert!(!off.alert_clips.enabled);
    }

    /// Every alert/event must get an underlying full-resolution motion
    /// clip by default, so a keyframe-only detection (no sustained
    /// motion → no tracker `Born`) still leaves surrounding native-res
    /// video. Pins the default ON and the explicit override OFF.
    #[test]
    fn record_motion_clip_on_alert_is_on_by_default() {
        assert!(ClipsConfig::default().record_motion_clip_on_alert);
        // Missing key deserialises to the enabled default.
        let clips: ClipsConfig = toml::from_str("").unwrap();
        assert!(clips.record_motion_clip_on_alert);
        // Explicit `false` disables it (reverts to unlinked alerts on
        // motionless frames).
        let off: ClipsConfig = toml::from_str("record_motion_clip_on_alert = false\n").unwrap();
        assert!(!off.record_motion_clip_on_alert);
    }

    #[test]
    fn sureview_secret_redact_round_trip() {
        let json = r#"{"kind":"sureview","name":"front","api_key":"LIVE-KEY","system_identifier":"site-1"}"#;
        let stored: SinkConfig = serde_json::from_str(json).unwrap();

        // Redaction replaces the live key with the sentinel.
        let mut redacted = stored.clone();
        redacted.redact_secrets();
        assert!(redacted.has_redacted_secret());
        if let SinkConfig::SureView(s) = &redacted {
            assert_eq!(s.api_key, SinkConfig::REDACTED_SECRET);
        } else {
            panic!("expected sureview");
        }

        // An incoming PUT that echoes the sentinel back gets the
        // live key restored from the stored config.
        let mut incoming = redacted.clone();
        incoming.restore_redacted_secrets_from(&stored);
        assert!(!incoming.has_redacted_secret());
        if let SinkConfig::SureView(s) = &incoming {
            assert_eq!(s.api_key, "LIVE-KEY");
        } else {
            panic!("expected sureview");
        }
    }

    #[test]
    fn restore_is_a_noop_when_secret_changed() {
        let stored: SinkConfig = serde_json::from_str(
            r#"{"kind":"sureview","name":"front","api_key":"OLD","system_identifier":"s"}"#,
        )
        .unwrap();
        // Operator typed a brand-new key — NOT the sentinel — so
        // restore must leave it untouched.
        let mut incoming: SinkConfig = serde_json::from_str(
            r#"{"kind":"sureview","name":"front","api_key":"NEW","system_identifier":"s"}"#,
        )
        .unwrap();
        incoming.restore_redacted_secrets_from(&stored);
        if let SinkConfig::SureView(s) = &incoming {
            assert_eq!(s.api_key, "NEW");
        } else {
            panic!("expected sureview");
        }
    }

    #[test]
    fn pool_requires_workers() {
        let mut cfg = Config::default();
        cfg.inference.backend = InferenceBackendKind::Pool;
        cfg.inference.workers = 0;
        assert!(cfg.validate().is_err());
    }

    // M-Admin Phase 0 closeout — secure-by-default. The legacy
    // `None` and `DevToken` variants are gone; a fresh install
    // (empty TOML, or one with `[auth]\n` only) lands on `Local`
    // and the engine auto-provisions an admin secret + prints a
    // one-time admin OTP at WARN. Failing this test would mean a
    // new install could silently boot without a real credential.
    #[test]
    fn auth_mode_default_is_local() {
        let auth: AuthConfig = Default::default();
        assert_eq!(auth.mode, AuthMode::Local);
        let parsed: AuthConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.mode, AuthMode::Local);
    }

    // M-Admin Phase 0 closeout — pre-existing dev installs whose
    // nexus.toml has no `[auth]` block now land on `Local` (the
    // new default). No grandfathering; the engine auto-provisions
    // an admin secret on first boot.
    #[test]
    fn load_with_compat_missing_auth_lands_on_local() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(&path, "[server]\napi_bind = \"127.0.0.1:8089\"\n").unwrap();
        let (cfg, _notice) = Config::load_with_compat(&path).unwrap();
        assert_eq!(cfg.auth.mode, AuthMode::Local);
    }

    /// Legacy `auth.mode = "none"` from pre-Phase-0 configs MUST
    /// be rejected at load time with a clear error so operators
    /// upgrade explicitly rather than silently landing on a
    /// different posture.
    #[test]
    fn load_with_compat_rejects_legacy_mode_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(&path, "[auth]\nmode = \"none\"\n").unwrap();
        let err = Config::load_with_compat(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("none") && msg.contains("no longer supported"),
            "{msg}"
        );
    }

    /// Same as above for the retired `dev_token` mode.
    #[test]
    fn load_with_compat_rejects_legacy_mode_dev_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(&path, "[auth]\nmode = \"dev_token\"\n").unwrap();
        let err = Config::load_with_compat(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("dev_token") && msg.contains("no longer supported"),
            "{msg}"
        );
    }

    /// The legacy-mode reject must ignore the same string when it
    /// only appears inside a `#` comment (e.g. nexus.example.toml
    /// listing the historical option set in a doc comment).
    #[test]
    fn load_with_compat_ignores_legacy_mode_in_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(
            &path,
            "[auth]\nmode = \"local\"  # historical: none | dev_token | local\n",
        )
        .unwrap();
        let (cfg, _) = Config::load_with_compat(&path).unwrap();
        assert_eq!(cfg.auth.mode, AuthMode::Local);
    }

    // -----------------------------------------------------------------------
    // M6 — AuthMode + OidcConfig + RuntimeAuthConfig
    // -----------------------------------------------------------------------

    #[test]
    fn auth_mode_local_and_hybrid_parse() {
        for (s, expected) in [
            ("local", AuthMode::Local),
            ("oidc", AuthMode::Oidc),
            ("hybrid", AuthMode::Hybrid),
        ] {
            let toml_src = format!("mode = \"{s}\"\n");
            let parsed: AuthConfig = toml::from_str(&toml_src).unwrap();
            assert_eq!(parsed.mode, expected, "round-trip for {s:?}");
        }
    }

    #[test]
    fn auth_mode_allows_local_and_oidc_matrix() {
        // Pinned matrix so future variants don't accidentally flip
        // a bit and let an `Oidc`-only deployment accept local login.
        let cases: &[(AuthMode, bool, bool)] = &[
            (AuthMode::Local, true, false),
            (AuthMode::Oidc, false, true),
            (AuthMode::Hybrid, true, true),
        ];
        for (mode, local, oidc) in cases.iter().copied() {
            assert_eq!(mode.allows_local(), local, "{mode:?}.allows_local");
            assert_eq!(mode.allows_oidc(), oidc, "{mode:?}.allows_oidc");
        }
    }

    #[test]
    fn oidc_config_defaults_supply_sane_scopes_and_claims() {
        let src = r#"
issuer = "https://auth.example.com"
audience = "nexus"
"#;
        let cfg: OidcConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.scopes, vec!["openid", "profile", "email", "groups"]);
        assert_eq!(
            cfg.role_claims,
            vec!["groups", "roles", "https://nexus.local/role"]
        );
        assert!(cfg.role_map.is_empty());
        assert!(!cfg.deny_unmapped);
        assert!(cfg.client_id.is_none());
        assert!(cfg.display_name.is_none());
    }

    #[test]
    fn oidc_role_map_parses_per_role_lists() {
        let src = r#"
issuer = "https://auth.example.com"
audience = "nexus"
deny_unmapped = true

[role_map]
admin = ["nexus-admins"]
operator = ["nexus-operators", "security-team"]
"#;
        let cfg: OidcConfig = toml::from_str(src).unwrap();
        assert!(cfg.deny_unmapped);
        assert_eq!(cfg.role_map.admin, vec!["nexus-admins"]);
        assert_eq!(
            cfg.role_map.operator,
            vec!["nexus-operators", "security-team"]
        );
        assert!(cfg.role_map.viewer.is_empty());
        assert!(!cfg.role_map.is_empty());
    }

    #[test]
    fn runtime_auth_lockout_defaults_match_design() {
        // The defaults are wire-pinned (5 / 15min / 15min) — these
        // are the OWASP-ish baseline the M6 design committed to. If
        // a future PR wants to tune them, change this test in lock-
        // step with the doc.
        let r: RuntimeAuthConfig = Default::default();
        assert_eq!(r.lockout.max_attempts, 5);
        assert_eq!(r.lockout.window_secs, 900);
        assert_eq!(r.lockout.lockout_secs, 900);
    }

    #[test]
    fn runtime_audit_retention_default_is_one_year() {
        let r: RuntimeAuditConfig = Default::default();
        assert_eq!(r.retention_days, 365);
    }

    #[test]
    fn runtime_auth_overrides_round_trip_via_toml() {
        let src = r#"
state_dir = "/var/lib/nexus/state"

[auth.lockout]
max_attempts = 10
window_secs = 300
lockout_secs = 60

[audit]
retention_days = 90
"#;
        let rc: RuntimeConfig = toml::from_str(src).unwrap();
        assert_eq!(rc.auth.lockout.max_attempts, 10);
        assert_eq!(rc.auth.lockout.window_secs, 300);
        assert_eq!(rc.auth.lockout.lockout_secs, 60);
        assert_eq!(rc.audit.retention_days, 90);
    }

    // -----------------------------------------------------------------
    // [runtime.decode] hardware-decode knob
    // -----------------------------------------------------------------

    /// Upgrade-reach guarantee (Wedge P3.1): a config that predates
    /// the `[runtime.decode]` knob (no `[decode]` table at all)
    /// deserialises with `DecodeMode::Auto`, so an old core that
    /// upgrades onto a VA-capable box auto-enables hardware decode
    /// with zero config migration.
    #[test]
    fn runtime_decode_defaults_to_auto() {
        let from_default: RuntimeConfig = Default::default();
        assert_eq!(from_default.decode.mode, DecodeMode::Auto);
        let from_empty: RuntimeConfig = toml::from_str("").unwrap();
        assert_eq!(from_empty.decode.mode, DecodeMode::Auto);
        // A legacy config that sets an unrelated [runtime] knob but
        // carries no [decode] table still defaults to Auto.
        let legacy: RuntimeConfig = toml::from_str("state_dir = \"/var/lib/nexus/state\"").unwrap();
        assert_eq!(legacy.decode.mode, DecodeMode::Auto);
    }

    /// Upgrade-reach guarantee at the TOP-LEVEL `Config` boundary (Wedge
    /// P3.1): a complete operator `nexus.toml` that predates the decode
    /// knob — i.e. has `[runtime]`, `[server]`, `[inference]` … but no
    /// `[runtime.decode]` section — still yields `DecodeMode::Auto`. This
    /// is the exact shape a preserved pre-upgrade config has, and Auto is
    /// what lets the upgraded engine auto-probe for VA decode without any
    /// config migration. If this regresses (e.g. the default flips to
    /// `Software`), every upgraded box silently loses hardware decode.
    #[test]
    fn full_config_without_decode_section_defaults_to_auto() {
        // A trimmed-but-realistic legacy config: several sections present,
        // deliberately NO [runtime.decode].
        let legacy = r#"
[runtime]
worker_threads = 16
blocking_threads = 16

[server]
api_bind = "0.0.0.0:8089"

[inference]
backend = "pool"
ep_priority = ["hailo", "cpu"]

[bus]
capacity = 2048
"#;
        let cfg: Config = toml::from_str(legacy).expect("legacy config must parse");
        assert_eq!(cfg.runtime.decode.mode, DecodeMode::Auto);
        // Sanity: the sections that ARE present still parsed.
        assert_eq!(cfg.runtime.worker_threads, 16);
        assert_eq!(cfg.inference.ep_priority, vec!["hailo", "cpu"]);
    }

    /// `[decode] mode = "..."` round-trips for every variant, and the
    /// serialised token matches the documented lowercase spelling
    /// (the engine and installer both rely on these exact tokens).
    #[test]
    fn runtime_decode_mode_round_trips_via_toml() {
        for (token, mode) in [
            ("auto", DecodeMode::Auto),
            ("va", DecodeMode::Va),
            ("msdk", DecodeMode::Msdk),
            ("nvdec", DecodeMode::Nvdec),
            ("software", DecodeMode::Software),
        ] {
            let src = format!("[decode]\nmode = \"{token}\"\n");
            let rc: RuntimeConfig = toml::from_str(&src).unwrap();
            assert_eq!(rc.decode.mode, mode, "parse {token}");
            let out = toml::to_string(&RuntimeDecodeConfig { mode }).unwrap();
            assert!(
                out.contains(&format!("mode = \"{token}\"")),
                "serialise {token}: {out}"
            );
        }
    }

    /// `[runtime.decode]` rejects unknown keys (deny_unknown_fields)
    /// so a typo'd knob fails loudly at boot instead of being
    /// silently ignored.
    #[test]
    fn runtime_decode_rejects_unknown_key() {
        let src = "[decode]\nmode = \"auto\"\nbogus = true\n";
        let err = toml::from_str::<RuntimeConfig>(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus") || msg.contains("unknown"),
            "expected unknown-field error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // M3.1 — VisualPromptRef wire shape + defaults
    // -----------------------------------------------------------------

    /// `runtime.visual_prompts_dir` defaults to /var/lib/nexus/visual_prompts
    /// when the TOML omits it. Asserts the default helper agrees with
    /// the spec in docs/M3_OPEN_VOCAB_VISUAL.md so a future tweak of
    /// either side trips this lock.
    #[test]
    fn runtime_visual_prompts_dir_default_matches_spec() {
        let r: RuntimeConfig = Default::default();
        assert_eq!(
            r.visual_prompts_dir,
            std::path::PathBuf::from("/var/lib/nexus/visual_prompts")
        );
        let from_empty: RuntimeConfig = toml::from_str("").unwrap();
        assert_eq!(
            from_empty.visual_prompts_dir,
            std::path::PathBuf::from("/var/lib/nexus/visual_prompts")
        );
    }

    /// Operators can override `runtime.visual_prompts_dir` in TOML
    /// (operator may want it on a faster SSD partition separate from
    /// `state_dir`). Confirms serde sees the field.
    #[test]
    fn runtime_visual_prompts_dir_round_trips_via_toml() {
        let src = r#"
state_dir = "/var/lib/nexus/state"
visual_prompts_dir = "/mnt/fast/visual_prompts"
"#;
        let rc: RuntimeConfig = toml::from_str(src).unwrap();
        assert_eq!(
            rc.visual_prompts_dir,
            std::path::PathBuf::from("/mnt/fast/visual_prompts")
        );
    }

    /// Wire-shape lock: a `[[cameras]]` table with `visual_prompts =
    /// [{ id = 1, label = "amazon_van" }]` round-trips through TOML
    /// → CameraConfig → JSON. Catches accidental rename / removal of
    /// the field or its sub-keys.
    #[test]
    fn camera_visual_prompts_round_trip_via_toml() {
        let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
visual_prompts = [
  { id = 1, label = "amazon_van" },
  { id = 7, label = "fedex_truck" },
]
"#;
        let cam: CameraConfig = toml::from_str(src).unwrap();
        assert_eq!(cam.detector.visual_prompts.len(), 2);
        assert_eq!(cam.detector.visual_prompts[0].id, 1);
        assert_eq!(cam.detector.visual_prompts[0].label, "amazon_van");
        assert_eq!(cam.detector.visual_prompts[1].id, 7);
        assert_eq!(cam.detector.visual_prompts[1].label, "fedex_truck");

        // The field must remain flat at the wire boundary (no
        // `[detector]` envelope leaked by the existing #[serde(flatten)]
        // refactor).
        let v = serde_json::to_value(&cam).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("visual_prompts"));
        assert!(!obj.contains_key("detector"));
    }

    /// A camera that omits `visual_prompts` must still load (existing
    /// nexus.toml files predate M3.1). Defaults to an empty Vec via
    /// `#[serde(default)]` on the field.
    #[test]
    fn camera_visual_prompts_defaults_to_empty_when_absent() {
        let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
"#;
        let cam: CameraConfig = toml::from_str(src).unwrap();
        assert!(cam.detector.visual_prompts.is_empty());
    }

    /// Phase 7.6.1 — ONVIF endpoint + credentials round-trip through the
    /// `cameras.config_json` blob (which is just `serde_json` of
    /// `CameraConfig`), and a camera that omits them serializes WITHOUT
    /// an `onvif` key so existing blobs stay byte-identical (no SQLite
    /// migration). Credentials are edge-resident only; the roster
    /// redaction is asserted separately in `nexus-engine`.
    #[test]
    fn camera_onvif_round_trips_and_is_omitted_when_empty() {
        // Absent → empty, and serializes away (config_json unchanged).
        let bare: CameraConfig = toml::from_str(
            r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
"#,
        )
        .unwrap();
        assert!(bare.onvif.is_empty());
        let v = serde_json::to_value(&bare).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("onvif"),
            "empty onvif must be skipped so config_json is unchanged"
        );

        // Present → parses, and JSON round-trips losslessly.
        let cam: CameraConfig = toml::from_str(
            r#"
id = 2
name = "ptz_cam"
url = "rtsp://example/ptz"
onvif = { endpoint = "http://192.168.1.64/onvif/device_service", username = "admin", password = "s3cret" }
"#,
        )
        .unwrap();
        assert_eq!(
            cam.onvif.endpoint.as_deref(),
            Some("http://192.168.1.64/onvif/device_service")
        );
        assert_eq!(cam.onvif.username.as_deref(), Some("admin"));
        assert_eq!(cam.onvif.password.as_deref(), Some("s3cret"));

        let json = serde_json::to_string(&cam).unwrap();
        let back: CameraConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.onvif, cam.onvif);
    }

    /// Phase 7.6.7 — the `talk_down` (speaker) block round-trips through
    /// JSON and serializes away when empty so non-speaker cameras keep
    /// an unchanged `config_json` (no SQLite migration). Edge-resident
    /// only; the roster redaction is asserted in `nexus-engine`.
    #[test]
    fn camera_talk_down_round_trips_and_is_omitted_when_empty() {
        // Absent → empty, serializes away.
        let bare: CameraConfig = toml::from_str(
            r#"
id = 1
name = "fixed_cam"
url = "rtsp://example/cam"
"#,
        )
        .unwrap();
        assert!(bare.talk_down.is_empty());
        let v = serde_json::to_value(&bare).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("talk_down"),
            "empty talk_down must be skipped so config_json is unchanged"
        );

        // Present → parses, and JSON round-trips losslessly.
        let cam: CameraConfig = toml::from_str(
            r#"
id = 2
name = "speaker_cam"
url = "rtsp://example/spk"
talk_down = { speaker_present = true, backchannel_codec = "PCMU", backchannel_url = "rtsp://example/spk/backchannel" }
"#,
        )
        .unwrap();
        assert!(cam.talk_down.speaker_present);
        assert_eq!(cam.talk_down.backchannel_codec.as_deref(), Some("PCMU"));
        assert_eq!(
            cam.talk_down.backchannel_url.as_deref(),
            Some("rtsp://example/spk/backchannel")
        );

        let json = serde_json::to_string(&cam).unwrap();
        let back: CameraConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.talk_down, cam.talk_down);
    }

    /// `CameraConfigUpdate` (fan-pushed to every detector slot on
    /// reload) carries the visual-prompt attachments. JSON round-trip
    /// asserts the new field is on the wire and defaults to empty
    /// when an older publisher omits it.
    #[test]
    fn camera_config_update_visual_prompts_round_trip_via_json() {
        let update = CameraConfigUpdate {
            camera_id: 42,
            prompts: vec!["person".into()],
            visual_prompts: vec![VisualPromptRef {
                id: 9,
                label: "delivery_van".into(),
            }],
            model: ModelConfig::default(),
            generation: 3,
        };
        let json = serde_json::to_string(&update).unwrap();
        let back: CameraConfigUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.visual_prompts.len(), 1);
        assert_eq!(back.visual_prompts[0].id, 9);
        assert_eq!(back.visual_prompts[0].label, "delivery_van");

        // Backwards-compat: a publisher that predates the field
        // emits JSON without `visual_prompts`. Receiver must accept it.
        let legacy = r#"{
            "camera_id": 42,
            "prompts": ["person"],
            "model": {},
            "generation": 3
        }"#;
        let parsed: CameraConfigUpdate = serde_json::from_str(legacy).unwrap();
        assert!(parsed.visual_prompts.is_empty());
    }

    /// `VisualPromptRef` denies unknown fields — typos in admin JSON
    /// surface as an error rather than silently dropping (e.g.
    /// `lable` instead of `label`).
    #[test]
    fn visual_prompt_ref_denies_unknown_fields() {
        let bad = r#"{ "id": 1, "label": "amazon_van", "lable": "typo" }"#;
        assert!(serde_json::from_str::<VisualPromptRef>(bad).is_err());
    }

    /// Wire-shape lock for the camera/rule refactor: the public TOML
    /// keys for every shipped config under `config/` must still parse
    /// after the `#[serde(flatten)]` regrouping (no nested `[ingest]`,
    /// `[detector]`, `[gates]`, etc. tables introduced). Every camera
    /// keeps reading `url`, `enabled`, `max_fps`, `prompts`,
    /// `model_override`, `parking_lot_mode` at the top of the
    /// `[[cameras]]` array; every rule keeps reading `when`,
    /// `severity`, `camera_filter`, `zones`, `min_track_age_ms`,
    /// `consecutive_frames`, `cooldown_ms` at the top of `[[rules]]`.
    /// If this test ever needs a fixture update, you have broken
    /// every existing operator's nexus.toml — back out the change.
    #[test]
    fn shipped_configs_round_trip_flat_wire_shape() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root above crates/nexus-config");
        let config_dir = repo_root.join("config");
        // Scan the shipped top-level `config/` samples. The per-box
        // templates that used to live under `config/tiers/` were retired
        // with the capability-based generator (M_HWCONFIG): the live
        // /etc/nexus/nexus.toml is now produced by `nexus-probe
        // emit-config`, whose output is type-checked + `deny_unknown_fields`
        // safe by construction (built from a real `Config`), and the
        // generated shape is covered by the nexus-probe golden_config
        // fixtures. This test still guards the hand-written sample configs.
        let mut toml_paths: Vec<std::path::PathBuf> = Vec::new();
        let entries = std::fs::read_dir(&config_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", config_dir.display()));
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("toml") {
                toml_paths.push(path);
            }
        }
        let mut checked = 0usize;
        for path in &toml_paths {
            let cfg = Config::load(path).unwrap_or_else(|e| panic!("load {}: {e}", path.display()));
            // Validate via the same path the engine uses on boot.
            cfg.validate()
                .unwrap_or_else(|e| panic!("validate {}: {e}", path.display()));
            for cam in &cfg.cameras {
                let v = serde_json::to_value(cam).unwrap();
                let obj = v.as_object().expect("CameraConfig serializes as an object");
                // Top-level keys must be flat — no `ingest`/`detector`/`behavior` envelopes.
                for forbidden in ["ingest", "detector", "behavior"] {
                    assert!(
                        !obj.contains_key(forbidden),
                        "{}: CameraConfig leaked a `{forbidden}` envelope to the wire \
                         (broke #[serde(flatten)] guarantee)",
                        path.display()
                    );
                }
                // Anchor a few must-stay-flat keys so an accidental
                // un-flatten in the future tips this test over loudly.
                for required in ["id", "name", "url", "enabled"] {
                    assert!(
                        obj.contains_key(required),
                        "{}: CameraConfig dropped flat key `{required}`",
                        path.display()
                    );
                }
            }
            for rule in &cfg.rules.inline {
                let v = serde_json::to_value(rule).unwrap();
                let obj = v.as_object().expect("RuleConfig serializes as an object");
                for forbidden in ["predicate", "gates", "debounce"] {
                    assert!(
                        !obj.contains_key(forbidden),
                        "{}: RuleConfig leaked a `{forbidden}` envelope to the wire \
                         (broke #[serde(flatten)] guarantee)",
                        path.display()
                    );
                }
                for required in ["id", "name", "when", "severity"] {
                    assert!(
                        obj.contains_key(required),
                        "{}: RuleConfig dropped flat key `{required}`",
                        path.display()
                    );
                }
            }
            checked += 1;
        }
        assert!(
            checked >= 3,
            "expected to round-trip at least 3 shipped top-level sample TOMLs \
             (nexus.example.toml, single-camera.toml, single-camera.youtube.toml) \
             from {} (found {checked})",
            config_dir.display()
        );
    }

    // -----------------------------------------------------------------------
    // M_TILE_REINFER (G1) Phase B1 — per-camera tile knobs.
    // -----------------------------------------------------------------------

    /// Default `CameraBehavior` leaves every tile knob unset, so
    /// pre-G1 configs round-trip unchanged.
    #[test]
    fn tile_knobs_default_to_none() {
        let b = CameraBehavior::default();
        assert!(b.tile_enabled.is_none());
        assert!(b.tile_trigger.is_none());
        assert!(b.tile_max_per_frame.is_none());
        assert!(b.tile_grid.is_none());
    }

    /// Wire-shape lock — flat top-level TOML keys, snake-case grid
    /// variant. The 4 fields land directly under `[[cameras]]` thanks
    /// to the existing `#[serde(flatten)] behavior` on `CameraConfig`.
    #[test]
    fn tile_knobs_round_trip_via_toml() {
        let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
tile_enabled = true
tile_trigger = 12
tile_max_per_frame = 4
tile_grid = "g3x3"
"#;
        let cam: CameraConfig = toml::from_str(src).unwrap();
        assert_eq!(cam.behavior.tile_enabled, Some(true));
        assert_eq!(cam.behavior.tile_trigger, Some(12));
        assert_eq!(cam.behavior.tile_max_per_frame, Some(4));
        assert_eq!(cam.behavior.tile_grid, Some(TileGridConfig::G3x3));

        // Wire JSON must keep the keys flat (no `behavior` envelope).
        let v = serde_json::to_value(&cam).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("tile_enabled"));
        assert!(obj.contains_key("tile_grid"));
        assert!(!obj.contains_key("behavior"));
    }

    /// `tile_enabled = true` with the global model `kind = "ensemble"`
    /// must be rejected at load time. The ban is per the wedge plan
    /// (`docs/edge-core/M_TILE_REINFER.md`): per-member tile budgeting
    /// is out of scope for v1.
    #[test]
    fn tile_enabled_rejected_on_ensemble_global() {
        let mut cfg = Config::default();
        cfg.inference.model.kind = "ensemble".into();
        let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true
"#;
        cfg.cameras.push(toml::from_str(cam_src).unwrap());
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("ensemble") && err.to_string().contains("tile_enabled"),
            "{err}"
        );
    }

    /// Same ban, triggered via per-camera `model_override`. The
    /// effective-kind resolution prefers the override over the global.
    #[test]
    fn tile_enabled_rejected_on_ensemble_override() {
        let mut cfg = Config::default();
        // Global is fine (default kind), but the camera overrides
        // to ensemble.
        let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true

[model_override]
kind = "ensemble"
"#;
        cfg.cameras.push(toml::from_str(cam_src).unwrap());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("ensemble"), "{err}");
    }

    /// `tile_enabled = true` is fine when the camera overrides
    /// AWAY from a global ensemble back to a single detector — the
    /// override wins.
    #[test]
    fn tile_enabled_ok_when_override_escapes_global_ensemble() {
        let mut cfg = Config::default();
        cfg.inference.model.kind = "ensemble".into();
        let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true

[model_override]
kind = "yolo_world"
"#;
        cfg.cameras.push(toml::from_str(cam_src).unwrap());
        cfg.validate()
            .expect("override to non-ensemble must validate");
    }

    /// `tile_enabled = None` or `Some(false)` is always accepted,
    /// even on ensemble — the ban only fires for true.
    #[test]
    fn tile_disabled_is_always_accepted_even_on_ensemble() {
        let mut cfg = Config::default();
        cfg.inference.model.kind = "ensemble".into();
        let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = false
tile_trigger = 12
"#;
        cfg.cameras.push(toml::from_str(cam_src).unwrap());
        cfg.validate().unwrap();
    }
}
