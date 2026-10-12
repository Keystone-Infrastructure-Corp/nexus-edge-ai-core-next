//! Server

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
