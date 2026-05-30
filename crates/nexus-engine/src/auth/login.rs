//! M6 Phase 2 Step 2.7 — login / refresh / logout / change-password.
//!
//! Composes the leaf primitives shipped in 2.2 – 2.6 (argon2id
//! passwords, lockout FSM, access JWT, refresh-secret store,
//! audit log) into the four HTTP handlers that the SPA actually
//! calls.
//!
//! ## Wire shapes (POST JSON in, JSON out)
//!
//! ```text
//! POST /api/v1/auth/login            { username, password }
//! POST /api/v1/auth/refresh          { refresh_token }
//! POST /api/v1/auth/logout           { refresh_token } | {}
//! POST /api/v1/auth/change-password  { old_password, new_password }
//! ```
//!
//! `login` and `refresh` return the same body shape
//! ([`TokenResponse`]); `logout` returns `204 No Content`;
//! `change-password` returns `204` and clears
//! `users.force_password_reset`.
//!
//! ## Cookie
//!
//! On successful `login` and `refresh` the response also sets
//! `__Host-nexus_session=<access_token>` with `HttpOnly +
//! Secure + SameSite=Lax + Path=/ + Max-Age=<access TTL>`.
//! Logout sets the cookie to `Max-Age=0` to clear it.
//!
//! ## Error model
//!
//! Every "bad creds / disabled / locked" path returns
//! `401 Unauthorized` with `{"error": "invalid_credentials"}`.
//! The body is deliberately identical across all three failure
//! modes — leaking "user exists but disabled" or "user exists
//! but locked" would let an attacker enumerate accounts.
//! Detail lives in the `audit_log` row + `tracing::warn`.
//!
//! ## Refresh rotation
//!
//! Single-use. Every successful refresh:
//!
//! 1. Looks up the row by SHA-256 of the presented secret.
//! 2. Rejects (401 + revoke whole chain) if the row is already
//!    rotated, already revoked, or past expiry.
//! 3. Mints a new (access, refresh) pair.
//! 4. Marks the old row `rotated_at = now` and writes the new
//!    row with `parent_id = old.id` + the same `chain_id`.
//!
//! Replay → the second use of the same refresh secret finds a
//! `rotated_at IS NOT NULL` row, calls `revoke_chain(chain_id)`
//! (kills every generation in the chain, not just the old
//! row), and returns 401. The original chain — and any access
//! tokens minted from it — are dead.

use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::header::{HeaderMap, HeaderValue, SET_COOKIE, USER_AGENT};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use nexus_config::LockoutConfig;
use nexus_store::{
    AuditActorKind, AuditOutcome, NewAuditEntry, NewRefreshToken, RefreshTokenId, SessionsError,
    Store, StoreError, UserId, UsersError,
};
use nexus_types::Role;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

use crate::admin_auth::AdminAuthState;

use super::passwords::{hash_password, verify_password, PasswordError, PasswordPolicyError};
use super::require_role::SessionContext;
use super::sessions::{
    hash_refresh_secret, issue_access_token, new_refresh_secret, RefreshSecret, SessionError,
};

// ---------------------------------------------------------------------------
// Tunables.
// ---------------------------------------------------------------------------

/// Access JWT lifetime. 15 min is the M6-doc'd default.
/// Surfaced via `expires_in` on the token response so the SPA
/// can schedule its refresh proactively.
pub const ACCESS_TOKEN_TTL: Duration = Duration::minutes(15);

/// Refresh secret lifetime — 30 days rolling. Each successful
/// refresh issues a fresh 30-day window; the chain dies the
/// moment anyone is idle for > 30 days OR a replay is detected.
pub const REFRESH_TOKEN_TTL: Duration = Duration::days(30);

/// v0.1.36 — server-side sliding-window idle ceiling. A refresh
/// chain whose most-recent activity is older than this is
/// killed by [`post_refresh`] (NOT by [`post_login`]; the
/// fresh row's `last_active_at` is `now()` so login can't trip
/// the gate). Mirrors the cloud's `IDLE_TIMEOUT` in
/// `api-gateway/src/auth.rs` so a user sees the same drop-out
/// behaviour on the appliance UI and the cloud console.
pub const IDLE_TIMEOUT: Duration = Duration::minutes(20);

/// `__Host-` prefix forbids any non-`Path=/` cookie + forces
/// `Secure`. Browsers reject the cookie outright if either
/// constraint is violated, so this prefix is defence-in-depth
/// against a misconfigured proxy stripping the `Secure` attr.
pub const SESSION_COOKIE_NAME: &str = "__Host-nexus_session";

// ---------------------------------------------------------------------------
// Wire shapes.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Logout accepts an optional refresh token. The SPA's "log out
/// other sessions" UX (future) hands a specific refresh; the
/// default logout button just calls the endpoint with `{}` and
/// the handler reads the bearer from the session cookie.
#[derive(Debug, Default, Deserialize)]
pub struct LogoutRequest {
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

/// Body of `POST /api/v1/auth/first-run-setup`. The endpoint is
/// unauthenticated — the operator can't authenticate yet
/// because no admin exists. Race protection is server-side
/// (any 2nd hit after the row is written returns 409). The UI
/// presents this form only when `GET /auth/info` reports
/// `first_run_pending: true`.
#[derive(Debug, Deserialize)]
pub struct FirstRunSetupRequest {
    /// Username for the initial admin account. Defaults to
    /// `"admin"` if absent or blank so the operator can just
    /// type a password and submit.
    #[serde(default)]
    pub username: Option<String>,
    /// Plaintext password. Goes through the same
    /// argon2id + min-length + denylist gate as
    /// `change-password`.
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until [`access_token`] expires. Mirrors the
    /// OAuth 2.0 convention.
    pub expires_in: i64,
    /// Seconds until [`refresh_token`] expires.
    pub refresh_expires_in: i64,
    pub user: SessionUser,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionUser {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub force_password_reset: bool,
}

// ---------------------------------------------------------------------------
// Error type.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum AuthApiError {
    /// HTTP 401. Bad credentials, disabled account, lockout,
    /// expired or replayed refresh token. Body is intentionally
    /// constant: `{"error": "invalid_credentials"}`. The
    /// audit log + tracing carry the variant.
    #[error("invalid_credentials")]
    InvalidCredentials,
    /// HTTP 400. Password policy rejected the new password on
    /// `/change-password`. Body includes the variant so the UI
    /// can render specific guidance.
    #[error("password_policy: {0}")]
    PasswordPolicy(#[from] PasswordPolicyError),
    /// HTTP 503. Engine started without
    /// `auth.admin_secret_path` configured. We can't sign JWTs
    /// without a key, and we won't fall back to "just trust
    /// everyone" on the login path even though
    /// `admin_auth_layer` does on the legacy admin routes.
    /// Operator must configure a secret first.
    #[error("auth_not_configured")]
    AuthNotConfigured,
    /// HTTP 409. First-run-setup called after the `users`
    /// table is already populated (either a real prior install
    /// or a TOCTOU loser racing another setup request). The UI
    /// reacts by re-fetching `/auth/info` and routing to the
    /// normal login form.
    #[error("first_run_already_complete")]
    FirstRunAlreadyComplete,
    /// HTTP 403. First-run-setup called against an engine whose
    /// `auth.mode` doesn't allow local users (e.g. pure OIDC).
    /// The UI should never present the form in this case — we
    /// reject defensively because the endpoint is unauth.
    #[error("first_run_mode_disallowed")]
    FirstRunModeDisallowed,
    /// HTTP 500. Argon2 hash failure (almost certainly OOM
    /// under memory pressure — the cost params reserve 19 MiB
    /// per invocation).
    #[error("password_hash: {0}")]
    PasswordHash(String),
    /// HTTP 500. JWT signing / RNG failure. Surfaced as a
    /// generic 500 because there's no recovery the SPA can
    /// attempt.
    #[error("session_internal: {0}")]
    SessionInternal(String),
    /// HTTP 500. Underlying SQLite error.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// HTTP 500. Users-API surfaced error other than NotFound
    /// (we translate NotFound → InvalidCredentials upstream).
    #[error("users: {0}")]
    Users(#[from] UsersError),
    /// v0.1.36 — HTTP 401. The refresh chain's last activity is
    /// older than [`IDLE_TIMEOUT`]; the chain is killed and the
    /// user must re-authenticate. Carries the actual idle
    /// duration in seconds so the UI can log "signed out after
    /// 20m of inactivity" rather than a generic 401.
    #[error("idle_expired: {idle_seconds}s")]
    IdleExpired { idle_seconds: i64 },
}

impl From<PasswordError> for AuthApiError {
    fn from(value: PasswordError) -> Self {
        match value {
            PasswordError::Argon2(s) => AuthApiError::PasswordHash(s),
            PasswordError::Policy(p) => AuthApiError::PasswordPolicy(p),
        }
    }
}

impl From<SessionError> for AuthApiError {
    fn from(value: SessionError) -> Self {
        AuthApiError::SessionInternal(value.to_string())
    }
}

impl From<SessionsError> for AuthApiError {
    fn from(value: SessionsError) -> Self {
        match value {
            SessionsError::Store(e) => AuthApiError::Store(e),
            other => AuthApiError::SessionInternal(other.to_string()),
        }
    }
}

impl IntoResponse for AuthApiError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            AuthApiError::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"error": "invalid_credentials"}),
            ),
            AuthApiError::PasswordPolicy(p) => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "password_policy",
                    "detail": p.to_string(),
                }),
            ),
            AuthApiError::AuthNotConfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({"error": "auth_not_configured"}),
            ),
            AuthApiError::FirstRunAlreadyComplete => (
                StatusCode::CONFLICT,
                serde_json::json!({"error": "first_run_already_complete"}),
            ),
            AuthApiError::FirstRunModeDisallowed => (
                StatusCode::FORBIDDEN,
                serde_json::json!({"error": "first_run_mode_disallowed"}),
            ),
            AuthApiError::IdleExpired { idle_seconds } => (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": format!("session idle: {idle_seconds}s since last activity"),
                    "code": "idle_expired",
                    "idle_seconds": idle_seconds,
                }),
            ),
            AuthApiError::PasswordHash(_)
            | AuthApiError::SessionInternal(_)
            | AuthApiError::Store(_)
            | AuthApiError::Users(_) => {
                tracing::error!(error = %self, "auth handler internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": "internal"}),
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// Cookie helpers.
// ---------------------------------------------------------------------------

/// Format the `Set-Cookie` value for an access token.
/// `__Host-` prefix means we MUST omit `Domain=`, MUST set
/// `Path=/`, and MUST set `Secure`. SameSite=Lax is the right
/// default for top-level navigation flows; the SPA's XHR
/// background refresh works either way.
pub fn build_session_cookie(token: &str, ttl: Duration) -> String {
    format!(
        "{name}={token}; Max-Age={max_age}; Path=/; HttpOnly; Secure; SameSite=Lax",
        name = SESSION_COOKIE_NAME,
        token = token,
        max_age = ttl.num_seconds().max(0),
    )
}

/// Format the `Set-Cookie` value that clears the session
/// cookie. `Max-Age=0` plus an explicit empty value is what
/// every browser interprets as "delete this cookie now".
pub fn build_clear_session_cookie() -> String {
    format!(
        "{name}=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Lax",
        name = SESSION_COOKIE_NAME
    )
}

fn attach_session_cookie(response: &mut Response, value: String) {
    if let Ok(hv) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(SET_COOKIE, hv);
    }
}

// ---------------------------------------------------------------------------
// State surface.
// ---------------------------------------------------------------------------

/// Sub-state the handlers need from `ApiState`. Bundled so the
/// handlers don't depend on the full `ApiState` shape — which
/// matters for unit-testable isolation.
#[derive(Clone)]
pub struct LoginState {
    pub store: Arc<Store>,
    pub admin_auth: Arc<AdminAuthState>,
    pub lockout: LockoutConfig,
    /// Snapshot of `cfg.runtime.state_dir`. Needed by
    /// [`post_change_password`] so it can remove the
    /// first-boot bootstrap-password sentinel file
    /// (`<state_dir>/bootstrap-password.txt`) once the
    /// operator rotates the generated OTP. See
    /// [`crate::auth::bootstrap::clear_bootstrap_sentinel`].
    pub state_dir: std::path::PathBuf,
    /// Snapshot of `cfg.auth.mode`. The first-run-setup
    /// handler ([`post_first_run_setup`]) refuses when
    /// `!mode.allows_local()` so an OIDC-only deployment
    /// cannot have an admin silently provisioned via the
    /// unauth setup endpoint.
    pub auth_mode: nexus_config::AuthMode,
}

// ---------------------------------------------------------------------------
// Audit helpers.
// ---------------------------------------------------------------------------

async fn record_login_failure_audit(store: &Store, username: &str, ip: &str, ua: Option<&str>) {
    let entry = NewAuditEntry {
        ip: Some(ip),
        user_agent: ua,
        ..NewAuditEntry::login_failure(username)
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(error = %e, "failed to record login.failure audit row");
    }
}

async fn record_login_success_audit(
    store: &Store,
    user_id: UserId,
    username: &str,
    ip: &str,
    ua: Option<&str>,
) {
    let actor_id_str = user_id.to_string();
    let entry = NewAuditEntry {
        actor_id: Some(&actor_id_str),
        ip: Some(ip),
        user_agent: ua,
        ..NewAuditEntry::login_success(AuditActorKind::LocalUser, username)
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(error = %e, "failed to record login.success audit row");
    }
}

async fn record_lockout_audit(store: &Store, user_id: UserId, username: &str) {
    let actor_id_str = user_id.to_string();
    let entry = NewAuditEntry {
        actor_kind: Some(AuditActorKind::LocalUser),
        actor_id: Some(&actor_id_str),
        actor_label: username,
        action: "lockout.tripped",
        outcome: AuditOutcome::Success,
        ..Default::default()
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(error = %e, "failed to record lockout.tripped audit row");
    }
}

async fn record_action_audit(
    store: &Store,
    user_id: UserId,
    username: &str,
    action: &str,
    outcome: AuditOutcome,
    ip: &str,
    ua: Option<&str>,
) {
    let actor_id_str = user_id.to_string();
    let entry = NewAuditEntry {
        actor_kind: Some(AuditActorKind::LocalUser),
        actor_id: Some(&actor_id_str),
        actor_label: username,
        action,
        outcome,
        ip: Some(ip),
        user_agent: ua,
        ..Default::default()
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(error = %e, action, "failed to record audit row");
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by login + refresh.
// ---------------------------------------------------------------------------

fn extract_client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    // X-Forwarded-For: standard reverse-proxy hop list.
    // First entry is the original client. We trust the proxy
    // chain because the engine is meant to sit behind a single
    // trusted reverse proxy (Caddy/nginx) at the LAN boundary —
    // anyone bypassing the proxy is already on the LAN and the
    // IP is a logging cue, not a security boundary.
    if let Some(val) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = val.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    peer.ip().to_string()
}

fn extract_user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok())
}

/// Build the success response body + attach the session cookie.
/// Shared between `login` and `refresh`.
fn make_token_response(access_token: String, refresh_token: String, user: SessionUser) -> Response {
    let body = TokenResponse {
        access_token: access_token.clone(),
        refresh_token,
        expires_in: ACCESS_TOKEN_TTL.num_seconds(),
        refresh_expires_in: REFRESH_TOKEN_TTL.num_seconds(),
        user,
    };
    let mut response = (StatusCode::OK, Json(body)).into_response();
    attach_session_cookie(
        &mut response,
        build_session_cookie(&access_token, ACCESS_TOKEN_TTL),
    );
    response
}

/// Mint + insert a fresh refresh row. Returns the plaintext
/// secret (to hand back to the caller exactly once) and the row
/// id (so the rotation path can wire `parent_id`).
async fn mint_refresh_row(
    store: &Store,
    user_id: UserId,
    chain_id: &str,
    parent_id: Option<RefreshTokenId>,
    now: chrono::DateTime<Utc>,
    ip: &str,
    ua: Option<&str>,
) -> Result<(RefreshSecret, RefreshTokenId), AuthApiError> {
    let secret = new_refresh_secret().map_err(AuthApiError::from)?;
    let token_hash = hash_refresh_secret(&secret);
    let expires_at = now + REFRESH_TOKEN_TTL;
    let row = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: &token_hash,
            user_id,
            chain_id,
            parent_id,
            expires_at,
            user_agent: ua,
            ip: Some(ip),
        })
        .await?;
    Ok((secret, row.id))
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `POST /api/v1/auth/login`
///
/// Lookup → lockout check → password verify → emit tokens. Bad
/// path collapses every failure variant to `InvalidCredentials`
/// for the client; the audit row + tracing carry the variant.
pub async fn post_login(
    State(state): State<LoginState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AuthApiError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let secret = state
        .admin_auth
        .admin_secret()
        .ok_or(AuthApiError::AuthNotConfigured)?
        .to_string();

    let username = body.username.trim().to_lowercase();
    let now = Utc::now();

    // Lookup user. Branchless 401-on-missing — the audit row
    // is the only thing distinguishing this from "user exists,
    // password wrong".
    let Some(user) = state.store.get_user_by_username(&username).await? else {
        tracing::warn!(username = %username, ip = %ip, "login rejected: no such user");
        record_login_failure_audit(&state.store, &username, &ip, ua).await;
        return Err(AuthApiError::InvalidCredentials);
    };

    if user.disabled || user.deleted_at.is_some() {
        tracing::warn!(username = %username, ip = %ip, "login rejected: disabled/deleted user");
        record_login_failure_audit(&state.store, &username, &ip, ua).await;
        return Err(AuthApiError::InvalidCredentials);
    }

    if matches!(
        super::lockout::evaluate(now, &user),
        super::lockout::LockState::Locked { .. }
    ) {
        tracing::warn!(username = %username, ip = %ip, "login rejected: account locked");
        record_login_failure_audit(&state.store, &username, &ip, ua).await;
        return Err(AuthApiError::InvalidCredentials);
    }

    let Some((id, phc)) = state.store.get_password_hash_for_login(&username).await? else {
        // Shouldn't happen — `get_user_by_username` already
        // checked. But guard rails: log + 401.
        tracing::warn!(username = %username, ip = %ip, "login rejected: no password hash");
        record_login_failure_audit(&state.store, &username, &ip, ua).await;
        return Err(AuthApiError::InvalidCredentials);
    };

    if !verify_password(&body.password, &phc) {
        let outcome = super::lockout::evaluate_failure(now, &user, &state.lockout);
        state
            .store
            .record_login_failure(id, outcome.new_count, outcome.lock_until)
            .await?;
        if outcome.lockout_tripped {
            record_lockout_audit(&state.store, id, &username).await;
        }
        tracing::warn!(
            username = %username,
            ip = %ip,
            failed_count = outcome.new_count,
            locked = outcome.lockout_tripped,
            "login rejected: bad password",
        );
        record_login_failure_audit(&state.store, &username, &ip, ua).await;
        return Err(AuthApiError::InvalidCredentials);
    }

    // Success.
    state.store.record_login_success(id).await?;
    record_login_success_audit(&state.store, id, &username, &ip, ua).await;
    tracing::info!(username = %username, ip = %ip, "login success");

    let chain_id = Uuid::now_v7().to_string();
    let access_token = issue_access_token(
        id,
        user.role,
        secret.as_bytes(),
        now,
        ACCESS_TOKEN_TTL,
        Some(&chain_id),
    )?;
    let (refresh_secret, _row_id) =
        mint_refresh_row(&state.store, id, &chain_id, None, now, &ip, ua).await?;

    Ok(make_token_response(
        access_token,
        refresh_secret.expose().to_string(),
        SessionUser {
            id,
            username: user.username,
            role: user.role,
            force_password_reset: user.force_password_reset,
        },
    ))
}

/// `POST /api/v1/auth/refresh`
///
/// Single-use rotation with replay detection. See module
/// docstring for the chain semantics.
pub async fn post_refresh(
    State(state): State<LoginState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RefreshRequest>,
) -> Result<Response, AuthApiError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let secret = state
        .admin_auth
        .admin_secret()
        .ok_or(AuthApiError::AuthNotConfigured)?
        .to_string();

    let now = Utc::now();
    let presented = RefreshSecret::from_str_unchecked(body.refresh_token.clone());
    let hashed = hash_refresh_secret(&presented);

    let Some(row) = state.store.get_refresh_token_by_hash(&hashed).await? else {
        tracing::warn!(ip = %ip, "refresh rejected: unknown token hash");
        return Err(AuthApiError::InvalidCredentials);
    };

    // v0.1.36 \u2014 sliding-window idle check FIRST. We compare the
    // peak `last_active_at` across the whole live chain (not just
    // the row presented) so a stale tab can't refresh while a sibling
    // tab kept the user active. If the peak is older than
    // `IDLE_TIMEOUT`, kill the whole chain and 401 with `code:
    // idle_expired`. Done BEFORE replay detection so that an idle
    // chain that's also been replayed still reports `idle_expired`
    // (more useful to the SPA) rather than collapsing to
    // `invalid_credentials`.
    if let Some(peak) = state.store.max_chain_last_active_at(&row.chain_id).await? {
        let idle = now.signed_duration_since(peak);
        if idle > IDLE_TIMEOUT {
            if let Err(e) = state.store.idle_revoke_chain(&row.chain_id, now).await {
                tracing::error!(error = %e, "failed to idle-revoke chain");
            }
            tracing::warn!(
                chain_id = %row.chain_id,
                user_id = row.user_id,
                ip = %ip,
                idle_seconds = idle.num_seconds(),
                "refresh rejected: idle timeout exceeded, chain revoked"
            );
            return Err(AuthApiError::IdleExpired {
                idle_seconds: idle.num_seconds(),
            });
        }
    }

    // Replay detection FIRST. If the row is already rotated
    // OR already revoked, the chain is dead — revoke every
    // sibling generation and 401.
    if row.rotated_at.is_some() || row.revoked_at.is_some() {
        if let Err(e) = state.store.revoke_chain(&row.chain_id, now).await {
            tracing::error!(error = %e, "failed to revoke chain on replay");
        }
        tracing::warn!(
            chain_id = %row.chain_id,
            user_id = row.user_id,
            ip = %ip,
            "refresh rejected: replay detected, chain revoked"
        );
        return Err(AuthApiError::InvalidCredentials);
    }
    if !row.is_live_at(now) {
        tracing::warn!(
            chain_id = %row.chain_id,
            user_id = row.user_id,
            ip = %ip,
            "refresh rejected: expired"
        );
        return Err(AuthApiError::InvalidCredentials);
    }

    // Resolve the user — needed for the new access token + the
    // new refresh row. Account may have been disabled or
    // deleted since the original login.
    let Some(user) = state.store.get_user_by_id(row.user_id).await? else {
        tracing::warn!(user_id = row.user_id, ip = %ip, "refresh rejected: user gone");
        let _ = state.store.revoke_chain(&row.chain_id, now).await;
        return Err(AuthApiError::InvalidCredentials);
    };
    if user.disabled || user.deleted_at.is_some() {
        tracing::warn!(user_id = row.user_id, ip = %ip, "refresh rejected: user disabled/deleted");
        let _ = state.store.revoke_chain(&row.chain_id, now).await;
        return Err(AuthApiError::InvalidCredentials);
    }

    // Mint new pair.
    let access_token = issue_access_token(
        user.id,
        user.role,
        secret.as_bytes(),
        now,
        ACCESS_TOKEN_TTL,
        Some(&row.chain_id),
    )?;
    let (new_secret, new_row_id) = mint_refresh_row(
        &state.store,
        user.id,
        &row.chain_id,
        Some(row.id),
        now,
        &ip,
        ua,
    )
    .await?;

    // Mark old row rotated AFTER the new row exists — if the
    // insert fails, the old row is still usable, so the SPA
    // can retry without a forced re-login.
    if let Err(e) = state.store.mark_refresh_token_rotated(row.id, now).await {
        tracing::error!(
            error = %e,
            new_row_id,
            "rotated insert succeeded but marking old row failed — replay window briefly open",
        );
    }

    record_action_audit(
        &state.store,
        user.id,
        &user.username,
        "session.refresh",
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;

    Ok(make_token_response(
        access_token,
        new_secret.expose().to_string(),
        SessionUser {
            id: user.id,
            username: user.username,
            role: user.role,
            force_password_reset: user.force_password_reset,
        },
    ))
}

/// `POST /api/v1/auth/logout`
///
/// Idempotent. If a refresh token is supplied (in the body),
/// revoke the entire chain. Always clears the session cookie.
/// Returns 204 even when no chain is found — we don't want to
/// signal "that was a valid token" by the response shape.
pub async fn post_logout(
    State(state): State<LoginState>,
    Json(body): Json<LogoutRequest>,
) -> Response {
    if let Some(refresh) = body.refresh_token.as_deref() {
        let presented = RefreshSecret::from_str_unchecked(refresh.to_string());
        let hashed = hash_refresh_secret(&presented);
        match state.store.get_refresh_token_by_hash(&hashed).await {
            Ok(Some(row)) => {
                if let Err(e) = state.store.revoke_chain(&row.chain_id, Utc::now()).await {
                    tracing::warn!(error = %e, "logout: failed to revoke chain");
                }
            }
            Ok(None) => {
                tracing::debug!("logout: refresh token not found (already revoked?)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "logout: refresh lookup failed");
            }
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    attach_session_cookie(&mut response, build_clear_session_cookie());
    response
}

/// `POST /api/v1/auth/change-password`
///
/// Authenticated. Verifies the old password (re-fetched from
/// the store rather than trusting the bearer's identity alone
/// — defence-in-depth), runs the new password through the
/// argon2id policy gate, writes the new hash, clears
/// `force_password_reset`. Active refresh chains are
/// invalidated so a stolen session can't survive a forced
/// password rotation.
pub async fn post_change_password(
    State(state): State<LoginState>,
    ctx: SessionContext,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<StatusCode, AuthApiError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    // Legacy admin tokens (no user row) have user_id=0; reject
    // them — they shouldn't be calling change-password.
    if ctx.is_legacy_admin || ctx.user_id == 0 {
        tracing::warn!(jti = %ctx.jti, "change-password rejected: legacy admin token has no user row");
        return Err(AuthApiError::InvalidCredentials);
    }

    let Some(user) = state.store.get_user_by_id(ctx.user_id).await? else {
        tracing::warn!(user_id = ctx.user_id, "change-password rejected: user gone");
        return Err(AuthApiError::InvalidCredentials);
    };
    let Some((id, phc)) = state
        .store
        .get_password_hash_for_login(&user.username)
        .await?
    else {
        tracing::warn!(
            user_id = ctx.user_id,
            "change-password rejected: no password hash"
        );
        return Err(AuthApiError::InvalidCredentials);
    };
    if !verify_password(&body.old_password, &phc) {
        tracing::warn!(
            user_id = ctx.user_id,
            "change-password rejected: bad old password"
        );
        record_action_audit(
            &state.store,
            id,
            &user.username,
            "password.change",
            AuditOutcome::Failure,
            &ip,
            ua,
        )
        .await;
        return Err(AuthApiError::InvalidCredentials);
    }

    // Policy + hash the new one. The policy gate inside
    // hash_password short-circuits before the (~100 ms) argon2
    // call when too-short / common.
    let new_hash = hash_password(&body.new_password)?;
    state
        .store
        .update_user_password(id, &new_hash, false)
        .await?;

    // Invalidate every refresh chain the user has. Forces all
    // signed-in sessions (incl. the current one) to re-login on
    // the next access-token expiry.
    let live = state
        .store
        .list_active_refresh_tokens_for_user(id, Utc::now())
        .await?;
    for row in &live {
        if let Err(e) = state.store.revoke_chain(&row.chain_id, Utc::now()).await {
            tracing::warn!(error = %e, chain_id = %row.chain_id, "failed to revoke chain on password change");
        }
    }
    record_action_audit(
        &state.store,
        id,
        &user.username,
        "password.change",
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    // M-Install Checkpoint 3c — the first-boot OTP is now
    // dead. Wipe the on-disk sentinel that the installer
    // banner reads. Best-effort: never fails the request, the
    // helper logs at warn! if the unlink can't happen.
    crate::auth::bootstrap::clear_bootstrap_sentinel(&state.state_dir);
    tracing::info!(
        user_id = id,
        "password changed; {} chain(s) revoked",
        live.len()
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/auth/first-run-setup`
///
/// Unauthenticated. Provisions the initial local admin account
/// the first time the engine boots against an empty database
/// and immediately signs the operator in (returns the same
/// [`TokenResponse`] shape as `/auth/login`).
///
/// The UI presents this form ONLY when
/// `GET /auth/info` reports `first_run_pending: true`; the
/// server also enforces the precondition because the endpoint
/// is unauth.
///
/// Race / replay protection:
///   * `count_users() > 0` → 409 Conflict.
///   * `auth.mode` disallows local users → 403 Forbidden.
///   * `auth.admin_secret_path` not configured → 503 (can't
///     mint JWTs).
///   * Insert lost a race with another setup request →
///     [`UsersError::UsernameTaken`] surfaces as 409.
///
/// Any successful call writes one `setup.first_run` audit row
/// plus one `login.success` audit row tagged with the source IP
/// so the operator has provenance for who claimed the first
/// admin account.
pub async fn post_first_run_setup(
    State(state): State<LoginState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<FirstRunSetupRequest>,
) -> Result<Response, AuthApiError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    if !state.auth_mode.allows_local() {
        tracing::warn!(
            mode = ?state.auth_mode,
            ip = %ip,
            "first-run-setup rejected: mode disallows local users",
        );
        return Err(AuthApiError::FirstRunModeDisallowed);
    }

    let secret = state
        .admin_auth
        .admin_secret()
        .ok_or(AuthApiError::AuthNotConfigured)?
        .to_string();

    // Precondition check: any pre-existing user (active,
    // disabled, or tombstoned) means setup has already
    // happened. count_users() includes tombstones — see the
    // doc-comment on `Store::count_users` for the rationale.
    let n = state.store.count_users().await?;
    if n > 0 {
        tracing::warn!(
            user_count = n,
            ip = %ip,
            "first-run-setup rejected: users table already populated",
        );
        return Err(AuthApiError::FirstRunAlreadyComplete);
    }

    let username = body
        .username
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(crate::auth::bootstrap::BOOTSTRAP_USERNAME)
        .to_lowercase();

    // Policy + hash. Argon2 is ~100 ms; the policy gate inside
    // `hash_password` short-circuits before that for too-short
    // or denylisted passwords.
    let hash = hash_password(&body.password)?;

    let user_id = match state
        .store
        .create_user(&nexus_store::NewUser {
            username: &username,
            role: Role::Admin,
            password_hash: Some(&hash),
            oidc_subject_hash: None,
            force_password_reset: false,
        })
        .await
    {
        Ok(id) => id,
        // Lost a race with another concurrent setup request —
        // collapse to the same 409 so the UI's retry logic is
        // identical to the "populated DB" case.
        Err(UsersError::UsernameTaken) => {
            tracing::warn!(
                ip = %ip,
                "first-run-setup rejected: lost race to another concurrent setup request",
            );
            return Err(AuthApiError::FirstRunAlreadyComplete);
        }
        Err(other) => return Err(AuthApiError::Users(other)),
    };

    // Best-effort: any stale OTP sentinel left over from a
    // previous install / dev iteration is now obsolete.
    crate::auth::bootstrap::clear_bootstrap_sentinel(&state.state_dir);

    let now = Utc::now();
    record_action_audit(
        &state.store,
        user_id,
        &username,
        "setup.first_run",
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    state.store.record_login_success(user_id).await?;
    record_login_success_audit(&state.store, user_id, &username, &ip, ua).await;
    tracing::warn!(
        user_id,
        username = %username,
        ip = %ip,
        "FIRST-RUN ADMIN PROVISIONED via UI setup endpoint",
    );

    let chain_id = Uuid::now_v7().to_string();
    let access_token = issue_access_token(
        user_id,
        Role::Admin,
        secret.as_bytes(),
        now,
        ACCESS_TOKEN_TTL,
        Some(&chain_id),
    )?;
    let (refresh_secret, _row_id) =
        mint_refresh_row(&state.store, user_id, &chain_id, None, now, &ip, ua).await?;

    Ok(make_token_response(
        access_token,
        refresh_secret.expose().to_string(),
        SessionUser {
            id: user_id,
            username,
            role: Role::Admin,
            force_password_reset: false,
        },
    ))
}

// ---------------------------------------------------------------------------
// Bits the FromRef bridge needs on ApiState.
// ---------------------------------------------------------------------------

impl axum::extract::FromRef<crate::api::ApiState> for LoginState {
    fn from_ref(input: &crate::api::ApiState) -> Self {
        LoginState {
            store: input.store.clone(),
            admin_auth: input.admin_auth.clone(),
            lockout: input.lockout.clone(),
            state_dir: input.state_dir.clone(),
            auth_mode: input.auth_mode,
        }
    }
}

// `SessionContext` (and the role-newtypes) require
// `Arc<AdminAuthState>: FromRef<S>` where `S` is the router's
// state type. In production the router is built with
// `State<ApiState>` and the existing bridge in
// `require_role.rs` covers it. The integration tests below
// however build a lean router with `State<LoginState>`, so the
// extractor needs the same bridge against `LoginState` —
// otherwise `post_change_password` (the only handler that
// takes a `SessionContext`) won't satisfy the `Handler` trait
// bound for `.route(... post(...))`.
impl axum::extract::FromRef<LoginState> for std::sync::Arc<crate::admin_auth::AdminAuthState> {
    fn from_ref(input: &LoginState) -> Self {
        input.admin_auth.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests (integration: spin up an axum Router against a tempdir store).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use nexus_config::StoreConfig;
    use nexus_store::NewUser;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"test-shared-admin-secret-32-bytes!";
    const ADMIN_USER: &str = "admin";
    const ADMIN_PW: &str = "BootstrapPassword!1";

    async fn fresh_state() -> (LoginState, Arc<Store>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db_path.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        let admin_auth = Arc::new(AdminAuthState::from_secret_bytes(Some(TEST_SECRET), false));
        let lockout = LockoutConfig::default();
        let state = LoginState {
            store: store.clone(),
            admin_auth,
            lockout,
            state_dir: dir.path().to_path_buf(),
            auth_mode: nexus_config::AuthMode::Local,
        };
        // Seed an admin user with a known password.
        let phc = hash_password(ADMIN_PW).unwrap();
        store
            .create_user(&NewUser {
                username: ADMIN_USER,
                role: Role::Admin,
                password_hash: Some(&phc),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        (state, store, dir)
    }

    fn build_app(state: LoginState) -> Router {
        Router::new()
            .route("/auth/login", post(post_login))
            .route("/auth/refresh", post(post_refresh))
            .route("/auth/logout", post(post_logout))
            .route("/auth/change-password", post(post_change_password))
            .with_state(state)
            .layer(MockConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                12345,
            )))
    }

    fn json_request(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    fn json_request_with_bearer(uri: &str, body: serde_json::Value, bearer: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn parse_token_response(resp: Response) -> TokenResponse {
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ----- login: happy path -----

    #[tokio::test]
    async fn login_happy_path_returns_tokens_and_sets_cookie() {
        let (state, store, _g) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = resp
            .headers()
            .get(SET_COOKIE)
            .expect("set-cookie set")
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("__Host-nexus_session="), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        let body = parse_token_response(resp).await;
        assert!(!body.access_token.is_empty());
        assert!(!body.refresh_token.is_empty());
        assert_eq!(body.expires_in, ACCESS_TOKEN_TTL.num_seconds());
        assert_eq!(body.user.username, ADMIN_USER);
        assert_eq!(body.user.role, Role::Admin);
        // Audit row written.
        let audits = store
            .list_audit_filtered(
                &nexus_store::AuditFilter {
                    action: Some("login.success"),
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0].actor_label, ADMIN_USER);
    }

    // ----- login: every failure mode returns the same 401 body -----

    #[tokio::test]
    async fn login_unknown_user_returns_invalid_credentials() {
        let (state, store, _g) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": "ghost", "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, serde_json::json!({"error": "invalid_credentials"}));
        let audits = store
            .list_audit_filtered(
                &nexus_store::AuditFilter {
                    action: Some("login.failure"),
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(audits.len(), 1);
    }

    #[tokio::test]
    async fn login_bad_password_increments_failure_count_and_audits() {
        let (state, store, _g) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": "wrong-password-12"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let user = store
            .get_user_by_username(ADMIN_USER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.failed_login_count, 1);
        assert!(user.locked_until.is_none());
    }

    #[tokio::test]
    async fn login_after_threshold_locks_account_and_emits_lockout_audit() {
        let (state, store, _g) = fresh_state().await;
        let max = state.lockout.max_attempts as i64;
        for _ in 0..max {
            let app = build_app(state.clone());
            let resp = app
                .oneshot(json_request(
                    "/auth/login",
                    serde_json::json!({"username": ADMIN_USER, "password": "wrong-password-12"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let user = store
            .get_user_by_username(ADMIN_USER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.failed_login_count, max);
        assert!(user.locked_until.is_some(), "expected lockout trip");
        let audits = store
            .list_audit_filtered(
                &nexus_store::AuditFilter {
                    action: Some("lockout.tripped"),
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(audits.len(), 1);

        // And the next login (even with the right password) is
        // 401 with the same body.
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ----- refresh: happy path -----

    #[tokio::test]
    async fn refresh_returns_new_pair_and_old_token_no_longer_works() {
        let (state, _store, _g) = fresh_state().await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        let first = parse_token_response(resp).await;

        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/refresh",
                serde_json::json!({"refresh_token": first.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let second = parse_token_response(resp).await;
        assert_ne!(first.refresh_token, second.refresh_token);
        assert_ne!(first.access_token, second.access_token);

        // Reusing the FIRST refresh token now triggers replay
        // detection — 401 and the SECOND refresh is also dead.
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/refresh",
                serde_json::json!({"refresh_token": first.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // The second refresh token was rotated-from a now-revoked
        // chain. It should also fail.
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/refresh",
                serde_json::json!({"refresh_token": second.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_unknown_token_returns_401() {
        let (state, _store, _g) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/refresh",
                serde_json::json!({"refresh_token": "not-a-real-token"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ----- logout -----

    #[tokio::test]
    async fn logout_revokes_chain_and_clears_cookie() {
        let (state, store, _g) = fresh_state().await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        let first = parse_token_response(resp).await;
        let user_id = first.user.id;

        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/logout",
                serde_json::json!({"refresh_token": first.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let cookie = resp.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.contains("Max-Age=0"), "{cookie}");

        // Chain is dead — no active rows left.
        let active = store
            .list_active_refresh_tokens_for_user(user_id, Utc::now())
            .await
            .unwrap();
        assert!(active.is_empty(), "{active:?}");
    }

    // ----- change-password -----

    #[tokio::test]
    async fn change_password_rejects_bad_old_password() {
        let (state, _store, _g) = fresh_state().await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        let tokens = parse_token_response(resp).await;

        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "/auth/change-password",
                serde_json::json!({
                    "old_password": "wrong-current-pw-12",
                    "new_password": "BrandNewPassword99!",
                }),
                &tokens.access_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn change_password_happy_path_updates_hash_and_revokes_chains() {
        let (state, store, _g) = fresh_state().await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        let tokens = parse_token_response(resp).await;
        let new_pw = "BrandNewPassword99!";

        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request_with_bearer(
                "/auth/change-password",
                serde_json::json!({"old_password": ADMIN_PW, "new_password": new_pw}),
                &tokens.access_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Old refresh chain revoked.
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/refresh",
                serde_json::json!({"refresh_token": tokens.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // New password works on subsequent login.
        let app = build_app(state);
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": new_pw}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // force_password_reset cleared.
        let user = store
            .get_user_by_username(ADMIN_USER)
            .await
            .unwrap()
            .unwrap();
        assert!(!user.force_password_reset);
    }

    #[tokio::test]
    async fn change_password_rejects_short_new_password_with_400() {
        let (state, _store, _g) = fresh_state().await;
        let app = build_app(state.clone());
        let resp = app
            .oneshot(json_request(
                "/auth/login",
                serde_json::json!({"username": ADMIN_USER, "password": ADMIN_PW}),
            ))
            .await
            .unwrap();
        let tokens = parse_token_response(resp).await;

        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "/auth/change-password",
                serde_json::json!({
                    "old_password": ADMIN_PW,
                    "new_password": "short",
                }),
                &tokens.access_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ----- cookie helpers (pure functions) -----

    #[test]
    fn session_cookie_has_all_required_attributes() {
        let c = build_session_cookie("abc", Duration::minutes(15));
        assert!(c.starts_with("__Host-nexus_session=abc"), "{c}");
        assert!(c.contains("Max-Age=900"), "{c}");
        assert!(c.contains("Path=/"), "{c}");
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("Secure"), "{c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
    }

    #[test]
    fn clear_session_cookie_zeroes_max_age() {
        let c = build_clear_session_cookie();
        assert!(c.contains("Max-Age=0"), "{c}");
        assert!(c.starts_with("__Host-nexus_session=;"), "{c}");
    }
}
