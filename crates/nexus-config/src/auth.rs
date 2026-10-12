//! Auth

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
