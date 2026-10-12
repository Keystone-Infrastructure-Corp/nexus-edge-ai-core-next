//! TOML-backed configuration for the Nexus edge engine.
//!
//! Every backend-selectable layer exposes a `backend` field so operators can
//! pin the implementation. Scale knobs (`workers`, `capacity`, `worker_threads`)
//! live alongside the backend choice — the config file is the only place the
//! deployment topology is declared.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use thiserror::Error;

mod auth;
mod bus;
mod camera_update;
mod cameras;
mod clips;
mod hw_decode;
mod inference;
mod lan_proxy;
mod reid;
mod remote_shell;
mod rules;
mod runtime;
mod runtime_auth;
mod server;
mod sinks;
mod static_filter;
mod store;
mod telemetry;
#[cfg(test)]
mod tests;
mod tracker;

pub use auth::*;
pub use bus::*;
pub use camera_update::*;
pub use cameras::*;
pub use clips::*;
pub use hw_decode::*;
pub use inference::*;
pub use lan_proxy::*;
pub use reid::*;
pub use remote_shell::*;
pub use rules::*;
pub use runtime::*;
pub use runtime_auth::*;
pub use server::*;
pub use sinks::*;
pub use static_filter::*;
pub use store::*;
pub use telemetry::*;
pub use tracker::*;

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
    /// Remote shell. Ships OFF. The box owner opts in here; the cloud
    /// can never turn it on remotely, and it can never name the pipe's
    /// destination.
    #[serde(default)]
    pub remote_access: RemoteAccessConfig,
    /// Cloud-side behaviour that is a property of THIS box rather than
    /// of the enrollment: currently just the go-dark watchdog window.
    #[serde(default)]
    pub cloud: CloudConfig,
}

/// Per-appliance cloud behaviour.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CloudConfig {
    /// Hours the cloud tunnel may be dark before the engine reflips to
    /// its previous-good release.
    ///
    /// `0` disables the watchdog. The default (12 h) is chosen to be far
    /// longer than any plausible transient — ISP outages, DHCP renewals,
    /// a site losing power overnight — while still being short enough
    /// that a bad release is undone the same day rather than on the next
    /// site visit. The watchdog fires at most once per boot regardless.
    #[serde(default = "default_dark_window_hours")]
    pub dark_window_hours: u64,
}

const fn default_dark_window_hours() -> u64 {
    12
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            dark_window_hours: default_dark_window_hours(),
        }
    }
}

impl CloudConfig {
    /// The configured window, or `None` when the watchdog is disabled.
    #[must_use]
    pub fn dark_window(&self) -> Option<std::time::Duration> {
        if self.dark_window_hours == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(
                self.dark_window_hours * 3600,
            ))
        }
    }
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
        if self.remote_access.enabled && !self.remote_access.target_is_local() {
            return Err(ConfigError::Validation(format!(
                "remote_access.target '{}' must be a loopback or private address — \
                 the shell pipe terminates on this appliance, not on the internet",
                self.remote_access.target
            )));
        }
        if self.inference.workers == 0
            && matches!(self.inference.backend, InferenceBackendKind::Pool)
        {
            return Err(ConfigError::Validation(
                "inference.backend = 'pool' requires inference.workers >= 1".into(),
            ));
        }
        self.inference
            .model
            .validate_finite()
            .map_err(|why| ConfigError::Validation(format!("inference.model {why}")))?;
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
            cam.validate_finite()
                .map_err(|why| ConfigError::Validation(format!("camera {}: {why}", cam.id)))?;
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
