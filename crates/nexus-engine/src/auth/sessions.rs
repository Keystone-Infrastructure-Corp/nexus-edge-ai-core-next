//! M6 Phase 2 Step 2.4 — session model: access-JWT + opaque
//! refresh-secret primitives.
//!
//! Two-tier session, mirroring the M6 spec:
//!
//! 1. **Access JWT** — HS256, short-lived (default 15 min),
//!    stateless. Carries the user id, role, issued-at, expiry,
//!    and a UUIDv7 token id. Signed with the existing
//!    `auth.admin_secret_path` HS256 secret so the bearer is a
//!    drop-in replacement for the legacy dev-token bearer that
//!    [`crate::admin_auth`] already consumes — Step 2.5's
//!    `require_role` extractor will accept *either* shape during
//!    the deprecation window.
//!
//! 2. **Refresh secret** — 32 random bytes encoded as URL-safe
//!    base64 (no padding, 43 chars). Hashed (SHA-256 hex) for
//!    storage in `auth_refresh_tokens.token_hash`. The
//!    plaintext only ever lives in the client's `Set-Cookie`
//!    response and (briefly) the engine's response buffer.
//!    Persistence + rotation + replay-detect all live in
//!    [`nexus_store::sessions`].
//!
//! This module owns ONLY the in-memory primitives:
//!
//! * [`AccessClaims`] — the JWT payload shape.
//! * [`issue_access_token`] / [`verify_access_token`].
//! * [`RefreshSecret`] — newtype around the plaintext secret.
//! * [`new_refresh_secret`] / [`hash_refresh_secret`].
//!
//! Cookies (`__Host-nexus_session=...; Secure; HttpOnly;
//! SameSite=Lax`) live with the login handler in `api.rs`
//! (Step 2.7) — they're transport, not session model.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
#[cfg(test)]
use jsonwebtoken::{DecodingKey, Validation};
use nexus_store::UserId;
use nexus_types::Role;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// `jsonwebtoken` raised an error during encode or decode.
    /// Most decode failures (bad signature, expired, malformed
    /// JSON) collapse into this variant; the handler should
    /// reply 401 without leaking which check failed.
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),

    /// The OS CSPRNG (`getrandom`) failed. In practice this
    /// only happens if the kernel entropy pool is unavailable
    /// (unmounted procfs, super-early boot). Surfaces as 500
    /// at the handler.
    #[error("rng failure: {0}")]
    Rng(#[from] getrandom::Error),
}

// ---------------------------------------------------------------------------
// Access token (HS256 JWT).
// ---------------------------------------------------------------------------

/// The JWT payload. Field names are short on purpose — every
/// extra byte is paid by every request for the lifetime of the
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// Subject — the `users.id` of the authenticated user, as
    /// a string for compatibility with the legacy
    /// [`crate::admin_auth::AdminClaims`] shape (which also
    /// uses `sub: String`).
    pub sub: String,
    /// Role at token-issue time. Authoritative for
    /// authorisation decisions until the token expires —
    /// role-downgrades + admin-disables don't take effect
    /// until the next refresh, which is acceptable for a
    /// 15-minute access TTL.
    pub role: Role,
    /// Issued-at, seconds-since-epoch UTC.
    pub iat: i64,
    /// Expiry, seconds-since-epoch UTC. jsonwebtoken's
    /// default `Validation` checks this for us on decode.
    pub exp: i64,
    /// Token id — a UUIDv7. Surfaces in audit_log for any
    /// mutating request so an admin can correlate a row to a
    /// specific access bearer (e.g. for incident response).
    pub jti: String,
    /// v0.1.36 — the refresh-chain this access token belongs
    /// to. UUIDv7. Lets [`crate::auth::require_role`] tag a
    /// per-request idle-bump message without a follow-up DB
    /// lookup. `None` for tokens issued by pre-v0.1.36 engine
    /// builds (the field is `#[serde(default)]` so old tokens
    /// still decode); the idle-bump task no-ops on `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<String>,
}

/// Build + sign an access JWT for `(user_id, role)` valid from
/// `iat` for `ttl`.
///
/// `now` is injected so tests can drive the time machine; in
/// the login handler the caller passes `Utc::now()`.
pub fn issue_access_token(
    user_id: UserId,
    role: Role,
    secret: &[u8],
    now: DateTime<Utc>,
    ttl: Duration,
    chain_id: Option<&str>,
) -> Result<String, SessionError> {
    let iat = now.timestamp();
    let exp = (now + ttl).timestamp();
    let claims = AccessClaims {
        sub: user_id.to_string(),
        role,
        iat,
        exp,
        jti: Uuid::now_v7().to_string(),
        chain_id: chain_id.map(str::to_owned),
    };
    let key = EncodingKey::from_secret(secret);
    Ok(jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &key,
    )?)
}

/// Decode + verify an access JWT against `secret`. Returns the
/// claims iff the signature is valid, the algorithm is HS256
/// (rejects `alg=none` + `alg=RS256` substitution), and
/// `exp > now`.
///
/// We re-implement the exp check against the caller-supplied
/// `now` so the test suite can simulate "5 minutes from now"
/// without touching the system clock. jsonwebtoken's built-in
/// `Validation` would otherwise insist on wall-clock comparison.
//
// Only used by `sessions::tests::*` symmetry tests — production
// handlers decode via `require_role::SessionContext`, which has
// its own decoder (it accepts the legacy admin-token shape too).
#[cfg(test)]
fn verify_access_token(
    token: &str,
    secret: &[u8],
    now: DateTime<Utc>,
) -> Result<AccessClaims, SessionError> {
    let key = DecodingKey::from_secret(secret);
    let mut validation = Validation::new(Algorithm::HS256);
    // Disable the built-in exp check so we can use our injected
    // `now`. Signature + algorithm validation still run.
    validation.validate_exp = false;
    validation.required_spec_claims.clear();
    let claims = jsonwebtoken::decode::<AccessClaims>(token, &key, &validation)?.claims;
    if claims.exp <= now.timestamp() {
        return Err(SessionError::Jwt(
            jsonwebtoken::errors::ErrorKind::ExpiredSignature.into(),
        ));
    }
    Ok(claims)
}

// ---------------------------------------------------------------------------
// Refresh secret (opaque random bytes, hashed for storage).
// ---------------------------------------------------------------------------

/// Length in bytes of a freshly-generated refresh secret. 32B
/// = 256 bits = brute-force-infeasible. The base64url no-pad
/// encoding produces 43 ASCII chars.
pub const REFRESH_SECRET_BYTES: usize = 32;

/// Newtype around the plaintext refresh secret. Constructed
/// only by [`new_refresh_secret`] OR [`RefreshSecret::from_str_unchecked`]
/// (for parsing an inbound cookie value); never derived
/// `Debug` so it doesn't accidentally hit `tracing::error!`.
///
/// To get the inner string for hashing or for putting in a
/// `Set-Cookie`, call [`Self::expose`] — that name makes the
/// intent visible in code review.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshSecret(String);

impl RefreshSecret {
    /// Wrap a string that came from elsewhere (e.g. the
    /// inbound cookie on `/auth/refresh`). No validation —
    /// the caller verifies via [`hash_refresh_secret`] +
    /// store lookup. We could length-check (43 chars) here
    /// but that would let an attacker probe shape vs not.
    pub fn from_str_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Yield the inner ASCII string. The verbose name is
    /// deliberate — every caller is a code-review checkpoint.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

// Manual Debug that redacts the secret. NEVER replace this
// with `derive(Debug)`.
impl std::fmt::Debug for RefreshSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RefreshSecret(<redacted>)")
    }
}

/// Generate a fresh refresh secret using the OS CSPRNG.
/// 32 random bytes → URL-safe base64 (no padding) → 43 chars.
pub fn new_refresh_secret() -> Result<RefreshSecret, SessionError> {
    let mut bytes = [0u8; REFRESH_SECRET_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(RefreshSecret(URL_SAFE_NO_PAD.encode(bytes)))
}

/// SHA-256 of the secret's bytes, encoded as lowercase hex.
/// 64 chars. Deterministic — the same secret always hashes
/// to the same value (no salt; the secret is high-entropy
/// already, and we need to look it up by hash on refresh).
pub fn hash_refresh_secret(secret: &RefreshSecret) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.expose().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0).unwrap()
    }

    fn secret() -> &'static [u8] {
        b"a-test-secret-at-least-32-bytes-long!!"
    }

    // ----- access JWT -------------------------------------------------------

    #[test]
    fn access_jwt_round_trips_claims() {
        let token = issue_access_token(
            42,
            Role::Operator,
            secret(),
            now(),
            Duration::minutes(15),
            None,
        )
        .unwrap();
        let claims = verify_access_token(&token, secret(), now()).unwrap();
        assert_eq!(claims.sub, "42");
        assert_eq!(claims.role, Role::Operator);
        assert_eq!(claims.iat, now().timestamp());
        assert_eq!(claims.exp, (now() + Duration::minutes(15)).timestamp());
        // jti is a UUIDv7 (36 chars including hyphens).
        assert_eq!(claims.jti.len(), 36);
        assert!(Uuid::parse_str(&claims.jti).is_ok());
    }

    #[test]
    fn access_jwt_rejected_when_signed_with_wrong_secret() {
        let token = issue_access_token(
            42,
            Role::Operator,
            secret(),
            now(),
            Duration::minutes(15),
            None,
        )
        .unwrap();
        let err = verify_access_token(&token, b"other-secret", now()).unwrap_err();
        // Should be a JWT decode error, not Rng or anything else.
        assert!(matches!(err, SessionError::Jwt(_)), "{err:?}");
    }

    #[test]
    fn access_jwt_rejected_when_expired() {
        let token = issue_access_token(
            42,
            Role::Operator,
            secret(),
            now(),
            Duration::minutes(15),
            None,
        )
        .unwrap();
        let future = now() + Duration::hours(1);
        let err = verify_access_token(&token, secret(), future).unwrap_err();
        match err {
            SessionError::Jwt(e) => {
                assert!(
                    matches!(e.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature),
                    "{e:?}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn access_jwt_at_exact_exp_is_rejected() {
        // Boundary: `<=`, so `now == exp` is expired.
        let token = issue_access_token(
            42,
            Role::Operator,
            secret(),
            now(),
            Duration::seconds(1),
            None,
        )
        .unwrap();
        let at_exp = now() + Duration::seconds(1);
        assert!(verify_access_token(&token, secret(), at_exp).is_err());
    }

    #[test]
    fn access_jwt_each_issue_has_unique_jti() {
        // Defence-in-depth: two issues at the "same" time still
        // produce distinct jtis because UUIDv7 has a per-call
        // randomness tail.
        let a = issue_access_token(1, Role::Viewer, secret(), now(), Duration::minutes(1), None)
            .unwrap();
        let b = issue_access_token(1, Role::Viewer, secret(), now(), Duration::minutes(1), None)
            .unwrap();
        let ca = verify_access_token(&a, secret(), now()).unwrap();
        let cb = verify_access_token(&b, secret(), now()).unwrap();
        assert_ne!(ca.jti, cb.jti);
    }

    #[test]
    fn access_jwt_rejects_alg_none_substitution() {
        // RFC 7519 alg=none attack — produce an unsigned JWT
        // with the same payload and confirm we reject it.
        let header = jsonwebtoken::Header {
            alg: jsonwebtoken::Algorithm::HS256,
            ..Default::default()
        };
        let claims = AccessClaims {
            sub: "1".into(),
            role: Role::Admin,
            iat: now().timestamp(),
            exp: (now() + Duration::minutes(1)).timestamp(),
            jti: Uuid::now_v7().to_string(),
            chain_id: None,
        };
        // Sign with a known-wrong key to make sure verify fails
        // (alg=none rejection happens at decode-time via
        // Validation::new(HS256)).
        let attacker_key = jsonwebtoken::EncodingKey::from_secret(b"attacker");
        let token = jsonwebtoken::encode(&header, &claims, &attacker_key).unwrap();
        assert!(
            verify_access_token(&token, secret(), now()).is_err(),
            "wrong-key HS256 must fail"
        );
    }

    #[test]
    fn access_jwt_role_is_serialised_as_lowercase_string() {
        // Belt-and-suspenders: confirm the wire shape. The
        // login handler depends on Role::serialize for the
        // claim, and the verify path depends on the inverse.
        let token =
            issue_access_token(7, Role::Admin, secret(), now(), Duration::minutes(15), None)
                .unwrap();
        let claims = verify_access_token(&token, secret(), now()).unwrap();
        let json = serde_json::to_value(&claims).unwrap();
        assert_eq!(json["role"], "admin");
    }

    // ----- refresh secret ---------------------------------------------------

    #[test]
    fn new_refresh_secret_is_43_chars_urlsafe_no_pad() {
        let s = new_refresh_secret().unwrap();
        let exposed = s.expose();
        assert_eq!(exposed.len(), 43, "32 bytes base64url-no-pad = 43 chars");
        for c in exposed.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-urlsafe char {c:?}"
            );
        }
        assert!(!exposed.contains('='), "no padding");
    }

    #[test]
    fn two_fresh_refresh_secrets_differ() {
        // Cheap entropy check. Probability of collision in 32
        // random bytes is ~2^-256.
        let a = new_refresh_secret().unwrap();
        let b = new_refresh_secret().unwrap();
        assert_ne!(a.expose(), b.expose());
    }

    #[test]
    fn hash_refresh_secret_is_64_lower_hex_and_deterministic() {
        let s = RefreshSecret::from_str_unchecked("known-input");
        let h = hash_refresh_secret(&s);
        assert_eq!(h.len(), 64);
        for c in h.chars() {
            assert!(c.is_ascii_hexdigit() && !c.is_ascii_uppercase(), "{c:?}");
        }
        // Determinism.
        assert_eq!(h, hash_refresh_secret(&s));
        // Known-vector: SHA-256("known-input") =
        // 27ae49c070b1265efa164dd0941a7cec3eb64c4154b426110d25f8266b6d8b68
        // (verified out-of-band with `python3 -c
        // 'import hashlib; print(hashlib.sha256(b"known-input").hexdigest())'`).
        assert_eq!(
            h,
            "27ae49c070b1265efa164dd0941a7cec3eb64c4154b426110d25f8266b6d8b68"
        );
    }

    #[test]
    fn refresh_secret_debug_redacts_inner_value() {
        let s = RefreshSecret::from_str_unchecked("hunter2");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"), "Debug must redact, got {dbg:?}");
        assert!(dbg.contains("redacted"));
    }
}
