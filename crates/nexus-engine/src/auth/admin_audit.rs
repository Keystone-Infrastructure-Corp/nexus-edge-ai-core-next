//! M6 Phase 4 Step 4.1 — audit hooks on every admin mutation.
//!
//! The pre-Phase-4 admin mutations called `Store::write_audit`
//! which hard-coded the actor as `"api"` and only fired on the
//! success path. That left two structural gaps:
//!
//! 1. **No actor attribution.** Every camera / rule / delivery
//!    / storage change in the `audit_log` looks like it was
//!    performed by the system, not by the human (or service
//!    token) that triggered it. Forensics is impossible.
//! 2. **No failure outcomes.** A DELETE that 5xx'd at the DB
//!    layer never showed up in the audit log at all, because
//!    the handler bailed with `?` before reaching the audit
//!    write. Operators can't tell apart "the change was never
//!    attempted" from "the change failed".
//!
//! This module fixes both with a single helper —
//! [`audit_admin_action`] — that:
//!
//! * Extracts the [`SessionContext`] that the
//!   `admin_auth_layer` injected as a request extension. When
//!   present we know the actor (HS256 `sub` claim, or one of
//!   the synthetic actors `loopback` / `allow-remote`); when
//!   absent we fall back to `system:unknown` rather than
//!   silently dropping the row.
//! * Records the client IP and User-Agent so cross-tab forensics
//!   can correlate a write to a specific browser session.
//! * Takes an [`AuditOutcome`] so failure-path callers can emit
//!   the same row shape as success paths.
//! * Is a fire-and-forget tracing-only failure mode — if the
//!   audit write itself fails (DB constraint violation, disk
//!   full) we log at `warn!` and swallow the error so the
//!   actual mutation's response code is preserved. This mirrors
//!   the existing `Store::write_audit` behaviour and the
//!   `users_admin::record_user_audit` pattern.
//!
//! ## Why this is NOT `tx_with_audit`
//!
//! The original Phase 4 plan called for a transactional
//! `tx_with_audit(handler, action, kind, id)` helper that runs
//! the domain mutation + audit row in a single SQLite tx so
//! the two roll back together on failure. That requires each
//! `Store::upsert_*` / `Store::delete_*` method to expose a
//! tx-aware sibling, which is a separate API-shape change
//! across `nexus-store`. This module ships the *call-site*
//! refactor (actor attribution + failure-outcome coverage)
//! without touching the store API. The tx-merging variant
//! lands in a follow-up under the same Step 4.1 when the store
//! gains tx-aware mutators. The audit row landing in its own
//! tx today is the same behaviour as `Store::write_audit` and
//! `users_admin::record_user_audit`, both of which are already
//! in production.

use std::net::IpAddr;

use axum::http::HeaderMap;
use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};
use nexus_store::Store;

use super::require_role::SessionContext;

/// Pull a [`SessionContext`] out of the request extensions, if
/// the `admin_auth_layer` installed one. Useful for read paths
/// that want to surface the actor without forcing a re-extract
/// of the bearer. Currently unused by the in-tree call sites
/// (which take `SessionContext` directly via the existing
/// `FromRequestParts` impl) but kept public so future handlers
/// that already accept `Request` can avoid the round-trip.
#[allow(dead_code)]
pub fn session_from_extensions(ext: &axum::http::Extensions) -> Option<&SessionContext> {
    ext.get::<SessionContext>()
}

/// Best-effort `(actor_kind, actor_id, actor_label)` triple for
/// the audit row.
///
/// * When a [`SessionContext`] is present (the post-Phase-4
///   admin path), we use it. Legacy HS256 bearers map to
///   `LocalUser` with `actor_id = "0"` (no user row) and the
///   `jti` field (which the layer populates from the bearer's
///   `sub` claim, or `"loopback"` / `"allow-remote"` for the
///   no-secret paths) as the actor label.
/// * When no context is present, we tag the row as
///   `system:unknown` so the operator can spot the gap. This
///   should never happen on a properly-mounted route — flag
///   it in tracing so we notice if a handler regresses.
fn actor_triple(ctx: Option<&SessionContext>) -> (AuditActorKind, String, String) {
    match ctx {
        Some(sc) if sc.is_legacy_admin => (
            AuditActorKind::LocalUser,
            sc.user_id.to_string(),
            format!("system:legacy-admin:{}", sc.jti),
        ),
        Some(sc) => (
            AuditActorKind::LocalUser,
            sc.user_id.to_string(),
            format!("user:{}", sc.user_id),
        ),
        None => {
            tracing::warn!(
                "admin audit write reached without a SessionContext — handler is not behind admin_auth_layer; falling back to system:unknown"
            );
            (
                AuditActorKind::System,
                "0".to_string(),
                "system:unknown".to_string(),
            )
        }
    }
}

/// Read the `User-Agent` header value (if any) as `&str`,
/// silently skipping non-UTF8.
pub fn user_agent_from(headers: &HeaderMap) -> Option<&str> {
    headers.get("user-agent").and_then(|v| v.to_str().ok())
}

/// Write one `audit_log` row for an admin mutation.
///
/// `actor` is the verified [`SessionContext`] from the request
/// (handlers extract it via the existing `FromRequestParts`
/// impl); `None` is accepted so callers that fire outside the
/// admin gate (e.g. the OAuth callback) can still emit a row
/// tagged `system:unknown`.
///
/// `headers` + `peer_ip` come from the request; both are
/// recorded so cross-tab forensics can correlate the action
/// to a specific browser session. The handler typically pulls
/// them via the `HeaderMap` + `ConnectInfo<SocketAddr>`
/// extractors.
///
/// `before_json` + `after_json` are caller-shaped — typically
/// the pre- and post-mutation snapshots serialised via
/// `serde_json::to_string`. Either can be `None` (e.g.
/// `after` is None for a delete).
///
/// Fire-and-forget — failures are logged at `warn!` and never
/// returned, so the caller can chain `audit_admin_action`
/// before returning the response without affecting the HTTP
/// status code.
#[allow(clippy::too_many_arguments)]
pub async fn audit_admin_action(
    store: &Store,
    actor: Option<&SessionContext>,
    headers: &HeaderMap,
    peer_ip: IpAddr,
    action: &str,
    resource_kind: &str,
    resource_id: Option<&str>,
    outcome: AuditOutcome,
    before_json: Option<&str>,
    after_json: Option<&str>,
) {
    let (actor_kind, actor_id, actor_label) = actor_triple(actor);
    let ua = user_agent_from(headers);
    let ip = peer_ip.to_string();

    let entry = NewAuditEntry {
        actor_kind: Some(actor_kind),
        actor_id: Some(actor_id.as_str()),
        actor_label: actor_label.as_str(),
        action,
        resource_kind: Some(resource_kind),
        resource_id,
        before_json,
        after_json,
        outcome,
        ip: Some(ip.as_str()),
        user_agent: ua,
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(
            error = %e,
            action,
            resource_kind,
            resource_id,
            outcome = ?outcome,
            "admin audit write failed",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::Role;

    #[test]
    fn actor_triple_legacy_admin_uses_system_label_with_jti() {
        let ctx = SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "admin-cli".to_string(),
            is_legacy_admin: true,
        };
        let (kind, id, label) = actor_triple(Some(&ctx));
        assert_eq!(kind, AuditActorKind::LocalUser);
        assert_eq!(id, "0");
        assert_eq!(label, "system:legacy-admin:admin-cli");
    }

    #[test]
    fn actor_triple_non_legacy_uses_user_id_label() {
        let ctx = SessionContext {
            user_id: 42,
            role: Role::Admin,
            jti: "abc".to_string(),
            is_legacy_admin: false,
        };
        let (kind, id, label) = actor_triple(Some(&ctx));
        assert_eq!(kind, AuditActorKind::LocalUser);
        assert_eq!(id, "42");
        assert_eq!(label, "user:42");
    }

    #[test]
    fn actor_triple_missing_context_falls_back_to_system_unknown() {
        let (kind, id, label) = actor_triple(None);
        assert_eq!(kind, AuditActorKind::System);
        assert_eq!(id, "0");
        assert_eq!(label, "system:unknown");
    }

    #[test]
    fn actor_triple_loopback_session_distinguishable_via_jti() {
        let ctx = SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "loopback".to_string(),
            is_legacy_admin: true,
        };
        let (_, _, label) = actor_triple(Some(&ctx));
        assert_eq!(label, "system:legacy-admin:loopback");
    }
}
