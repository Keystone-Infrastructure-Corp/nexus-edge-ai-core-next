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
            if cam.url.scheme() != "rtsp"
                && cam.url.scheme() != "rtsps"
                && cam.url.scheme() != "file"
                && cam.url.scheme() != "virtual"
            {
                return Err(ConfigError::Validation(format!(
                    "camera {} url has unsupported scheme '{}'",
                    cam.id,
                    cam.url.scheme()
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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            blocking_threads: default_blocking_threads(),
        }
    }
}

fn default_blocking_threads() -> usize {
    8
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
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// No auth (dev only).
    #[default]
    None,
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
    LegacyJson,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub id: String,
    pub name: String,
    pub camera_filter: Option<Vec<CameraId>>,
    /// CEL expression (or JSON-AST string when `RulesBackendKind::LegacyJson`).
    pub when: String,
    pub severity: String,
    #[serde(default = "default_min_track_age_ms")]
    pub min_track_age_ms: u64,
    #[serde(default = "default_consecutive_frames")]
    pub consecutive_frames: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    pub id: CameraId,
    pub name: String,
    pub url: Url,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Open-vocab prompts, or labels-of-interest for ensemble.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Per-camera overrides for the inference model (kind, pack, thresholds).
    #[serde(default)]
    pub model_override: Option<ModelConfig>,
    /// Polygon zones used by motion gate / dwell rules.
    #[serde(default)]
    pub zones: Vec<ZoneConfig>,
    /// Per-camera FPS cap. 0 = unbounded.
    #[serde(default)]
    pub max_fps: u32,
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
}
