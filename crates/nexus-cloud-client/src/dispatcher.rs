//! RPC dispatcher — verifies `actor_token` then invokes a handler.
//!
//! Phase 1.7 ships the dispatcher contract; the actual engine handler
//! wiring (cameras CRUD, OTA apply, etc.) happens in Phase 1.11 once
//! the tunnel is connected. The dispatcher is intentionally generic
//! over the handler type so unit tests can substitute a mock.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody, RpcCallPayload, RpcResponsePayload};

use crate::actor_token::{EnvelopeContext, VerifiedActor, Verifier};
use crate::error::{DispatchError, InvalidReason, RejectReason};
use crate::response_cache::RpcResponseCache;

/// Logical operation name extracted by the engine from
/// `rpc_call.payload.path`. Phase 1.7 keeps the contract simple — the
/// caller hands the dispatcher a `&str`; how it's derived from the
/// HTTP path is the engine's concern.
pub type MethodName<'a> = &'a str;

/// Policy controlling which methods accept a `system:`-sub token.
///
/// The Phase 1.7 acceptance criteria enumerate one positive case
/// (`entitlement_update` accepted) and one negative case (`create_camera`
/// rejected). [`Self::default`] ships the minimum-viable list; the
/// engine extends it as system services are added in later phases
/// (e.g. `notify` for the alert pipeline, `ota_apply` for the OTA worker
/// in Phase 7).
#[derive(Debug, Clone)]
pub struct SystemMethodPolicy {
    permitted: HashSet<String>,
}

impl SystemMethodPolicy {
    /// Build an empty policy — every `system:`-sub token is rejected.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            permitted: HashSet::new(),
        }
    }

    /// Build a policy that admits the named methods.
    #[must_use]
    pub fn permitting<I, S>(methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            permitted: methods.into_iter().map(Into::into).collect(),
        }
    }

    /// Add a method to the allow-list.
    pub fn permit(&mut self, method: impl Into<String>) {
        self.permitted.insert(method.into());
    }

    /// Returns `true` if a `system:`-sub token may invoke `method`.
    #[must_use]
    pub fn allows(&self, method: &str) -> bool {
        self.permitted.contains(method)
    }
}

impl Default for SystemMethodPolicy {
    /// Phase 1.7 default — admits exactly `entitlement_update`. Engine
    /// callers MUST extend this for each subsequent system integration.
    fn default() -> Self {
        Self::permitting(["entitlement_update"])
    }
}

/// Handler trait the engine implements once per logical method. The
/// dispatcher invokes it only after the `actor_token` has been verified.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Apply the verified RPC. The `actor` is recorded in the engine
    /// audit log by the dispatcher before this is called, so handlers
    /// don't need to re-log.
    async fn handle(
        &self,
        method: &str,
        envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String>;
}

/// Function adapter so simple handlers can skip implementing the trait.
/// Reserved for tests — production code wires a `Handler` impl.
pub struct FnHandler<F>(pub F);

#[async_trait]
impl<F, Fut> Handler for FnHandler<F>
where
    F: Send + Sync + Fn(String, VerifiedActor, Option<Vec<u8>>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>> + Send,
{
    async fn handle(
        &self,
        method: &str,
        _envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        (self.0)(method.to_string(), actor.clone(), body.map(Vec::from)).await
    }
}

/// Audit hook the engine implements to write `(sub, role, jti)` plus
/// method/path to its local audit log BEFORE the handler is invoked.
/// Phase 1.7 leaves the storage backend to the engine — this trait is
/// the only contract the dispatcher needs.
///
/// The trait is **async** because every realistic backend (SQLite via
/// `sqlx`, Kafka, etc.) is async. Implementations that need no I/O
/// (e.g. [`NullAuditSink`]) just return immediately.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Record one successful authorisation. The dispatcher awaits this
    /// after `verify` succeeds and BEFORE invoking the handler so a
    /// crash mid-handler still leaves an audit trail. Failures should
    /// be logged by the sink itself; the trait returns `()` so an audit
    /// outage never blocks the dispatch path.
    async fn record(&self, method: &str, envelope: EnvelopeContext<'_>, actor: &VerifiedActor);
}

/// No-op audit sink — useful for tests and the Phase 1.7 scaffold.
/// Engine integrators MUST replace this with a sink that writes to
/// the local SQLite audit table.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullAuditSink;

#[async_trait]
impl AuditSink for NullAuditSink {
    async fn record(&self, _method: &str, _envelope: EnvelopeContext<'_>, _actor: &VerifiedActor) {}
}

/// Top-level dispatcher. Holds the verifier, the system-sub policy, the
/// audit sink, and a handler.
pub struct RpcDispatcher<H: Handler> {
    verifier: Verifier,
    policy: SystemMethodPolicy,
    audit: Arc<dyn AuditSink>,
    handler: H,
    /// Phase 1.16: optional response-replay cache. When configured
    /// (and the inbound envelope carries a `request_id`), the
    /// dispatcher serves the cached `rpc_response` body byte-identically
    /// instead of running the handler a second time — closing the
    /// engine half of the 1.16 acceptance criterion. See
    /// [`RpcResponseCache`] for the storage contract.
    response_cache: Option<Arc<RpcResponseCache>>,
}

impl<H: Handler> RpcDispatcher<H> {
    /// Build a dispatcher. The handler is moved in; share via
    /// `Arc<DispatchedHandler>` if multiple call sites need access.
    pub fn new(verifier: Verifier, policy: SystemMethodPolicy, handler: H) -> Self {
        Self {
            verifier,
            policy,
            audit: Arc::new(NullAuditSink),
            handler,
            response_cache: None,
        }
    }

    /// Override the audit sink (default is [`NullAuditSink`]).
    #[must_use]
    pub fn with_audit_sink(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Phase 1.16: install a response-replay cache. When set, the
    /// dispatcher replays the cached `rpc_response` byte-identically
    /// for any retry that carries the same `(jti, request_id)`.
    /// Default is no cache (preserves the v1.7 contract of rejecting
    /// replays with [`InvalidReason::Replay`]).
    #[must_use]
    pub fn with_response_cache(mut self, cache: Arc<RpcResponseCache>) -> Self {
        self.response_cache = Some(cache);
        self
    }

    /// Reference to the configured verifier — exposed so the engine
    /// can pass the same JTI cache to other components.
    #[must_use]
    pub const fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    /// Top-level entry point. The engine calls this for every inbound
    /// state-mutating `rpc_call`.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::Reject`] if the token is missing or
    /// invalid (the wire response will carry `actor_token_missing` /
    /// `actor_token_invalid` accordingly), or [`DispatchError::Handler`]
    /// if the handler itself errored after a successful verification.
    pub async fn dispatch(
        &self,
        method: &str,
        envelope: EnvelopeContext<'_>,
        token: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, DispatchError> {
        self.dispatch_inner(method, envelope, token, None, body)
            .await
    }

    /// Convenience wrapper that pulls `method`, `path`, `token`,
    /// `body`, and `request_id` straight off a wire-protocol
    /// [`Envelope`]. Phase 1.16: the verifier's replay cache is keyed
    /// by `(jti, request_id)` when `payload.request_id` is present so a
    /// retry that carries the same idempotency key (per
    /// [WIRE_PROTOCOL.md §3.9](../../../../nexus-cloud-console/docs/WIRE_PROTOCOL.md#39))
    /// is dedup-recognised even if it also rebuilds a fresh `actor_token`.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::Reject(RejectReason::Invalid(InvalidReason::MalformedClaims))`]
    /// if the envelope is not an `rpc_call` (defensive — the engine
    /// should never call this on anything else; the variant rides on
    /// the existing `actor_token_invalid` wire code so we don't widen
    /// the public surface for an internal programming error).
    pub async fn dispatch_envelope(
        &self,
        envelope: &Envelope,
    ) -> Result<RpcResponsePayload, DispatchError> {
        let payload = match &envelope.body {
            EnvelopeBody::RpcCall(p) => p,
            _ => {
                return Err(DispatchError::Reject(RejectReason::Invalid(
                    InvalidReason::MalformedClaims,
                )));
            }
        };
        let env = EnvelopeContext {
            method: &payload.method,
            path: &payload.path,
        };
        let body_bytes = decode_payload_body(payload)?;
        let method_name = derive_method_name(payload);
        let token = payload.actor_token.as_deref();
        let request_id = payload.request_id.as_deref();
        let result = self
            .dispatch_inner(method_name, env, token, request_id, body_bytes.as_deref())
            .await;
        match result {
            Ok(body) => {
                let body_json = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::String(
                        String::from_utf8_lossy(&body).into_owned(),
                    ))
                };
                Ok(RpcResponsePayload {
                    body: body_json,
                    status: 200,
                })
            }
            Err(err) => Err(err),
        }
    }

    async fn dispatch_inner(
        &self,
        method: &str,
        envelope: EnvelopeContext<'_>,
        token: Option<&str>,
        request_id: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, DispatchError> {
        let token = match token {
            Some(t) => t,
            None => {
                // ARCHITECTURE.md §5.7: read-only RPCs MAY omit
                // `actor_token` for v1; the engine treats them as
                // anonymous reads served from the loopback admin API.
                // Mutating methods without a token continue to reject
                // with `actor_token_missing`.
                if is_safe_http_method(envelope.method) {
                    let actor = anonymous_read_actor();
                    return self
                        .handler
                        .handle(method, envelope, &actor, body)
                        .await
                        .map_err(DispatchError::Handler);
                }
                return Err(DispatchError::Reject(RejectReason::Missing));
            }
        };

        // Phase 1.16: the response-cache-equipped path verifies WITHOUT
        // touching the [`crate::jti_cache::JtiReplayCache`] so a
        // legitimate retry (same `request_id`, possibly fresh `jti`)
        // can be served the byte-identical cached body instead of
        // being rejected as a replay. The replay cache is still
        // consulted as a safety net AFTER the response-cache miss
        // path, so a retry whose response was evicted before it
        // arrived is still rejected rather than silently re-run.
        if let (Some(cache), Some(rid)) = (self.response_cache.as_ref(), request_id) {
            let actor = self.verifier.verify_no_replay(token, envelope)?;
            if let Some(cached) = cache.get(&actor.jti, rid) {
                // Byte-identical replay — bypass handler, audit, and
                // system-sub gate. The original call already ran them.
                return Ok(cached);
            }
            // Cache miss. Admit-or-reject via the replay cache: if
            // the tuple was already admitted (i.e. response evicted
            // before retry arrived), refuse rather than re-run.
            if !self
                .verifier
                .replay_cache()
                .insert_keyed(&actor.jti, Some(rid))
            {
                return Err(DispatchError::Reject(RejectReason::Invalid(
                    InvalidReason::Replay,
                )));
            }
            self.check_system_sub_and_audit(method, envelope, &actor)
                .await?;
            let body_out = self
                .handler
                .handle(method, envelope, &actor, body)
                .await
                .map_err(DispatchError::Handler)?;
            cache.insert(&actor.jti, rid, body_out.clone());
            return Ok(body_out);
        }

        // v1.7 path — no response cache OR no request_id. The
        // verifier's `verify_with_request_id` handles dedup-and-reject
        // for us (request_id may be `None`).
        let actor = self
            .verifier
            .verify_with_request_id(token, envelope, request_id)?;
        self.check_system_sub_and_audit(method, envelope, &actor)
            .await?;
        self.handler
            .handle(method, envelope, &actor, body)
            .await
            .map_err(DispatchError::Handler)
    }

    /// System-sub gate + audit hook. Extracted so both the v1.7 and
    /// the Phase 1.16 response-cache paths run the same pre-handler
    /// checks in the same order.
    async fn check_system_sub_and_audit(
        &self,
        method: &str,
        envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
    ) -> Result<(), DispatchError> {
        // System-sub gate. We check AFTER signature verification so the
        // policy decision is bound to a cryptographically authenticated
        // `sub` — never to an attacker-supplied claim.
        if actor.sub.starts_with("system:") && !self.policy.allows(method) {
            return Err(DispatchError::Reject(RejectReason::Invalid(
                InvalidReason::SystemSubNotPermittedForMethod,
            )));
        }
        // Audit BEFORE dispatch — Phase 1.7 acceptance bullet.
        self.audit.record(method, envelope, actor).await;
        Ok(())
    }
}

/// Convert `RpcCallPayload.body` (optional base64) to raw bytes.
fn decode_payload_body(payload: &RpcCallPayload) -> Result<Option<Vec<u8>>, DispatchError> {
    let Some(b64) = payload.body.as_deref() else {
        return Ok(None);
    };
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD
        .decode(b64)
        .map(Some)
        .map_err(|_| DispatchError::Reject(RejectReason::Invalid(InvalidReason::MalformedClaims)))
}

/// Phase 1.7 derives the dispatcher method name from the HTTP path.
/// The engine may replace this with a richer routing layer that
/// matches against its admin-API tree; for now we use the last path
/// segment so `POST /admin/v1/cameras` becomes `cameras` for the
/// system-sub method whitelist.
fn derive_method_name(payload: &RpcCallPayload) -> &str {
    payload
        .path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("_root")
}

/// `sub` claim stamped onto the synthesised actor for token-less
/// read-only RPCs (ARCHITECTURE.md §5.7). Engine handlers that want
/// to special-case the anonymous-read path can match on this value.
pub const ANONYMOUS_READ_SUB: &str = "anonymous:cloud-read";

/// `role` claim stamped onto the synthesised actor for token-less
/// read-only RPCs. Sits below `viewer` in the engine's role lattice
/// so it cannot accidentally satisfy any privileged-role gate.
pub const ANONYMOUS_READ_ROLE: &str = "anonymous";

fn is_safe_http_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS"
    )
}

fn anonymous_read_actor() -> VerifiedActor {
    VerifiedActor {
        sub: ANONYMOUS_READ_SUB.to_string(),
        role: ANONYMOUS_READ_ROLE.to_string(),
        jti: String::new(),
        org_id: String::new(),
    }
}
