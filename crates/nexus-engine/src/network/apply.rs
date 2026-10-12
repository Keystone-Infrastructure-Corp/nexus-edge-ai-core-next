//! Apply orchestrator. Holds the privileged shell-out + the
//! lockout-safe rollback timer.
//!
//! ## Protocol with `nexus-netd`
//!
//! The engine never touches `/etc/netplan/*` directly. Three
//! subcommands, all over `sudo -n /usr/local/lib/nexus/nexus-netd <cmd>`:
//!
//! ```text
//!   apply <plan.yaml>
//!     Atomically replace /etc/netplan/90-nexus.yaml with the
//!     bytes at <plan.yaml>, then `netplan generate` + `netplan
//!     apply`. Returns 0 on success; non-zero stderr-tagged on
//!     YAML parse / netplan error.
//!
//!   confirm
//!     Mark the previously-applied plan as accepted. Today this
//!     just removes the rollback sentinel file; the rollback
//!     timer below cancels itself on confirm. Idempotent.
//!
//!   rollback
//!     Restore the previous /etc/netplan/90-nexus.yaml.bak (if
//!     any) and `netplan apply`. Safe to call even if no apply
//!     is in flight (no-op + exit 0).
//! ```
//!
//! ## Lockout-safe apply
//!
//! `try_apply()` writes the plan + spawns a tokio task that
//! waits `ROLLBACK_TIMEOUT`. If `confirm()` isn't called before
//! the deadline, the task fires `rollback()` and the operator's
//! prior config comes back. The UI is expected to call
//! `confirm()` only after re-fetching `GET /v1/admin/network/plan`
//! over the new config (i.e. after re-handshaking against
//! whatever bind the operator just changed).
//!
//! ## macOS dev
//!
//! `try_apply()` returns `Err(ApplyError::PlatformUnsupported)`
//! on non-Linux so the UI can render "OS network config is only
//! available on Linux production deployments" without the rest
//! of the page erroring.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use thiserror::Error;
#[cfg(target_os = "linux")]
use tokio::process::Command;
#[cfg(target_os = "linux")]
use tokio::time::Instant;
#[cfg(target_os = "linux")]
use tracing::{info, warn};

use super::plan::NetplanPlan;

/// Default sudoers-gated helper path. Overridable via env for
/// tests (the test harness writes a stub bash script).
#[cfg(target_os = "linux")]
const NETD_BIN_ENV: &str = "NEXUS_NETD_PATH";
#[cfg(target_os = "linux")]
const DEFAULT_NETD_BIN: &str = "/usr/local/lib/nexus/nexus-netd";

/// How long after a successful `try_apply` we'll wait for the
/// operator to call `confirm()` before rolling back. Matches
/// the `netplan try --timeout` default that ops are already
/// familiar with.
#[cfg(target_os = "linux")]
pub const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("OS network configuration is only available on Linux production deployments")]
    PlatformUnsupported,
    #[error("helper binary spawn failed: {0}")]
    Spawn(std::io::Error),
    #[error("helper binary exited {code:?}: {stderr}")]
    Helper { code: Option<i32>, stderr: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("plan: {0}")]
    Plan(#[from] super::plan::PlanError),
    #[error("an apply is already in flight (apply_token: {0})")]
    InFlight(String),
    #[error("no apply in flight to confirm")]
    NothingToConfirm,
    #[error("apply_token mismatch: have {have}, given {given}")]
    TokenMismatch { have: String, given: String },
}

/// In-flight apply session. One at a time, globally.
#[derive(Debug, Clone, Serialize)]
pub struct ApplySession {
    /// Unguessable token the UI must echo back on `confirm`.
    /// Prevents a stale "Confirm" click from accepting a
    /// different apply.
    pub apply_token: String,
    /// When the apply was started.
    pub started_at: DateTime<Utc>,
    /// Wall-clock instant the rollback timer fires at.
    pub rollback_at: DateTime<Utc>,
}

/// Process-wide singleton holding the in-flight session and
/// the rollback-timer abort handle.
#[derive(Default, Clone)]
pub struct ApplyRegistry {
    inner: Arc<Mutex<Option<InFlight>>>,
}

struct InFlight {
    session: ApplySession,
    /// Cancellation handle for the rollback timer task. Drop
    /// (or `.abort()`) it on confirm or on a successful rollback.
    rollback_task: tokio::task::AbortHandle,
}

impl ApplyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the currently-pending apply, if any. Surfaced
    /// by `GET /v1/admin/network/apply/status` so the UI can
    /// resume mid-flight (e.g. if the operator refreshed).
    pub fn pending(&self) -> Option<ApplySession> {
        self.inner.lock().as_ref().map(|f| f.session.clone())
    }

    /// Write the plan + invoke the helper to swap in /etc/netplan
    /// + run `netplan apply`. On success, arms a rollback timer.
    pub async fn try_apply(
        &self,
        plan: &NetplanPlan,
        known_physical: &[String],
    ) -> Result<ApplySession, ApplyError> {
        plan.validate(known_physical)?;
        if self.inner.lock().is_some() {
            return Err(ApplyError::InFlight(
                self.inner
                    .lock()
                    .as_ref()
                    .map(|f| f.session.apply_token.clone())
                    .unwrap_or_default(),
            ));
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = plan;
            let _ = known_physical;
            Err(ApplyError::PlatformUnsupported)
        }

        #[cfg(target_os = "linux")]
        {
            let yaml = plan.to_yaml()?;
            let staged = tempfile::Builder::new()
                .prefix("nexus-netplan-")
                .suffix(".yaml")
                .tempfile()?;
            tokio::fs::write(staged.path(), yaml.as_bytes()).await?;
            let staged_path: PathBuf = staged.path().to_path_buf();
            // Persist the tempfile across the helper invocation —
            // dropping the handle would unlink before sudo runs.
            let _kept = staged.keep().map_err(|e| ApplyError::Io(e.error))?;

            invoke_helper(&[
                "apply",
                staged_path
                    .to_str()
                    .ok_or_else(|| ApplyError::Io(std::io::Error::other("non-utf8 staged path")))?,
            ])
            .await?;

            // Clean up the staged file post-apply. nexus-netd has
            // already copied bytes to /etc/netplan/90-nexus.yaml.
            let _ = std::fs::remove_file(&staged_path);

            let token = mint_token();
            let now = Utc::now();
            #[expect(
                clippy::unwrap_used,
                reason = "ROLLBACK_TIMEOUT is a 120 s constant, far inside chrono's range"
            )]
            let rollback_after = chrono::Duration::from_std(ROLLBACK_TIMEOUT).unwrap();
            let session = ApplySession {
                apply_token: token.clone(),
                started_at: now,
                rollback_at: now + rollback_after,
            };

            let inner = self.inner.clone();
            let task = tokio::spawn(async move {
                tokio::time::sleep_until(Instant::now() + ROLLBACK_TIMEOUT).await;
                warn!(
                    apply_token = %token,
                    "rollback timer fired; reverting netplan",
                );
                match invoke_helper(&["rollback"]).await {
                    Ok(()) => info!("rollback completed cleanly"),
                    Err(e) => warn!(error = %e, "rollback helper failed"),
                }
                inner.lock().take();
            })
            .abort_handle();

            *self.inner.lock() = Some(InFlight {
                session: session.clone(),
                rollback_task: task,
            });
            Ok(session)
        }
    }

    /// Accept the in-flight apply. Cancels the rollback timer
    /// + calls `nexus-netd confirm` so the helper can drop its
    ///   `.bak`.
    pub async fn confirm(&self, token: &str) -> Result<(), ApplyError> {
        let in_flight = self
            .inner
            .lock()
            .take()
            .ok_or(ApplyError::NothingToConfirm)?;
        if in_flight.session.apply_token != token {
            let have = in_flight.session.apply_token.clone();
            // Put it back so the rollback timer keeps running.
            *self.inner.lock() = Some(in_flight);
            return Err(ApplyError::TokenMismatch {
                have,
                given: token.to_string(),
            });
        }
        in_flight.rollback_task.abort();

        #[cfg(target_os = "linux")]
        {
            invoke_helper(&["confirm"]).await?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ApplyError::PlatformUnsupported)
        }
    }

    /// Explicit rollback (e.g. operator hit the "Revert now"
    /// button instead of letting the timer expire). Idempotent
    /// at the helper level.
    pub async fn rollback(&self) -> Result<(), ApplyError> {
        let in_flight = self.inner.lock().take();
        if let Some(in_flight) = in_flight {
            in_flight.rollback_task.abort();
        }
        #[cfg(target_os = "linux")]
        {
            invoke_helper(&["rollback"]).await?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ApplyError::PlatformUnsupported)
        }
    }
}

/// Shell out to `sudo -n <nexus-netd> <args...>`. `-n` keeps
/// us non-interactive — if the sudoers entry is wrong we get
/// a 1-exit + stderr we can surface, not a wedged stdin.
#[cfg(target_os = "linux")]
async fn invoke_helper(args: &[&str]) -> Result<(), ApplyError> {
    let bin = std::env::var(NETD_BIN_ENV).unwrap_or_else(|_| DEFAULT_NETD_BIN.to_string());

    // The test path uses NEXUS_NETD_PATH to point at a stub
    // script that doesn't need sudo. Detect via a marker env
    // var so the live path is unambiguous.
    let skip_sudo = std::env::var("NEXUS_NETD_SKIP_SUDO").ok().as_deref() == Some("1");

    let mut cmd = if skip_sudo {
        Command::new(&bin)
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(&bin);
        c
    };
    cmd.args(args);

    let output = cmd.output().await.map_err(ApplyError::Spawn)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Err(ApplyError::Helper {
        code: output.status.code(),
        stderr,
    })
}

#[cfg(target_os = "linux")]
fn mint_token() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn token_is_url_safe() {
        let t = mint_token();
        assert!(!t.is_empty());
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
