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
        let cfg: Config = toml::from_str(&txt)?;
        cfg.validate()?;
        Ok((cfg, CompatNotice::default()))
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
            auth: RuntimeAuthConfig::default(),
            audit: RuntimeAuditConfig::default(),
        }
    }
}

fn default_blocking_threads() -> usize {
    8
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
    /// `yolo` matches the v1 ship — `models/yolo26n_<size>.onnx` driven
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
}

impl SinkConfig {
    /// Discriminator — matches `nexus_sinks::AlertSink::kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            SinkConfig::Webhook(_) => "webhook",
        }
    }

    /// Operator-chosen identifier — the `<name>` half of the
    /// `<kind>:<name>` SinkId every outbox row references.
    pub fn name(&self) -> &str {
        match self {
            SinkConfig::Webhook(cfg) => &cfg.name,
        }
    }

    /// Per-kind validation invoked from `Config::validate`. Cheap
    /// structural checks only — the sink crate does protocol-level
    /// validation lazily on first `deliver()` call.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            SinkConfig::Webhook(cfg) => cfg.validate(),
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
    /// M_PERF_CROWD B4 — worker-side bbox-width floor (pixels) for
    /// re-id extraction. Snapshots whose bbox width is below this
    /// value are dropped before the batched ORT call so we don't
    /// burn compute on crops too small to embed reliably. `0` (the
    /// default) disables the filter. Spec recommends 64 for a
    /// 64×128 floor at the camera's working resolution.
    #[serde(default = "default_reid_min_crop_w_px")]
    pub min_crop_w_px: u32,
    /// M_PERF_CROWD B4 — worker-side bbox-height floor (pixels) for
    /// re-id extraction. See [`min_crop_w_px`]. `0` (the default)
    /// disables the filter.
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
    0
}
fn default_reid_min_crop_h_px() -> u32 {
    0
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
        let entries = std::fs::read_dir(&config_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", config_dir.display()));
        let mut checked = 0usize;
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let cfg =
                Config::load(&path).unwrap_or_else(|e| panic!("load {}: {e}", path.display()));
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
