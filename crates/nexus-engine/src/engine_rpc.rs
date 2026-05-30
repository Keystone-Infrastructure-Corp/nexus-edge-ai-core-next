//! Engine-side `rpc_call` handler — Phase 2 Step 2.1c.
//!
//! Routes inbound cloud-initiated mutating RPCs (verified by
//! [`nexus_cloud_client::RpcDispatcher`]) onto local engine actions.
//! The first method shipped is the Expedite endpoint paired with the
//! cloud `POST /v1/orgs/.../clips/.../expedite` button:
//!
//! ```text
//! POST /admin/clips/{edge_clip_id}/replicate
//! ```
//!
//! On success the handler bumps the matching `motion_clips.priority`
//! from 0 → 1 (idempotent), pokes the cold-replicator's `Notify` so
//! the next tick is immediate, and returns `{"queue_position": N}`.
//! Errors are encoded as JSON in the `Result<_, String>` error arm of
//! the [`Handler`] trait and translated back into the wire
//! [`RpcResponsePayload.status`] field by [`engine_rpc_response`].
//!
//! ## Status-code encoding
//!
//! The [`Handler`] trait return type is `Result<Vec<u8>, String>`,
//! which natively only expresses "ran OK with body" vs "internal
//! error". The engine needs to distinguish 404 (unknown clip), 409
//! (already replicated), and 400 (invalid args) too — so we encode
//! the desired HTTP status inside the `Err(String)` channel as a
//! JSON object `{"status":n,"error":code,"message":msg}` and
//! [`engine_rpc_response`] parses it back out when assembling the
//! `rpc_response` envelope. Bodies that fail to parse fall back to
//! HTTP 500 (`internal_error`) so a buggy handler can't mask itself
//! as a 200.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use nexus_cloud_client::{
    AuditSink, DispatchError, EnvelopeContext, Handler, RejectReason, RpcDispatcher, VerifiedActor,
};
use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody, RpcResponsePayload};
use nexus_store::{AuditActorKind, AuditOutcome, NewAuditEntry, Store};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{debug, warn};

/// Engine-side RPC handler. Owns the `Store` (for clip lookups
/// and audit), a `Notify` shared with the cold replicator (so the
/// Expedite fast-path can wake it immediately), and an HTTP client
/// pointed at the engine's own loopback admin API so every other
/// `/admin/*` envelope is forwarded verbatim to the local API
/// (Phase A Step 5 — see `handle_admin_passthrough`).
///
/// The passthrough relies on the `admin_auth_layer` loopback-bypass
/// rule (`admin_auth.rs` decision matrix item 3): connecting from
/// `127.0.0.1` is implicitly trusted, so we don't have to mint an
/// admin bearer token for each forwarded call. The dispatcher has
/// already verified the cloud-issued `actor_token` and the audit
/// sink has already recorded the cloud actor BEFORE this handler
/// runs, so the audit chain is preserved end-to-end.
pub struct EngineRpcHandler {
    pub store: Arc<Store>,
    pub replicator_kick: Arc<Notify>,
    /// `http://127.0.0.1:<port>` — derived from the engine's
    /// effective `server.api_bind`. Wrapped in `ArcSwap` so
    /// `main.rs` can update it after the boot-time runtime-setting
    /// override resolves (the supervisor that owns this handler
    /// runs before the listener actually binds).
    pub loopback_admin_base: Arc<ArcSwap<String>>,
    pub http_client: reqwest::Client,
}

/// Granular handler error. Each variant maps to an HTTP status code
/// in [`Self::status`] and a wire `error` code in [`Self::code`].
/// Variants intentionally carry a `String` body — the caller stamps
/// it into the wire `message` field so the cloud handler / operator
/// has something specific to surface.
#[derive(Debug)]
pub enum EngineRpcError {
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Internal(String),
}

impl EngineRpcError {
    pub const fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::BadRequest(_) => 400,
            Self::Internal(_) => 500,
        }
    }

    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::BadRequest(_) => "bad_request",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::BadRequest(m) | Self::Internal(m) => m,
        }
    }

    /// Encode the error as a JSON envelope the dispatcher round-trips
    /// through the `Result::Err` arm of [`Handler::handle`].
    pub fn into_wire_json(self) -> String {
        let wire = HandlerErrorWire {
            status: self.status(),
            error: self.code(),
            message: self.message().to_string(),
        };
        // `serde_json::to_string` on the closed `HandlerErrorWire`
        // shape never fails — `unwrap_or_else` keeps the
        // engine on the fail-open path even if it somehow did.
        serde_json::to_string(&wire).unwrap_or_else(|_| {
            r#"{"status":500,"error":"internal_error","message":"serde failure"}"#.to_string()
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HandlerErrorWire {
    status: u16,
    error: &'static str,
    #[serde(default)]
    message: String,
}

// Deserialize side uses a separate owned variant so we don't fight
// the `'static` lifetime on `error`.
#[derive(Debug, Deserialize)]
struct HandlerErrorWireOwned {
    status: u16,
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
}

#[async_trait]
impl Handler for EngineRpcHandler {
    async fn handle(
        &self,
        _method: &str,
        envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        // Fast-path: the Expedite endpoint is implemented in-process
        // because it bumps a SQL priority + kicks the cold-replicator
        // `Notify` directly — going through the local admin API would
        // add an HTTP round-trip and a duplicate audit row for no
        // gain.
        if envelope.method.eq_ignore_ascii_case("POST") && is_expedite_path(envelope.path) {
            return self
                .handle_expedite(envelope.path, actor, body)
                .await
                .map_err(EngineRpcError::into_wire_json);
        }
        // Phase A Step 5: every other `/admin/*` path is forwarded
        // verbatim to the engine's own local admin API. This unblocks
        // the full cloud-side administration & troubleshooting
        // surface (cameras CRUD, discovery, telemetry, network,
        // cloud enrollment, …) without hand-coding a matching arm
        // per endpoint on the engine side. See
        // `handle_admin_passthrough` for the wire-shape contract.
        if envelope.path.starts_with("/admin/") {
            return self.handle_admin_passthrough(envelope, actor, body).await;
        }
        Err(EngineRpcError::NotFound(format!(
            "no handler for {} {}",
            envelope.method, envelope.path
        ))
        .into_wire_json())
    }
}

impl EngineRpcHandler {
    async fn handle_expedite(
        &self,
        path: &str,
        actor: &VerifiedActor,
        _body: Option<&[u8]>,
    ) -> Result<Vec<u8>, EngineRpcError> {
        // Role gate — owner/admin/operator only. We return NotFound
        // (rather than Forbidden) for any other role to avoid
        // leaking the existence of the endpoint to viewers.
        if !is_priviledged_role(&actor.role) {
            return Err(EngineRpcError::NotFound(
                "no handler for this path".to_string(),
            ));
        }

        let clip_id = parse_expedite_clip_id(path).ok_or_else(|| {
            EngineRpcError::BadRequest(format!("could not parse clip id from path {path:?}"))
        })?;

        let row = self
            .store
            .get_clip(clip_id)
            .await
            .map_err(|e| EngineRpcError::Internal(format!("get_clip: {e}")))?;

        let row =
            row.ok_or_else(|| EngineRpcError::NotFound(format!("clip {clip_id} not found")))?;

        if row.cold_handle.is_some() {
            return Err(EngineRpcError::Conflict(format!(
                "clip {clip_id} already replicated"
            )));
        }
        if row.ended_at.is_none() {
            return Err(EngineRpcError::Conflict(format!(
                "clip {clip_id} is still recording"
            )));
        }
        if row.sha256.is_none() {
            return Err(EngineRpcError::Conflict(format!(
                "clip {clip_id} has no integrity hash yet"
            )));
        }

        // Idempotent bump. `bump_clip_priority` returns false when
        // the row's priority is already >= the new value.
        let bumped = self
            .store
            .bump_clip_priority(clip_id, 1)
            .await
            .map_err(|e| EngineRpcError::Internal(format!("bump_clip_priority: {e}")))?;

        // Wake the cold replicator either way — even if the priority
        // was already 1 (e.g. the operator clicked twice), the
        // operator clearly wants this clip out now.
        self.replicator_kick.notify_one();

        let position = self
            .store
            .pending_cold_upload_position(clip_id)
            .await
            .map_err(|e| EngineRpcError::Internal(format!("pending_cold_upload_position: {e}")))?
            .unwrap_or(1);

        debug!(
            clip_id = clip_id,
            bumped = bumped,
            queue_position = position,
            actor_sub = %actor.sub,
            actor_role = %actor.role,
            "expedite_clip handled",
        );

        let body = json!({ "queue_position": position });
        Ok(serde_json::to_vec(&body).unwrap_or_default())
    }

    /// Forward an arbitrary `/admin/...` envelope to the engine's
    /// own loopback admin API. This is Phase A Step 5's generic
    /// proxy that replaces per-endpoint hand-routed arms.
    ///
    /// Wire contract:
    /// * Method: any of `GET`/`POST`/`PUT`/`PATCH`/`DELETE`.
    /// * Path: the cloud sends the `/admin/X` shorthand; the
    ///   engine's local admin API actually serves at
    ///   `/api/v1/admin/X` (because `api.rs::router` nests the
    ///   whole API under `/api`), so we prepend `/api/v1` before
    ///   firing the HTTP call.
    /// * Body: forwarded verbatim with `Content-Type:
    ///   application/json` (the cloud's outbound `forward_envelope`
    ///   only ever sends JSON bodies — see
    ///   `services/api-gateway/src/outbound.rs`).
    /// * Response: on `2xx` we return the body bytes as-is (the
    ///   dispatcher stamps `RpcResponsePayload::status = 200`,
    ///   smoothing `201/204` to `200`; the body content the cloud
    ///   actually consumes is preserved verbatim). On non-2xx we
    ///   round-trip the local API's `(status, error, message)`
    ///   through the existing `HandlerErrorWire` JSON envelope so
    ///   `parse_handler_error` reconstructs the right wire status
    ///   on the cloud side.
    ///
    /// Auth model: the loopback peer is implicitly trusted by
    /// `admin_auth_layer` (decision matrix item 3 in
    /// `admin_auth.rs`), so we don't have to mint an admin bearer
    /// token per call. The dispatcher has already verified the
    /// cloud-issued `actor_token` upstream; the audit sink has
    /// already recorded the cloud actor BEFORE this handler runs.
    /// The downstream `audit_admin_action` call inside the local
    /// admin handler records a sibling row with `actor_kind =
    /// system` (label = `loopback`), so an external auditor sees
    /// both: the cloud-side row identifying the human, and the
    /// engine-side row identifying the local-API mutation. Hard
    /// Rule 6 (R4c — every mutating cloud→edge RPC carries a
    /// verified `actor_token`) is satisfied by the upstream
    /// dispatcher check; we re-enforce a `priviledged_role` gate
    /// here as defence-in-depth.
    ///
    /// Non-JSON responses (e.g. live-frame JPEGs from
    /// `/cameras/:id/frames/latest`) MUST NOT travel through the
    /// `rpc_call` tunnel per Hard Rule 7 (SAS URLs for media). The
    /// dispatcher's `dispatch_envelope` falls back to a
    /// `String(utf8_lossy)` body when our return value doesn't
    /// parse as JSON, so binary responses do technically pass —
    /// but the cloud-side gateway will surface a 502 / "non_json_
    /// response" envelope to the caller and the UI is expected to
    /// use a separate media-relay path for those routes (TBD).
    async fn handle_admin_passthrough(
        &self,
        envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        // Defence-in-depth role gate. The cloud side already gates
        // owner/admin on mutating proxies and owner/admin/viewer on
        // GETs, but a buggy cloud-side ACL change MUST NOT silently
        // open the surface to viewers. NotFound (not Forbidden) so
        // we don't leak the existence of the surface to lower-role
        // tokens.
        if !is_priviledged_role(&actor.role) {
            return Err(
                EngineRpcError::NotFound("no handler for this path".to_string()).into_wire_json(),
            );
        }

        let base = self.loopback_admin_base.load();
        let result = forward_admin_request(
            &self.http_client,
            base.as_str(),
            envelope.method,
            envelope.path,
            body,
        )
        .await;

        match &result {
            Ok(bytes) => debug!(
                method = %envelope.method,
                path = %envelope.path,
                body_len = bytes.len(),
                actor_sub = %actor.sub,
                actor_role = %actor.role,
                "admin passthrough succeeded",
            ),
            Err(wire) => debug!(
                method = %envelope.method,
                path = %envelope.path,
                actor_sub = %actor.sub,
                actor_role = %actor.role,
                wire = %wire,
                "admin passthrough returned non-success",
            ),
        }

        result
    }
}

/// HTTP forwarding core for the cloud-tunnel admin passthrough.
/// Split out as a free function so the unit tests can exercise the
/// reqwest call against a stub axum server without standing up a
/// full `Store` + `Notify` + dispatcher.
///
/// Returns `Ok(body_bytes)` for any 2xx response (body forwarded
/// verbatim), or `Err(wire_json)` where `wire_json` is the
/// `HandlerErrorWire` envelope `parse_handler_error` round-trips
/// back into a `RpcResponsePayload` with the right status. The
/// caller layers a role gate on top — see
/// `EngineRpcHandler::handle_admin_passthrough`.
async fn forward_admin_request(
    http_client: &reqwest::Client,
    loopback_base: &str,
    method_str: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let method = match method_str.to_ascii_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        other => {
            return Err(EngineRpcError::BadRequest(format!(
                "unsupported HTTP method for admin passthrough: {other}"
            ))
            .into_wire_json());
        }
    };

    // Cloud sends the `/admin/X` shorthand; local admin API serves
    // at `/api/v1/admin/X` (api.rs `.nest("/api", api)`).
    let url = format!("{}/api/v1{}", loopback_base.trim_end_matches('/'), path);

    let mut req = http_client.request(method, &url);
    if let Some(b) = body {
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(b.to_vec());
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                method = %method_str,
                path = %path,
                url = %url,
                error = %e,
                "admin passthrough: loopback request failed",
            );
            return Err(
                EngineRpcError::Internal(format!("loopback admin call failed: {e}"))
                    .into_wire_json(),
            );
        }
    };

    let status = resp.status();
    let body_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(
                method = %method_str,
                path = %path,
                error = %e,
                "admin passthrough: reading loopback response body failed",
            );
            return Err(EngineRpcError::Internal(format!(
                "reading loopback admin response body failed: {e}"
            ))
            .into_wire_json());
        }
    };

    if status.is_success() {
        return Ok(body_bytes.to_vec());
    }

    // Non-2xx: round-trip the local API's status + (error,
    // message) so `parse_handler_error` on the cloud side
    // reconstructs the right wire status. If the body is JSON
    // with `error`/`message` fields (the engine API's
    // convention via `ApiError`), reuse them; otherwise fall
    // back to a default code + lossy-utf8 body.
    let (code, message) = match serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        Ok(v) => {
            let code = v
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| default_error_code(status.as_u16()).to_string());
            let msg = v
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            (code, msg)
        }
        Err(_) => (
            default_error_code(status.as_u16()).to_string(),
            String::from_utf8_lossy(&body_bytes).into_owned(),
        ),
    };

    Err(passthrough_error_wire(status.as_u16(), &code, &message))
}

/// Map a non-2xx HTTP status code to a stable wire `error` code
/// for the cloud-side response envelope. Used only when the local
/// admin API response body doesn't already carry an `error` field.
const fn default_error_code(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        429 => "rate_limited",
        s if s >= 500 && s < 600 => "internal_error",
        _ => "edge_error",
    }
}

/// Encode a non-2xx passthrough response as the `HandlerErrorWire`
/// JSON envelope `parse_handler_error` round-trips back into a
/// `RpcResponsePayload`. We can't use `EngineRpcError::into_wire_
/// json` here because the engine-side enum is closed (NotFound /
/// Conflict / BadRequest / Internal) and we need to preserve
/// arbitrary local-API status codes (e.g. 403, 429, 503) verbatim.
fn passthrough_error_wire(status: u16, code: &str, message: &str) -> String {
    json!({
        "status": status,
        "error": code,
        "message": message,
    })
    .to_string()
}

/// Derive the engine's own loopback admin URL (e.g. `http://127.0.0.1:8089`)
/// from the configured / runtime-overridden `server.api_bind` string.
///
/// The cloud-tunnel admin passthrough always connects via `127.0.0.1`
/// regardless of what interface the listener is bound to: the
/// `admin_auth_layer` loopback bypass triggers on the connecting
/// peer's address, not on the listening address, so connecting via
/// loopback keeps the bypass valid even when the operator pinned
/// `api_bind` to a specific non-loopback interface (in which case
/// the passthrough naturally won't reach the listener and the
/// operator is on the hook for documenting that edge configuration).
///
/// Accepts both `host:port` and `IP:port` shapes. Falls back to the
/// engine's default port (8089) if parsing fails — the call sites
/// log a warn separately when this defaults engage.
pub fn loopback_admin_url_from_bind(bind: &str) -> String {
    // Prefer a strict `SocketAddr` parse (covers `0.0.0.0:8089`,
    // `127.0.0.1:8089`, `[::1]:8089`), then a fallback hostname-
    // style split on the last `:` to handle `localhost:8089` etc.
    let port = bind
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|sa| sa.port())
        .or_else(|| bind.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()))
        .unwrap_or(8089);
    format!("http://127.0.0.1:{port}")
}

/// Audit sink that mirrors every cloud-initiated `rpc_call` into the
/// engine's local `audit_log` table.
///
/// Per Phase 1.7 design, the dispatcher calls
/// [`AuditSink::record`] AFTER verification succeeds and BEFORE the
/// handler runs, so a handler crash still leaves an audit trail.
/// Sink errors are logged and swallowed — an audit-store outage MUST
/// NOT block dispatch (Hard Rule 5 / fail-open).
pub struct EngineAuditSink {
    pub store: Arc<Store>,
}

#[async_trait]
impl AuditSink for EngineAuditSink {
    async fn record(&self, method: &str, envelope: EnvelopeContext<'_>, actor: &VerifiedActor) {
        let actor_kind = if actor.sub.starts_with("system:") {
            AuditActorKind::System
        } else {
            AuditActorKind::OidcUser
        };
        let action = format!("cloud_rpc.{method}");
        // For Expedite the cloud's path is
        // `/admin/clips/{edge_clip_id}/replicate`; the resource is
        // the clip itself.
        let (resource_kind, resource_id_owned) = if is_expedite_path(envelope.path) {
            (
                Some("clip"),
                parse_expedite_clip_id(envelope.path).map(|i| i.to_string()),
            )
        } else {
            (None, None)
        };
        let entry = NewAuditEntry {
            actor_kind: Some(actor_kind),
            actor_id: Some(actor.sub.as_str()),
            actor_label: actor.sub.as_str(),
            action: action.as_str(),
            resource_kind,
            resource_id: resource_id_owned.as_deref(),
            before_json: None,
            after_json: None,
            outcome: AuditOutcome::Success,
            ip: None,
            user_agent: Some(envelope.method),
        };
        if let Err(e) = self.store.record_audit_event_standalone(&entry).await {
            warn!(error = %e, "cloud rpc audit write failed; swallowing");
        }
    }
}

/// Run an inbound `rpc_call` envelope through the dispatcher and
/// build the matching `RpcResponsePayload`.
///
/// Maps:
/// - `Ok(payload)` → returned as-is (dispatcher already stamped
///   `status = 200`).
/// - `Err(DispatchError::Reject(_))` → `status = 401`,
///   `body = {"error": wire_code, "message": .. }`.
/// - `Err(DispatchError::Handler(json))` → parsed via
///   [`HandlerErrorWireOwned`] for `(status, error, message)`. If
///   the inner JSON doesn't parse, we fall through to status 500
///   `internal_error` so an undecoded payload never masquerades as
///   success.
pub async fn engine_rpc_response<H: Handler>(
    dispatcher: &RpcDispatcher<H>,
    env: &Envelope,
) -> RpcResponsePayload {
    match dispatcher.dispatch_envelope(env).await {
        Ok(payload) => payload,
        Err(DispatchError::Reject(reason)) => {
            // The wire body intentionally only carries `actor_token_missing`
            // vs `actor_token_invalid` (see `RejectReason::wire_code`) so an
            // attacker can't probe individual checks. Surface the granular
            // `InvalidReason` + the JWS `kid` to the local engine logs so the
            // operator can tell `UnknownKeyId` (kid mismatch / rotation drift
            // between enrollment-svc and entitlement-svc) from `BadSignature`
            // (ephemeral signer drift) from `PathMismatch` / `WrongCoreId`
            // (cloud-side bug) without having to attach a debugger.
            let (method, path, jws_kid) = if let EnvelopeBody::RpcCall(payload) = &env.body {
                (
                    payload.method.as_str(),
                    payload.path.as_str(),
                    payload
                        .actor_token
                        .as_deref()
                        .and_then(extract_jws_kid)
                        .unwrap_or_else(|| "<unextractable>".to_string()),
                )
            } else {
                ("<not-rpc-call>", "<not-rpc-call>", "<no-token>".to_string())
            };
            warn!(
                envelope_id = %env.meta.id,
                method = method,
                path = path,
                jws_kid = %jws_kid,
                wire_code = reason.wire_code(),
                reason = ?reason,
                "cloud->edge rpc rejected by actor_token verifier",
            );
            let body = json!({
                "error": reason.wire_code(),
                "message": reason.to_string(),
            });
            RpcResponsePayload {
                body,
                status: reject_status(reason) as u64,
            }
        }
        Err(DispatchError::Handler(msg)) => parse_handler_error(&msg),
    }
}

/// Pull the JWS `kid` header out of a compact JWT without verifying the
/// signature. Returns `None` if the token is malformed; logging-only —
/// the actual verification still happens in [`nexus_cloud_client::Verifier`].
fn extract_jws_kid(token: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let header_b64 = token.split('.').next()?;
    let header_bytes = URL_SAFE_NO_PAD.decode(header_b64).ok()?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes).ok()?;
    header.get("kid")?.as_str().map(str::to_string)
}

/// Translate a [`RejectReason`] into the engine-stamped HTTP status
/// code on the wire. The dispatcher's own `wire_code` only
/// distinguishes `actor_token_missing` vs `actor_token_invalid`; both
/// ride on `status = 401` per Phase 1.7.
const fn reject_status(_: RejectReason) -> u16 {
    401
}

fn parse_handler_error(msg: &str) -> RpcResponsePayload {
    match serde_json::from_str::<HandlerErrorWireOwned>(msg) {
        Ok(parsed) => RpcResponsePayload {
            status: u64::from(parsed.status),
            body: json!({
                "error": parsed.error,
                "message": parsed.message,
            }),
        },
        Err(_) => RpcResponsePayload {
            status: 500,
            body: json!({
                "error": "internal_error",
                "message": msg,
            }),
        },
    }
}

/// Parse the trailing `/admin/clips/{id}/replicate` path into the
/// matching `motion_clips.id`. Returns `None` for any other shape.
fn parse_expedite_clip_id(path: &str) -> Option<i64> {
    // Be liberal about a leading slash; the cloud always sends one,
    // but defensive against future call sites.
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let mut parts = trimmed.split('/');
    if parts.next()? != "admin" {
        return None;
    }
    if parts.next()? != "clips" {
        return None;
    }
    let id_str = parts.next()?;
    if parts.next()? != "replicate" {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    id_str.parse::<i64>().ok()
}

fn is_expedite_path(path: &str) -> bool {
    parse_expedite_clip_id(path).is_some()
}

fn is_priviledged_role(role: &str) -> bool {
    matches!(role, "owner" | "admin" | "operator")
}

/// Build an outbound `rpc_response` envelope that replies to `req`
/// with `payload`. Shared by the cloud-tunnel dispatch pump and any
/// future inbound RPC test harness.
pub fn build_rpc_response_envelope(req: &Envelope, payload: RpcResponsePayload) -> Envelope {
    use nexus_cloud_protocol::v1::EnvelopeMeta;
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: Some(req.meta.id.clone()),
            seq: None,
            trace: req.meta.trace.clone(),
            ts: chrono::Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::RpcResponse(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expedite_clip_id() {
        assert_eq!(
            parse_expedite_clip_id("/admin/clips/42/replicate"),
            Some(42)
        );
        assert_eq!(
            parse_expedite_clip_id("admin/clips/100/replicate"),
            Some(100)
        );
        assert!(parse_expedite_clip_id("/admin/clips/abc/replicate").is_none());
        assert!(parse_expedite_clip_id("/admin/clips/42").is_none());
        assert!(parse_expedite_clip_id("/admin/clips/42/replicate/extra").is_none());
        assert!(parse_expedite_clip_id("/other/path").is_none());
    }

    #[test]
    fn handler_error_wire_roundtrip() {
        let err = EngineRpcError::Conflict("clip 7 already replicated".to_string());
        let wire = err.into_wire_json();
        let parsed: HandlerErrorWireOwned = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed.status, 409);
        assert_eq!(parsed.error, "conflict");
        assert_eq!(parsed.message, "clip 7 already replicated");
    }

    #[test]
    fn parse_handler_error_falls_back_to_500_on_garbage() {
        let resp = parse_handler_error("not json at all");
        assert_eq!(resp.status, 500);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("internal_error")
        );
        assert_eq!(
            resp.body.get("message").and_then(Value::as_str),
            Some("not json at all")
        );
    }

    #[test]
    fn parse_handler_error_status_round_trips() {
        let wire = EngineRpcError::NotFound("clip 99 not found".to_string()).into_wire_json();
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 404);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("not_found")
        );
        assert_eq!(
            resp.body.get("message").and_then(Value::as_str),
            Some("clip 99 not found")
        );
    }

    #[test]
    fn priviledged_role_gate() {
        assert!(is_priviledged_role("owner"));
        assert!(is_priviledged_role("admin"));
        assert!(is_priviledged_role("operator"));
        assert!(!is_priviledged_role("viewer"));
        assert!(!is_priviledged_role("system:foo"));
        assert!(!is_priviledged_role(""));
    }

    #[test]
    fn loopback_admin_url_from_bind_parses_ipv4() {
        assert_eq!(
            loopback_admin_url_from_bind("0.0.0.0:8089"),
            "http://127.0.0.1:8089"
        );
        assert_eq!(
            loopback_admin_url_from_bind("127.0.0.1:8089"),
            "http://127.0.0.1:8089"
        );
        assert_eq!(
            loopback_admin_url_from_bind("192.168.1.10:9000"),
            "http://127.0.0.1:9000"
        );
    }

    #[test]
    fn loopback_admin_url_from_bind_parses_ipv6() {
        assert_eq!(
            loopback_admin_url_from_bind("[::]:8089"),
            "http://127.0.0.1:8089"
        );
        assert_eq!(
            loopback_admin_url_from_bind("[::1]:8089"),
            "http://127.0.0.1:8089"
        );
    }

    #[test]
    fn loopback_admin_url_from_bind_hostname_fallback() {
        // Hostname-style binds don't parse as SocketAddr, but the
        // rsplit fallback recovers the port.
        assert_eq!(
            loopback_admin_url_from_bind("localhost:8089"),
            "http://127.0.0.1:8089"
        );
    }

    #[test]
    fn loopback_admin_url_from_bind_default_port_on_garbage() {
        assert_eq!(
            loopback_admin_url_from_bind("not-a-bind"),
            "http://127.0.0.1:8089"
        );
        assert_eq!(loopback_admin_url_from_bind(""), "http://127.0.0.1:8089");
    }

    #[test]
    fn default_error_code_covers_common_codes() {
        assert_eq!(default_error_code(400), "bad_request");
        assert_eq!(default_error_code(401), "unauthorized");
        assert_eq!(default_error_code(403), "forbidden");
        assert_eq!(default_error_code(404), "not_found");
        assert_eq!(default_error_code(409), "conflict");
        assert_eq!(default_error_code(429), "rate_limited");
        assert_eq!(default_error_code(500), "internal_error");
        assert_eq!(default_error_code(503), "internal_error");
        assert_eq!(default_error_code(418), "edge_error");
    }

    #[test]
    fn passthrough_error_wire_round_trips_through_parse_handler_error() {
        // Passthrough preserves arbitrary local-API status codes
        // (e.g. 403, 503) end-to-end via the existing
        // `HandlerErrorWire` envelope.
        let wire = passthrough_error_wire(503, "service_unavailable", "store is offline");
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 503);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("service_unavailable")
        );
        assert_eq!(
            resp.body.get("message").and_then(Value::as_str),
            Some("store is offline")
        );
    }

    // --- integration tests for `forward_admin_request` against a
    //     stub axum server. These exercise the actual reqwest +
    //     HTTP round-trip path without standing up a `Store`.

    /// Spawn a tiny axum app on an OS-assigned port that mimics
    /// the engine's local admin API for a handful of paths the
    /// passthrough cares about. Returns the base URL the caller
    /// should pass as `loopback_base` (e.g. `http://127.0.0.1:NNNNN`).
    async fn spawn_admin_stub() -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::Path;
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Json;
        use axum::Router;

        async fn list_cameras() -> Json<serde_json::Value> {
            Json(json!({ "cameras": [{"id": 1, "name": "cam-A"}] }))
        }

        async fn create_camera(
            Json(body): Json<serde_json::Value>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            // Engine's create_camera returns 200 OK with the
            // populated CameraConfig (rowid assigned). Mimic that.
            let mut populated = body.clone();
            if let Some(obj) = populated.as_object_mut() {
                obj.insert("id".to_string(), json!(42));
            }
            (StatusCode::OK, Json(populated))
        }

        async fn delete_camera_not_found(
            Path(_id): Path<i64>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not_found", "message": "no such camera"})),
            )
        }

        async fn discovery_session_id_only_text(
        ) -> (StatusCode, [(&'static str, &'static str); 1], String) {
            // Non-JSON response body — exercises the lossy-utf8
            // fallback in forward_admin_request for non-2xx paths.
            (
                StatusCode::BAD_REQUEST,
                [("content-type", "text/plain")],
                "session expired".to_string(),
            )
        }

        let app = Router::new()
            // The stub server mounts under `/api/v1` so the
            // passthrough's `/api/v1` prefix lands on real routes.
            .nest(
                "/api/v1",
                Router::new()
                    .route("/admin/cameras", get(list_cameras).post(create_camera))
                    .route(
                        "/admin/cameras/{id}",
                        axum::routing::delete(delete_camera_not_found),
                    )
                    .route(
                        "/admin/discovery/sessions/{id}",
                        get(discovery_session_id_only_text),
                    ),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        // Tiny yield so the listener is ready when the test fires.
        tokio::task::yield_now().await;
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn forward_admin_request_get_2xx_round_trips_body() {
        let (base, _handle) = spawn_admin_stub().await;
        let client = reqwest::Client::new();
        let body = forward_admin_request(&client, &base, "GET", "/admin/cameras", None)
            .await
            .expect("expected success");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["cameras"][0]["id"], 1);
        assert_eq!(parsed["cameras"][0]["name"], "cam-A");
    }

    #[tokio::test]
    async fn forward_admin_request_post_with_body_returns_populated_response() {
        let (base, _handle) = spawn_admin_stub().await;
        let client = reqwest::Client::new();
        let body = serde_json::to_vec(&json!({"name": "cam-B"})).unwrap();
        let resp = forward_admin_request(
            &client,
            &base,
            "POST",
            "/admin/cameras",
            Some(body.as_slice()),
        )
        .await
        .expect("expected success");
        let parsed: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        // The stub assigned id=42 on insert; the passthrough
        // forwarded the response body verbatim.
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["name"], "cam-B");
    }

    #[tokio::test]
    async fn forward_admin_request_404_json_round_trips_status_and_message() {
        let (base, _handle) = spawn_admin_stub().await;
        let client = reqwest::Client::new();
        let wire = forward_admin_request(&client, &base, "DELETE", "/admin/cameras/999", None)
            .await
            .expect_err("expected 404");
        // Round-trip the wire JSON back into a RpcResponsePayload
        // the way the dispatcher does on the way out.
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 404);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("not_found")
        );
        assert_eq!(
            resp.body.get("message").and_then(Value::as_str),
            Some("no such camera")
        );
    }

    #[tokio::test]
    async fn forward_admin_request_non_json_error_body_falls_back_to_default_code() {
        let (base, _handle) = spawn_admin_stub().await;
        let client = reqwest::Client::new();
        let wire =
            forward_admin_request(&client, &base, "GET", "/admin/discovery/sessions/abc", None)
                .await
                .expect_err("expected non-2xx");
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 400);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("bad_request")
        );
        // Non-JSON body is round-tripped as lossy-utf8 string.
        assert_eq!(
            resp.body.get("message").and_then(Value::as_str),
            Some("session expired")
        );
    }

    #[tokio::test]
    async fn forward_admin_request_unknown_method_returns_bad_request_wire() {
        let (base, _handle) = spawn_admin_stub().await;
        let client = reqwest::Client::new();
        let wire = forward_admin_request(&client, &base, "OPTIONS", "/admin/cameras", None)
            .await
            .expect_err("OPTIONS not supported by passthrough");
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 400);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("bad_request")
        );
    }

    #[tokio::test]
    async fn forward_admin_request_connection_refused_maps_to_internal_500() {
        // Use a port nothing is bound to (high range; chance of
        // collision is negligible) — reqwest returns
        // ConnectionRefused / Network error, which the
        // passthrough wraps in `Internal(500)`.
        let client = reqwest::Client::new();
        let wire = forward_admin_request(
            &client,
            "http://127.0.0.1:1", // port 1 is privileged + unbound
            "GET",
            "/admin/cameras",
            None,
        )
        .await
        .expect_err("expected connection error");
        let resp = parse_handler_error(&wire);
        assert_eq!(resp.status, 500);
        assert_eq!(
            resp.body.get("error").and_then(Value::as_str),
            Some("internal_error")
        );
    }
}
