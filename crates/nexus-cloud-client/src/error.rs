//! Error and reject-reason types shared by the dispatcher and verifier.
//!
//! The wire-protocol error codes (`actor_token_missing`,
//! `actor_token_invalid`) defined by Phase 1.1 of the cloud-console
//! [`WIRE_PROTOCOL.md`] are surfaced verbatim through
//! [`DispatchError::wire_code`] so the engine can stamp them into the
//! `rpc_response` body without re-encoding.

use thiserror::Error;

/// Wire-level reject reasons mirroring `WIRE_PROTOCOL.md §4.3`.
///
/// Both `actor_token_missing` and `actor_token_invalid` are sent back in
/// the `RpcResponsePayload.body.error` field with HTTP-equivalent
/// `status = 401`. We deliberately collapse all post-parse rejections
/// (bad sig, wrong aud, wrong core_id, expired, replayed jti, mismatched
/// path, system-sub on non-whitelisted method) into a single
/// `actor_token_invalid` code so attackers can't probe individual claim
/// checks via timing or error-string differentiation. The granular
/// [`InvalidReason`] is still returned in-process so the engine can log
/// the specific failure into its local audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RejectReason {
    /// `rpc_call.payload.actor_token` was absent on a state-mutating method.
    #[error("actor_token_missing")]
    Missing,
    /// `actor_token` failed verification — see [`InvalidReason`] for the
    /// specific check that failed. Wire-code is always
    /// `actor_token_invalid`.
    #[error("actor_token_invalid")]
    Invalid(InvalidReason),
}

impl RejectReason {
    /// String code stamped into `rpc_response.payload.body.error`.
    #[must_use]
    pub const fn wire_code(self) -> &'static str {
        match self {
            Self::Missing => "actor_token_missing",
            Self::Invalid(_) => "actor_token_invalid",
        }
    }
}

/// Granular reason a token failed verification. Surfaced to the engine
/// audit log; never sent on the wire (see [`RejectReason`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    /// Compact JWS was not `<header>.<payload>.<sig>` or one of the
    /// segments was not valid base64url.
    MalformedJws,
    /// Header JSON did not parse or `alg` was not `EdDSA`.
    UnsupportedAlg,
    /// Claims JSON did not parse against
    /// [`nexus_cloud_protocol::v1::ActorTokenClaims`].
    MalformedClaims,
    /// Ed25519 signature did not verify against the bundled trusted key.
    BadSignature,
    /// `kid` in the JWS header did not match any trusted key (or the
    /// header omitted `kid` and the verifier was configured with more
    /// than one trusted key so no implicit selection is safe).
    UnknownKeyId,
    /// `aud` claim was not the engine-bound `nexus-edge-rpc`.
    WrongAudience,
    /// `core_id` claim did not match this engine's core id.
    WrongCoreId,
    /// `http_method` claim did not match the envelope `rpc_call.method`.
    MethodMismatch,
    /// `path` claim did not match the envelope `rpc_call.path`.
    PathMismatch,
    /// Signed permission does not authorize the actual action or body target.
    PermissionMismatch,
    /// Token expired more than the 30 s skew window ago.
    Expired,
    /// Token's `iat` is more than 30 s in the future (forged ahead of
    /// time, or the edge's clock is badly behind — Phase 1.15 surfaces
    /// the latter as `cores.last_skew_ms > 30_000` and the SPA shows the
    /// remediation banner).
    NotYetValid,
    /// `jti` was seen in the replay cache.
    Replay,
    /// `sub` started with `system:` but the requested method was not in
    /// the configured allow-list.
    SystemSubNotPermittedForMethod,
}

/// All ways the dispatcher can refuse to invoke a handler. The variants
/// map 1:1 to wire-protocol error codes via [`Self::wire_code`].
#[derive(Debug, Clone, Error)]
pub enum DispatchError {
    /// Token-related rejection. The wire body MUST carry [`Self::wire_code`].
    #[error("{0}")]
    Reject(#[from] RejectReason),
    /// Handler itself returned an error after the token was accepted.
    /// The caller decides the status code; the verifier just ferries it.
    #[error("handler error: {0}")]
    Handler(String),
}

impl DispatchError {
    /// Convenience wire-code accessor. Returns `internal_error` for the
    /// `Handler` arm because handler-side failures are not Phase 1.7's
    /// concern — those map to whatever the engine handler chooses.
    #[must_use]
    pub fn wire_code(&self) -> &'static str {
        match self {
            Self::Reject(r) => r.wire_code(),
            Self::Handler(_) => "internal_error",
        }
    }
}
