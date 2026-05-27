//! Ed25519 `actor_token` verifier.
//!
//! Verifies the compact JWS produced by the cloud-console
//! `entitlement-svc /v1/internal/sign-actor-token` endpoint. The wire
//! format is documented in `WIRE_PROTOCOL.md §11` (cloud repo); the
//! claim shape is [`nexus_cloud_protocol::v1::ActorTokenClaims`].
//!
//! ## Verification checks
//!
//! In order, the verifier:
//!
//! 1. Splits the JWS into `<header>.<payload>.<sig>`.
//! 2. Base64url-decodes each segment.
//! 3. Parses the header; requires `alg == "EdDSA"` and matches `kid`
//!    against the configured trusted-key set.
//! 4. Parses the payload as `ActorTokenClaims`.
//! 5. Verifies the Ed25519 signature over `<header>.<payload>` raw bytes.
//! 6. Checks `aud == "nexus-edge-rpc"`.
//! 7. Checks `core_id == self.core_id`.
//! 8. Checks `http_method == envelope.method` and `path == envelope.path`.
//! 9. Checks `iat <= now + SKEW` and `exp > now - SKEW`. The 30 s skew
//!    window matches Phase 1.15 (`docs/ARCHITECTURE.md §3.7`).
//! 10. Consults the [`JtiReplayCache`] — replays are rejected.
//!
//! All post-parse rejections collapse to `RejectReason::Invalid(_)` on
//! the wire to avoid leaking which check failed. The fine-grained
//! [`InvalidReason`] is returned in-process so the engine audit log can
//! record exactly why a token was refused.

use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signature, Verifier as DalekVerifier, VerifyingKey};
use nexus_cloud_protocol::v1::ActorTokenClaims;
use serde::Deserialize;

use crate::error::{InvalidReason, RejectReason};
use crate::jti_cache::JtiReplayCache;

/// Audience claim that all `actor_token`s targeting the engine MUST carry.
pub const EXPECTED_AUDIENCE: &str = "nexus-edge-rpc";

/// Clock-skew window per Phase 1.15. ±30 s on both sides.
pub const CLOCK_SKEW_SECS: i64 = 30;

/// A single trusted Ed25519 verification key bundled at enrollment.
///
/// The cloud-console may rotate the entitlement signing key without a
/// re-enrollment, so the verifier accepts a set of keys indexed by `kid`.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    /// `kid` carried in the JWS header.
    pub kid: String,
    /// Public key bytes (32 bytes). Loaded from the enrollment artifact.
    pub key: VerifyingKey,
}

/// Resolved actor extracted from a successfully verified token. The
/// engine writes this to its local audit log before dispatching the
/// handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActor {
    /// `sub` claim — UUID for human users, `system:<svc>` for service
    /// accounts.
    pub sub: String,
    /// `role` claim — `viewer`, `operator`, `admin`, `owner`, or
    /// `system:<svc>` for system tokens.
    pub role: String,
    /// `jti` of the token. Recorded so the audit log can be replayed
    /// against the source `actor_token` if needed.
    pub jti: String,
    /// Tenant the token was minted for. Equals `Verifier`'s `core_id`
    /// resolution context.
    pub org_id: String,
}

/// What the verifier was asked to authorise.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeContext<'a> {
    /// `rpc_call.payload.method` — HTTP verb (e.g. `POST`).
    pub method: &'a str,
    /// `rpc_call.payload.path` — absolute path on the edge loopback admin API.
    pub path: &'a str,
}

/// Verifier configured at engine startup. Cloneable (cheap `Arc`
/// internals) so it can be shared across dispatcher tasks.
#[derive(Debug, Clone)]
pub struct Verifier {
    inner: Arc<VerifierInner>,
}

#[derive(Debug)]
struct VerifierInner {
    core_id: String,
    trusted_keys: HashMap<String, VerifyingKey>,
    replay: Arc<JtiReplayCache>,
    /// When `true`, the verifier accepts an unsigned-`kid` header iff the
    /// trusted-key set has exactly one key. Useful in dev where the
    /// cloud-console hasn't rotated yet; off in prod.
    allow_kidless_when_single: bool,
}

impl Verifier {
    /// Build a verifier from a core id and a non-empty set of trusted
    /// keys. Returns `None` if `trusted_keys` is empty — a verifier with
    /// no keys would reject every token and is almost certainly a config
    /// bug worth surfacing early.
    #[must_use]
    pub fn new(
        core_id: impl Into<String>,
        trusted_keys: Vec<TrustedKey>,
        replay: Arc<JtiReplayCache>,
    ) -> Option<Self> {
        if trusted_keys.is_empty() {
            return None;
        }
        let map = trusted_keys
            .into_iter()
            .map(|tk| (tk.kid, tk.key))
            .collect();
        Some(Self {
            inner: Arc::new(VerifierInner {
                core_id: core_id.into(),
                trusted_keys: map,
                replay,
                allow_kidless_when_single: false,
            }),
        })
    }

    /// Returns the configured core id.
    #[must_use]
    pub fn core_id(&self) -> &str {
        &self.inner.core_id
    }

    /// Shared handle to the JTI replay cache.
    #[must_use]
    pub fn replay_cache(&self) -> Arc<JtiReplayCache> {
        Arc::clone(&self.inner.replay)
    }

    /// Verify a compact JWS against the envelope it travelled in.
    ///
    /// On success, returns the resolved [`VerifiedActor`]; the dispatcher
    /// then uses [`VerifiedActor::sub`] for the system-sub method
    /// whitelist check.
    ///
    /// # Errors
    ///
    /// Returns [`RejectReason::Invalid`] for any post-parse failure
    /// (bad sig, mismatched claims, expired, replay, etc.). The caller
    /// stamps `actor_token_invalid` into the wire response; the granular
    /// [`InvalidReason`] is for engine audit logging only.
    pub fn verify(
        &self,
        token: &str,
        envelope: EnvelopeContext<'_>,
    ) -> Result<VerifiedActor, RejectReason> {
        self.verify_with_request_id(token, envelope, None)
    }

    /// Phase 1.16 — verify against a `(jti, request_id)` replay key.
    /// Pass `request_id = Some(envelope.payload.request_id)` so a
    /// legitimate idempotent retry (same `request_id`, fresh `jti`) is
    /// admitted while a true replay (same tuple) is rejected. The
    /// envelope-less [`Self::verify`] keeps the v1.7 contract for
    /// callers that don't propagate idempotency keys.
    ///
    /// # Errors
    ///
    /// Same as [`Self::verify`].
    pub fn verify_with_request_id(
        &self,
        token: &str,
        envelope: EnvelopeContext<'_>,
        request_id: Option<&str>,
    ) -> Result<VerifiedActor, RejectReason> {
        let actor = self.verify_no_replay(token, envelope)?;
        // Replay cache — last check so that bad sig / claims rejections
        // don't pollute the cache (otherwise an attacker could flood it).
        if !self.inner.replay.insert_keyed(&actor.jti, request_id) {
            return Err(RejectReason::Invalid(InvalidReason::Replay));
        }
        Ok(actor)
    }

    /// Phase 1.16 — perform every check in [`Self::verify_with_request_id`]
    /// **except** the [`JtiReplayCache`] insert. Returns the
    /// [`VerifiedActor`] (with the resolved `jti`) so the caller can
    /// make its own replay decision.
    ///
    /// Used by [`crate::dispatcher::RpcDispatcher`] when it has been
    /// configured with an [`crate::response_cache::RpcResponseCache`]
    /// so that the dispatcher can serve the cached response for a
    /// legitimate retry instead of rejecting it as a replay. The
    /// dispatcher still consults the replay cache as a safety net for
    /// the (rare) case where a response was evicted before its retry
    /// arrived.
    ///
    /// Callers that do NOT operate a response cache should keep
    /// using [`Self::verify_with_request_id`] — bypassing the replay
    /// check without compensating dedup machinery would re-open the
    /// replay-attack surface the cache exists to close.
    ///
    /// # Errors
    ///
    /// Same as [`Self::verify_with_request_id`] minus
    /// [`InvalidReason::Replay`] (the caller must guard against that
    /// case on its own).
    pub fn verify_no_replay(
        &self,
        token: &str,
        envelope: EnvelopeContext<'_>,
    ) -> Result<VerifiedActor, RejectReason> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(RejectReason::Invalid(InvalidReason::MalformedJws));
        }
        let header_b64 = parts[0];
        let claims_b64 = parts[1];
        let sig_b64 = parts[2];

        let header_bytes = URL_SAFE_NO_PAD
            .decode(header_b64)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedJws))?;
        let claims_bytes = URL_SAFE_NO_PAD
            .decode(claims_b64)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedJws))?;
        let sig_bytes = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedJws))?;

        let header: JwsHeader = serde_json::from_slice(&header_bytes)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedJws))?;
        if header.alg != "EdDSA" {
            return Err(RejectReason::Invalid(InvalidReason::UnsupportedAlg));
        }

        // Resolve the verifying key by `kid`. If `kid` is absent and the
        // verifier was configured with exactly one trusted key AND
        // `allow_kidless_when_single` was set, fall back to that key.
        let verifying_key = match header.kid.as_deref() {
            Some(kid) => self
                .inner
                .trusted_keys
                .get(kid)
                .ok_or(RejectReason::Invalid(InvalidReason::UnknownKeyId))?,
            None if self.inner.allow_kidless_when_single && self.inner.trusted_keys.len() == 1 => {
                self.inner
                    .trusted_keys
                    .values()
                    .next()
                    .expect("len == 1 above")
            }
            None => {
                return Err(RejectReason::Invalid(InvalidReason::UnknownKeyId));
            }
        };

        // The signature covers `<header_b64>.<claims_b64>` ASCII bytes.
        let signing_input = format!("{header_b64}.{claims_b64}");
        if sig_bytes.len() != Signature::BYTE_SIZE {
            return Err(RejectReason::Invalid(InvalidReason::BadSignature));
        }
        let mut sig_arr = [0_u8; Signature::BYTE_SIZE];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);
        verifying_key
            .verify(signing_input.as_bytes(), &signature)
            .map_err(|_| RejectReason::Invalid(InvalidReason::BadSignature))?;

        // Signature OK — parse claims.
        let claims: ActorTokenClaims = serde_json::from_slice(&claims_bytes)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedClaims))?;

        if claims.aud != EXPECTED_AUDIENCE {
            return Err(RejectReason::Invalid(InvalidReason::WrongAudience));
        }
        if claims.core_id != self.inner.core_id {
            return Err(RejectReason::Invalid(InvalidReason::WrongCoreId));
        }
        if claims.http_method != envelope.method {
            return Err(RejectReason::Invalid(InvalidReason::MethodMismatch));
        }
        if claims.path != envelope.path {
            return Err(RejectReason::Invalid(InvalidReason::PathMismatch));
        }

        let now = Utc::now().timestamp();
        // `iat` and `exp` are `u64` in the schema; widen carefully.
        let iat_i = i64::try_from(claims.iat)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedClaims))?;
        let exp_i = i64::try_from(claims.exp)
            .map_err(|_| RejectReason::Invalid(InvalidReason::MalformedClaims))?;
        if iat_i > now + CLOCK_SKEW_SECS {
            return Err(RejectReason::Invalid(InvalidReason::NotYetValid));
        }
        if exp_i <= now - CLOCK_SKEW_SECS {
            return Err(RejectReason::Invalid(InvalidReason::Expired));
        }

        Ok(VerifiedActor {
            sub: claims.sub,
            role: claims.role,
            jti: claims.jti,
            org_id: claims.org_id,
        })
    }
}

/// Builder so dev-mode callers can opt into `allow_kidless_when_single`
/// without changing the production [`Verifier::new`] signature.
#[derive(Debug)]
pub struct VerifierBuilder {
    core_id: String,
    trusted_keys: Vec<TrustedKey>,
    replay: Option<Arc<JtiReplayCache>>,
    allow_kidless_when_single: bool,
}

impl VerifierBuilder {
    /// Start a builder with the engine's core id.
    #[must_use]
    pub fn new(core_id: impl Into<String>) -> Self {
        Self {
            core_id: core_id.into(),
            trusted_keys: Vec::new(),
            replay: None,
            allow_kidless_when_single: false,
        }
    }

    /// Add a trusted Ed25519 key. Multiple calls accumulate.
    #[must_use]
    pub fn trusted_key(mut self, key: TrustedKey) -> Self {
        self.trusted_keys.push(key);
        self
    }

    /// Override the default `JtiReplayCache::new()` with a shared one
    /// (useful when several verifier instances need a single cache).
    #[must_use]
    pub fn replay(mut self, cache: Arc<JtiReplayCache>) -> Self {
        self.replay = Some(cache);
        self
    }

    /// Allow tokens that omit `kid` if exactly one trusted key exists.
    /// Off by default — keep on only for local dev.
    #[must_use]
    pub fn allow_kidless_when_single(mut self) -> Self {
        self.allow_kidless_when_single = true;
        self
    }

    /// Build the verifier. Returns `None` if no trusted key was added.
    #[must_use]
    pub fn build(self) -> Option<Verifier> {
        if self.trusted_keys.is_empty() {
            return None;
        }
        let map = self
            .trusted_keys
            .into_iter()
            .map(|tk| (tk.kid, tk.key))
            .collect();
        let replay = self
            .replay
            .unwrap_or_else(|| Arc::new(JtiReplayCache::new()));
        Some(Verifier {
            inner: Arc::new(VerifierInner {
                core_id: self.core_id,
                trusted_keys: map,
                replay,
                allow_kidless_when_single: self.allow_kidless_when_single,
            }),
        })
    }
}

#[derive(Debug, Deserialize)]
struct JwsHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}
