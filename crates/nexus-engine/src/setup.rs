//! M-Install Checkpoint 3c — first-boot setup wizard surface.
//!
//! Two HTTP endpoints back the `/setup` SPA route:
//!
//! ```text
//!   GET  /api/v1/setup/status
//!     Public-ish probe (requires a valid session bearer just
//!     like every other tenant-scoped read). Returns the
//!     handful of counts the wizard needs to decide which
//!     steps are still pending. Cheap to call \u2014 a few
//!     `COUNT(*)` queries and one runtime-setting read.
//!
//!   POST /api/v1/setup/complete
//!     Admin-only. Persists `engine_runtime_settings.setup_complete = "true"`
//!     and writes a `setup.complete` audit row. Subsequent
//!     calls to `GET /v1/setup/status` return
//!     `setup_complete: true` and the SPA's router stops
//!     redirecting to `/setup`.
//! ```
//!
//! The wizard itself is **non-modal** \u2014 nothing in the
//! engine prevents an admin from skipping it. Operators who
//! want a hands-off install can hit `POST /v1/setup/complete`
//! from `curl` immediately after the first login and never
//! see the wizard. The empty-camera / empty-rule states
//! degrade gracefully (the dashboard shows zero-state cards,
//! every list page renders the standard "no entries yet"
//! empty state).
//!
//! `setup_complete` is a one-way latch: the engine never
//! flips it back to `false`. Operators who want to "run the
//! wizard again" can clear it manually with
//! `DELETE FROM engine_runtime_settings WHERE key='setup_complete';`
//! or by editing the row \u2014 there's no admin surface for
//! resetting it because the wizard would be redundant on a
//! populated database anyway.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use serde::Serialize;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::{AdminContext, SessionContext};

/// `engine_runtime_settings` key that records whether the
/// first-boot wizard has been dismissed. Value is the literal
/// string `"true"` once set; the row is otherwise absent.
pub const KEY_SETUP_COMPLETE: &str = "setup_complete";

// ---------------------------------------------------------------------------
// GET /v1/setup/status
// ---------------------------------------------------------------------------

/// Wire-shape returned by `GET /api/v1/setup/status`. Every
/// field is meant to be rendered directly in the wizard UI
/// (no client-side derivation beyond display formatting).
#[derive(Debug, Serialize)]
pub struct SetupStatus {
    /// `true` once the operator (or any caller) has POSTed
    /// `/v1/setup/complete`. The SPA router uses this to
    /// decide whether to redirect logged-in users to `/setup`
    /// or let them through to `/dashboard`.
    pub setup_complete: bool,

    /// Number of cameras currently configured. Drives the
    /// "Step 3: Add cameras" badge in the wizard.
    pub cameras_count: usize,

    /// Number of rules currently configured. Drives the
    /// "Step 4: Configure rules" badge.
    pub rules_count: usize,

    /// Number of active admin users. Always >= 1 once the
    /// first-boot bootstrap has fired; surfaced so the
    /// wizard can warn if some misconfiguration deleted them.
    pub admin_count: usize,

    /// Engine semver as baked into the binary
    /// (`CARGO_PKG_VERSION`). Shown on the Welcome step so the
    /// operator can confirm they're running the version they
    /// just installed.
    pub version: String,

    /// OS hostname. Best-effort \u2014 falls back to
    /// `"this device"` when the lookup fails. Shown on the
    /// Welcome step.
    pub hostname: String,

    /// `true` iff the session that made the request has
    /// `force_password_reset` set. The wizard uses this to
    /// gate Step 2 (`/admin/users` change-password flow) as
    /// mandatory \u2014 the operator MUST rotate the bootstrap
    /// OTP before they can mark setup complete.
    pub session_force_password_reset: bool,
}

/// `GET /api/v1/setup/status`. Authenticated; any role can
/// read. The wizard's `router.beforeLoad` hits this on every
/// login so the unauth path (`/login` \u2192 `/setup` redirect)
/// already has the data by the time the wizard mounts.
pub async fn get_status(
    State(s): State<ApiState>,
    session: SessionContext,
) -> Result<Json<SetupStatus>, ApiError> {
    let setup_complete = is_setup_complete(&s).await?;

    let cameras_count = s.store.list_cameras().await.map_err(ApiError::from)?.len();

    let rules_count = s.store.list_rules().await.map_err(ApiError::from)?.len();

    // `count_users` includes tombstones; the wizard cares about
    // *active* admins specifically. Roll our own filter so the
    // count matches what the operator sees on `/admin/users`.
    let admin_count = s
        .store
        .list_users(false)
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .filter(|u| !u.disabled && u.role == nexus_types::Role::Admin)
        .count();

    Ok(Json(SetupStatus {
        setup_complete,
        cameras_count,
        rules_count,
        admin_count,
        // See `build.rs` — release-tag at CI build-time, falls back
        // to `CARGO_PKG_VERSION` for local dev builds.
        version: env!("NEXUS_BUILD_VERSION").to_string(),
        hostname: read_hostname(),
        // The session.force_password_reset bit on SessionContext
        // is *not* currently surfaced \u2014 fetch the user row to
        // get the live value (the bearer may pre-date a recent
        // change-password). Cheap: one indexed lookup by id.
        session_force_password_reset: lookup_force_password_reset(&s, &session).await,
    }))
}

async fn is_setup_complete(s: &ApiState) -> Result<bool, ApiError> {
    match s
        .store
        .read_runtime_setting(KEY_SETUP_COMPLETE)
        .await
        .map_err(ApiError::from)?
    {
        Some(Some(v)) => Ok(v == "true"),
        Some(None) => Ok(false),
        None => Ok(false),
    }
}

async fn lookup_force_password_reset(s: &ApiState, session: &SessionContext) -> bool {
    if session.is_legacy_admin || session.user_id == 0 {
        return false;
    }
    match s.store.get_user_by_id(session.user_id).await {
        Ok(Some(u)) => u.force_password_reset,
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                error = %e,
                user_id = session.user_id,
                "setup_status: get_user_by_id failed; treating as no-reset",
            );
            false
        }
    }
}

/// Best-effort hostname lookup. Returns `"this device"` when
/// the system call fails (rare; hermetic test sandboxes are
/// the only realistic offender).
fn read_hostname() -> String {
    // `gethostname` is in std as of 1.91; we target an older
    // toolchain so use the shell-out fallback that has worked
    // identically since BSD 4.4.
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // Linux + macOS: /proc isn't reliable for hostname,
            // but `hostnamectl` / `scutil` are both heavy. The
            // `uname` syscall via the libc crate is overkill
            // for a one-shot. Read the kernel-exposed file
            // directly where available; otherwise drop.
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this device".to_string())
}

// ---------------------------------------------------------------------------
// POST /v1/setup/complete
// ---------------------------------------------------------------------------

/// `POST /api/v1/setup/complete`. Admin-only. Idempotent:
/// hitting it twice flips the flag once and writes one audit
/// row each call (the second one with the same before/after
/// snapshot \u2014 useful for "operator clicked Finish twice"
/// forensics).
pub async fn post_complete(
    State(s): State<ApiState>,
    AdminContext(session): AdminContext,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let before = is_setup_complete(&s).await?;
    s.store
        .write_runtime_setting(KEY_SETUP_COMPLETE, Some("true"))
        .await
        .map_err(ApiError::from)?;

    audit_admin_action(
        &s.store,
        Some(&session),
        &headers,
        peer.ip(),
        "setup.complete",
        "setup",
        None,
        AuditOutcome::Success,
        Some(&format!("{{\"setup_complete\":{before}}}")),
        Some("{\"setup_complete\":true}"),
    )
    .await;

    tracing::info!(
        user_id = session.user_id,
        was_complete = before,
        "first-boot setup marked complete",
    );
    Ok(StatusCode::NO_CONTENT)
}
