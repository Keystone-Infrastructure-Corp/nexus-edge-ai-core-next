//! Shared OAuth 2.0 plumbing for cloud cold backends.
//!
//! This module owns BOTH halves of the OAuth dance against Google
//! and Microsoft:
//!
//! * The long-running refresh-token grant ([`OAuthClient`]) used by
//!   the per-backend access-token cache.
//! * The one-shot authorisation-code grant ([`exchange_code`],
//!   [`authorize_url`], [`new_state`]) used by the engine's
//!   `/api/v1/admin/oauth/{provider}/{start,callback}` endpoints to
//!   produce that refresh token without ever bouncing through a
//!   sibling app. The auth-code flow runs end-to-end inside
//!   nexus-engine + the core-next UI; the only external endpoints
//!   are Google's / Microsoft's consent + token URLs.
//!
//! Both Google Drive and Microsoft Graph use the same OAuth 2.0
//! refresh shape:
//!
//! ```text
//! POST <token_url>
//! Content-Type: application/x-www-form-urlencoded
//!
//! grant_type=refresh_token
//! &client_id=<...>
//! &client_secret=<...>
//! &refresh_token=<...>
//! ```
//!
//! The response is JSON:
//!
//! ```json
//! {
//!   "access_token": "ya29.A0…",
//!   "expires_in": 3599,
//!   "token_type": "Bearer",
//!   "scope": "https://www.googleapis.com/auth/drive.file",
//!   "refresh_token": "1//0eAa…"   /* optional — Microsoft rotates */
//! }
//! ```
//!
//! [`OAuthClient`] caches the access token in memory + refreshes
//! 30 s before expiry; concurrent calls coalesce on a single
//! refresh via the inner [`tokio::sync::Mutex`]. On any 401 from
//! the upstream API the backend invalidates the cache and retries
//! once.
//!
//! ### Why we don't use `oauth2` (the crate)
//!
//! The `oauth2` crate is great for *implementing* an OAuth client
//! from scratch — auth-code flows, PKCE, JWT-bearer assertions.
//! We only need the refresh-grant slice, and we need precise
//! control over the request headers + error mapping (every 4xx
//! must become `BackendError::Auth` so the admin UI can prompt for
//! re-consent, not a generic "backend unreachable" banner). A
//! 200-line direct-`reqwest` impl is clearer than wiring through
//! the abstractions of a general-purpose crate.

use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_storage::token_crypto::RefreshToken;
use nexus_storage::BackendError;
use parking_lot::RwLock;
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, warn};

/// Safety margin: we treat a token as "about to expire" this many
/// seconds before its actual `exp`. Avoids the race where we send
/// a request with a token that expires server-side mid-flight.
const EXPIRY_SKEW_SECS: i64 = 30;

/// Fallback when the token endpoint omits `expires_in`. Matches
/// both Google and Microsoft's de-facto default of 3600 s.
const DEFAULT_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Gdrive,
    Onedrive,
}

impl Provider {
    /// Provider tag used in tracing fields and route paths. Stable
    /// across versions so log aggregations + redirect-URI
    /// registrations don't break.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gdrive => "gdrive",
            Self::Onedrive => "onedrive",
        }
    }

    /// Parse the lowercase tag used in `storage_backends.kind` and
    /// in the engine's OAuth route segments. `None` means "not a
    /// cloud provider we know about".
    pub fn from_kind(s: &str) -> Option<Self> {
        match s {
            "gdrive" => Some(Self::Gdrive),
            "onedrive" => Some(Self::Onedrive),
            _ => None,
        }
    }

    /// OAuth scope string sent on both the consent URL and (later)
    /// the token endpoint. Drive uses the per-app `drive.file`
    /// sandbox so we can only see clips we wrote ourselves; Graph
    /// uses `Files.ReadWrite.AppFolder` for the same property,
    /// plus `offline_access` to be granted a refresh token at all.
    pub fn scope(self) -> &'static str {
        match self {
            Self::Gdrive => "https://www.googleapis.com/auth/drive.file",
            Self::Onedrive => "Files.ReadWrite.AppFolder offline_access",
        }
    }
}

/// Shape of the JSON response body from the token endpoint. We
/// ignore every field except `access_token` + `expires_in` — but
/// `serde(default)` makes us forward-compatible with whatever
/// extras Google / Microsoft add.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    // Some providers (Microsoft) rotate the refresh token on every
    // refresh. We DO NOT persist the new one yet — that requires a
    // store roundtrip + re-encryption that's out of scope for the
    // closeout PR. The old refresh token remains valid for the
    // documented grace period (Google: 6 months idle, Microsoft:
    // 90 days idle).
    #[serde(default)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

/// Cached access token with its computed expiry instant. Stored
/// behind an `RwLock` so the hot path can read with a shared lock;
/// refresh acquires an exclusive lock briefly to swap the value
/// in (the actual network call holds the [`AsyncMutex`] in
/// [`OAuthClient`], not this lock).
#[derive(Debug, Clone)]
struct CachedAccessToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

/// OAuth refresh-token client. One per cloud backend instance.
///
/// Holds the `client_id` / `client_secret` / `refresh_token`
/// triple, the token endpoint URL, and the cached access token.
/// `Send + Sync` so backends can share clones across tokio tasks
/// (the `AsyncMutex` makes concurrent refresh calls coalesce on a
/// single network request).
pub(crate) struct OAuthClient {
    pub(crate) provider: Provider,
    client_id: String,
    client_secret: String,
    refresh_token: RefreshToken,
    token_url: String,
    /// Outer mutex so concurrent `access_token()` callers that
    /// both see an expired cache don't both fire a refresh.
    refresh_lock: AsyncMutex<()>,
    cached: RwLock<Option<CachedAccessToken>>,
}

impl OAuthClient {
    pub(crate) fn new(
        provider: Provider,
        client_id: String,
        client_secret: String,
        refresh_token: RefreshToken,
        token_url: String,
    ) -> Self {
        Self {
            provider,
            client_id,
            client_secret,
            refresh_token,
            token_url,
            refresh_lock: AsyncMutex::new(()),
            cached: RwLock::new(None),
        }
    }

    /// Drop the cached access token. Called after a 401 from the
    /// upstream API so the next [`access_token`] call refreshes
    /// even though the cached entry hasn't visibly expired.
    pub(crate) fn invalidate(&self) {
        *self.cached.write() = None;
    }

    /// Return a valid access token, refreshing transparently if
    /// the cache is empty or within [`EXPIRY_SKEW_SECS`] of expiry.
    /// Returns the bearer string with no `Bearer ` prefix — the
    /// caller assembles the Authorization header.
    pub(crate) async fn access_token(
        &self,
        client: &reqwest::Client,
    ) -> Result<String, BackendError> {
        // Fast path: cache hit + not near expiry.
        if let Some(cached) = self.cached.read().as_ref() {
            if cached.expires_at > Utc::now() + Duration::from_secs(EXPIRY_SKEW_SECS as u64) {
                return Ok(cached.access_token.clone());
            }
        }

        // Slow path: contend for the refresh lock. The first
        // caller does the network round-trip; subsequent waiters
        // re-check the cache on wake and almost always short-circuit.
        let _guard = self.refresh_lock.lock().await;
        if let Some(cached) = self.cached.read().as_ref() {
            if cached.expires_at > Utc::now() + Duration::from_secs(EXPIRY_SKEW_SECS as u64) {
                return Ok(cached.access_token.clone());
            }
        }
        self.refresh(client).await
    }

    /// Force an unconditional refresh. Used by tests; production
    /// code goes through [`Self::access_token`] which handles
    /// caching.
    async fn refresh(&self, client: &reqwest::Client) -> Result<String, BackendError> {
        debug!(
            provider = self.provider.as_str(),
            token_url = %self.token_url,
            "OAuth refresh: posting to token endpoint"
        );

        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", self.refresh_token.expose_secret()),
        ];

        let resp = client
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                // Network-level failures (DNS, TLS, connect refused)
                // count as Unreachable so the replicator emits the
                // sticky banner and pauses; the next tick retries.
                BackendError::Unreachable(format!("OAuth token endpoint network: {e}"))
            })?;

        let status = resp.status();
        if !status.is_success() {
            // The body MAY include a structured error
            // (`{"error":"invalid_grant","error_description":"..."}`).
            // We don't trust it as the user-facing message — log
            // for ops, surface a sanitised string.
            let body_preview = resp.text().await.unwrap_or_default();
            let preview: String = body_preview.chars().take(200).collect();
            warn!(
                provider = self.provider.as_str(),
                %status,
                body = %preview,
                "OAuth refresh failed"
            );
            // 4xx → refresh token revoked / app de-authorised; the
            // admin UI must prompt for re-consent.
            // 5xx → transient. Both surface as Auth for now; the
            // replicator distinguishes via repeated failures.
            return Err(BackendError::Auth(format!(
                "OAuth refresh failed: {status}"
            )));
        }

        let body: TokenResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::Auth(format!("OAuth response not valid JSON: {e}")))?;
        if body.access_token.is_empty() {
            return Err(BackendError::Auth(
                "OAuth response missing access_token".into(),
            ));
        }
        let ttl = body.expires_in.unwrap_or(DEFAULT_TTL_SECS).max(60);
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl);

        debug!(
            provider = self.provider.as_str(),
            ttl_secs = ttl,
            "OAuth refresh: success"
        );

        let mut w = self.cached.write();
        let access_token = body.access_token.clone();
        *w = Some(CachedAccessToken {
            access_token: body.access_token,
            expires_at,
        });
        Ok(access_token)
    }
}

/// Resolve the token endpoint URL for a provider, honouring the
/// matching `NEXUS_*_TOKEN_URL` environment override.
pub(crate) fn token_url(provider: Provider) -> String {
    match provider {
        Provider::Gdrive => std::env::var("NEXUS_GDRIVE_TOKEN_URL")
            .unwrap_or_else(|_| "https://oauth2.googleapis.com/token".to_string()),
        Provider::Onedrive => std::env::var("NEXUS_ONEDRIVE_TOKEN_URL").unwrap_or_else(|_| {
            "https://login.microsoftonline.com/common/oauth2/v2.0/token".to_string()
        }),
    }
}

/// Resolve the consent ("authorize") endpoint URL for a provider,
/// honouring the matching `NEXUS_*_AUTH_URL` environment override.
/// Used only by the auth-code flow in [`authorize_url`].
pub fn auth_url(provider: Provider) -> String {
    match provider {
        Provider::Gdrive => std::env::var("NEXUS_GDRIVE_AUTH_URL")
            .unwrap_or_else(|_| "https://accounts.google.com/o/oauth2/v2/auth".to_string()),
        Provider::Onedrive => std::env::var("NEXUS_ONEDRIVE_AUTH_URL").unwrap_or_else(|_| {
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".to_string()
        }),
    }
}

// ===========================================================================
// Authorization-code flow — interactive "Connect <Provider>" handshake.
// ===========================================================================
//
// Lives here (next to the refresh-grant code) because both halves
// share the same provider enum, env-override surface, and HTTP
// client. The engine's API layer ties them together: `start`
// generates a state, `callback` exchanges the code → refresh_token,
// then immediately encrypts + persists it via `nexus-storage`.

/// Refresh + access token pair returned by [`exchange_code`].
/// Cleartext; the caller (engine) is responsible for piping
/// `refresh_token` through `nexus_storage::token_crypto::encrypt`
/// before any of it touches disk.
#[derive(Debug)]
pub struct ExchangedTokens {
    /// The long-lived refresh token — the whole point of the
    /// auth-code dance. Must be encrypted at rest.
    pub refresh_token: String,
    /// Short-lived access token. Currently unused by the engine
    /// (the per-backend [`OAuthClient`] will mint a fresh one on
    /// first use) but surfaced so the UI can render "Connected as
    /// …" without an extra round-trip.
    pub access_token: String,
    /// Seconds until `access_token` expires, per the token
    /// endpoint. Optional because the spec allows omission.
    pub expires_in_secs: Option<i64>,
    /// Space-separated list of granted scopes. Provider may
    /// downgrade what we requested; surface it verbatim so the
    /// UI can warn on a partial grant.
    pub scope: Option<String>,
}

/// Generate a random CSRF-state token suitable for the OAuth
/// `state` parameter. 32 bytes of OS entropy → base64url-no-pad
/// (43 ASCII chars). Unguessable in any practical sense so the
/// `/callback` handler can use "valid state" as authentication
/// for the browser redirect (which has no admin bearer).
pub fn new_state() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let bytes = nexus_storage::token_crypto::random_state_bytes();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the provider's consent URL — what the operator's browser
/// is sent to. `redirect_uri` MUST match a value registered on the
/// OAuth app at the provider's developer console (Google Cloud
/// console / Entra app registration), AND it must be the exact
/// URL the engine binds for the `/callback` handler.
///
/// `state` is the random token from [`new_state`]. The provider
/// echoes it back on the `/callback` redirect; the engine matches
/// it against the in-memory pending-session map to defeat CSRF.
pub fn authorize_url(
    provider: Provider,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> String {
    // Common params per RFC 6749 §4.1.1.
    let mut url = url::Url::parse(&auth_url(provider))
        .expect("auth_url is provider-constant or operator-set; must be valid");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("response_type", "code");
        q.append_pair("scope", provider.scope());
        q.append_pair("state", state);
        match provider {
            Provider::Gdrive => {
                // Google-specific: `access_type=offline` is what
                // unlocks the refresh-token grant; `prompt=consent`
                // ensures we get one even on a re-authorise where
                // the user previously consented (otherwise Google
                // omits the refresh_token from the response).
                q.append_pair("access_type", "offline");
                q.append_pair("prompt", "consent");
                q.append_pair("include_granted_scopes", "true");
            }
            Provider::Onedrive => {
                // Microsoft-specific: refresh tokens are tied to
                // the `offline_access` scope (already in `scope()`).
                // `prompt=select_account` is friendly UX so the
                // operator can pick the org/personal account in
                // the popup.
                q.append_pair("prompt", "select_account");
                q.append_pair("response_mode", "query");
            }
        }
    }
    url.to_string()
}

/// Exchange an OAuth authorization `code` for a refresh + access
/// token pair. Called by the engine's `/callback` handler after
/// the provider has redirected the operator's browser back with a
/// `?code=…&state=…`.
///
/// Internally builds its own short-lived `reqwest::Client` with a
/// 20-second timeout so callers don't have to depend on reqwest
/// directly. The token exchange is a one-shot call per OAuth
/// flow — ~once per backend registration — so connection-pool
/// re-use isn't worth the API-surface cost.
///
/// On any non-2xx response we surface [`BackendError::Auth`] —
/// the engine's callback handler turns that into a user-facing
/// error page and marks the pending session as failed so the UI's
/// status poll picks it up.
pub async fn exchange_code(
    provider: Provider,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<ExchangedTokens, BackendError> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| BackendError::Other(format!("reqwest client build: {e}")))?;
    let url = token_url(provider);
    exchange_code_with_client(
        provider,
        code,
        redirect_uri,
        client_id,
        client_secret,
        &url,
        &http,
    )
    .await
}

/// Internal exchange implementation that takes an externally-owned
/// `reqwest::Client` and an explicit `token_url`. Exposed only to
/// this crate's unit tests so wiremock can swap in a mock endpoint
/// without racing on the shared `NEXUS_*_TOKEN_URL` env vars.
pub(crate) async fn exchange_code_with_client(
    provider: Provider,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: &str,
    token_url: &str,
    http: &reqwest::Client,
) -> Result<ExchangedTokens, BackendError> {
    debug!(
        provider = provider.as_str(),
        "OAuth auth-code exchange: posting to token endpoint"
    );

    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("redirect_uri", redirect_uri),
    ];

    let resp = http
        .post(token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| BackendError::Unreachable(format!("OAuth token endpoint network: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body_preview = resp.text().await.unwrap_or_default();
        let preview: String = body_preview.chars().take(200).collect();
        warn!(
            provider = provider.as_str(),
            %status,
            body = %preview,
            "OAuth auth-code exchange failed"
        );
        return Err(BackendError::Auth(format!(
            "OAuth auth-code exchange failed: {status}"
        )));
    }

    /// Wire shape for the authorization-code response. Same fields
    /// as the refresh response plus the `refresh_token` we actually
    /// care about. We map this to the public [`ExchangedTokens`]
    /// so callers don't depend on serde-deserialised types from
    /// this module's private types.
    #[derive(Debug, Deserialize)]
    struct AuthCodeResponse {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
        #[serde(default)]
        scope: Option<String>,
    }

    let body: AuthCodeResponse = resp
        .json()
        .await
        .map_err(|e| BackendError::Auth(format!("OAuth response not valid JSON: {e}")))?;

    let Some(refresh_token) = body.refresh_token else {
        // Google omits `refresh_token` on re-consent unless
        // `prompt=consent` is set — we DO set it, so reaching this
        // branch in practice means an app misconfig. Microsoft
        // omits it when `offline_access` is missing from scope.
        return Err(BackendError::Auth(
            "OAuth response missing refresh_token (check that the OAuth app's scopes \
             include offline_access / drive.file with prompt=consent)"
                .into(),
        ));
    };
    if refresh_token.is_empty() || body.access_token.is_empty() {
        return Err(BackendError::Auth(
            "OAuth response had empty refresh_token or access_token".into(),
        ));
    }

    Ok(ExchangedTokens {
        refresh_token,
        access_token: body.access_token,
        expires_in_secs: body.expires_in,
        scope: body.scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_storage::token_crypto::RefreshToken;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build(server_url: &str) -> OAuthClient {
        OAuthClient::new(
            Provider::Gdrive,
            "client-id".into(),
            "client-secret".into(),
            RefreshToken::new("refresh-abc".into()),
            format!("{server_url}/token"),
        )
    }

    #[tokio::test]
    async fn refresh_caches_access_token_until_expiry_skew() {
        let server = MockServer::start().await;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_c = counter.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(move |_: &wiremock::Request| {
                counter_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "ya29.AT-1",
                    "expires_in": 3600,
                    "token_type": "Bearer"
                }))
            })
            .mount(&server)
            .await;

        let oauth = build(&server.uri());
        let client = reqwest::Client::new();
        let t1 = oauth.access_token(&client).await.unwrap();
        let t2 = oauth.access_token(&client).await.unwrap();
        assert_eq!(t1, "ya29.AT-1");
        assert_eq!(t2, "ya29.AT-1");
        // Second call MUST hit the cache, not the token endpoint.
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_sends_form_encoded_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_id=client-id"))
            .and(body_string_contains("client_secret=client-secret"))
            .and(body_string_contains("refresh_token=refresh-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ok",
                "expires_in": 60,
            })))
            .mount(&server)
            .await;
        let oauth = build(&server.uri());
        let t = oauth
            .access_token(&reqwest::Client::new())
            .await
            .expect("refresh ok");
        assert_eq!(t, "ok");
    }

    #[tokio::test]
    async fn refresh_400_invalid_grant_surfaces_as_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Bad Request"
            })))
            .mount(&server)
            .await;
        let oauth = build(&server.uri());
        let err = oauth
            .access_token(&reqwest::Client::new())
            .await
            .unwrap_err();
        match err {
            BackendError::Auth(msg) => {
                assert!(!msg.contains("refresh-abc"), "leaked refresh token: {msg}");
                assert!(msg.contains("400"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalidate_forces_next_call_to_re_refresh() {
        let server = MockServer::start().await;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_c = counter.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                let n = counter_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": format!("ya29.AT-{n}"),
                    "expires_in": 3600,
                }))
            })
            .mount(&server)
            .await;

        let oauth = build(&server.uri());
        let client = reqwest::Client::new();
        let t1 = oauth.access_token(&client).await.unwrap();
        oauth.invalidate();
        let t2 = oauth.access_token(&client).await.unwrap();
        assert_ne!(t1, t2, "invalidate must force a fresh refresh");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn network_failure_is_unreachable_not_auth() {
        // Point at a closed port. reqwest's connect should fail
        // synchronously; we want that to bubble out as Unreachable
        // so the replicator emits STORAGE_COLD_UNREACHABLE rather
        // than prompting the admin for re-consent.
        let oauth = OAuthClient::new(
            Provider::Gdrive,
            "c".into(),
            "s".into(),
            RefreshToken::new("r".into()),
            "http://127.0.0.1:1/token".to_string(),
        );
        let err = oauth
            .access_token(&reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::Unreachable(_)), "got {err:?}");
    }

    // -----------------------------------------------------------------
    // M2.2 closeout — auth-code flow.
    //
    // Coverage:
    //   * authorize_url shape for both providers (required params,
    //     scope, Google's access_type=offline + prompt=consent).
    //   * exchange_code happy path → ExchangedTokens.
    //   * exchange_code rejects a response that omits refresh_token
    //     (e.g. operator forgot offline_access in the scope set).
    //   * exchange_code maps 4xx to Auth, network failure to
    //     Unreachable — same contract as the refresh-grant path.
    //   * new_state produces 43 base64url-no-pad chars and never
    //     repeats across calls.
    // -----------------------------------------------------------------

    #[test]
    fn new_state_is_43_base64url_chars_and_unique() {
        let a = new_state();
        let b = new_state();
        assert_eq!(a.len(), 43, "32 bytes → base64url-no-pad → 43 chars");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        assert_ne!(a, b, "two calls must produce distinct tokens");
    }

    #[test]
    fn authorize_url_gdrive_carries_offline_and_consent() {
        let url = authorize_url(
            Provider::Gdrive,
            "client.apps.googleusercontent.com",
            "https://engine.local/api/v1/admin/oauth/gdrive/callback",
            "STATE-1",
        );
        assert!(url.contains("client_id=client.apps.googleusercontent.com"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fengine.local"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file"));
        assert!(url.contains("state=STATE-1"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    #[test]
    fn authorize_url_onedrive_carries_offline_access_scope() {
        let url = authorize_url(
            Provider::Onedrive,
            "00000000-0000-0000-0000-000000000000",
            "https://engine.local/api/v1/admin/oauth/onedrive/callback",
            "STATE-2",
        );
        assert!(url.contains("response_type=code"));
        // Microsoft requires `offline_access` to grant a refresh
        // token. Encoded form of the space-separated scope:
        assert!(
            url.contains("Files.ReadWrite.AppFolder") && url.contains("offline_access"),
            "url: {url}"
        );
        assert!(url.contains("state=STATE-2"));
        assert!(url.contains("prompt=select_account"));
    }

    #[tokio::test]
    async fn exchange_code_happy_path_returns_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=AUTH-CODE-1"))
            .and(body_string_contains("client_id=cid"))
            .and(body_string_contains("client_secret=csec"))
            .and(body_string_contains("redirect_uri="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.AT-fresh",
                "refresh_token": "1//0eRefresh",
                "expires_in": 3599,
                "scope": "https://www.googleapis.com/auth/drive.file",
                "token_type": "Bearer"
            })))
            .mount(&server)
            .await;

        let tokens = exchange_code_with_client(
            Provider::Gdrive,
            "AUTH-CODE-1",
            "https://engine.local/api/v1/admin/oauth/gdrive/callback",
            "cid",
            "csec",
            &format!("{}/token", server.uri()),
            &reqwest::Client::new(),
        )
        .await
        .expect("exchange_code ok");

        assert_eq!(tokens.refresh_token, "1//0eRefresh");
        assert_eq!(tokens.access_token, "ya29.AT-fresh");
        assert_eq!(tokens.expires_in_secs, Some(3599));
        assert_eq!(
            tokens.scope.as_deref(),
            Some("https://www.googleapis.com/auth/drive.file")
        );
    }

    #[tokio::test]
    async fn exchange_code_rejects_response_with_no_refresh_token() {
        // Provider returned only access_token — common when scope
        // omits offline_access (Microsoft) or prompt!=consent on
        // re-consent (Google). Surface as Auth so the UI prompts
        // for re-consent rather than treating as transient.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.AT-without-refresh",
                "expires_in": 3599
            })))
            .mount(&server)
            .await;

        let err = exchange_code_with_client(
            Provider::Gdrive,
            "code",
            "https://engine.local/api/v1/admin/oauth/gdrive/callback",
            "cid",
            "csec",
            &format!("{}/token", server.uri()),
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        match err {
            BackendError::Auth(msg) => {
                assert!(msg.to_lowercase().contains("refresh_token"), "got: {msg}");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exchange_code_4xx_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant"
            })))
            .mount(&server)
            .await;

        let err = exchange_code_with_client(
            Provider::Onedrive,
            "bad-code",
            "https://engine.local/api/v1/admin/oauth/onedrive/callback",
            "cid",
            "csec",
            &format!("{}/token", server.uri()),
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BackendError::Auth(_)), "got {err:?}");
    }
}
