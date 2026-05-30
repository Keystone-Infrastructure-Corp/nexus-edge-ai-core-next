//! M6 Phase 3 Steps 3.3 + 3.4 — OIDC auth-code + PKCE login flow.
//!
//! Two HTTP handlers + an in-memory PKCE/state session manager.
//! The flow:
//!
//! 1. UI calls `POST /api/v1/auth/oidc/start` to obtain a
//!    redirect URL. Engine mints PKCE verifier + state + nonce,
//!    stashes them in [`OidcLoginSessions`], returns the URL.
//!    The UI then sets `window.location = <url>`.
//!
//! 2. IdP redirects the browser to
//!    `GET /api/v1/auth/oidc/callback?code=…&state=…`. Engine
//!    pops the matching pending session (single-use), exchanges
//!    the code at the IdP's token endpoint with the PKCE
//!    verifier, verifies the returned ID token via
//!    [`OidcClient::verify_id_token`], validates the `nonce`
//!    matches what we stashed, runs [`map_role`] over the
//!    claims, looks-up-or-creates the local user keyed by a
//!    SHA-256 of `iss + "\0" + sub` (Step 3.4), mints local
//!    session tokens via the same primitives `auth::login`
//!    uses, sets the `__Host-nexus_session` cookie, and
//!    responds with a 302 redirect back to `/#/dashboard` (or
//!    the operator-supplied `redirect_to`).
//!
//! Security pins:
//!
//! * **PKCE S256 is mandatory** — never `plain`. The challenge
//!   is `b64url(sha256(verifier))` of a 64-byte random verifier.
//! * **State is single-use** — popped from the session map on
//!   `/callback`, never re-issued. A replay returns 400
//!   `unknown_state` (deliberately the same body as a state
//!   that was never issued, so an attacker can't probe).
//! * **Nonce is checked against the stashed value** — defends
//!   against a stolen ID token that wasn't minted for this
//!   exact login attempt.
//! * **`redirect_to` is constrained to relative paths**
//!   beginning with `/` and not `//` (else any open-redirect to
//!   `https://evil.example/` is possible). Rejected requests
//!   land on `/` rather than failing closed; logged at WARN.
//! * **OIDC subjects are hashed (SHA-256) before SQL** — even
//!   our store team never sees plaintext subs. The hash uses
//!   `iss + "\0" + sub` so two IdPs publishing the same
//!   subject value can't collide. Matches the audit pin in
//!   [`migrations/0010_local_users.sql`](../../../../crates/nexus-store/migrations/0010_local_users.sql).
//! * **Last-admin protection is enforced on lookup, not login**
//!   — first-OIDC-user-becomes-admin would be a footgun, so we
//!   instead require the operator to wire the role_map. Step
//!   3.2's `deny_unmapped` knob is the right place to harden
//!   this further; this module just consumes the mapped role.
//!
//! Auto-provisioning rule (Step 3.4):
//!
//! * If `get_user_by_oidc_subject(hash)` finds a row → update
//!   `last_login_at`, mint tokens, redirect.
//! * If absent → call `create_user` with a generated username
//!   derived from the `preferred_username` / `email` /
//!   `sub` claim (in that order). If the username collides
//!   with an existing LOCAL user, append `-oidc-<6 hex>` to
//!   disambiguate. The store's username UNIQUE index enforces
//!   the rest.
//! * If the user is `disabled` or `deleted` → refuse login
//!   with a generic `oidc_user_not_eligible` body. The audit
//!   row carries the variant.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::extract::FromRef;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{Duration, Utc};
use getrandom::fill as getrandom_fill;
use nexus_config::OidcConfig;
use nexus_store::{NewUser, Store, User as StoreUser};
use nexus_types::Role;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use uuid::Uuid;

use crate::admin_auth::AdminAuthState;
use crate::api::ApiState;

use super::login::{
    build_session_cookie, SessionUser, TokenResponse, ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL,
};
use super::oidc::{IdTokenClaims, OidcClient, OidcError, VerifiedIdToken};
use super::oidc_role_map::{map_role, MapError};
use super::sessions::{hash_refresh_secret, issue_access_token, new_refresh_secret, SessionError};

/// Pending login sessions are tossed after this. The IdP round
/// trip takes seconds in practice; 10 min accommodates a
/// distracted operator going for coffee mid-consent without
/// keeping cold sessions around forever.
pub const PENDING_TTL: StdDuration = StdDuration::from_secs(600);

/// Cookie that carries the per-attempt CSRF / state binding so
/// the callback can be sure the same browser that initiated the
/// flow is the one returning the code. Belt-and-suspenders on
/// top of the `state` URL parameter — defends against an
/// attacker who learns `state` via a leaked referer.
pub const STATE_COOKIE_NAME: &str = "__Host-nexus_oidc_state";

/// In-memory PKCE/state session registry. Keyed by `state`.
///
/// `Arc<Mutex<HashMap>>` chosen over `DashMap` because the
/// per-request work is tiny (one `insert` / `remove` per HTTP
/// round-trip per browser) and the lock-coarsening saves us
/// from an entire dep on dashmap for this module alone.
#[derive(Clone, Default)]
pub struct OidcLoginSessions {
    inner: Arc<Mutex<HashMap<String, PendingLogin>>>,
}

/// One pending login attempt. Field order is what
/// `start` writes + what `callback` reads.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub pkce_verifier: String,
    pub nonce: String,
    pub redirect_to: String,
    pub created_at: chrono::DateTime<Utc>,
}

impl PendingLogin {
    fn is_expired(&self, now: chrono::DateTime<Utc>) -> bool {
        // Convert std::time::Duration → chrono::Duration once.
        let ttl = Duration::from_std(PENDING_TTL).expect("PENDING_TTL fits chrono::Duration");
        now - self.created_at > ttl
    }
}

impl OidcLoginSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert. Sweeps expired entries on every insert so the map
    /// can't grow without bound on a long-lived engine. O(n) per
    /// insert is fine because in practice n is single-digit
    /// (one per concurrent operator mid-login).
    pub fn insert(&self, state: String, pending: PendingLogin) {
        let now = pending.created_at;
        let mut g = self.inner.lock();
        g.retain(|_, p| !p.is_expired(now));
        g.insert(state, pending);
    }

    /// Pop the session for `state`. Returns `None` for missing,
    /// expired, or unknown — the callback handler must not
    /// distinguish so an attacker can't probe.
    pub fn take(&self, state: &str, now: chrono::DateTime<Utc>) -> Option<PendingLogin> {
        let mut g = self.inner.lock();
        let entry = g.remove(state)?;
        if entry.is_expired(now) {
            return None;
        }
        Some(entry)
    }

    /// Test helper: read without removing. Not exposed in
    /// production.
    #[cfg(test)]
    pub fn peek(&self, state: &str) -> Option<PendingLogin> {
        self.inner.lock().get(state).cloned()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

// ---------------------------------------------------------------------------
// Substate for the two handlers. Mirrors the `LoginState`
// pattern in `auth::login` — pulled out of `ApiState` via
// `FromRef` so the handlers don't ingest the whole engine
// state.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OidcLoginState {
    pub store: Arc<Store>,
    pub admin_auth: Arc<AdminAuthState>,
    pub oidc_client: Arc<OidcClient>,
    pub cfg: OidcConfig,
    pub sessions: OidcLoginSessions,
    /// Optional OIDC client secret, loaded from
    /// `cfg.auth.oidc.client_secret_file` at engine boot. When
    /// present, sent alongside PKCE to the token endpoint
    /// (confidential web-app flow); when `None`, PKCE-only.
    /// Wrapped in `Arc` so cloning `OidcLoginState` doesn't
    /// duplicate the secret in the heap.
    pub client_secret: Option<Arc<String>>,
}

impl FromRef<ApiState> for OidcLoginState {
    fn from_ref(s: &ApiState) -> Self {
        // `ApiState::oidc_login` is `Option<OidcLoginState>`. We
        // unwrap with `.expect()` here because the router only
        // mounts the OIDC routes when this Option is `Some` —
        // see `api::router`. A None here is a wire-up bug.
        s.oidc_login
            .clone()
            .expect("OidcLoginState requested but not configured; router wiring bug")
    }
}

// ---------------------------------------------------------------------------
// Wire types.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct StartRequest {
    /// Where to redirect the browser after a successful login.
    /// Must be a relative path beginning with `/` and NOT `//`
    /// (else any open-redirect to `https://evil.example/` is
    /// possible). Defaults to `/`.
    #[serde(default)]
    pub redirect_to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    pub authorization_url: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// Set by the IdP when the user cancels / consent is
    /// denied. Surfaced to the UI so it can render a friendly
    /// "you cancelled" toast rather than a generic 400.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Error type.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum OidcLoginError {
    /// 400 — malformed request, unknown state, replay,
    /// rejected redirect_to, etc. Body is constant so an
    /// attacker can't probe.
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    /// 403 — claims didn't match any role_map entry AND
    /// `deny_unmapped = true`. Body: `unmapped_role`.
    #[error("unmapped role")]
    UnmappedRole,
    /// 403 — user exists but is disabled / deleted.
    /// Body: `oidc_user_not_eligible`.
    #[error("user not eligible for oidc login")]
    NotEligible,
    /// 502 — the IdP's token endpoint returned non-2xx.
    /// Body: `idp_token_endpoint`.
    #[error("idp token endpoint: {0}")]
    IdpTokenEndpoint(String),
    /// 502 — the IdP returned a token response we can't parse
    /// or an ID token that didn't validate.
    /// Body: `idp_invalid_token`.
    #[error("idp invalid token: {0}")]
    IdpInvalidToken(String),
    /// 503 — engine has no admin HS256 secret configured so we
    /// can't mint local session tokens. Body: `auth_not_configured`.
    #[error("auth not configured")]
    AuthNotConfigured,
    /// 500 — internal failure (DB, hash, etc.).
    #[error("internal: {0}")]
    Internal(String),
}

impl From<SessionError> for OidcLoginError {
    fn from(e: SessionError) -> Self {
        Self::Internal(format!("session: {e}"))
    }
}

impl From<nexus_store::StoreError> for OidcLoginError {
    fn from(e: nexus_store::StoreError) -> Self {
        Self::Internal(format!("store: {e}"))
    }
}

impl From<nexus_store::SessionsError> for OidcLoginError {
    fn from(e: nexus_store::SessionsError) -> Self {
        Self::Internal(format!("sessions store: {e}"))
    }
}

impl IntoResponse for OidcLoginError {
    fn into_response(self) -> Response {
        use OidcLoginError as E;
        let (status, body) = match &self {
            E::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            E::UnmappedRole => (StatusCode::FORBIDDEN, "unmapped_role"),
            E::NotEligible => (StatusCode::FORBIDDEN, "oidc_user_not_eligible"),
            E::IdpTokenEndpoint(_) => (StatusCode::BAD_GATEWAY, "idp_token_endpoint"),
            E::IdpInvalidToken(_) => (StatusCode::BAD_GATEWAY, "idp_invalid_token"),
            E::AuthNotConfigured => (StatusCode::SERVICE_UNAVAILABLE, "auth_not_configured"),
            E::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let detail = self.to_string();
        tracing::warn!(target: "nexus::oidc::login", status = %status, body, %detail, "oidc login error");
        (status, Json(serde_json::json!({"error": body}))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `POST /api/v1/auth/oidc/start`
///
/// Mints PKCE verifier + state + nonce, stashes them, returns
/// the authorization URL the UI should redirect to.
pub async fn post_start(
    State(state): State<OidcLoginState>,
    Json(body): Json<StartRequest>,
) -> Result<Response, OidcLoginError> {
    let redirect_to = sanitise_redirect_to(body.redirect_to.as_deref());
    let now = Utc::now();

    let pkce_verifier = mint_url_safe(64);
    let state_tok = mint_url_safe(32);
    let nonce = mint_url_safe(32);
    let pkce_challenge = pkce_challenge_s256(&pkce_verifier);

    let url = build_authorization_url(
        &state.oidc_client.authorization_endpoint(),
        &state.cfg,
        &state_tok,
        &nonce,
        &pkce_challenge,
    );

    state.sessions.insert(
        state_tok.clone(),
        PendingLogin {
            pkce_verifier,
            nonce,
            redirect_to,
            created_at: now,
        },
    );

    // The browser-side state cookie is set so the callback can
    // assert it matches the URL state. Mirrors the OAuth state
    // pattern in `oauth_sessions` but scoped to `/api/v1/auth`.
    let cookie = format!(
        "{name}={value}; Path=/api/v1/auth; HttpOnly; Secure; SameSite=Lax; Max-Age=600",
        name = STATE_COOKIE_NAME,
        value = state_tok,
    );

    let mut resp = (
        StatusCode::OK,
        Json(StartResponse {
            authorization_url: url,
            state: state_tok.clone(),
        }),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut()
            .insert(HeaderName::from_static("set-cookie"), v);
    }
    Ok(resp)
}

/// `GET /api/v1/auth/oidc/callback?code=…&state=…`
///
/// 1. Pop session by `state` (single-use; CSRF cookie also
///    asserted when present).
/// 2. Exchange code for tokens at IdP's token endpoint with
///    PKCE verifier.
/// 3. Verify ID token signature/iss/aud/exp/nbf via OidcClient.
/// 4. Assert `nonce` claim matches stashed nonce.
/// 5. Map role per role_map.
/// 6. Lookup-or-create user keyed by sha256(iss + "\0" + sub).
/// 7. Mint local session, set cookie, 302 to redirect_to.
pub async fn get_callback(
    State(s): State<OidcLoginState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, OidcLoginError> {
    // IdP-side rejection: surface to UI by 302'ing back to `/`
    // with `?oidc_error=...` so the login page can render a
    // toast. Don't 4xx here — the user did nothing wrong from
    // our perspective.
    if let Some(err) = q.error.as_deref() {
        let detail = q.error_description.as_deref().unwrap_or("");
        tracing::info!(target: "nexus::oidc::login", err, detail, "IdP returned consent error");
        return Ok(redirect_with_oidc_error("/", err));
    }

    let code = q.code.ok_or(OidcLoginError::BadRequest("missing_code"))?;
    let state_tok = q.state.ok_or(OidcLoginError::BadRequest("missing_state"))?;

    // Belt-and-suspenders: the browser-side cookie MUST match
    // the URL state. Missing cookie tolerated (some browsers
    // strip cross-site cookies after the IdP hop with strict
    // tracking-prevention) but if PRESENT it must match. This
    // catches an attacker who learned `state` via leaked
    // referer but doesn't control the victim's cookie jar.
    if let Some(c) = extract_cookie(&headers, STATE_COOKIE_NAME) {
        if c != state_tok {
            return Err(OidcLoginError::BadRequest("state_mismatch"));
        }
    }

    let now = Utc::now();
    let pending = s
        .sessions
        .take(&state_tok, now)
        .ok_or(OidcLoginError::BadRequest("unknown_state"))?;

    // 2 — exchange code for tokens.
    let token_resp = exchange_code(
        &s.oidc_client.token_endpoint(),
        &s.cfg,
        s.client_secret.as_deref().map(|s| s.as_str()),
        &code,
        &pending.pkce_verifier,
    )
    .await
    .map_err(OidcLoginError::IdpTokenEndpoint)?;

    // 3 — verify ID token via the OidcClient.
    let verified: VerifiedIdToken = s
        .oidc_client
        .verify_id_token(&token_resp.id_token, now)
        .map_err(|e: OidcError| OidcLoginError::IdpInvalidToken(format!("{e}")))?;
    let claims: IdTokenClaims = verified.claims;

    // 4 — nonce check.
    match claims.extra.get("nonce") {
        Some(serde_json::Value::String(n)) if *n == pending.nonce => (),
        _ => {
            return Err(OidcLoginError::IdpInvalidToken(
                "nonce mismatch".to_string(),
            ));
        }
    }

    // 5 — role mapping.
    let role = map_role(&claims, &s.cfg).map_err(|e| match e {
        MapError::Unmapped => OidcLoginError::UnmappedRole,
    })?;

    // 6 — lookup-or-create the local user.
    let subject_hash = hash_oidc_subject(&claims.iss, &claims.sub);
    let user = lookup_or_create_oidc_user(&s.store, &claims, role, &subject_hash).await?;

    if user.disabled || user.deleted_at.is_some() {
        return Err(OidcLoginError::NotEligible);
    }

    // 7 — mint local session tokens.
    let admin_secret = s
        .admin_auth
        .admin_secret()
        .ok_or(OidcLoginError::AuthNotConfigured)?
        .to_string();

    let chain_id = Uuid::now_v7().to_string();
    let access_token = issue_access_token(
        user.id,
        user.role,
        admin_secret.as_bytes(),
        now,
        ACCESS_TOKEN_TTL,
        Some(&chain_id),
    )?;

    // Refresh row — single chain per OIDC login, same as the
    // local-password login path.
    let refresh_secret = new_refresh_secret().map_err(OidcLoginError::from)?;
    let token_hash = hash_refresh_secret(&refresh_secret);
    let expires_at = now + REFRESH_TOKEN_TTL;
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    s.store
        .insert_refresh_token(nexus_store::NewRefreshToken {
            token_hash: &token_hash,
            user_id: user.id,
            chain_id: &chain_id,
            parent_id: None,
            expires_at,
            user_agent: ua,
            ip: Some(&ip),
        })
        .await?;

    s.store.record_login_success(user.id).await?;

    // Build the response: 302 to redirect_to with the session
    // cookie attached so the UI is logged in by the time it
    // lands on /#/dashboard.
    let mut resp = redirect_to(&pending.redirect_to);
    let cookie = build_session_cookie(&access_token, ACCESS_TOKEN_TTL);
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut()
            .append(HeaderName::from_static("set-cookie"), v);
    }
    // Also clear the per-attempt state cookie.
    let clear_state = format!(
        "{name}=; Path=/api/v1/auth; HttpOnly; Secure; SameSite=Lax; Max-Age=0",
        name = STATE_COOKIE_NAME,
    );
    if let Ok(v) = HeaderValue::from_str(&clear_state) {
        resp.headers_mut()
            .append(HeaderName::from_static("set-cookie"), v);
    }

    // Stash the refresh token + a tiny session-user payload in
    // a separate cookie so the UI can hydrate its session state
    // immediately on landing without an extra round-trip. The
    // local-password login returns these in the JSON body; the
    // OIDC flow lands on a redirect so we expose them via a
    // short-lived "handoff" cookie that the UI reads + clears
    // on its first paint.
    let handoff = build_handoff_cookie(&TokenResponse {
        access_token: access_token.clone(),
        refresh_token: refresh_secret.expose().to_string(),
        expires_in: ACCESS_TOKEN_TTL.num_seconds(),
        refresh_expires_in: REFRESH_TOKEN_TTL.num_seconds(),
        user: SessionUser {
            id: user.id,
            username: user.username.clone(),
            role: user.role,
            force_password_reset: false,
        },
    });
    if let Ok(v) = HeaderValue::from_str(&handoff) {
        resp.headers_mut()
            .append(HeaderName::from_static("set-cookie"), v);
    }

    Ok(resp)
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// True iff `path` is a same-origin relative path safe to use
/// as a post-login redirect target. Rejects `//foo` and absolute
/// URLs.
fn is_safe_redirect(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && !path.contains("://")
}

fn sanitise_redirect_to(input: Option<&str>) -> String {
    match input {
        Some(p) if is_safe_redirect(p) => p.to_string(),
        Some(_) => {
            tracing::warn!(target: "nexus::oidc::login", "rejected unsafe redirect_to; using /");
            "/".to_string()
        }
        None => "/".to_string(),
    }
}

fn redirect_to(path: &str) -> Response {
    let mut resp = (StatusCode::FOUND, "").into_response();
    if let Ok(v) = HeaderValue::from_str(path) {
        resp.headers_mut()
            .insert(HeaderName::from_static("location"), v);
    }
    resp
}

fn redirect_with_oidc_error(base: &str, err: &str) -> Response {
    let sep = if base.contains('?') { '&' } else { '?' };
    // We URL-encode only `err`; `base` is operator-controlled
    // via the safe-redirect path so we trust its shape.
    let encoded: String = err
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect();
    redirect_to(&format!("{base}{sep}oidc_error={encoded}"))
}

fn mint_url_safe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom_fill(&mut buf).expect("getrandom must succeed");
    URL_SAFE_NO_PAD.encode(&buf)
}

fn pkce_challenge_s256(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

fn build_authorization_url(
    auth_endpoint: &str,
    cfg: &OidcConfig,
    state: &str,
    nonce: &str,
    pkce_challenge: &str,
) -> String {
    let client_id = cfg.client_id.as_deref().unwrap_or(cfg.audience.as_str());
    let scopes = cfg.scopes.join(" ");
    let redirect_uri = oidc_redirect_uri(cfg);
    // `state_redirect_to` is held in the session map, NOT in the
    // URL — keeping the URL short and not surfacing operator
    // navigation paths to the IdP.
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", &redirect_uri),
        ("scope", &scopes),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", pkce_challenge),
        ("code_challenge_method", "S256"),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let sep = if auth_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{auth_endpoint}{sep}{qs}")
}

/// The redirect URI we hand to the IdP. The OIDC spec requires
/// this be byte-identical between the `/authorize` redirect
/// and the `/token` exchange.
///
/// Resolution order:
///
///   1. `cfg.redirect_uri` if set — used verbatim. Operators
///      MUST set this to the exact URL registered with the IdP
///      (Entra requires a full HTTPS URL; localhost over HTTP
///      is allowed for development).
///   2. Fallback to the relative path
///      `/api/v1/auth/oidc/callback` — accepted by Authentik,
///      Keycloak, Okta, Google; rejected by Entra.
fn oidc_redirect_uri(cfg: &OidcConfig) -> String {
    cfg.redirect_uri
        .clone()
        .unwrap_or_else(|| "/api/v1/auth/oidc/callback".to_string())
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
            out.push(c);
        } else {
            for b in c.to_string().bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    for part in raw.split(';') {
        let trimmed = part.trim();
        if let Some(eq) = trimmed.find('=') {
            let (k, v) = trimmed.split_at(eq);
            if k == name {
                return Some(v[1..].to_string());
            }
        }
    }
    None
}

fn extract_client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
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
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
}

fn build_handoff_cookie(tr: &TokenResponse) -> String {
    // Hand-off cookie is the JSON-encoded TokenResponse,
    // base64-url-no-pad, with a short Max-Age. The UI's
    // login-overlay code reads + deletes it on first paint.
    // NOT HttpOnly (the UI needs to read it from JS) — this is
    // safe because the value is single-use: the UI deletes it
    // immediately, and even if exfiltrated the access token is
    // already in the session cookie which IS HttpOnly.
    let body = serde_json::to_vec(tr).unwrap_or_else(|_| b"{}".to_vec());
    let encoded = URL_SAFE_NO_PAD.encode(&body);
    format!("nexus_oidc_handoff={encoded}; Path=/; Secure; SameSite=Lax; Max-Age=60",)
}

/// SHA-256(iss || 0x00 || sub) in lowercase hex. The NUL byte
/// ensures `iss="https://a/"; sub="b"` and `iss="https://"; sub="a/b"`
/// don't collide. Matches the comment in
/// `migrations/0010_local_users.sql`.
pub fn hash_oidc_subject(iss: &str, sub: &str) -> String {
    let mut h = Sha256::new();
    h.update(iss.as_bytes());
    h.update([0u8]);
    h.update(sub.as_bytes());
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

async fn lookup_or_create_oidc_user(
    store: &Store,
    claims: &IdTokenClaims,
    role: Role,
    subject_hash: &str,
) -> Result<StoreUser, OidcLoginError> {
    if let Some(existing) = store.get_user_by_oidc_subject(subject_hash).await? {
        return Ok(existing);
    }
    let username_hint = derive_username_hint(claims);
    // Try the hint first; fall back to `-oidc-<6 hex>` suffix
    // on collision so two operators with the same
    // `preferred_username` from different IdPs can coexist.
    let username = match store
        .create_user(&NewUser {
            username: &username_hint,
            role,
            password_hash: None,
            oidc_subject_hash: Some(subject_hash),
            force_password_reset: false,
        })
        .await
    {
        Ok(_) => username_hint.clone(),
        Err(nexus_store::UsersError::UsernameTaken) => {
            let suffix = &subject_hash[..6];
            let alt = format!("{username_hint}-oidc-{suffix}");
            store
                .create_user(&NewUser {
                    username: &alt,
                    role,
                    password_hash: None,
                    oidc_subject_hash: Some(subject_hash),
                    force_password_reset: false,
                })
                .await
                .map_err(|e| OidcLoginError::Internal(format!("create_user: {e}")))?;
            alt
        }
        Err(e) => return Err(OidcLoginError::Internal(format!("create_user: {e}"))),
    };

    store
        .get_user_by_username(&username)
        .await?
        .ok_or_else(|| OidcLoginError::Internal("create_user race".to_string()))
}

fn derive_username_hint(claims: &IdTokenClaims) -> String {
    // Prefer `preferred_username`, then `email`, then the
    // subject. Normalise to lowercase and strip the domain
    // portion of an email so `alice@example.com` becomes
    // `alice` (collision-tolerant via the -oidc-XXXXXX
    // fallback).
    if let Some(serde_json::Value::String(s)) = claims.extra.get("preferred_username") {
        return normalise_username(s);
    }
    if let Some(serde_json::Value::String(e)) = claims.extra.get("email") {
        let local = e.split('@').next().unwrap_or(e);
        return normalise_username(local);
    }
    normalise_username(&claims.sub)
}

fn normalise_username(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if cleaned.is_empty() {
        // Fall back to a stable placeholder so create_user
        // doesn't see an empty username (the store's invariants
        // require >= 1 char).
        "oidc-user".to_string()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// Token-endpoint exchange.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    id_token: String,
    // `access_token` + `refresh_token` are part of the
    // OAuth-conformant response but we never use them: the engine
    // mints its own local-session access + refresh tokens after
    // ID-token verification, so the IdP-issued bearer values are
    // discarded. Surfacing them here would tempt callers to use
    // them and bypass the local session model.
    #[serde(default, rename = "access_token")]
    _access_token: Option<String>,
    #[serde(default, rename = "refresh_token")]
    _refresh_token: Option<String>,
}

async fn exchange_code(
    token_endpoint: &str,
    cfg: &OidcConfig,
    client_secret: Option<&str>,
    code: &str,
    pkce_verifier: &str,
) -> Result<TokenEndpointResponse, String> {
    let client = reqwest::Client::new();
    let client_id = cfg.client_id.as_deref().unwrap_or(cfg.audience.as_str());
    let redirect_uri = oidc_redirect_uri(cfg);
    // Build the form params dynamically because
    // `client_secret` is optional. The fixed five always go in;
    // the secret only when an operator has wired it via
    // `client_secret_file`.
    let mut params: Vec<(&str, &str)> = Vec::with_capacity(6);
    params.push(("grant_type", "authorization_code"));
    params.push(("code", code));
    params.push(("redirect_uri", &redirect_uri));
    params.push(("client_id", client_id));
    params.push(("code_verifier", pkce_verifier));
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let resp = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "status {} from token endpoint; body: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------
//
// These cover the helper layer that doesn't need a live IdP or
// engine. The full end-to-end (start → IdP redirect → callback
// → cookie) round-trip is exercised in
// `crates/nexus-engine/src/auth/login.rs`'s integration tests
// once Step 3.3's UI lands.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_safety_accepts_relative_paths() {
        assert!(is_safe_redirect("/"));
        assert!(is_safe_redirect("/#/dashboard"));
        assert!(is_safe_redirect("/admin/users"));
    }

    #[test]
    fn redirect_safety_rejects_absolute_urls() {
        assert!(!is_safe_redirect("https://evil.example/"));
        assert!(!is_safe_redirect("http://evil.example/"));
        assert!(!is_safe_redirect("//evil.example/"));
        assert!(!is_safe_redirect("javascript:alert(1)"));
    }

    #[test]
    fn sanitise_redirect_to_falls_back_to_root() {
        assert_eq!(sanitise_redirect_to(None), "/");
        assert_eq!(sanitise_redirect_to(Some("//bad")), "/");
        assert_eq!(sanitise_redirect_to(Some("https://x")), "/");
        assert_eq!(sanitise_redirect_to(Some("/good")), "/good");
    }

    fn cfg_with_redirect(redirect_uri: Option<&str>) -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.test/".into(),
            audience: "cid".into(),
            jwks_uri: None,
            client_id: None,
            display_name: None,
            scopes: vec!["openid".into()],
            role_claims: vec!["groups".into()],
            role_map: Default::default(),
            deny_unmapped: false,
            redirect_uri: redirect_uri.map(str::to_string),
            client_secret_file: None,
            client_secret_env: None,
        }
    }

    #[test]
    fn redirect_uri_defaults_to_relative_callback() {
        assert_eq!(
            oidc_redirect_uri(&cfg_with_redirect(None)),
            "/api/v1/auth/oidc/callback",
        );
    }

    #[test]
    fn redirect_uri_override_is_used_verbatim() {
        // The OIDC spec mandates byte-identity between the
        // authorize redirect_uri and the token redirect_uri, so
        // we must NOT canonicalise or trim. This test pins that.
        let cfg = cfg_with_redirect(Some("https://nexus.example.com/api/v1/auth/oidc/callback"));
        assert_eq!(
            oidc_redirect_uri(&cfg),
            "https://nexus.example.com/api/v1/auth/oidc/callback",
        );
    }

    #[test]
    fn build_auth_url_uses_redirect_uri_override_when_set() {
        // Entra-style absolute URL — must appear URL-encoded in
        // the authorize URL.
        let cfg = cfg_with_redirect(Some("http://localhost:8089/api/v1/auth/oidc/callback"));
        let url = build_authorization_url(
            "https://login.microsoftonline.com/TENANT/oauth2/v2.0/authorize",
            &cfg,
            "S",
            "N",
            "C",
        );
        assert!(url.contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A8089%2Fapi%2Fv1%2Fauth%2Foidc%2Fcallback"
        ));
        // The fallback relative path must NOT appear.
        assert!(!url.contains("redirect_uri=%2Fapi%2Fv1"));
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 Appendix B fixture:
        //   verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        //   challenge (S256) = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c = pkce_challenge_s256(v);
        assert_eq!(c, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn url_encode_passes_unreserved_and_percent_encodes_others() {
        assert_eq!(url_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(url_encode(" "), "%20");
        assert_eq!(url_encode("a&b"), "a%26b");
        assert_eq!(url_encode("https://idp.test/"), "https%3A%2F%2Fidp.test%2F");
    }

    #[test]
    fn build_auth_url_contains_required_params() {
        let cfg = OidcConfig {
            issuer: "https://idp.test/".into(),
            audience: "client-id".into(),
            jwks_uri: None,
            client_id: Some("client-id".into()),
            display_name: None,
            scopes: vec!["openid".into(), "profile".into(), "groups".into()],
            role_claims: vec!["groups".into()],
            role_map: Default::default(),
            deny_unmapped: false,
            redirect_uri: None,
            client_secret_file: None,
            client_secret_env: None,
        };
        let url = build_authorization_url(
            "https://idp.test/auth",
            &cfg,
            "STATE_TOK",
            "NONCE_TOK",
            "CHALLENGE",
        );
        assert!(url.starts_with("https://idp.test/auth?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-id"));
        assert!(url.contains("redirect_uri=%2Fapi%2Fv1%2Fauth%2Foidc%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20groups"));
        assert!(url.contains("state=STATE_TOK"));
        assert!(url.contains("nonce=NONCE_TOK"));
        assert!(url.contains("code_challenge=CHALLENGE"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn build_auth_url_appends_with_amp_when_endpoint_already_has_query() {
        let cfg = OidcConfig {
            issuer: "https://idp.test/".into(),
            audience: "cid".into(),
            jwks_uri: None,
            client_id: None,
            display_name: None,
            scopes: vec!["openid".into()],
            role_claims: vec!["groups".into()],
            role_map: Default::default(),
            deny_unmapped: false,
            redirect_uri: None,
            client_secret_file: None,
            client_secret_env: None,
        };
        // Some IdPs publish auth endpoints with an existing `?prompt=...`
        let url =
            build_authorization_url("https://idp.test/auth?prompt=login", &cfg, "S", "N", "C");
        assert!(url.contains("?prompt=login&response_type=code"));
    }

    #[test]
    fn hash_oidc_subject_differs_per_issuer() {
        let a = hash_oidc_subject("https://a.example/", "subj-1");
        let b = hash_oidc_subject("https://b.example/", "subj-1");
        assert_ne!(a, b, "issuer must be folded into the hash");
    }

    #[test]
    fn hash_oidc_subject_avoids_concat_ambiguity() {
        // The NUL byte separates iss + sub so these distinct
        // (iss, sub) pairs MUST hash differently even though
        // their naïve concat happens to be the same.
        let a = hash_oidc_subject("ab", "cd");
        let b = hash_oidc_subject("abc", "d");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_oidc_subject_is_64_hex_chars() {
        let h = hash_oidc_subject("https://idp.test/", "user-1");
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn derive_username_prefers_preferred_username() {
        let mut extra = HashMap::new();
        extra.insert(
            "preferred_username".into(),
            serde_json::json!("Alice.Smith"),
        );
        extra.insert("email".into(), serde_json::json!("ignored@example.com"));
        let claims = test_claims(extra);
        assert_eq!(derive_username_hint(&claims), "alice.smith");
    }

    #[test]
    fn derive_username_falls_back_to_email_local_part() {
        let mut extra = HashMap::new();
        extra.insert("email".into(), serde_json::json!("Bob.JONES@example.com"));
        let claims = test_claims(extra);
        assert_eq!(derive_username_hint(&claims), "bob.jones");
    }

    #[test]
    fn derive_username_falls_back_to_subject() {
        let extra = HashMap::new();
        let claims = test_claims(extra);
        // `claims.sub` is "user-abc" in test_claims.
        assert_eq!(derive_username_hint(&claims), "user-abc");
    }

    #[test]
    fn normalise_username_filters_disallowed_chars() {
        assert_eq!(normalise_username("Alice Smith"), "alicesmith");
        assert_eq!(normalise_username("a!b@c#d"), "abcd");
        assert_eq!(normalise_username(""), "oidc-user");
        assert_eq!(normalise_username("!!!"), "oidc-user");
    }

    #[test]
    fn pending_session_insert_and_take_roundtrip() {
        let sessions = OidcLoginSessions::new();
        let now = Utc::now();
        sessions.insert(
            "S1".into(),
            PendingLogin {
                pkce_verifier: "V".into(),
                nonce: "N".into(),
                redirect_to: "/".into(),
                created_at: now,
            },
        );
        assert_eq!(sessions.len(), 1);
        let popped = sessions.take("S1", now).unwrap();
        assert_eq!(popped.nonce, "N");
        assert_eq!(sessions.len(), 0);
        // Single-use: a second take is a miss.
        assert!(sessions.take("S1", now).is_none());
    }

    #[test]
    fn pending_session_take_evicts_expired() {
        let sessions = OidcLoginSessions::new();
        let created = Utc::now();
        sessions.insert(
            "S1".into(),
            PendingLogin {
                pkce_verifier: "V".into(),
                nonce: "N".into(),
                redirect_to: "/".into(),
                created_at: created,
            },
        );
        let later = created + Duration::seconds(700); // > 10 min
        assert!(sessions.take("S1", later).is_none());
    }

    #[test]
    fn pending_session_insert_sweeps_expired_entries() {
        let sessions = OidcLoginSessions::new();
        let t0 = Utc::now();
        sessions.insert(
            "OLD".into(),
            PendingLogin {
                pkce_verifier: "V".into(),
                nonce: "N".into(),
                redirect_to: "/".into(),
                created_at: t0,
            },
        );
        let t1 = t0 + Duration::seconds(700);
        sessions.insert(
            "NEW".into(),
            PendingLogin {
                pkce_verifier: "V2".into(),
                nonce: "N2".into(),
                redirect_to: "/".into(),
                created_at: t1,
            },
        );
        // OLD was swept by the insert of NEW.
        assert!(sessions.peek("OLD").is_none());
        assert!(sessions.peek("NEW").is_some());
    }

    #[test]
    fn redirect_with_oidc_error_encodes_error_param() {
        let resp = redirect_with_oidc_error("/", "access_denied");
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc, "/?oidc_error=access_denied");

        let resp2 = redirect_with_oidc_error("/?existing=1", "consent_required");
        let loc2 = resp2.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc2, "/?existing=1&oidc_error=consent_required");
    }

    #[test]
    fn extract_cookie_finds_named_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            HeaderValue::from_static("a=1; __Host-nexus_oidc_state=STOK; b=2"),
        );
        assert_eq!(
            extract_cookie(&h, STATE_COOKIE_NAME),
            Some("STOK".to_string())
        );
        assert_eq!(extract_cookie(&h, "missing"), None);
    }

    fn test_claims(extra: HashMap<String, serde_json::Value>) -> IdTokenClaims {
        IdTokenClaims {
            iss: "https://idp.test/".into(),
            aud: serde_json::json!("cid"),
            sub: "user-abc".into(),
            exp: 9_999_999_999,
            nbf: None,
            iat: None,
            extra,
        }
    }
}
