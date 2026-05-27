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

/// Engine-side RPC handler. Owns the `Store` (for clip lookups +
/// audit) and a `Notify` shared with the cold replicator so the
/// Expedite path can wake it immediately.
pub struct EngineRpcHandler {
    pub store: Arc<Store>,
    pub replicator_kick: Arc<Notify>,
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
        // Phase 2 Step 2.1c only ships one route — Expedite. Future
        // RPCs add arms here.
        if envelope.method.eq_ignore_ascii_case("POST") && is_expedite_path(envelope.path) {
            return self
                .handle_expedite(envelope.path, actor, body)
                .await
                .map_err(EngineRpcError::into_wire_json);
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
}
