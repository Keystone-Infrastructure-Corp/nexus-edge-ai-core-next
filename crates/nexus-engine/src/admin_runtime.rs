//! M-Admin Phase 0 — runtime knobs that today require a
//! restart to take effect but are now mutable through the
//! admin API instead of by hand-editing `nexus.toml`.
//!
//! Three endpoint groups live here, all gated by the
//! `admin_auth_layer`:
//!
//! 1. **Socket rebind** —
//!    `GET|PUT /v1/admin/server/bind`. Validates the new
//!    `host:port` by probe-binding a TCP socket, persists
//!    to `engine_runtime_settings.api_bind`, and returns
//!    `{ restart_required: true }`. Boot picks the override
//!    up via [`crate::main::run`]'s persisted-bind lookup.
//!
//! 2. **Auth-mode flip + OIDC dry-run** —
//!    `GET|PUT /v1/admin/auth/config` and
//!    `POST /v1/admin/auth/oidc/test-discovery`. The PUT
//!    validates the whole [`AuthConfig`] (running an OIDC
//!    discovery hop when `mode = oidc|hybrid`) before
//!    persisting to `engine_runtime_settings.auth_config_json`.
//!    The POST is a one-shot probe with no persistence;
//!    handy for the "Test discovery" button next to the IdP
//!    URL fields.
//!
//! 3. **Streaming diagnostics tarball** —
//!    `GET /v1/admin/diagnostics/export`. Pure HTTP-level
//!    replacement for `nexus-doctor bundle --output …`.
//!    Streams a `.tar.gz` with: redacted `nexus.toml`, system
//!    metrics snapshot, last-1000 audit rows, last-100 motion
//!    events, build info. Tar is built on a `spawn_blocking`
//!    worker that writes through a `GzEncoder` wrapping the
//!    sender half of a bounded mpsc; axum streams the
//!    receiver half. Memory stays O(buffer size).
//!
//! ## Why restart-based vs hot-reload
//!
//! Hot-rebinding the axum listener and hot-rebuilding the
//! `AdminAuthState` + `OidcLoginState` would require a
//! restart of the per-task ownership graph (the listener is
//! owned by a `tokio::spawn`'d future, the admin gate is
//! captured into `route_layer`, the OIDC client is captured
//! into both `oidc_login::start` and `oidc_login::callback`).
//! Restart-based change keeps the impl small and the
//! invariants obvious: at any given moment the engine is
//! running with exactly one bind + one auth config, and the
//! transition between two configs is atomic at the process
//! boundary instead of mid-request.

use std::io::Write;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use chrono::{Duration as ChronoDuration, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use nexus_config::{AuthConfig, AuthMode, OidcConfig};
use nexus_store::audit::{AuditFilter, AuditOutcome};
use nexus_store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::oidc::OidcClient;
use crate::auth::require_role::AdminContext;

// ===========================================================
// Section 1 — Socket rebind
// ===========================================================

const KEY_API_BIND: &str = "api_bind";
const KEY_UI_BIND: &str = "ui_bind";
const KEY_AUTH_CONFIG: &str = "auth_config_json";
const KEY_LOW_WATERMARK_PCT: &str = "low_watermark_pct";
const KEY_PANIC_WATERMARK_PCT: &str = "panic_watermark_pct";

#[derive(Debug, Deserialize)]
pub struct PutServerBindReq {
    /// New `host:port` string for the primary (engine) listener.
    /// Parsed via [`SocketAddr::from_str`] then probe-bound to
    /// make sure the address is reachable AND the port is free,
    /// before we persist anything.
    pub addr: String,
    /// Optional update for the second (admin/UI alias) listener.
    /// When omitted the persisted `ui_bind` row is left alone.
    /// When present the discriminator chooses between
    /// `set { addr }` (probe-bind + persist a `host:port`),
    /// `clear` (persist explicit "off" — no second listener at
    /// next boot, even if `nexus.toml` defines one), and
    /// `reset` (drop the persisted row entirely, fall back to
    /// the on-disk `nexus.toml` value at next boot).
    #[serde(default)]
    pub ui_bind: Option<UiBindUpdate>,
}

/// Operator-supplied action for the optional UI alias listener.
/// Serialised with an external tag so the discriminator and
/// payload share the same JSON shape used by every other admin
/// surface in this module.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiBindUpdate {
    /// Persist a new `host:port`. Engine will bind a second
    /// listener to it at next restart.
    Set {
        /// `host:port` string. Same parser + probe-bind rules as
        /// the primary `addr`.
        addr: String,
    },
    /// Persist explicit "off". Engine will NOT start a second
    /// listener at next restart even if `nexus.toml` defines
    /// `server.ui_bind`.
    Clear,
    /// Drop the persisted override row. Engine falls back to
    /// `server.ui_bind` in `nexus.toml` at next restart.
    Reset,
}

/// What the engine has persisted for the UI alias listener,
/// resolved to a discriminated shape the UI can render without
/// triple-Option introspection.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiBindPending {
    /// An operator-persisted `host:port`. Engine will bind here
    /// at next boot.
    Set { addr: String },
    /// Operator-persisted "off". Engine will start with no
    /// second listener at next boot regardless of TOML.
    Clear,
}

#[derive(Debug, Serialize)]
pub struct ServerBindOut {
    /// What the primary listener is currently bound to (boot-time
    /// resolved). After a PUT this stays the same until the
    /// operator restarts; `pending` is what they'll get next boot.
    pub current: String,
    /// `Some` if there's a persisted primary-bind override that
    /// hasn't taken effect yet (`pending != current`). `None`
    /// when the persisted value matches the active bind.
    pub pending: Option<String>,
    /// What the UI alias listener is currently bound to.
    /// `None` = no second listener started at boot (either TOML
    /// didn't define `server.ui_bind` or the operator persisted
    /// "off"). Reported separately from `pending` so the UI can
    /// render "currently off / will be on" transitions cleanly.
    pub ui_current: Option<String>,
    /// `Some` when there's a persisted ui-bind override row.
    /// `None` when no override exists (engine falls back to TOML).
    /// The discriminated `UiBindPending` makes the "explicit
    /// clear" vs "explicit set" distinction visible to the UI.
    pub ui_pending: Option<UiBindPending>,
}

#[derive(Debug, Serialize)]
pub struct PutServerBindOut {
    pub current: String,
    pub pending: String,
    /// Same shape as `ServerBindOut.ui_pending`. Echoed back so
    /// the UI doesn't have to refetch to confirm what got
    /// persisted.
    pub ui_current: Option<String>,
    pub ui_pending: Option<UiBindPending>,
    pub restart_required: bool,
}

pub async fn get_server_bind(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<ServerBindOut>, ApiError> {
    let persisted = s
        .store
        .read_runtime_setting(KEY_API_BIND)
        .await
        .ok()
        .flatten()
        .flatten();
    let pending = persisted.filter(|p| p != &s.current_bind);

    // ui_bind has three-state storage semantics (see KEY_UI_BIND
    // notes in [`super::main`]'s resolver block): row absent →
    // no override, row present + NULL → operator-cleared override,
    // row present + Some(addr) → operator-set override.
    let ui_pending = match s.store.read_runtime_setting(KEY_UI_BIND).await {
        Ok(Some(Some(addr))) => Some(UiBindPending::Set { addr }),
        Ok(Some(None)) => Some(UiBindPending::Clear),
        Ok(None) | Err(_) => None,
    };
    Ok(Json(ServerBindOut {
        current: s.current_bind.clone(),
        pending,
        ui_current: s.current_ui_bind.clone(),
        ui_pending,
    }))
}

pub async fn put_server_bind(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<PutServerBindReq>,
) -> Result<Json<PutServerBindOut>, ApiError> {
    let trimmed = req.addr.trim().to_string();
    let parsed: SocketAddr = trimmed.parse().map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("addr must be host:port (parse error: {e})"),
        )
    })?;

    // Probe-bind so we fail with a useful 400 BEFORE persisting
    // a value that would only show up as a startup error after
    // the next restart. Bind + immediately drop — listener exit
    // closes the socket.
    match tokio::net::TcpListener::bind(parsed).await {
        Ok(l) => drop(l),
        Err(e) => {
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "server.bind.put",
                "admin/server/bind",
                Some("singleton"),
                AuditOutcome::Failure,
                None,
                Some(&serde_json::json!({ "addr": trimmed }).to_string()),
            )
            .await;
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("probe-bind to {trimmed} failed: {e}"),
            ));
        }
    }

    // Validate + normalise the optional ui_bind update BEFORE we
    // touch the DB. We don't want to persist the primary bind and
    // then fail on a bad ui_addr halfway through.
    let ui_decision = match &req.ui_bind {
        None => None,
        Some(UiBindUpdate::Reset) => Some(UiBindDecision::Reset),
        Some(UiBindUpdate::Clear) => Some(UiBindDecision::Clear),
        Some(UiBindUpdate::Set { addr }) => {
            let ui_trimmed = addr.trim().to_string();
            if ui_trimmed.is_empty() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "ui_bind set: addr must be a non-empty host:port".into(),
                ));
            }
            // Refuse the obvious foot-gun of pointing both
            // listeners at the same address — axum would bind one
            // and the other would fail at boot with EADDRINUSE.
            if ui_trimmed == trimmed {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "ui_bind addr must differ from primary addr".into(),
                ));
            }
            let ui_parsed: SocketAddr = ui_trimmed.parse().map_err(|e| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("ui_bind addr must be host:port (parse error: {e})"),
                )
            })?;
            match tokio::net::TcpListener::bind(ui_parsed).await {
                Ok(l) => drop(l),
                Err(e) => {
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        format!("probe-bind to ui_bind {ui_trimmed} failed: {e}"),
                    ));
                }
            }
            Some(UiBindDecision::Set(ui_trimmed))
        }
    };

    let before_json = serde_json::json!({
        "addr": &s.current_bind,
        "ui_addr": &s.current_ui_bind,
    });
    let after_json = serde_json::json!({
        "addr": &trimmed,
        "ui_bind": req.ui_bind.as_ref().map(ui_update_to_audit_json),
    });
    let before_str = serde_json::to_string(&before_json).ok();
    let after_str = serde_json::to_string(&after_json).ok();

    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_API_BIND, Some(&trimmed))
            .await?;
        match &ui_decision {
            None => {}
            Some(UiBindDecision::Reset) => {
                s.store
                    .delete_runtime_setting_tx(&mut tx, KEY_UI_BIND)
                    .await?;
            }
            Some(UiBindDecision::Clear) => {
                // SQL NULL = operator-persisted "off" (see resolver).
                s.store
                    .write_runtime_setting_tx(&mut tx, KEY_UI_BIND, None)
                    .await?;
            }
            Some(UiBindDecision::Set(addr)) => {
                s.store
                    .write_runtime_setting_tx(&mut tx, KEY_UI_BIND, Some(addr))
                    .await?;
            }
        }
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "server.bind.put",
            "admin/server/bind",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;

    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    let ui_pending = match &ui_decision {
        // No change requested → echo back what's already persisted.
        None => match s.store.read_runtime_setting(KEY_UI_BIND).await {
            Ok(Some(Some(addr))) => Some(UiBindPending::Set { addr }),
            Ok(Some(None)) => Some(UiBindPending::Clear),
            _ => None,
        },
        Some(UiBindDecision::Reset) => None,
        Some(UiBindDecision::Clear) => Some(UiBindPending::Clear),
        Some(UiBindDecision::Set(addr)) => Some(UiBindPending::Set { addr: addr.clone() }),
    };

    tracing::warn!(
        new_bind = %trimmed,
        current = %s.current_bind,
        ui_change = ?ui_decision,
        "admin set new server.bind; restart required to apply",
    );

    Ok(Json(PutServerBindOut {
        current: s.current_bind.clone(),
        pending: trimmed,
        ui_current: s.current_ui_bind.clone(),
        ui_pending,
        restart_required: true,
    }))
}

/// Resolved ui_bind action after request validation. Lives only
/// long enough to drive the transactional write below; mapped
/// back to `UiBindPending` for the response.
#[derive(Debug)]
enum UiBindDecision {
    Reset,
    Clear,
    Set(String),
}

/// Tiny adapter so the audit `after` JSON renders something
/// human-readable for `Reset` (which has no payload) without
/// needing a second serde-only enum.
fn ui_update_to_audit_json(u: &UiBindUpdate) -> serde_json::Value {
    match u {
        UiBindUpdate::Reset => serde_json::json!({ "action": "reset" }),
        UiBindUpdate::Clear => serde_json::json!({ "action": "clear" }),
        UiBindUpdate::Set { addr } => serde_json::json!({ "action": "set", "addr": addr }),
    }
}

// ===========================================================
// Section 2 — Auth-mode flip + OIDC discovery dry-run
// ===========================================================

#[derive(Debug, Serialize)]
pub struct AuthConfigOut {
    /// The auth config that's actually active right now (the
    /// snapshot from boot). This is the merge of `nexus.toml`
    /// + any persisted override that was applied at startup.
    pub current: SafeAuthConfig,
    /// What's persisted in `engine_runtime_settings` right
    /// now. `Some` only if an operator has explicitly PUT a
    /// value (which becomes the active config on next boot).
    pub pending: Option<SafeAuthConfig>,
}

/// Redacted projection of [`AuthConfig`] — same shape minus
/// the bits we shouldn't surface to a logged-in admin's
/// browser. `admin_secret_path` is preserved (it's a path,
/// not the secret itself); the OIDC client_secret_* knobs
/// are intentionally NOT round-tripped (operator re-enters
/// them on PUT if they need to change).
#[derive(Debug, Serialize, Deserialize)]
pub struct SafeAuthConfig {
    pub mode: AuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oidc: Option<SafeOidcConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_secret_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SafeOidcConfig {
    pub issuer: String,
    pub audience: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub scopes: Vec<String>,
    pub role_claims: Vec<String>,
    pub deny_unmapped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

impl SafeAuthConfig {
    fn from_full(cfg: &AuthConfig) -> Self {
        Self {
            mode: cfg.mode,
            oidc: cfg.oidc.as_ref().map(SafeOidcConfig::from_full),
            admin_secret_path: cfg.admin_secret_path.clone(),
        }
    }
}

impl SafeOidcConfig {
    fn from_full(o: &OidcConfig) -> Self {
        Self {
            issuer: o.issuer.clone(),
            audience: o.audience.clone(),
            jwks_uri: o.jwks_uri.clone(),
            client_id: o.client_id.clone(),
            display_name: o.display_name.clone(),
            scopes: o.scopes.clone(),
            role_claims: o.role_claims.clone(),
            deny_unmapped: o.deny_unmapped,
            redirect_uri: o.redirect_uri.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PutAuthConfigOut {
    pub restart_required: bool,
    pub mode: AuthMode,
    /// Echoed-back issuer when `mode = oidc|hybrid` and
    /// discovery succeeded. Lets the UI confirm exactly what
    /// IdP was validated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oidc_issuer: Option<String>,
}

pub async fn get_auth_config(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<AuthConfigOut>, ApiError> {
    let current = SafeAuthConfig {
        mode: s.auth_mode,
        // The active OidcConfig isn't on ApiState (we only
        // hold the discovered OidcLoginState). Surfacing
        // a stripped projection from the persisted blob is
        // close enough for the UI — and if there's no
        // persisted blob we just report mode.
        oidc: None,
        admin_secret_path: None,
    };
    let pending = match s.store.read_runtime_setting(KEY_AUTH_CONFIG).await {
        Ok(Some(Some(json))) => serde_json::from_str::<AuthConfig>(&json)
            .ok()
            .map(|c| SafeAuthConfig::from_full(&c)),
        _ => None,
    };
    Ok(Json(AuthConfigOut { current, pending }))
}

pub async fn put_auth_config(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<AuthConfig>,
) -> Result<Json<PutAuthConfigOut>, ApiError> {
    // Cross-field validation — same shape as the boot-time
    // checks in `cfg.validate()`. We deliberately re-check
    // here (rather than relying on the upstream validator)
    // so the failure mode is "PUT returns 400 with a clear
    // message" instead of "PUT succeeds + the next restart
    // panics with a config validation error".
    if req.mode.allows_oidc() && req.oidc.is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "auth.mode = oidc|hybrid requires an [oidc] block".to_string(),
        ));
    }
    if req.mode == AuthMode::Local && req.oidc.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "auth.mode = local rejects an [oidc] block".to_string(),
        ));
    }

    // For OIDC modes, run the discovery dry-run server-side
    // BEFORE persisting. If the issuer is unreachable / the
    // discovery doc is malformed / the JWKS can't be fetched,
    // the operator gets a 400 with a useful tag instead of
    // a silent boot failure on the next restart.
    let oidc_issuer = if req.mode.allows_oidc() {
        let oidc_cfg = req.oidc.as_ref().expect("checked above");
        match OidcClient::discover(oidc_cfg.clone()).await {
            Ok(_) => Some(oidc_cfg.issuer.clone()),
            Err(e) => {
                let (tag, msg) = discovery_error_to_tag(&e);
                audit_admin_action(
                    &s.store,
                    Some(&admin.0),
                    &headers,
                    peer.ip(),
                    "auth.config.put",
                    "admin/auth/config",
                    Some("singleton"),
                    AuditOutcome::Failure,
                    None,
                    Some(
                        &serde_json::json!({
                            "mode": format!("{:?}", req.mode),
                            "oidc_issuer": oidc_cfg.issuer,
                            "discovery_error": tag,
                        })
                        .to_string(),
                    ),
                )
                .await;
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("OIDC discovery dry-run failed ({tag}): {msg}"),
                ));
            }
        }
    } else {
        None
    };

    let json = serde_json::to_string(&req).map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not serialise AuthConfig: {e}"),
        )
    })?;

    let before_str = serde_json::to_string(&SafeAuthConfig {
        mode: s.auth_mode,
        oidc: None,
        admin_secret_path: None,
    })
    .ok();
    let after_str = serde_json::to_string(&SafeAuthConfig::from_full(&req)).ok();

    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_AUTH_CONFIG, Some(&json))
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "auth.config.put",
            "admin/auth/config",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;

    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    tracing::warn!(
        new_mode = ?req.mode,
        oidc_issuer = ?oidc_issuer,
        "admin set new auth.config; restart required to apply",
    );

    Ok(Json(PutAuthConfigOut {
        restart_required: true,
        mode: req.mode,
        oidc_issuer,
    }))
}

// --- OIDC discovery dry-run -------------------------------

#[derive(Debug, Deserialize)]
pub struct TestDiscoveryReq {
    pub issuer: String,
    /// Optional — only used to satisfy [`OidcConfig`]'s
    /// non-optional `audience` field. The dry-run doesn't
    /// verify any tokens so the value is never checked.
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub jwks_uri: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestDiscoveryOut {
    pub ok: bool,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userinfo_endpoint: Option<String>,
    pub supports_pkce_s256: bool,
}

pub async fn post_test_discovery(
    State(_s): State<ApiState>,
    _admin: AdminContext,
    Json(req): Json<TestDiscoveryReq>,
) -> Result<Json<TestDiscoveryOut>, ApiError> {
    let issuer = req.issuer.trim();
    if issuer.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "issuer must be non-empty".to_string(),
        ));
    }

    let cfg = OidcConfig {
        issuer: issuer.to_string(),
        audience: req
            .audience
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "discovery-test".to_string()),
        jwks_uri: req.jwks_uri.clone(),
        client_id: None,
        display_name: None,
        // Inlined defaults — the matching helpers in
        // nexus_config aren't pub. Values must stay in sync
        // with `nexus_config::default_oidc_{scopes,role_claims}`.
        scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
            "groups".to_string(),
        ],
        role_claims: vec![
            "groups".to_string(),
            "roles".to_string(),
            "https://nexus.local/role".to_string(),
        ],
        role_map: Default::default(),
        deny_unmapped: false,
        redirect_uri: None,
        client_secret_file: None,
        client_secret_env: None,
    };

    let client = OidcClient::discover(cfg).await.map_err(|e| {
        let (tag, msg) = discovery_error_to_tag(&e);
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("discovery failed ({tag}): {msg}"),
        )
    })?;
    let doc = client.discovery();
    Ok(Json(TestDiscoveryOut {
        ok: true,
        issuer: doc.issuer.clone(),
        authorization_endpoint: doc.authorization_endpoint.clone(),
        token_endpoint: doc.token_endpoint.clone(),
        jwks_uri: doc.jwks_uri.clone(),
        userinfo_endpoint: doc.userinfo_endpoint.clone(),
        supports_pkce_s256: doc
            .code_challenge_methods_supported
            .iter()
            .any(|m| m.eq_ignore_ascii_case("S256")),
    }))
}

fn discovery_error_to_tag(e: &crate::auth::oidc::OidcError) -> (&'static str, String) {
    use crate::auth::oidc::OidcError;
    match e {
        OidcError::Http(m) => ("http", m.clone()),
        OidcError::InvalidDiscovery(m) => ("invalid_discovery", m.clone()),
        OidcError::NoUsableKeys => ("no_usable_keys", "jwks had no supported keys".into()),
        OidcError::InvalidIdToken(t) => ("invalid_id_token", (*t).to_string()),
    }
}

// ===========================================================
// Section 3 — Streaming diagnostics tarball
// ===========================================================

const DEFAULT_AUDIT_LIMIT: i64 = 1000;
const DEFAULT_MOTION_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    /// Cap on the number of recent audit rows to include.
    /// Default 1000; capped at 10 000 to keep the tarball
    /// bounded.
    #[serde(default)]
    pub audit_limit: Option<i64>,
    /// Cap on the number of recent motion events. Default
    /// 100; capped at 10 000.
    #[serde(default)]
    pub motion_limit: Option<i64>,
}

pub async fn get_diagnostics_export(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let audit_limit = q
        .audit_limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(0, 10_000);
    let motion_limit = q
        .motion_limit
        .unwrap_or(DEFAULT_MOTION_LIMIT)
        .clamp(0, 10_000);

    // Gather everything that needs the tokio runtime BEFORE
    // we hand off to spawn_blocking. The tar writer itself
    // runs sync; pre-computing the bytes here keeps the
    // blocking task pure-CPU and avoids smuggling a Handle
    // across the thread boundary.
    let snapshot = build_snapshot(&s, audit_limit, motion_limit).await;

    audit_admin_action(
        &s.store,
        Some(&admin.0),
        &headers,
        peer.ip(),
        "diagnostics.export",
        "admin/diagnostics",
        Some("export"),
        AuditOutcome::Success,
        None,
        Some(
            &serde_json::json!({
                "audit_limit": audit_limit,
                "motion_limit": motion_limit,
                "audit_count": snapshot.audit_count,
                "motion_count": snapshot.motion_count,
                "redacted": true,
            })
            .to_string(),
        ),
    )
    .await;

    let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);

    tokio::task::spawn_blocking(move || {
        let writer = MpscWriter::new(tx.clone());
        let gz = GzEncoder::new(writer, Compression::default());
        let mut tar = tar::Builder::new(gz);
        if let Err(e) = write_tar_entries(&mut tar, &snapshot) {
            tracing::warn!(error = %e, "diagnostics tar build failed mid-stream");
            // Force the channel closed so the client gets a
            // truncated body instead of hanging.
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("tar build: {e}"))));
            return;
        }
        // `into_inner()` on tar::Builder flushes the trailer;
        // GzEncoder's drop flushes its footer when the
        // builder drops. `finish()` is the explicit form.
        let gz = match tar.into_inner() {
            Ok(gz) => gz,
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::other(format!("tar trailer: {e}"))));
                return;
            }
        };
        if let Err(e) = gz.finish() {
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("gzip finish: {e}"))));
        }
    });

    let stream = ReceiverStream::new(rx);
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("nexus-diagnostics-{ts}.tar.gz");

    let body = Body::from_stream(stream);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header("X-Content-Type-Options", "nosniff")
        .body(body)
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("response build: {e}"),
            )
        })?;
    Ok(resp)
}

struct DiagnosticsSnapshot {
    redacted_config_toml: String,
    system_metrics_json: String,
    audit_json: String,
    motion_events_json: String,
    storage_backends_json: String,
    build_info_json: String,
    audit_count: usize,
    motion_count: usize,
    generated_at: chrono::DateTime<chrono::Utc>,
}

async fn build_snapshot(s: &ApiState, audit_limit: i64, motion_limit: i64) -> DiagnosticsSnapshot {
    let now = Utc::now();

    let redacted_config_toml = build_redacted_config_toml(s).await;

    let system_metrics_json = {
        let snap = crate::system_metrics::snapshot();
        serde_json::to_string_pretty(&*snap).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };

    let (audit_json, audit_count) = match s
        .store
        .list_audit_filtered(&AuditFilter::default(), audit_limit, 0)
        .await
    {
        Ok(rows) => {
            let n = rows.len();
            let body = serde_json::to_string_pretty(&rows)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            (body, n)
        }
        Err(e) => (format!("{{\"error\":\"{e}\"}}"), 0),
    };

    let (motion_events_json, motion_count) = {
        // 24h window, newest-first cap of `motion_limit`. The
        // store API doesn't have a "give me the last N rows
        // across all cameras" call without a time bound; 24h
        // matches the existing UI default for the events feed
        // and keeps the JSON well under a few MB for typical
        // deployments.
        let to = now;
        let from = now - ChronoDuration::hours(24);
        match s
            .store
            .list_motion_events_across_cameras(None, from, to, motion_limit)
            .await
        {
            Ok(rows) => {
                let n = rows.len();
                let body = serde_json::to_string_pretty(&rows)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                (body, n)
            }
            Err(e) => (format!("{{\"error\":\"{e}\"}}"), 0),
        }
    };

    let storage_backends_json = {
        // `Registry::snapshot()` returns `Vec<(handle, kind)>`.
        // Backend-specific config is intentionally omitted —
        // many backends include OAuth refresh tokens or API
        // keys; we surface only the operator-visible label +
        // kind so support can map the bundle back to the
        // storage admin tab without leaking secrets.
        let entries = s.registry.snapshot();
        let view: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|(handle, kind)| {
                serde_json::json!({
                    "handle": handle,
                    "kind": kind,
                })
            })
            .collect();
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };

    let build_info_json = serde_json::to_string_pretty(&serde_json::json!({
        // See `build.rs` — release-tag at CI build-time, falls back
        // to `CARGO_PKG_VERSION` for local dev builds.
        "engine_version": env!("NEXUS_BUILD_VERSION"),
        "generated_at": now.to_rfc3339(),
        "current_bind": s.current_bind,
        "auth_mode": format!("{:?}", s.auth_mode),
        "recorder_kind": s.recorder.kind(),
        "low_watermark_pct": s.low_watermark_pct,
        "panic_watermark_pct": s.panic_watermark_pct,
    }))
    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));

    DiagnosticsSnapshot {
        redacted_config_toml,
        system_metrics_json,
        audit_json,
        motion_events_json,
        storage_backends_json,
        build_info_json,
        audit_count,
        motion_count,
        generated_at: now,
    }
}

async fn build_redacted_config_toml(s: &ApiState) -> String {
    // We don't hold the parsed `Config` on ApiState. The
    // operator-visible view is reconstructed from the bits
    // we DO have: current bind, auth mode (sans secrets),
    // recorder kind, watermarks. This is a stripped projection
    // for support — operators who want the byte-exact
    // `nexus.toml` can read it off disk; this snapshot exists
    // so the bundle is self-contained without paths.
    let mut s_out = String::new();
    s_out.push_str("# Redacted runtime view — secrets stripped\n");
    s_out.push_str("# Generated by GET /v1/admin/diagnostics/export\n\n");
    s_out.push_str("[server]\n");
    s_out.push_str(&format!("api_bind = \"{}\"\n\n", s.current_bind));
    s_out.push_str("[auth]\n");
    s_out.push_str(&format!("mode = \"{}\"\n", auth_mode_str(s.auth_mode)));
    if s.oidc_display_name.is_some() {
        s_out.push_str("oidc = { issuer = \"<redacted>\", audience = \"<redacted>\" }\n");
    }
    s_out.push_str("\n[runtime.clips]\n");
    s_out.push_str(&format!("low_watermark_pct = {}\n", s.low_watermark_pct));
    s_out.push_str(&format!(
        "panic_watermark_pct = {}\n",
        s.panic_watermark_pct
    ));
    s_out.push_str(&format!("recorder = \"{}\"\n", s.recorder.kind()));
    if let Some(label) = s.preferred_usb_label.get() {
        s_out.push_str(&format!("preferred_usb_label = \"{label}\"\n"));
    }
    s_out
}

fn auth_mode_str(m: AuthMode) -> &'static str {
    match m {
        AuthMode::Local => "local",
        AuthMode::Oidc => "oidc",
        AuthMode::Hybrid => "hybrid",
    }
}

fn write_tar_entries<W: Write>(
    tar: &mut tar::Builder<W>,
    snap: &DiagnosticsSnapshot,
) -> std::io::Result<()> {
    let mtime = snap.generated_at.timestamp().max(0) as u64;
    write_entry(
        tar,
        "nexus.toml.redacted",
        &snap.redacted_config_toml,
        mtime,
    )?;
    write_entry(tar, "system-metrics.json", &snap.system_metrics_json, mtime)?;
    write_entry(tar, "audit-log.json", &snap.audit_json, mtime)?;
    write_entry(tar, "motion-events.json", &snap.motion_events_json, mtime)?;
    write_entry(
        tar,
        "storage-backends.json",
        &snap.storage_backends_json,
        mtime,
    )?;
    write_entry(tar, "build-info.json", &snap.build_info_json, mtime)?;
    Ok(())
}

fn write_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    body: &str,
    mtime: u64,
) -> std::io::Result<()> {
    let bytes = body.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path(name)?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    header.set_cksum();
    tar.append(&header, bytes)
}

// Tiny std::io::Write impl that ships bytes into a tokio
// mpsc channel as `bytes::Bytes`. Blocking-side (the tar
// writer runs in spawn_blocking) uses `blocking_send` so
// backpressure from a slow client surfaces here as a wait
// rather than silently dropping data. If the channel is
// closed mid-stream (client disconnected) we return
// `BrokenPipe` so the tar builder bails out instead of
// looping forever.
struct MpscWriter {
    tx: mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
}

impl MpscWriter {
    fn new(tx: mpsc::Sender<Result<bytes::Bytes, std::io::Error>>) -> Self {
        Self { tx }
    }
}

impl Write for MpscWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len();
        let chunk = bytes::Bytes::copy_from_slice(buf);
        self.tx.blocking_send(Ok(chunk)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected")
        })?;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // mpsc has no flush; the next write will block on
        // capacity anyway.
        Ok(())
    }
}

// Note on backpressure: when the client is reading slowly
// the blocking sender naturally backs up. tokio::sync::mpsc's
// `blocking_send` doesn't take a timeout, so we lean on the
// upstream HTTP server's per-request body timeout to bound
// the worst case under pathologically slow clients.

// ===========================================================
// Section 4 — Storage watermark thresholds (M-Admin Phase 0)
// ===========================================================
//
// Mirrors the `server.bind` pattern: PUT validates ranges,
// persists to `engine_runtime_settings`, returns
// `restart_required = true`. The watermark FSM in
// `storage_safety` reads `low_watermark_pct` /
// `panic_watermark_pct` from `ApiState` (set at boot in
// `main.rs`); boot-time override resolution wires these from
// the persisted row when present.

#[derive(Debug, Serialize)]
pub struct WatermarkOut {
    pub low_pct: u8,
    pub panic_pct: u8,
    pub pending_low_pct: Option<u8>,
    pub pending_panic_pct: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct PutWatermarkReq {
    pub low_pct: Option<u8>,
    pub panic_pct: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct PutWatermarkOut {
    pub current_low_pct: u8,
    pub current_panic_pct: u8,
    pub pending_low_pct: u8,
    pub pending_panic_pct: u8,
    pub restart_required: bool,
}

async fn read_persisted_pct(s: &ApiState, key: &str) -> Option<u8> {
    s.store
        .read_runtime_setting(key)
        .await
        .ok()
        .flatten()
        .flatten()
        .and_then(|raw| raw.parse::<u8>().ok())
        .filter(|v| *v <= 100)
}

pub async fn get_watermarks(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<WatermarkOut>, ApiError> {
    let low_persisted = read_persisted_pct(&s, KEY_LOW_WATERMARK_PCT).await;
    let panic_persisted = read_persisted_pct(&s, KEY_PANIC_WATERMARK_PCT).await;
    Ok(Json(WatermarkOut {
        low_pct: s.low_watermark_pct,
        panic_pct: s.panic_watermark_pct,
        pending_low_pct: low_persisted.filter(|v| *v != s.low_watermark_pct),
        pending_panic_pct: panic_persisted.filter(|v| *v != s.panic_watermark_pct),
    }))
}

pub async fn put_watermarks(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<PutWatermarkReq>,
) -> Result<Json<PutWatermarkOut>, ApiError> {
    // Resolve the effective values (incoming or fall-back to
    // current snapshot) so we can validate the combined pair
    // against the FSM invariant `panic_pct > low_pct`.
    let new_low = req.low_pct.unwrap_or(s.low_watermark_pct);
    let new_panic = req.panic_pct.unwrap_or(s.panic_watermark_pct);

    if new_low > 100 || new_panic > 100 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "low_pct and panic_pct must be 0..=100".into(),
        ));
    }
    if new_panic <= new_low {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("panic_pct ({new_panic}) must be strictly greater than low_pct ({new_low})"),
        ));
    }

    let before_str = serde_json::to_string(&serde_json::json!({
        "low_pct": s.low_watermark_pct,
        "panic_pct": s.panic_watermark_pct,
    }))
    .ok();
    let after_str = serde_json::to_string(&serde_json::json!({
        "low_pct": new_low,
        "panic_pct": new_panic,
    }))
    .ok();

    let low_str = new_low.to_string();
    let panic_str = new_panic.to_string();
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_LOW_WATERMARK_PCT, Some(&low_str))
            .await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_PANIC_WATERMARK_PCT, Some(&panic_str))
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "server.watermarks.put",
            "admin/server/watermarks",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;

    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    tracing::warn!(
        new_low_pct = new_low,
        new_panic_pct = new_panic,
        current_low_pct = s.low_watermark_pct,
        current_panic_pct = s.panic_watermark_pct,
        "admin set new storage watermarks; restart required to apply",
    );

    Ok(Json(PutWatermarkOut {
        current_low_pct: s.low_watermark_pct,
        current_panic_pct: s.panic_watermark_pct,
        pending_low_pct: new_low,
        pending_panic_pct: new_panic,
        restart_required: true,
    }))
}

/// Resolve the boot-time effective watermark values: persisted
/// row in `engine_runtime_settings` wins over `nexus.toml`.
/// Returns `(low_pct, panic_pct)`. Invalid persisted values
/// (parse failure, out of range, or panic <= low) fall back
/// silently to the on-disk pair with a `warn!` log.
pub async fn resolve_persisted_watermarks(store: &Store, toml_low: u8, toml_panic: u8) -> (u8, u8) {
    let read = |key: &'static str| async move {
        store
            .read_runtime_setting(key)
            .await
            .ok()
            .flatten()
            .flatten()
            .and_then(|raw| raw.parse::<u8>().ok())
            .filter(|v| *v <= 100)
    };

    let persisted_low = read(KEY_LOW_WATERMARK_PCT).await;
    let persisted_panic = read(KEY_PANIC_WATERMARK_PCT).await;

    let low = persisted_low.unwrap_or(toml_low);
    let panic = persisted_panic.unwrap_or(toml_panic);

    if panic <= low {
        tracing::warn!(
            persisted_low = ?persisted_low,
            persisted_panic = ?persisted_panic,
            toml_low,
            toml_panic,
            "engine_runtime_settings watermark pair fails panic > low invariant; falling back to nexus.toml",
        );
        return (toml_low, toml_panic);
    }

    if persisted_low.is_some() || persisted_panic.is_some() {
        tracing::warn!(
            low_pct = low,
            panic_pct = panic,
            "applying operator-persisted storage watermarks from engine_runtime_settings (overrides nexus.toml)",
        );
    }

    (low, panic)
}

// ===========================================================
// Section 5 — Default inference model (M-Admin Phase 0
//              follow-up)
// ===========================================================
//
// Lets operators flip the default detector kind / preset /
// confidence threshold / input dims / model-pack path from
// the admin UI instead of hand-editing `nexus.toml`. Persists
// the full editable shape as JSON in
// `engine_runtime_settings.inference_model_json`; the engine
// reads that row at boot (see
// [`resolve_persisted_inference_model`]) and substitutes the
// persisted values into `cfg.inference.model` BEFORE the
// router walks its known-kinds set. So a flip that requires
// a new detector kind (e.g. `yolo` → `yolo_world`) only takes
// effect after restart, matching the bind / watermark
// precedent (see [`crate::api::router`] docstring on the
// router's lifecycle for the deeper reason — the
// `InferenceRouter` is built once at boot and not rebuilt
// per-frame).
//
// Advanced fields (`members`, `top_k`) are intentionally NOT
// exposed here — ensemble shapes are still TOML-only because
// they involve nested per-member config that the form would
// have to grow a recursive editor for. The persisted JSON
// preserves whatever the operator had under those keys
// (`#[serde(default)]` + `skip_serializing_if = Vec::is_empty`
// on `members` round-trips an empty Vec losslessly).

const KEY_INFERENCE_MODEL_JSON: &str = "inference_model_json";

/// Editable projection of [`nexus_config::ModelConfig`]. Every
/// field is `Option` so the UI can send a partial patch (the
/// handler merges onto the current snapshot before persisting).
/// The advanced `members` / `top_k` fields are deliberately
/// absent — they round-trip unchanged on disk because we
/// always re-serialise the merged ModelConfig, not the patch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_threshold: Option<f32>,
    /// Path string or empty-string-to-clear. We accept an
    /// explicit `Some("")` as "remove pack_path"; `None` means
    /// "leave existing pack_path alone".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_path: Option<String>,
}

/// Read-only snapshot returned by `GET /v1/admin/server/inference`.
/// Same shape both for the active (`current`) value and any
/// pending override (`pending`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelView {
    pub kind: String,
    pub preset: String,
    pub input_width: u32,
    pub input_height: u32,
    pub score_threshold: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack_path: Option<String>,
}

impl ModelView {
    fn from_full(m: &nexus_config::ModelConfig) -> Self {
        Self {
            kind: m.kind.clone(),
            preset: m.preset.clone(),
            input_width: m.input_width,
            input_height: m.input_height,
            score_threshold: m.score_threshold,
            pack_path: m
                .pack_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InferenceModelOut {
    pub current: ModelView,
    /// `Some` iff a persisted override differs from the
    /// active boot-time value.
    pub pending: Option<ModelView>,
    /// Canonical detector kinds the engine knows how to
    /// build. The UI uses this for the kind dropdown so we
    /// don't drift away from `models_catalog::KNOWN_KINDS`.
    pub available_kinds: &'static [&'static str],
}

#[derive(Debug, Serialize)]
pub struct PutInferenceModelOut {
    pub current: ModelView,
    pub pending: ModelView,
    pub restart_required: bool,
}

pub async fn get_inference_model(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<InferenceModelOut>, ApiError> {
    let current = ModelView::from_full(&s.current_inference_model);
    let pending = read_persisted_model(&s.store)
        .await
        .map(|m| ModelView::from_full(&m))
        .filter(|p| {
            // Hide the row when it matches the active boot
            // value (e.g. immediately after a successful
            // restart) so the UI doesn't keep nagging the
            // operator with a "pending restart" banner.
            p.kind != current.kind
                || p.preset != current.preset
                || p.input_width != current.input_width
                || p.input_height != current.input_height
                || (p.score_threshold - current.score_threshold).abs() > f32::EPSILON
                || p.pack_path != current.pack_path
        });
    Ok(Json(InferenceModelOut {
        current,
        pending,
        available_kinds: crate::models_catalog::KNOWN_KINDS,
    }))
}

pub async fn put_inference_model(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(patch): Json<ModelPatch>,
) -> Result<Json<PutInferenceModelOut>, ApiError> {
    // Start from whatever's persisted (so successive PUTs
    // accumulate); fall back to the boot-time snapshot.
    let mut merged = read_persisted_model(&s.store)
        .await
        .unwrap_or_else(|| (*s.current_inference_model).clone());

    if let Some(kind) = patch.kind {
        let trimmed = kind.trim();
        if trimmed.is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "kind must be a non-empty string".into(),
            ));
        }
        if !crate::models_catalog::KNOWN_KINDS.contains(&trimmed) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown detector kind '{trimmed}'; expected one of {:?}",
                    crate::models_catalog::KNOWN_KINDS
                ),
            ));
        }
        merged.kind = trimmed.to_string();
    }
    if let Some(preset) = patch.preset {
        let trimmed = preset.trim();
        if trimmed.is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "preset must be a non-empty string".into(),
            ));
        }
        merged.preset = trimmed.to_string();
    }
    if let Some(w) = patch.input_width {
        if !(32..=4096).contains(&w) || w % 32 != 0 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("input_width must be a multiple of 32 in 32..=4096 (got {w})"),
            ));
        }
        merged.input_width = w;
    }
    if let Some(h) = patch.input_height {
        if !(32..=4096).contains(&h) || h % 32 != 0 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("input_height must be a multiple of 32 in 32..=4096 (got {h})"),
            ));
        }
        merged.input_height = h;
    }
    if let Some(thr) = patch.score_threshold {
        if !thr.is_finite() || !(0.0..=1.0).contains(&thr) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("score_threshold must be a finite value in 0.0..=1.0 (got {thr})"),
            ));
        }
        merged.score_threshold = thr;
    }
    if let Some(path) = patch.pack_path {
        merged.pack_path = if path.trim().is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(path))
        };
    }

    let before_view = ModelView::from_full(&s.current_inference_model);
    let after_view = ModelView::from_full(&merged);
    let before_str = serde_json::to_string(&before_view).ok();
    let after_str = serde_json::to_string(&after_view).ok();

    let serialised = serde_json::to_string(&merged).map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialise merged ModelConfig: {e}"),
        )
    })?;

    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_INFERENCE_MODEL_JSON, Some(&serialised))
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "server.inference.put",
            "admin/server/inference",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;

    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    tracing::warn!(
        kind = %after_view.kind,
        preset = %after_view.preset,
        width = after_view.input_width,
        height = after_view.input_height,
        score_threshold = after_view.score_threshold,
        "admin set new default inference model; restart required to apply",
    );

    Ok(Json(PutInferenceModelOut {
        current: before_view,
        pending: after_view,
        restart_required: true,
    }))
}

async fn read_persisted_model(store: &Store) -> Option<nexus_config::ModelConfig> {
    let raw = store
        .read_runtime_setting(KEY_INFERENCE_MODEL_JSON)
        .await
        .ok()
        .flatten()
        .flatten()?;
    match serde_json::from_str::<nexus_config::ModelConfig>(&raw) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "engine_runtime_settings.inference_model_json failed to decode; ignoring",
            );
            None
        }
    }
}

/// Boot-time helper: merge any persisted override on top of the
/// `nexus.toml` value. Operator-persisted fields win; advanced
/// fields they can't edit through the UI (`members`, `top_k`)
/// fall back to the persisted blob's own values, which were
/// captured from the toml the last time someone hit Save.
///
/// Failure modes are all loud (`warn!`) but non-fatal — a
/// corrupt JSON row falls back to the on-disk config so the
/// engine still boots.
pub async fn resolve_persisted_inference_model(
    store: &Store,
    toml_model: &nexus_config::ModelConfig,
) -> nexus_config::ModelConfig {
    match read_persisted_model(store).await {
        Some(persisted) => {
            tracing::warn!(
                kind = %persisted.kind,
                preset = %persisted.preset,
                "applying operator-persisted inference.model from engine_runtime_settings (overrides nexus.toml)",
            );
            persisted
        }
        None => toml_model.clone(),
    }
}

// ===========================================================
// Section 6 — Engine self-restart (M-Admin Phase 0 follow-up)
// ===========================================================
//
// `POST /v1/admin/server/restart` lets the operator apply the
// pending bind / watermark / auth / inference / OIDC changes
// without shelling into the host. We replace the running
// process image with a fresh copy of the same binary using
// `execv(2)` — preserves PID, parent FDs (so a supervisor
// like systemd / launchd / docker doesn't see the process
// disappear) and the original argv (so `--config` and any
// other CLI flags survive).
//
// Sequencing:
//
//   1. Handler validates admin auth (route layer).
//   2. Writes an `admin.server.restart` audit row.
//   3. Returns `202 Accepted` with `{ restart_scheduled: true,
//      delay_ms }` immediately so the UI can show a toast.
//   4. A `tokio::spawn`'d task waits `delay_ms` (default
//      500 ms — enough to flush the response body), drops
//      its handle on the runtime, and calls `Command::exec()`.
//
// On non-unix targets (we don't ship there, but the dev
// machine might be Windows occasionally) the handler returns
// 501 — the `exec` syscall is unix-only.

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PostRestartReq {
    /// How long to wait (in ms) before exec()'ing the new
    /// process image. Clamped to 0..=5000. Default 500.
    /// Bumping this is only useful when the response body is
    /// large or you're behind a slow proxy; for the normal
    /// SPA-fetch case the default is fine.
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct PostRestartOut {
    pub restart_scheduled: bool,
    pub delay_ms: u64,
    pub current_bind: String,
    /// Echoed for the same reason as `current_bind`: the UI can
    /// confirm that the listener it's about to lose contact with
    /// is the one it intended to restart, and offers a graceful
    /// reconnect target on the alias port if configured.
    pub current_ui_bind: Option<String>,
}

pub async fn post_restart(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    body: Option<Json<PostRestartReq>>,
) -> Result<(StatusCode, Json<PostRestartOut>), ApiError> {
    let delay_ms = body
        .as_ref()
        .and_then(|j| j.0.delay_ms)
        .unwrap_or(500)
        .min(5_000);

    let exe = std::env::current_exe().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("current_exe(): {e}"),
        )
    })?;

    audit_admin_action(
        &s.store,
        Some(&admin.0),
        &headers,
        peer.ip(),
        "server.restart",
        "admin/server/restart",
        Some("singleton"),
        AuditOutcome::Success,
        None,
        Some(
            &serde_json::json!({
                "exe": exe.display().to_string(),
                "delay_ms": delay_ms,
            })
            .to_string(),
        ),
    )
    .await;

    let current_bind = s.current_bind.clone();
    let current_ui_bind = s.current_ui_bind.clone();

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            do_self_exec(&exe);
        });

        Ok((
            StatusCode::ACCEPTED,
            Json(PostRestartOut {
                restart_scheduled: true,
                delay_ms,
                current_bind,
                current_ui_bind,
            }),
        ))
    }

    #[cfg(not(unix))]
    {
        let _ = (delay_ms, current_bind, current_ui_bind, exe);
        Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "engine self-restart is only implemented on unix targets".into(),
        ))
    }
}

#[cfg(unix)]
fn do_self_exec(exe: &std::path::Path) {
    use std::os::unix::process::CommandExt;

    // Preserve the original argv tail so `--config <path>` and
    // any other CLI flags carry over to the new image. The
    // first element is the binary path, which we overwrite
    // with the resolved `current_exe()` (symlink-followed) so
    // the new process sees the same `argv[0]` it'd have if
    // launched fresh by the operator.
    let mut args = std::env::args_os();
    let _ = args.next();
    let rest: Vec<std::ffi::OsString> = args.collect();

    tracing::warn!(
        exe = %exe.display(),
        argc = rest.len(),
        "admin-triggered self-restart: exec()'ing new engine image now",
    );

    // `Command::exec()` only returns on failure — on success
    // the new image takes over the current process. If it
    // does fail (extremely rare: missing exec permission,
    // disappearing binary, ETXTBSY), exit non-zero so any
    // supervisor (systemd / docker / k8s) restarts us.
    let err = std::process::Command::new(exe).args(&rest).exec();
    tracing::error!(error = %err, "exec() failed during self-restart; exiting non-zero so the supervisor can recover");
    std::process::exit(70);
}

// ============================================================
// Tests — serde round-trips for the LAN-settings wire shapes.
// The handler-level integration tests live alongside the rest
// of the admin endpoint coverage in `api.rs::tests`; the unit
// tests here just pin the externally-tagged JSON shape so a
// rename or `#[serde(rename_all = ...)]` regression caught by
// the UI's typed client lights up here first.
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_bind_update_deserialises_all_three_actions() {
        let set: UiBindUpdate =
            serde_json::from_str(r#"{"action":"set","addr":"0.0.0.0:80"}"#).unwrap();
        match set {
            UiBindUpdate::Set { addr } => assert_eq!(addr, "0.0.0.0:80"),
            other => panic!("unexpected variant: {other:?}"),
        }

        let clear: UiBindUpdate = serde_json::from_str(r#"{"action":"clear"}"#).unwrap();
        assert!(matches!(clear, UiBindUpdate::Clear));

        let reset: UiBindUpdate = serde_json::from_str(r#"{"action":"reset"}"#).unwrap();
        assert!(matches!(reset, UiBindUpdate::Reset));
    }

    #[test]
    fn put_server_bind_req_accepts_omitted_ui_bind() {
        // Pre-extension callers (and the UI's `noop` action) PUT
        // just `{addr}` — must still deserialise.
        let req: PutServerBindReq = serde_json::from_str(r#"{"addr":"0.0.0.0:8089"}"#).unwrap();
        assert_eq!(req.addr, "0.0.0.0:8089");
        assert!(req.ui_bind.is_none());
    }

    #[test]
    fn put_server_bind_req_accepts_explicit_null_ui_bind() {
        // Equivalent to omitting via `#[serde(default)]`.
        let req: PutServerBindReq =
            serde_json::from_str(r#"{"addr":"0.0.0.0:8089","ui_bind":null}"#).unwrap();
        assert!(req.ui_bind.is_none());
    }

    #[test]
    fn ui_bind_pending_serialises_with_action_tag() {
        let set = UiBindPending::Set {
            addr: "0.0.0.0:80".into(),
        };
        assert_eq!(
            serde_json::to_value(&set).unwrap(),
            serde_json::json!({"action":"set","addr":"0.0.0.0:80"}),
        );
        let clear = UiBindPending::Clear;
        assert_eq!(
            serde_json::to_value(&clear).unwrap(),
            serde_json::json!({"action":"clear"}),
        );
    }

    #[test]
    fn server_bind_out_serialises_with_null_ui_fields_when_absent() {
        let out = ServerBindOut {
            current: "0.0.0.0:8089".into(),
            pending: None,
            ui_current: None,
            ui_pending: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["current"], "0.0.0.0:8089");
        assert!(v["pending"].is_null());
        assert!(v["ui_current"].is_null());
        assert!(v["ui_pending"].is_null());
    }
}
