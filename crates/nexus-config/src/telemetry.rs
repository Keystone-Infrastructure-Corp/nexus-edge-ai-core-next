//! Telemetry

use serde::{Deserialize, Serialize};

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
