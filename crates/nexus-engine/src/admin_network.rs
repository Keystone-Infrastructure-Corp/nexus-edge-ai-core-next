//! M-Admin Network — HTTP surface for OS-level network
//! configuration.
//!
//! Lives in its own module (rather than slotting into
//! `admin_runtime.rs`) because the failure model is different:
//! [`crate::admin_runtime`] writes to `engine_runtime_settings`
//! and is restart-required; this module writes to
//! `/etc/netplan/*.yaml` via a privileged helper and applies
//! immediately with a `netplan try`-style auto-rollback.
//!
//! Endpoint shape (all admin-gated):
//!
//! ```text
//!   GET  /v1/admin/network/interfaces
//!     Lists every NIC the OS knows about (cross-platform).
//!     Backs the bind-by-interface dropdowns and the Network
//!     page.
//!
//!   GET  /v1/admin/network/plan
//!     The currently-persisted, operator-edited plan (from
//!     `engine_runtime_settings.network_plan_json`). NOT the
//!     live netplan state on disk — that may diverge from the
//!     persisted plan if an apply is in flight and the rollback
//!     hasn't yet fired.
//!
//!   PUT  /v1/admin/network/plan
//!     Validate + persist a new plan. Does NOT apply. The two
//!     are separate so an operator can compose a multi-step
//!     change (e.g. add a VLAN + then move the engine bind to
//!     it) without intermediate broken states reaching the OS.
//!
//!   POST /v1/admin/network/plan/apply
//!     Push the persisted plan into `/etc/netplan/90-nexus.yaml`
//!     and run `netplan apply`. Returns an
//!     `apply_token` + `rollback_at` deadline. The UI MUST
//!     re-handshake against the new config and POST
//!     `/confirm` with the token before the deadline or the
//!     helper auto-reverts.
//!
//!   POST /v1/admin/network/plan/confirm
//!     Accept the in-flight apply. Body: `{ "apply_token": "..." }`.
//!
//!   POST /v1/admin/network/plan/rollback
//!     Explicit revert. Idempotent — a no-op when no apply is
//!     in flight (returns 200).
//!
//!   GET  /v1/admin/network/apply/status
//!     Snapshot of the current `ApplyRegistry`. `null` when no
//!     apply is in flight. Lets the UI resume mid-flight after
//!     a refresh.
//!
//!   GET  /v1/admin/network/roles
//!     The persisted interface-role assignment (which NIC
//!     cameras are on, which NIC carries internet egress) plus
//!     each role resolved against the live NIC table (primary
//!     IPv4 + subnet + up/present flags). No elevation needed.
//!
//!   PUT  /v1/admin/network/roles
//!     Validate + persist a role assignment. Body:
//!     `{ "camera": "eno1"|null, "internet": "eno1.20"|null }`.
//!     Names must resolve to a real, non-loopback NIC; `null` /
//!     `""` clears the role. Persists to
//!     `engine_runtime_settings.iface_roles_json`; discovery
//!     source-binds probes to the camera NIC on the next sweep,
//!     no restart needed.
//! ```

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::AdminContext;
use crate::network::apply::{ApplyError, ApplySession};
use crate::network::roles::{
    load_roles, resolve_role, validate_role_name, InterfaceRoles, ResolvedRole, KEY_IFACE_ROLES,
};
use crate::network::{list_interfaces, NetplanPlan, NetworkInterface};

const KEY_NETWORK_PLAN: &str = "network_plan_json";

#[derive(Debug, Serialize)]
pub struct InterfacesOut {
    pub interfaces: Vec<NetworkInterface>,
}

pub async fn get_interfaces(
    State(_s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<InterfacesOut>, ApiError> {
    let interfaces = list_interfaces().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("nic enumeration: {e}"),
        )
    })?;
    Ok(Json(InterfacesOut { interfaces }))
}

#[derive(Debug, Serialize)]
pub struct PlanOut {
    pub plan: NetplanPlan,
    /// `Some` when an apply is in flight (matches
    /// `GET /apply/status`). Surfaced on the plan response so
    /// the UI can render the rollback banner without a second
    /// round-trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_pending: Option<ApplySession>,
}

pub async fn get_plan(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<PlanOut>, ApiError> {
    let plan = read_persisted_plan(&s).await?;
    Ok(Json(PlanOut {
        plan,
        apply_pending: s.network_apply.pending(),
    }))
}

pub async fn put_plan(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(plan): Json<NetplanPlan>,
) -> Result<Json<PlanOut>, ApiError> {
    // Validate against the live OS state so the operator can't
    // persist a plan that references NICs that don't exist —
    // even though apply will re-validate, persisting nonsense
    // lets the UI render stale state.
    let nics = list_interfaces().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("nic enumeration: {e}"),
        )
    })?;
    let physical: Vec<String> = nics
        .iter()
        .filter(|n| !n.is_loopback)
        .filter(|n| {
            !matches!(
                n.kind,
                crate::network::enumerate::InterfaceKind::Vlan
                    | crate::network::enumerate::InterfaceKind::Loopback
            )
        })
        .map(|n| n.name.clone())
        .collect();

    if let Err(e) = plan.validate(&physical) {
        audit_admin_action(
            &s.store,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "network.plan.put",
            "admin/network/plan",
            Some("singleton"),
            AuditOutcome::Failure,
            None,
            Some(&serde_json::json!({ "error": format!("{e}") }).to_string()),
        )
        .await;
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("{e}")));
    }

    let prior = read_persisted_plan(&s).await.ok();
    let json = plan.to_json().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialise plan: {e}"),
        )
    })?;

    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_NETWORK_PLAN, Some(&json))
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "network.plan.put",
            "admin/network/plan",
            Some("singleton"),
            prior.as_ref().and_then(|p| p.to_json().ok()).as_deref(),
            Some(&json),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    Ok(Json(PlanOut {
        plan,
        apply_pending: s.network_apply.pending(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ApplyOut {
    pub session: ApplySession,
}

pub async fn post_apply(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
) -> Result<Json<ApplyOut>, ApiError> {
    let plan = read_persisted_plan(&s).await?;
    let nics = list_interfaces().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("nic enumeration: {e}"),
        )
    })?;
    let physical: Vec<String> = nics
        .iter()
        .filter(|n| !n.is_loopback)
        .filter(|n| {
            !matches!(
                n.kind,
                crate::network::enumerate::InterfaceKind::Vlan
                    | crate::network::enumerate::InterfaceKind::Loopback
            )
        })
        .map(|n| n.name.clone())
        .collect();

    match s.network_apply.try_apply(&plan, &physical).await {
        Ok(session) => {
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.apply",
                "admin/network/plan/apply",
                Some("singleton"),
                AuditOutcome::Success,
                None,
                Some(
                    &serde_json::json!({
                        "apply_token": session.apply_token,
                        "rollback_at": session.rollback_at,
                    })
                    .to_string(),
                ),
            )
            .await;
            Ok(Json(ApplyOut { session }))
        }
        Err(e) => {
            let code = match e {
                ApplyError::PlatformUnsupported => StatusCode::NOT_IMPLEMENTED,
                ApplyError::InFlight(_) => StatusCode::CONFLICT,
                ApplyError::Plan(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.apply",
                "admin/network/plan/apply",
                Some("singleton"),
                AuditOutcome::Failure,
                None,
                Some(&serde_json::json!({ "error": format!("{e}") }).to_string()),
            )
            .await;
            Err(ApiError(code, format!("{e}")))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfirmReq {
    pub apply_token: String,
}

pub async fn post_confirm(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<ConfirmReq>,
) -> Result<StatusCode, ApiError> {
    match s.network_apply.confirm(&req.apply_token).await {
        Ok(()) => {
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.confirm",
                "admin/network/plan/confirm",
                Some("singleton"),
                AuditOutcome::Success,
                None,
                Some(&serde_json::json!({ "apply_token": req.apply_token }).to_string()),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            let code = match e {
                ApplyError::PlatformUnsupported => StatusCode::NOT_IMPLEMENTED,
                ApplyError::NothingToConfirm => StatusCode::CONFLICT,
                ApplyError::TokenMismatch { .. } => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.confirm",
                "admin/network/plan/confirm",
                Some("singleton"),
                AuditOutcome::Failure,
                None,
                Some(&serde_json::json!({ "error": format!("{e}") }).to_string()),
            )
            .await;
            Err(ApiError(code, format!("{e}")))
        }
    }
}

pub async fn post_rollback(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
) -> Result<StatusCode, ApiError> {
    match s.network_apply.rollback().await {
        Ok(()) => {
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.rollback",
                "admin/network/plan/rollback",
                Some("singleton"),
                AuditOutcome::Success,
                None,
                None,
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            let code = match e {
                ApplyError::PlatformUnsupported => StatusCode::NOT_IMPLEMENTED,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            audit_admin_action(
                &s.store,
                Some(&admin.0),
                &headers,
                peer.ip(),
                "network.plan.rollback",
                "admin/network/plan/rollback",
                Some("singleton"),
                AuditOutcome::Failure,
                None,
                Some(&serde_json::json!({ "error": format!("{e}") }).to_string()),
            )
            .await;
            Err(ApiError(code, format!("{e}")))
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApplyStatusOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<ApplySession>,
}

pub async fn get_apply_status(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<ApplyStatusOut>, ApiError> {
    Ok(Json(ApplyStatusOut {
        session: s.network_apply.pending(),
    }))
}

async fn read_persisted_plan(s: &ApiState) -> Result<NetplanPlan, ApiError> {
    match s.store.read_runtime_setting(KEY_NETWORK_PLAN).await {
        Ok(Some(Some(json))) => NetplanPlan::from_json(&json).map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persisted plan is malformed: {e}"),
            )
        }),
        Ok(_) => Ok(NetplanPlan::default()),
        Err(e) => Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read persisted plan: {e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Interface roles — which NIC cameras are on / carries internet egress.
// ---------------------------------------------------------------------------

/// Response for `GET`/`PUT /v1/admin/network/roles`. Echoes the
/// persisted assignment plus each role resolved against the live NIC
/// table so the UI can show the concrete address / subnet the role
/// points at (and flag a dangling assignment when the NIC vanished).
#[derive(Debug, Serialize)]
pub struct RolesOut {
    pub roles: InterfaceRoles,
    /// Camera role resolved against the live NIC table, if assigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<ResolvedRole>,
    /// Internet role resolved against the live NIC table, if assigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internet: Option<ResolvedRole>,
}

fn resolve_roles_out(roles: InterfaceRoles, nics: &[NetworkInterface]) -> RolesOut {
    let camera = roles.camera.as_deref().map(|n| resolve_role(n, nics));
    let internet = roles.internet.as_deref().map(|n| resolve_role(n, nics));
    RolesOut {
        roles,
        camera,
        internet,
    }
}

pub async fn get_roles(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<RolesOut>, ApiError> {
    let roles = load_roles(&s.store).await;
    let nics = list_interfaces().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("nic enumeration: {e}"),
        )
    })?;
    Ok(Json(resolve_roles_out(roles, &nics)))
}

pub async fn put_roles(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<InterfaceRoles>,
) -> Result<Json<RolesOut>, ApiError> {
    let roles = req.normalised();

    let nics = list_interfaces().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("nic enumeration: {e}"),
        )
    })?;

    // Validate each assigned name against the live NIC table. A
    // stale form or a NIC removed between the GET and the PUT is
    // rejected loudly rather than persisted as a dangling role.
    let validation = roles
        .camera
        .as_deref()
        .map(|n| validate_role_name(n, &nics))
        .transpose()
        .and_then(|_| {
            roles
                .internet
                .as_deref()
                .map(|n| validate_role_name(n, &nics))
                .transpose()
        });
    if let Err(e) = validation {
        audit_admin_action(
            &s.store,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "network.roles.put",
            "admin/network/roles",
            Some("singleton"),
            AuditOutcome::Failure,
            None,
            Some(&serde_json::json!({ "error": e }).to_string()),
        )
        .await;
        return Err(ApiError(StatusCode::BAD_REQUEST, e));
    }

    let prior = load_roles(&s.store).await;
    let json = serde_json::to_string(&roles).map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialise roles: {e}"),
        )
    })?;

    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, KEY_IFACE_ROLES, Some(&json))
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&admin.0),
            &headers,
            peer.ip(),
            "network.roles.put",
            "admin/network/roles",
            Some("singleton"),
            serde_json::to_string(&prior).ok().as_deref(),
            Some(&json),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {e}"),
        ));
    }

    Ok(Json(resolve_roles_out(roles, &nics)))
}
