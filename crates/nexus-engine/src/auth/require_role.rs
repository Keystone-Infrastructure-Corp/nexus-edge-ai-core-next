//! M6 Phase 2 Step 2.5 — `SessionContext` extractor + role gates.
//!
//! The bridge between the access JWTs minted by
//! [`super::sessions::issue_access_token`] and the per-handler
//! authorisation check. Every handler that needs to know "who
//! is asking?" takes [`SessionContext`] as one of its axum
//! extractor arguments:
//!
//! ```ignore
//! async fn put_camera(
//!     ctx: SessionContext,
//!     State(s): State<ApiState>,
//!     ...
//! ) -> Result<...> {
//!     ctx.require(Role::Operator)?;     // 403 if viewer
//!     // ... mutate ...
//!     audit::record(&s, &ctx, "camera.update", ...);
//!     Ok(...)
//! }
//! ```
//!
//! The [`AdminContext`] newtype wrapper lets a handler declare
//! its admin requirement in its signature instead of via a
//! `.require()` call. Operator + viewer handlers use
//! `ctx.require(Role::Operator)` / `Role::Viewer` directly
//! against [`SessionContext`].
//!
//! ## Backwards-compatibility bridge
//!
//! The existing [`crate::admin_auth::AdminClaims`] shape (legacy
//! `dev_token`) has no `role` field. To keep the deprecation
//! window smooth, the extractor accepts EITHER:
//!
//! * a new access JWT minted by `auth::sessions` (carries
//!   `sub`, `role`, `iat`, `exp`, `jti`), OR
//! * a legacy JWT with no `role` claim (anything signed with
//!   the same HS256 secret). Legacy tokens map to
//!   `role = Admin` and `is_legacy_admin = true` so the audit
//!   log can flag them.
//!
//! Both decode through one [`EitherClaims`] shape — we don't
//! verify the JWT twice. The `role` field is `Option<Role>`;
//! `None` means "legacy".
//!
//! ## Dev-mode (no admin secret configured)
//!
//! When `auth.admin_secret_path` is unset (no key material),
//! [`AdminAuthState::admin_secret`] is `None`. In that mode the
//! existing [`crate::admin_auth::admin_auth_layer`] allows
//! loopback writes — the engine is effectively in "everyone is
//! admin" mode. The extractor mirrors that: no secret →
//! synthetic admin context, no header required. This keeps the
//! `cargo run` happy-path unchanged and bypasses no security
//! barrier that wasn't already bypassed by the legacy middleware.
//!
//! Cookies (`__Host-nexus_session`) land in Step 2.7 with the
//! login handler; this extractor only reads `Authorization:
//! Bearer ...` for now.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use nexus_store::UserId;
use nexus_types::Role;
use serde::{Deserialize, Serialize};

use crate::admin_auth::AdminAuthState;

// ---------------------------------------------------------------------------
// SessionContext + role newtypes.
// ---------------------------------------------------------------------------

/// Verified caller identity attached to every authenticated
/// request. Lifetime is exactly one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    /// `users.id` of the authenticated principal, or `0` when
    /// `is_legacy_admin` (no user row, just a shared secret).
    pub user_id: UserId,
    /// Effective role for authorisation decisions. For legacy
    /// tokens this is always `Role::Admin`.
    pub role: Role,
    /// JWT id from the token. For legacy tokens with no `jti`
    /// claim we synthesise `"legacy"` so the audit log row is
    /// still populated.
    pub jti: String,
    /// `true` iff the caller authenticated via the legacy
    /// `AdminClaims` shape (no `role` claim). The audit log
    /// surfaces this so an operator can spot stale callers.
    pub is_legacy_admin: bool,
    /// v0.1.36 — refresh-chain id this access token belongs to,
    /// when present. Populated from `AccessClaims.chain_id`;
    /// `None` for legacy / dev-mode tokens. Used by the idle-
    /// bump pipeline to identify which refresh chain to roll
    /// `last_active_at` forward on.
    pub chain_id: Option<String>,
}

/// v0.1.36 — message shape the request extractor sends into the
/// background idle-bump drain task. The drain coalesces by
/// chain_id so a 60-req/s SPA produces 1 UPDATE per chain per
/// 250 ms tick.
pub type IdleBump = (String, chrono::DateTime<chrono::Utc>);

impl SessionContext {
    /// `true` iff the caller's role satisfies `required`. See
    /// [`Role::satisfies`] — `admin >= operator >= viewer`.
    pub fn satisfies(&self, required: Role) -> bool {
        self.role.satisfies(required)
    }

    /// `Ok(())` iff [`Self::satisfies`] returns `true`, else a
    /// [`SessionRejection::InsufficientRole`] suitable to bubble
    /// from the handler with `?`.
    pub fn require(&self, required: Role) -> Result<(), SessionRejection> {
        if self.satisfies(required) {
            Ok(())
        } else {
            Err(SessionRejection::InsufficientRole {
                have: self.role,
                need: required,
            })
        }
    }

    /// Convenience for tests + the bootstrap path — synthesise a
    /// dev-mode admin context with no real JWT. NEVER call this
    /// from a handler; it bypasses authentication.
    #[cfg(test)]
    pub fn test_admin() -> Self {
        Self {
            user_id: 0,
            role: Role::Admin,
            jti: "test".into(),
            is_legacy_admin: true,
            chain_id: None,
        }
    }
}

/// Newtype extractor that requires `Role::Admin`. Use in handler
/// signatures when you want the role check at compile-time-ish
/// instead of inside the handler body. The inner [`SessionContext`]
/// is still available via `.0` for audit-log writes.
//
// AdminContext is consumed by `auth::users_admin` (Step 2.8).
// Operator + viewer handlers call `ctx.require(Role::Operator)`
// / `Role::Viewer` against `SessionContext` directly — if those
// patterns prove repetitive enough to warrant their own newtype
// extractors, re-introduce `OperatorContext` / `ViewerContext`
// here in the same shape as `AdminContext`.
#[derive(Debug, Clone)]
pub struct AdminContext(pub SessionContext);

// ---------------------------------------------------------------------------
// Rejection type.
// ---------------------------------------------------------------------------

/// Reasons the extractor can fail. Each variant renders to a
/// specific HTTP status, but the response body is intentionally
/// terse — we don't reveal which check tripped (signature vs
/// expiry vs role). Tracing logs carry the detail.
#[derive(Debug, Clone, Copy)]
pub enum SessionRejection {
    /// No `Authorization: Bearer ...` header AND a secret IS
    /// configured (the dev no-secret bypass didn't fire).
    /// Renders 401.
    Missing,
    /// Header present but the JWT did not verify (bad
    /// signature / wrong alg / expired / malformed claims).
    /// Renders 401.
    Invalid,
    /// JWT verified but `role` insufficient. Renders 403.
    InsufficientRole { have: Role, need: Role },
}

impl IntoResponse for SessionRejection {
    fn into_response(self) -> Response {
        match self {
            SessionRejection::Missing => {
                tracing::warn!("session rejected: no valid bearer");
                (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "authentication required"})),
                )
                    .into_response()
            }
            SessionRejection::Invalid => {
                tracing::warn!("session rejected: invalid bearer");
                (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "authentication required"})),
                )
                    .into_response()
            }
            SessionRejection::InsufficientRole { have, need } => {
                tracing::warn!(have = %have, need = %need, "session rejected: insufficient role");
                (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "insufficient role"})),
                )
                    .into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JWT decode helper.
// ---------------------------------------------------------------------------

/// One-shape decoder for both the new `AccessClaims` and the
/// legacy `AdminClaims`. The `role` field's `Option` is what
/// makes the bridge work: present → new shape; missing → legacy
/// (treat as admin).
#[derive(Debug, Deserialize, Serialize)]
struct EitherClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    role: Option<Role>,
    exp: i64,
    #[serde(default)]
    iat: Option<i64>,
    #[serde(default)]
    jti: Option<String>,
    /// v0.1.36 — added to `AccessClaims`. `Option` because
    /// legacy tokens (pre-v0.1.36, the bootstrap dev token,
    /// any future ad-hoc impersonation token) won't carry it.
    #[serde(default)]
    chain_id: Option<String>,
}

/// Decode `token` against `key` and produce a [`SessionContext`]
/// or a [`SessionRejection`].
///
/// We disable jsonwebtoken's built-in exp check so the test
/// suite can inject a fake `now`; the real-clock comparison
/// happens here.
fn decode_to_context(token: &str, key: &DecodingKey) -> Result<SessionContext, SessionRejection> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = false;
    validation.required_spec_claims.clear();
    let data = jsonwebtoken::decode::<EitherClaims>(token, key, &validation).map_err(|e| {
        tracing::debug!(error = %e, "JWT decode failed");
        SessionRejection::Invalid
    })?;
    let now = Utc::now().timestamp();
    if data.claims.exp <= now {
        tracing::debug!(exp = data.claims.exp, now, "JWT expired");
        return Err(SessionRejection::Invalid);
    }
    let (role, is_legacy) = match data.claims.role {
        Some(r) => (r, false),
        None => (Role::Admin, true),
    };
    let user_id: UserId = data
        .claims
        .sub
        .as_deref()
        .and_then(|s| UserId::from_str(s).ok())
        .unwrap_or(0);
    let jti = data.claims.jti.unwrap_or_else(|| "legacy".into());
    Ok(SessionContext {
        user_id,
        role,
        jti,
        is_legacy_admin: is_legacy,
        chain_id: data.claims.chain_id,
    })
}

/// Header-parser shared with both axum + the unit tests. Reads
/// the first `Authorization` header and returns the token half
/// iff the scheme is `Bearer` (case-insensitive, per RFC 7235).
fn extract_bearer(parts: &Parts) -> Option<&str> {
    let h = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}

/// Shared by both the axum FromRequestParts impl and the unit
/// tests. The axum impl just wraps the header read; this is
/// what actually does the auth decision.
fn authorise(
    auth: &AdminAuthState,
    bearer: Option<&str>,
) -> Result<SessionContext, SessionRejection> {
    let secret = auth.admin_secret();
    match (secret, bearer) {
        // No secret configured AND no bearer → dev-mode admin.
        (None, None) => Ok(SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "dev-no-secret".into(),
            is_legacy_admin: true,
            chain_id: None,
        }),
        // No secret configured BUT bearer present → still
        // dev-mode admin. The token's signature can't be
        // checked because there's no key, and the legacy
        // admin_auth middleware behaves identically (any
        // request is accepted in this mode). Don't try to
        // decode unsigned-claims.
        (None, Some(_)) => Ok(SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "dev-no-secret".into(),
            is_legacy_admin: true,
            chain_id: None,
        }),
        // Secret configured BUT no bearer → 401.
        (Some(_), None) => Err(SessionRejection::Missing),
        // Secret configured AND bearer present → verify.
        (Some(s), Some(t)) => {
            let key = DecodingKey::from_secret(s.as_bytes());
            decode_to_context(t, &key)
        }
    }
}

// ---------------------------------------------------------------------------
// FromRequestParts impls.
// ---------------------------------------------------------------------------

impl<S> FromRequestParts<S> for SessionContext
where
    S: Send + Sync,
    Arc<AdminAuthState>: FromRef<S>,
{
    type Rejection = SessionRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = Arc::<AdminAuthState>::from_ref(state);
        let bearer = extract_bearer(parts);
        let ctx = authorise(&auth, bearer)?;
        emit_idle_bump(&auth, &ctx);
        Ok(ctx)
    }
}

// M6 Phase 4 Step 4.1 — `Option<SessionContext>` for handlers that
// live OUTSIDE the admin gate (e.g. `/api/v1/rules/{id}/delivery`
// PUT, `/api/cameras/{id}` PUT/DELETE). These routes are not yet
// behind `admin_auth_layer`, so a request may legitimately arrive
// without a bearer. The audit-hooks Step 4.1 then records the
// mutation with actor `system:unknown` rather than 401'ing — the
// route's existing auth posture is preserved while still surfacing
// the attempt in the audit log. Any auth *failure* (bad/expired
// JWT, decode error) collapses to `None` as well so the route stays
// permissive in axum 0.8's `OptionalFromRequestParts` contract.
impl<S> axum::extract::OptionalFromRequestParts<S> for SessionContext
where
    S: Send + Sync,
    Arc<AdminAuthState>: FromRef<S>,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        let auth = Arc::<AdminAuthState>::from_ref(state);
        let bearer = extract_bearer(parts);
        let ctx = authorise(&auth, bearer).ok();
        if let Some(ref c) = ctx {
            emit_idle_bump(&auth, c);
        }
        Ok(ctx)
    }
}

/// v0.1.36 \u2014 try-send a `(chain_id, now)` bump into the drain
/// channel. No-op when:
///
/// * the channel isn't wired (tests, dev-mode no-secret),
/// * the session has no `chain_id` (legacy token, dev bypass), or
/// * the channel is full (per-tick coalescing already handles
///   the storm \u2014 dropping a tick of bumps just means
///   `last_active_at` lags by one tick).
fn emit_idle_bump(auth: &AdminAuthState, ctx: &SessionContext) {
    let (Some(tx), Some(chain_id)) = (auth.idle_bump_tx(), ctx.chain_id.as_deref()) else {
        return;
    };
    let _ = tx.try_send((chain_id.to_owned(), chrono::Utc::now()));
}

macro_rules! impl_role_extractor {
    ($name:ident, $role:expr) => {
        impl<S> FromRequestParts<S> for $name
        where
            S: Send + Sync,
            Arc<AdminAuthState>: FromRef<S>,
        {
            type Rejection = SessionRejection;

            async fn from_request_parts(
                parts: &mut Parts,
                state: &S,
            ) -> Result<Self, Self::Rejection> {
                let ctx = SessionContext::from_request_parts(parts, state).await?;
                ctx.require($role)?;
                Ok($name(ctx))
            }
        }
    };
}

impl_role_extractor!(AdminContext, Role::Admin);

// ---------------------------------------------------------------------------
// FromRef bridge for `ApiState`.
// ---------------------------------------------------------------------------

// Lets every handler take `SessionContext` (or one of the role
// newtypes) directly when the router is built with
// `State<ApiState>`. Cheap — Arc clone.
impl FromRef<crate::api::ApiState> for Arc<AdminAuthState> {
    fn from_ref(input: &crate::api::ApiState) -> Self {
        input.admin_auth.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::sessions::{issue_access_token, AccessClaims};
    use chrono::Duration;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use uuid::Uuid;

    fn secret_bytes() -> &'static [u8] {
        b"test-shared-admin-secret-32-bytes!"
    }

    fn auth_with_secret() -> AdminAuthState {
        AdminAuthState::from_secret_bytes(Some(secret_bytes()), false)
    }

    fn auth_without_secret() -> AdminAuthState {
        AdminAuthState::from_secret_bytes(None, false)
    }

    // ----- authorise() with no secret configured ---------------------------

    #[test]
    fn no_secret_no_bearer_yields_dev_admin() {
        let ctx = authorise(&auth_without_secret(), None).unwrap();
        assert_eq!(ctx.role, Role::Admin);
        assert!(ctx.is_legacy_admin);
        assert_eq!(ctx.user_id, 0);
    }

    #[test]
    fn no_secret_with_bearer_still_yields_dev_admin() {
        // Token isn't even valid; doesn't matter, no secret to
        // verify against. Mirrors admin_auth_layer behaviour.
        let ctx = authorise(&auth_without_secret(), Some("anything")).unwrap();
        assert_eq!(ctx.role, Role::Admin);
        assert!(ctx.is_legacy_admin);
    }

    // ----- authorise() with secret configured ------------------------------

    #[test]
    fn secret_configured_but_no_bearer_returns_missing() {
        let err = authorise(&auth_with_secret(), None).unwrap_err();
        assert!(matches!(err, SessionRejection::Missing), "{err:?}");
    }

    #[test]
    fn valid_access_jwt_yields_role_from_claim() {
        let now = Utc::now();
        let token = issue_access_token(
            42,
            Role::Operator,
            secret_bytes(),
            now,
            Duration::minutes(15),
            None,
        )
        .unwrap();
        let ctx = authorise(&auth_with_secret(), Some(&token)).unwrap();
        assert_eq!(ctx.user_id, 42);
        assert_eq!(ctx.role, Role::Operator);
        assert!(!ctx.is_legacy_admin);
        assert!(!ctx.jti.is_empty());
        assert_ne!(ctx.jti, "legacy");
    }

    #[test]
    fn expired_jwt_rejected_as_invalid() {
        // Issue with a 1s TTL then sleep-equivalent: we can't
        // really sleep in a unit test, but issuing with negative
        // TTL produces an already-expired token.
        let now = Utc::now() - Duration::hours(1);
        let token = issue_access_token(
            42,
            Role::Admin,
            secret_bytes(),
            now,
            Duration::minutes(1),
            None,
        )
        .unwrap();
        let err = authorise(&auth_with_secret(), Some(&token)).unwrap_err();
        assert!(matches!(err, SessionRejection::Invalid), "{err:?}");
    }

    #[test]
    fn wrong_signature_rejected_as_invalid() {
        let now = Utc::now();
        // Sign with one secret, verify with another.
        let token = issue_access_token(
            1,
            Role::Admin,
            b"other-secret",
            now,
            Duration::minutes(15),
            None,
        )
        .unwrap();
        let err = authorise(&auth_with_secret(), Some(&token)).unwrap_err();
        assert!(matches!(err, SessionRejection::Invalid), "{err:?}");
    }

    #[test]
    fn legacy_jwt_without_role_claim_yields_admin_marked_legacy() {
        // Hand-craft a legacy `AdminClaims`-shaped JWT (no role
        // field). The extractor must accept it as Admin and
        // flag `is_legacy_admin`.
        #[derive(Serialize)]
        struct LegacyClaims {
            sub: String,
            exp: i64,
            iat: i64,
        }
        let claims = LegacyClaims {
            sub: "admin-tool".into(),
            exp: (Utc::now() + Duration::minutes(15)).timestamp(),
            iat: Utc::now().timestamp(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret_bytes()),
        )
        .unwrap();
        let ctx = authorise(&auth_with_secret(), Some(&token)).unwrap();
        assert!(ctx.is_legacy_admin);
        assert_eq!(ctx.role, Role::Admin);
        // sub isn't numeric so user_id falls back to 0.
        assert_eq!(ctx.user_id, 0);
        assert_eq!(ctx.jti, "legacy");
    }

    #[test]
    fn jwt_with_non_numeric_sub_yields_user_id_zero_not_a_reject() {
        // Defensive: a hand-crafted token claiming to be a new
        // shape (has role) but with non-numeric sub. We accept
        // the role but zero out user_id rather than 401.
        let claims = AccessClaims {
            sub: "not-a-number".into(),
            role: Role::Operator,
            iat: Utc::now().timestamp(),
            exp: (Utc::now() + Duration::minutes(15)).timestamp(),
            jti: Uuid::now_v7().to_string(),
            chain_id: None,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret_bytes()),
        )
        .unwrap();
        let ctx = authorise(&auth_with_secret(), Some(&token)).unwrap();
        assert_eq!(ctx.role, Role::Operator);
        assert_eq!(ctx.user_id, 0);
        assert!(!ctx.is_legacy_admin);
    }

    // ----- require() role check --------------------------------------------

    #[test]
    fn require_admin_passes_for_admin() {
        let ctx = SessionContext::test_admin();
        assert!(ctx.require(Role::Admin).is_ok());
        assert!(ctx.require(Role::Operator).is_ok());
        assert!(ctx.require(Role::Viewer).is_ok());
    }

    #[test]
    fn require_operator_fails_for_viewer_passes_for_operator() {
        let viewer = SessionContext {
            user_id: 1,
            role: Role::Viewer,
            jti: "x".into(),
            is_legacy_admin: false,
            chain_id: None,
        };
        let err = viewer.require(Role::Operator).unwrap_err();
        match err {
            SessionRejection::InsufficientRole { have, need } => {
                assert_eq!(have, Role::Viewer);
                assert_eq!(need, Role::Operator);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let op = SessionContext {
            role: Role::Operator,
            ..viewer
        };
        assert!(op.require(Role::Operator).is_ok());
        assert!(op.require(Role::Viewer).is_ok());
        assert!(op.require(Role::Admin).is_err());
    }

    // ----- extract_bearer() header-parser ----------------------------------

    fn parts_with_header(name: &'static str, val: &str) -> Parts {
        let req = axum::http::Request::builder()
            .header(name, val)
            .body(())
            .unwrap();
        req.into_parts().0
    }

    fn parts_without_auth() -> Parts {
        let req = axum::http::Request::builder().body(()).unwrap();
        req.into_parts().0
    }

    #[test]
    fn extract_bearer_finds_token() {
        let p = parts_with_header("authorization", "Bearer abc.def.ghi");
        assert_eq!(extract_bearer(&p), Some("abc.def.ghi"));
    }

    #[test]
    fn extract_bearer_case_insensitive_scheme() {
        let p = parts_with_header("authorization", "bearer abc.def.ghi");
        assert_eq!(extract_bearer(&p), Some("abc.def.ghi"));
    }

    #[test]
    fn extract_bearer_rejects_other_schemes() {
        let p = parts_with_header("authorization", "Basic Zm9vOmJhcg==");
        assert_eq!(extract_bearer(&p), None);
    }

    #[test]
    fn extract_bearer_rejects_empty_token() {
        let p = parts_with_header("authorization", "Bearer ");
        assert_eq!(extract_bearer(&p), None);
    }

    #[test]
    fn extract_bearer_returns_none_when_header_missing() {
        let p = parts_without_auth();
        assert_eq!(extract_bearer(&p), None);
    }

    // ----- IntoResponse for SessionRejection -------------------------------

    #[test]
    fn missing_renders_401() {
        let r = SessionRejection::Missing.into_response();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn invalid_renders_401() {
        let r = SessionRejection::Invalid.into_response();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn insufficient_role_renders_403() {
        let r = SessionRejection::InsufficientRole {
            have: Role::Viewer,
            need: Role::Admin,
        }
        .into_response();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
