//! M6 Phase 2 Step 2.8 — admin user-CRUD HTTP handlers.
//!
//! Wires the six routes that let an authenticated admin manage
//! the local-user roster:
//!
//!     GET    /api/v1/admin/users
//!     POST   /api/v1/admin/users
//!     PUT    /api/v1/admin/users/:id
//!     POST   /api/v1/admin/users/:id/reset-password
//!     POST   /api/v1/admin/users/:id/unlock
//!     DELETE /api/v1/admin/users/:id
//!
//! Each handler:
//!
//! 1. Lives behind the existing `admin_auth_layer` (HS256 JWT)
//!    *and* extracts an [`AdminContext`] from the request, so a
//!    valid bearer with `role != admin` is 403'd at the
//!    extractor before the handler body runs.
//! 2. Writes an `audit_log` row via `record_audit_event_standalone`
//!    on every state-mutating call. The audit row captures the
//!    `before` / `after` JSON projections of the affected user
//!    so an operator can reconstruct exactly what changed.
//! 3. Defers last-admin protection to the store layer — every
//!    one of `update_user_role`, `set_user_disabled`,
//!    `soft_delete_user` returns [`UsersError::LastAdmin`] in
//!    the same SQLite transaction as the count, which we map to
//!    HTTP 409 `{"error":"last_admin"}`.
//!
//! Password-reset works the same way as the first-boot
//! bootstrap: generate 24 random URL-safe-base64 bytes (192
//! bits), hash with argon2id, write with
//! `force_password_reset = true`. The plaintext is returned
//! exactly once in the HTTP response (so the admin can SMS /
//! Slack it to the target user) and never logged.
//!
//! The dispatch is identical to Step 2.7's login module: each
//! handler takes `State<UsersAdminState>` (a substate of
//! [`crate::api::ApiState`]) so the handlers stay
//! integration-testable against a bare `Router`.

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use nexus_store::{
    AuditActorKind, AuditOutcome, NewAuditEntry, NewUser, Store, StoreError, User, UserId,
    UsersError,
};
use nexus_types::Role;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::bootstrap::generate_one_time_password;
use super::passwords::hash_password;
use super::require_role::AdminContext;
use crate::admin_auth::AdminAuthState;

// ---------------------------------------------------------------------------
// Wire types.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ListUsersQuery {
    /// `true` includes soft-deleted users in the response.
    /// Default false — admin UI hides them behind a "show
    /// deleted" toggle.
    #[serde(default)]
    pub include_deleted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserListResponse {
    pub users: Vec<UserView>,
}

/// Public projection of a [`User`] — same fields as the store
/// returns, minus the `password_hash` (never exposed).
/// Identical layout per existing convention so the UI binds
/// directly without remapping.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserView {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub force_password_reset: bool,
    pub disabled: bool,
    pub failed_login_count: i64,
    pub locked_until: Option<chrono::DateTime<Utc>>,
    pub last_login_at: Option<chrono::DateTime<Utc>>,
    pub last_failed_login_at: Option<chrono::DateTime<Utc>>,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
    pub deleted_at: Option<chrono::DateTime<Utc>>,
    pub has_oidc: bool,
    pub has_password: bool,
}

impl From<User> for UserView {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            username: u.username,
            role: u.role,
            force_password_reset: u.force_password_reset,
            disabled: u.disabled,
            failed_login_count: u.failed_login_count,
            locked_until: u.locked_until,
            last_login_at: u.last_login_at,
            last_failed_login_at: u.last_failed_login_at,
            created_at: u.created_at,
            updated_at: u.updated_at,
            deleted_at: u.deleted_at,
            has_oidc: u.has_oidc,
            has_password: u.has_password,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub role: Role,
    /// Optional initial password. When `Some`, the user can log
    /// in immediately with it (and is forced to rotate it on
    /// first login because `force_password_reset` is set true).
    /// When `None`, a 192-bit password is generated and
    /// returned in the response.
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateUserResponse {
    pub user: UserView,
    /// The plaintext password — present iff the server
    /// generated it (i.e. the client did NOT supply one), OR
    /// when the client supplied one we echo `None` (the client
    /// already has it).
    pub one_time_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    /// Change the user's role. `None` leaves it as-is.
    pub role: Option<Role>,
    /// Disable / re-enable. `None` leaves it as-is.
    pub disabled: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResetPasswordResponse {
    /// The plaintext one-time password to hand to the user.
    /// `force_password_reset = true` is set on the row so the
    /// user MUST change it on next login.
    pub one_time_password: String,
}

// ---------------------------------------------------------------------------
// Error type.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum UsersAdminError {
    /// HTTP 404. Target user does not exist (or is soft-deleted
    /// and the caller didn't pass `include_deleted=true` on a
    /// path that doesn't accept it).
    #[error("not_found")]
    NotFound,
    /// HTTP 409. The mutation would leave zero active admins —
    /// returned as `{"error":"last_admin"}` so the UI can
    /// render a specific warning.
    #[error("last_admin")]
    LastAdmin,
    /// HTTP 409. Username collision on create.
    #[error("username_taken")]
    UsernameTaken,
    /// HTTP 400. Password failed the argon2id policy gate.
    #[error("password_policy: {0}")]
    PasswordPolicy(String),
    /// HTTP 400. Username failed validation (empty / too long /
    /// reserved). The store accepts NOCASE-unique strings; we
    /// add a thin policy gate at the handler edge.
    #[error("invalid_username")]
    InvalidUsername,
    /// HTTP 500. Argon2 hash failure / RNG failure.
    #[error("password_hash: {0}")]
    PasswordHash(String),
    /// HTTP 500. SQLite error.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// HTTP 500. Users-API error other than the ones folded
    /// above.
    #[error("users: {0}")]
    Users(UsersError),
}

impl From<UsersError> for UsersAdminError {
    fn from(value: UsersError) -> Self {
        match value {
            UsersError::Store(s) => UsersAdminError::Store(s),
            UsersError::NotFound => UsersAdminError::NotFound,
            UsersError::LastAdmin => UsersAdminError::LastAdmin,
            UsersError::UsernameTaken => UsersAdminError::UsernameTaken,
            other => UsersAdminError::Users(other),
        }
    }
}

impl From<super::passwords::PasswordError> for UsersAdminError {
    fn from(value: super::passwords::PasswordError) -> Self {
        match value {
            super::passwords::PasswordError::Argon2(s) => UsersAdminError::PasswordHash(s),
            super::passwords::PasswordError::Policy(p) => {
                UsersAdminError::PasswordPolicy(p.to_string())
            }
        }
    }
}

impl IntoResponse for UsersAdminError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            UsersAdminError::NotFound => (
                StatusCode::NOT_FOUND,
                serde_json::json!({"error": "not_found"}),
            ),
            UsersAdminError::LastAdmin => (
                StatusCode::CONFLICT,
                serde_json::json!({"error": "last_admin"}),
            ),
            UsersAdminError::UsernameTaken => (
                StatusCode::CONFLICT,
                serde_json::json!({"error": "username_taken"}),
            ),
            UsersAdminError::InvalidUsername => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "invalid_username"}),
            ),
            UsersAdminError::PasswordPolicy(p) => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "password_policy", "detail": p}),
            ),
            UsersAdminError::PasswordHash(_)
            | UsersAdminError::Store(_)
            | UsersAdminError::Users(_) => {
                tracing::error!(error = %self, "users-admin internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": "internal"}),
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// State.
// ---------------------------------------------------------------------------

/// Substate of [`crate::api::ApiState`] consumed by these
/// handlers. `FromRef` bridge is in `api.rs` adjacent to the
/// other substates.
#[derive(Clone)]
pub struct UsersAdminState {
    pub store: Arc<Store>,
    pub admin_auth: Arc<AdminAuthState>,
}

impl axum::extract::FromRef<crate::api::ApiState> for UsersAdminState {
    fn from_ref(input: &crate::api::ApiState) -> Self {
        UsersAdminState {
            store: input.store.clone(),
            admin_auth: input.admin_auth.clone(),
        }
    }
}

// Same bridge as `LoginState` — lets `AdminContext` extract
// from this substate when the integration tests below build a
// lean router with `State<UsersAdminState>` directly.
impl axum::extract::FromRef<UsersAdminState> for Arc<AdminAuthState> {
    fn from_ref(input: &UsersAdminState) -> Self {
        input.admin_auth.clone()
    }
}

// ---------------------------------------------------------------------------
// Username policy.
// ---------------------------------------------------------------------------

const USERNAME_MIN: usize = 1;
const USERNAME_MAX: usize = 64;

/// Lightweight username gate. The store enforces NOCASE
/// uniqueness; we add length bounds + a no-control-char rule
/// + a "no colon" rule (colon is the separator the soft-delete
///   rename uses — `<id>:deleted-<ts>` — so allowing colons in
///   fresh usernames would risk colliding with a tombstone name
///   if an admin ever typed one).
fn validate_username(s: &str) -> Result<(), UsersAdminError> {
    let trimmed = s.trim();
    if trimmed.len() < USERNAME_MIN || trimmed.len() > USERNAME_MAX {
        return Err(UsersAdminError::InvalidUsername);
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || c == ':' || c.is_whitespace())
    {
        return Err(UsersAdminError::InvalidUsername);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Audit helpers.
// ---------------------------------------------------------------------------

fn extract_client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let t = first.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    peer.ip().to_string()
}

fn extract_user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get("user-agent").and_then(|v| v.to_str().ok())
}

// 9 args is intentional — the audit row has a fixed shape and
// the inputs map 1:1 onto its columns. Packing into a struct
// would just rename the same set of fields.
#[allow(clippy::too_many_arguments)]
async fn record_user_audit(
    store: &Store,
    actor: &AdminContext,
    action: &str,
    target_user: UserId,
    before: Option<&User>,
    after: Option<&User>,
    outcome: AuditOutcome,
    ip: &str,
    ua: Option<&str>,
) {
    let before_str = before.and_then(|u| serde_json::to_string(&UserView::from(u.clone())).ok());
    let after_str = after.and_then(|u| serde_json::to_string(&UserView::from(u.clone())).ok());
    let actor_id_str = actor.0.user_id.to_string();
    let target_str = target_user.to_string();
    let actor_label = if actor.0.is_legacy_admin {
        "system:legacy-admin-token".to_string()
    } else {
        format!("user:{}", actor.0.user_id)
    };
    let entry = NewAuditEntry {
        actor_kind: Some(AuditActorKind::LocalUser),
        actor_id: Some(&actor_id_str),
        actor_label: &actor_label,
        action,
        resource_kind: Some("user"),
        resource_id: Some(&target_str),
        before_json: before_str.as_deref(),
        after_json: after_str.as_deref(),
        outcome,
        ip: Some(ip),
        user_agent: ua,
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(error = %e, action, target_user, "audit write failed");
    }
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `GET /api/v1/admin/users?include_deleted=false`
pub async fn list_users(
    State(state): State<UsersAdminState>,
    _admin: AdminContext,
    Query(q): Query<ListUsersQuery>,
) -> Result<Json<UserListResponse>, UsersAdminError> {
    let users = state.store.list_users(q.include_deleted).await?;
    Ok(Json(UserListResponse {
        users: users.into_iter().map(UserView::from).collect(),
    }))
}

/// `POST /api/v1/admin/users`
pub async fn create_user(
    State(state): State<UsersAdminState>,
    admin: AdminContext,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreateUserResponse>), UsersAdminError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    validate_username(&body.username)?;

    // Either use the caller-supplied password OR generate a
    // one-time. In both cases we set `force_password_reset =
    // true` so the user must rotate on first login.
    let (plaintext_for_response, password) = match body.password {
        Some(p) if !p.is_empty() => (None, p),
        _ => {
            let p = generate_one_time_password()
                .map_err(|e| UsersAdminError::PasswordHash(format!("rng failure: {e}")))?;
            (Some(p.clone()), p)
        }
    };
    let phc = hash_password(&password)?;
    let new = NewUser {
        username: body.username.trim(),
        role: body.role,
        password_hash: Some(&phc),
        oidc_subject_hash: None,
        force_password_reset: true,
    };
    let new_id = match state.store.create_user(&new).await {
        Ok(id) => id,
        Err(e) => {
            // Map + return — failure is logged by IntoResponse;
            // no audit row because we have no target user id.
            return Err(e.into());
        }
    };
    let user = state
        .store
        .get_user_by_id(new_id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    record_user_audit(
        &state.store,
        &admin,
        "user.create",
        new_id,
        None,
        Some(&user),
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreateUserResponse {
            user: user.into(),
            one_time_password: plaintext_for_response,
        }),
    ))
}

/// `PUT /api/v1/admin/users/:id`
pub async fn update_user(
    State(state): State<UsersAdminState>,
    admin: AdminContext,
    Path(id): Path<UserId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<UserView>, UsersAdminError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let before = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    if before.deleted_at.is_some() {
        return Err(UsersAdminError::NotFound);
    }

    if let Some(new_role) = body.role {
        if new_role != before.role {
            state.store.update_user_role(id, new_role).await?;
        }
    }
    if let Some(new_disabled) = body.disabled {
        if new_disabled != before.disabled {
            state.store.set_user_disabled(id, new_disabled).await?;
        }
    }
    let after = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    record_user_audit(
        &state.store,
        &admin,
        "user.update",
        id,
        Some(&before),
        Some(&after),
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    Ok(Json(after.into()))
}

/// `POST /api/v1/admin/users/:id/reset-password`
pub async fn reset_password(
    State(state): State<UsersAdminState>,
    admin: AdminContext,
    Path(id): Path<UserId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<ResetPasswordResponse>, UsersAdminError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let before = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    if before.deleted_at.is_some() {
        return Err(UsersAdminError::NotFound);
    }
    let otp = generate_one_time_password()
        .map_err(|e| UsersAdminError::PasswordHash(format!("rng failure: {e}")))?;
    let phc = hash_password(&otp)?;
    state.store.update_user_password(id, &phc, true).await?;

    // Same admin-driven rotation as `/change-password` — kill
    // every active refresh chain for this user so a stolen
    // session can't survive past the next access-token expiry.
    let live = state
        .store
        .list_active_refresh_tokens_for_user(id, Utc::now())
        .await?;
    for row in &live {
        if let Err(e) = state.store.revoke_chain(&row.chain_id, Utc::now()).await {
            tracing::warn!(error = %e, chain_id = %row.chain_id, "reset-password: failed to revoke chain");
        }
    }

    let after = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    record_user_audit(
        &state.store,
        &admin,
        "user.reset_password",
        id,
        Some(&before),
        Some(&after),
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    Ok(Json(ResetPasswordResponse {
        one_time_password: otp,
    }))
}

/// `POST /api/v1/admin/users/:id/unlock`
pub async fn unlock_user(
    State(state): State<UsersAdminState>,
    admin: AdminContext,
    Path(id): Path<UserId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<StatusCode, UsersAdminError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let before = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    if before.deleted_at.is_some() {
        return Err(UsersAdminError::NotFound);
    }
    state.store.clear_lockout(id).await?;
    let after = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    record_user_audit(
        &state.store,
        &admin,
        "user.unlock",
        id,
        Some(&before),
        Some(&after),
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/admin/users/:id`
pub async fn delete_user(
    State(state): State<UsersAdminState>,
    admin: AdminContext,
    Path(id): Path<UserId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<StatusCode, UsersAdminError> {
    let ip = extract_client_ip(&headers, peer);
    let ua = extract_user_agent(&headers);

    let before = state
        .store
        .get_user_by_id(id)
        .await?
        .ok_or(UsersAdminError::NotFound)?;
    if before.deleted_at.is_some() {
        return Err(UsersAdminError::NotFound);
    }
    state.store.soft_delete_user(id).await?;

    // Revoke every active session — the user is gone.
    let live = state
        .store
        .list_active_refresh_tokens_for_user(id, Utc::now())
        .await?;
    for row in &live {
        if let Err(e) = state.store.revoke_chain(&row.chain_id, Utc::now()).await {
            tracing::warn!(error = %e, chain_id = %row.chain_id, "delete: failed to revoke chain");
        }
    }
    let after = state.store.get_user_by_id(id).await?;
    record_user_audit(
        &state.store,
        &admin,
        "user.delete",
        id,
        Some(&before),
        after.as_ref(),
        AuditOutcome::Success,
        &ip,
        ua,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_auth::AdminAuthState;
    use crate::auth::sessions::issue_access_token;
    use axum::{
        body::{to_bytes, Body},
        extract::connect_info::MockConnectInfo,
        http::Request,
        routing::{get, post, put},
        Router,
    };
    use nexus_config::StoreConfig;
    use nexus_store::{NewUser, Store};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const ADMIN_SECRET: &[u8] = b"unit-test-secret-2.8-users-admin";
    const ADMIN_USER: &str = "admin";
    const ADMIN_PW: &str = "BootstrapPassword!1";

    async fn fresh_store() -> (Arc<Store>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let cfg = StoreConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        };
        let store = Arc::new(Store::open(&cfg).await.expect("open"));
        (store, dir)
    }

    async fn seed_admin(store: &Store) -> UserId {
        let phc = hash_password(ADMIN_PW).expect("hash");
        store
            .create_user(&NewUser {
                username: ADMIN_USER,
                role: Role::Admin,
                password_hash: Some(&phc),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .expect("seed admin")
    }

    async fn fresh_state() -> (UsersAdminState, Arc<Store>, TempDir, UserId) {
        let (store, dir) = fresh_store().await;
        let admin_id = seed_admin(&store).await;
        let admin_auth = Arc::new(AdminAuthState::from_secret_bytes(Some(ADMIN_SECRET), true));
        let state = UsersAdminState {
            store: store.clone(),
            admin_auth,
        };
        (state, store, dir, admin_id)
    }

    fn build_app(state: UsersAdminState) -> Router {
        Router::new()
            .route("/admin/users", get(list_users).post(create_user))
            .route("/admin/users/{id}", put(update_user).delete(delete_user))
            .route("/admin/users/{id}/reset-password", post(reset_password))
            .route("/admin/users/{id}/unlock", post(unlock_user))
            .with_state(state)
            .layer(MockConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                12345,
            )))
    }

    fn admin_bearer(admin_id: UserId) -> String {
        let now = Utc::now();
        issue_access_token(
            admin_id,
            Role::Admin,
            ADMIN_SECRET,
            now,
            chrono::Duration::minutes(15),
            None,
        )
        .expect("token")
    }

    fn json_request_with_bearer(
        method: &str,
        uri: &str,
        bearer: &str,
        body: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = match body {
            Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
            None => Body::empty(),
        };
        builder.body(body).unwrap()
    }

    async fn parse_json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).expect("valid json")
    }

    #[tokio::test]
    async fn list_users_returns_seeded_admin() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .clone()
            .oneshot(json_request_with_bearer(
                "GET",
                "/admin/users",
                &admin_bearer(admin_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: UserListResponse = parse_json(resp).await;
        assert_eq!(body.users.len(), 1);
        assert_eq!(body.users[0].username, ADMIN_USER);
        assert_eq!(body.users[0].role, Role::Admin);
    }

    #[tokio::test]
    async fn list_users_requires_bearer() {
        let (state, _store, _dir, _) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/admin/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_users_rejects_non_admin_bearer() {
        let (state, store, _dir, _) = fresh_state().await;
        let viewer_id = store
            .create_user(&NewUser {
                username: "viewer1",
                role: Role::Viewer,
                password_hash: Some(&hash_password("ViewerPw!1234").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        let viewer_token = issue_access_token(
            viewer_id,
            Role::Viewer,
            ADMIN_SECRET,
            Utc::now(),
            chrono::Duration::minutes(15),
            None,
        )
        .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "GET",
                "/admin/users",
                &viewer_token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_user_generates_otp_when_password_omitted() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                "/admin/users",
                &admin_bearer(admin_id),
                Some(serde_json::json!({
                    "username": "alice",
                    "role": "operator",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: CreateUserResponse = parse_json(resp).await;
        assert_eq!(body.user.username, "alice");
        assert_eq!(body.user.role, Role::Operator);
        assert!(body.user.force_password_reset);
        let otp = body.one_time_password.expect("server generated");
        assert!(otp.len() >= 24);

        // Verify the OTP actually authenticates against the
        // stored argon2id PHC.
        let (uid, phc) = store
            .get_password_hash_for_login("alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, body.user.id);
        assert!(super::super::passwords::verify_password(&otp, &phc));
    }

    #[tokio::test]
    async fn create_user_with_supplied_password_omits_otp() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                "/admin/users",
                &admin_bearer(admin_id),
                Some(serde_json::json!({
                    "username": "bob",
                    "role": "viewer",
                    "password": "BobPassword!12345",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: CreateUserResponse = parse_json(resp).await;
        assert!(body.one_time_password.is_none());
        assert!(body.user.force_password_reset);
    }

    #[tokio::test]
    async fn create_user_rejects_taken_username_with_409() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                "/admin/users",
                &admin_bearer(admin_id),
                Some(serde_json::json!({
                    "username": ADMIN_USER,
                    "role": "operator",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = parse_json(resp).await;
        assert_eq!(body["error"], "username_taken");
    }

    #[tokio::test]
    async fn create_user_rejects_invalid_username_with_400() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                "/admin/users",
                &admin_bearer(admin_id),
                Some(serde_json::json!({
                    "username": "has spaces",
                    "role": "operator",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = parse_json(resp).await;
        assert_eq!(body["error"], "invalid_username");
    }

    #[tokio::test]
    async fn create_user_rejects_weak_password_with_400() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                "/admin/users",
                &admin_bearer(admin_id),
                Some(serde_json::json!({
                    "username": "weakpw",
                    "role": "viewer",
                    "password": "short",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = parse_json(resp).await;
        assert_eq!(body["error"], "password_policy");
    }

    #[tokio::test]
    async fn update_user_changes_role() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let target = store
            .create_user(&NewUser {
                username: "target",
                role: Role::Viewer,
                password_hash: Some(&hash_password("TargetPw!12345").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "PUT",
                &format!("/admin/users/{target}"),
                &admin_bearer(admin_id),
                Some(serde_json::json!({"role": "operator"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: UserView = parse_json(resp).await;
        assert_eq!(body.role, Role::Operator);
    }

    #[tokio::test]
    async fn update_user_can_disable_account() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let target = store
            .create_user(&NewUser {
                username: "todisable",
                role: Role::Viewer,
                password_hash: Some(&hash_password("DisablePw!12345").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "PUT",
                &format!("/admin/users/{target}"),
                &admin_bearer(admin_id),
                Some(serde_json::json!({"disabled": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: UserView = parse_json(resp).await;
        assert!(body.disabled);
    }

    #[tokio::test]
    async fn update_user_last_admin_rejects_with_409() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "PUT",
                &format!("/admin/users/{admin_id}"),
                &admin_bearer(admin_id),
                Some(serde_json::json!({"role": "viewer"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = parse_json(resp).await;
        assert_eq!(body["error"], "last_admin");
    }

    #[tokio::test]
    async fn delete_user_last_admin_rejects_with_409() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "DELETE",
                &format!("/admin/users/{admin_id}"),
                &admin_bearer(admin_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_user_soft_deletes_and_renames() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let target = store
            .create_user(&NewUser {
                username: "todelete",
                role: Role::Viewer,
                password_hash: Some(&hash_password("DeletePw!12345").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "DELETE",
                &format!("/admin/users/{target}"),
                &admin_bearer(admin_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Old username is freed.
        let by_old = store.get_user_by_username("todelete").await.unwrap();
        assert!(by_old.is_none());

        // Row still exists by id with deleted_at set + renamed
        // (get_user_by_id does NOT filter tombstones — the
        // handler layer makes that policy choice).
        let by_id = store.get_user_by_id(target).await.unwrap().unwrap();
        assert!(by_id.deleted_at.is_some());
        assert!(by_id.disabled);
        assert!(by_id.username.contains(":deleted-"));

        // list_users(true) also surfaces it.
        let all = store.list_users(true).await.unwrap();
        let row = all
            .iter()
            .find(|u| u.id == target)
            .expect("row still present");
        assert!(row.deleted_at.is_some());
        assert!(row.username.contains(":deleted-"));
    }

    #[tokio::test]
    async fn reset_password_returns_otp_and_sets_force_reset() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let target = store
            .create_user(&NewUser {
                username: "resetme",
                role: Role::Viewer,
                password_hash: Some(&hash_password("OriginalPw!12345").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                &format!("/admin/users/{target}/reset-password"),
                &admin_bearer(admin_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: ResetPasswordResponse = parse_json(resp).await;
        assert!(body.one_time_password.len() >= 24);

        // OTP authenticates.
        let (uid, phc) = store
            .get_password_hash_for_login("resetme")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, target);
        assert!(super::super::passwords::verify_password(
            &body.one_time_password,
            &phc,
        ));

        // force_password_reset is now true.
        let row = store.get_user_by_id(target).await.unwrap().unwrap();
        assert!(row.force_password_reset);
    }

    #[tokio::test]
    async fn unlock_clears_lockout_counters() {
        let (state, store, _dir, admin_id) = fresh_state().await;
        let target = store
            .create_user(&NewUser {
                username: "lockedout",
                role: Role::Viewer,
                password_hash: Some(&hash_password("LockedPw!12345").unwrap()),
                oidc_subject_hash: None,
                force_password_reset: false,
            })
            .await
            .unwrap();
        // Force a lockout via the store API directly.
        store
            .record_login_failure(target, 5, Some(Utc::now() + chrono::Duration::minutes(15)))
            .await
            .unwrap();
        let before = store.get_user_by_id(target).await.unwrap().unwrap();
        assert!(before.locked_until.is_some());
        assert_eq!(before.failed_login_count, 5);

        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "POST",
                &format!("/admin/users/{target}/unlock"),
                &admin_bearer(admin_id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let after = store.get_user_by_id(target).await.unwrap().unwrap();
        assert!(after.locked_until.is_none());
        assert_eq!(after.failed_login_count, 0);
    }

    #[tokio::test]
    async fn update_user_returns_404_for_missing_id() {
        let (state, _store, _dir, admin_id) = fresh_state().await;
        let app = build_app(state);
        let resp = app
            .oneshot(json_request_with_bearer(
                "PUT",
                "/admin/users/999999",
                &admin_bearer(admin_id),
                Some(serde_json::json!({"disabled": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
