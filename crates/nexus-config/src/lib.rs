//! TOML-backed configuration for the Nexus edge engine.
//!
//! Every backend-selectable layer exposes a `backend` field so operators can
//! pin the implementation. Scale knobs (`workers`, `capacity`, `worker_threads`)
//! live alongside the backend choice — the config file is the only place the
//! deployment topology is declared.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use nexus_types::CameraId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
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
/// [`Config::load_with_compat`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompatNotice {
    /// True when the on-disk file had no `[auth]` section and so
    /// the loader pinned `auth.mode = none` (instead of using the
    /// new `dev_token` default that landed in M-Install
    /// Checkpoint 2). Engines emit a 7-day-deprecation WARN log
    /// when this is set.
    pub auth_grandfathered: bool,
}

/// Cheap line-scan check for `[<name>]` at the top level of a TOML
/// document. Used by [`Config::load_with_compat`] to detect a
/// missing `[auth]` section without re-parsing the file. Lines
/// inside other tables (`[runtime.foo]`) are intentionally ignored
/// — only an exact `[name]` header (after trimming whitespace and
/// stripping inline comments) counts.
///
/// This is a structural check, not a full TOML parser; it relies
/// on the standard TOML rule that table headers occupy their own
/// line. Round-tripping through `toml::from_str` afterwards is
/// what actually validates the file.
fn toml_has_top_level_table(txt: &str, name: &str) -> bool {
    let target = format!("[{name}]");
    for line in txt.lines() {
        // Strip inline comments — TOML comments start with `#` and
        // run to end-of-line. A `#` inside a string isn't a comment,
        // but table headers can't contain strings, so the simple
        // strip is sound for headers.
        let no_comment = match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        };
        if no_comment.trim() == target {
            return true;
        }
    }
    false
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
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let txt = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&txt)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Same as [`Config::load`] but reports compatibility shims
    /// applied to the parsed config so the engine can surface them
    /// at boot. Currently the only shim is the M-Install
    /// Checkpoint 2 auth grandfather: configs whose file predates
    /// the `[auth]` section keep `mode = none` for one
    /// deprecation window instead of being silently flipped to the
    /// new `dev_token` default.
    pub fn load_with_compat(path: impl AsRef<Path>) -> Result<(Self, CompatNotice), ConfigError> {
        let txt = std::fs::read_to_string(path)?;
        let auth_section_present = toml_has_top_level_table(&txt, "auth");
        let mut cfg: Config = toml::from_str(&txt)?;
        let mut notice = CompatNotice::default();
        if !auth_section_present {
            // Pre-Checkpoint-2 dev installs never wrote an [auth]
            // block; serde's `default` would now hand them
            // `DevToken` and lock them out on upgrade. Pin them
            // back to `None` and let the engine WARN for one week.
            cfg.auth.mode = AuthMode::None;
            notice.auth_grandfathered = true;
        }
        cfg.validate()?;
        Ok((cfg, notice))
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
    /// M2.1 motion-clip recording + safety-floor configuration.
    #[serde(default)]
    pub clips: ClipsConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            blocking_threads: default_blocking_threads(),
            state_dir: default_state_dir(),
            clips: ClipsConfig::default(),
        }
    }
}

fn default_blocking_threads() -> usize {
    8
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/state")
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

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_api_bind")]
    pub api_bind: String,
    /// Filesystem path served as the SPA root. The Dockerfile installs
    /// the built UI here; locally `npm run build` puts it under `ui/dist`.
    #[serde(default = "default_ui_root")]
    pub ui_root: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_bind: default_api_bind(),
            ui_root: default_ui_root(),
        }
    }
}

fn default_api_bind() -> String {
    "0.0.0.0:8089".to_string()
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
    "info,nexus=debug".to_string()
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
    /// When `mode = "dev_token"`, requests must include `Authorization: Bearer <token>`.
    #[serde(default)]
    pub dev_token: Option<String>,
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
    /// No auth (dev only). The engine refuses to bind anything other
    /// than loopback when this mode is active — see
    /// `nexus-engine`'s startup checks. Existing dev installs whose
    /// `nexus.toml` predates the auth section are grandfathered to
    /// this mode for 7 days; see `Config::load_with_compat`.
    None,
    // M-Install Checkpoint 2 — secure by default. Anything deployed
    // beyond a hand-rolled `mode = "none"` opt-in lands on the
    // dev-token path automatically.
    /// Default. On first boot the engine generates a 32-byte
    /// URL-safe random token at `/var/lib/nexus/dev-token` (mode
    /// 0600) and prints it to the WARN log; clients send it as
    /// `Authorization: Bearer <token>` on every request.
    #[default]
    DevToken,
    Oidc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: Option<String>,
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
    /// "yolo" (closed-vocab YOLOv26-nano, default) | "open_vocab" | "classifier_ensemble" | "mock".
    ///
    /// `yolo` matches the v1 ship — `models/yolo26n_dynamic.onnx` driven
    /// by a model-pack manifest with 320 / 640 / 1280 presets.
    #[serde(default = "default_model_kind")]
    pub kind: String,
    /// Optional model-pack directory containing `models-manifest.json`.
    /// When set, the engine resolves `preset` against the manifest and
    /// ignores `input_width` / `input_height`.
    #[serde(default)]
    pub pack_path: Option<PathBuf>,
    /// Pack preset name — "320" / "640" / "1280" for the shipped yolo26n
    /// dynamic model. T10 picks 320, T24/T36/T36-S/T64 pick 640.
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_input_width")]
    pub input_width: u32,
    #[serde(default = "default_input_height")]
    pub input_height: u32,
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
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
/// Default EP order matches the documented hardware pyramid:
///   T10/T24/T36/T36-S → openvino   (Intel iGPU/dGPU/NPU)
///   T64               → tensorrt → cuda
///   anything else     → cpu
/// Per-tier configs in `config/tiers/` override this with the right
/// short list for the box (e.g. T36-S adds "npu" between openvino and cpu;
/// T64 leads with "tensorrt"). `coreml` is dev-only and excluded from
/// production defaults — opt in explicitly in your config if you need it.
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
    "640".into()
}
fn default_input_width() -> u32 {
    640
}
fn default_input_height() -> u32 {
    640
}
fn default_score_threshold() -> f32 {
    0.30
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
    #[default]
    IouNaive,
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
    /// When true, write/load the per-camera anchor registry to disk
    /// under `runtime.state_dir`. v1 default: true.
    #[serde(default = "default_true")]
    pub persistence_enabled: bool,
}

impl Default for StaticObjectConfig {
    fn default() -> Self {
        Self {
            dwell_frames: default_static_object_dwell_frames(),
            significant_movement_pixels: default_static_object_significant_movement_pixels(),
            significant_movement_frames: default_static_object_significant_movement_frames(),
            movement_ema_alpha: default_static_object_movement_ema_alpha(),
            match_distance_pixels: default_static_object_match_distance_pixels(),
            persistence_enabled: true,
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
}

/// Detector-side knobs — open-vocab prompts and model overrides.
/// Anything that changes WHAT the inference layer is asked to
/// look for, vs. CameraIngest which controls how frames get there.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CameraDetector {
    /// Open-vocab prompts, or labels-of-interest for ensemble.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Per-camera overrides for the inference model (kind, pack, thresholds).
    #[serde(default)]
    pub model_override: Option<ModelConfig>,
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
    pub id: CameraId,
    pub name: String,
    #[serde(flatten)]
    pub ingest: CameraIngest,
    #[serde(flatten)]
    pub detector: CameraDetector,
    #[serde(flatten)]
    pub behavior: CameraBehavior,
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
    pub model: ModelConfig,
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let cfg = Config {
            cameras: vec![],
            ..Default::default()
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn pool_requires_workers() {
        let mut cfg = Config::default();
        cfg.inference.backend = InferenceBackendKind::Pool;
        cfg.inference.workers = 0;
        assert!(cfg.validate().is_err());
    }

    // M-Install Checkpoint 2 — secure-by-default. New deployments
    // (and any TOML that only writes `[auth]\n`) land on DevToken
    // automatically. Failing this test means a new install without
    // an explicit `mode = "..."` would silently leak a no-auth API.
    #[test]
    fn auth_mode_default_is_dev_token() {
        let auth: AuthConfig = Default::default();
        assert_eq!(auth.mode, AuthMode::DevToken);
        let parsed: AuthConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.mode, AuthMode::DevToken);
    }

    #[test]
    fn toml_top_level_table_detector() {
        assert!(toml_has_top_level_table(
            "[auth]\nmode = \"oidc\"\n",
            "auth"
        ));
        assert!(toml_has_top_level_table("  [auth]   # comment\n", "auth"));
        // Subtables (`[auth.oidc]`) must NOT count as the parent table.
        assert!(!toml_has_top_level_table(
            "[auth.oidc]\nissuer = \"x\"\n",
            "auth"
        ));
        assert!(!toml_has_top_level_table("[server]\n", "auth"));
        assert!(!toml_has_top_level_table("", "auth"));
    }

    // M-Install Checkpoint 2 — grandfather: pre-existing dev installs
    // whose nexus.toml has no `[auth]` block must be pinned back to
    // `mode = "none"` so the new DevToken default doesn't lock them
    // out on upgrade.
    #[test]
    fn load_with_compat_grandfathers_missing_auth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(&path, "[server]\napi_bind = \"127.0.0.1:8089\"\n").unwrap();
        let (cfg, notice) = Config::load_with_compat(&path).unwrap();
        assert!(notice.auth_grandfathered);
        assert_eq!(cfg.auth.mode, AuthMode::None);
    }

    #[test]
    fn load_with_compat_respects_explicit_auth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nexus.toml");
        std::fs::write(&path, "[auth]\nmode = \"dev_token\"\n").unwrap();
        let (cfg, notice) = Config::load_with_compat(&path).unwrap();
        assert!(!notice.auth_grandfathered);
        assert_eq!(cfg.auth.mode, AuthMode::DevToken);
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
        let entries = std::fs::read_dir(&config_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", config_dir.display()));
        let mut checked = 0usize;
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let cfg = Config::load(&path)
                .unwrap_or_else(|e| panic!("load {}: {e}", path.display()));
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
            checked >= 2,
            "expected to round-trip at least 2 TOMLs from {} (found {checked})",
            config_dir.display()
        );
    }
}
