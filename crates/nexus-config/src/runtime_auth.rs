//! Runtime auth + audit (M6)

use serde::{Deserialize, Serialize};

/// Runtime-tunable knobs for the M6 local-users lockout FSM.
/// Operators override these in `nexus.toml` under
/// `[runtime.auth.lockout]`. All defaults match the M6 design
/// (5 fails in 15 minutes → 15-minute lockout).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthConfig {
    #[serde(default)]
    pub lockout: LockoutConfig,
}

/// Failed-login lockout policy. The FSM lives in
/// `nexus-engine::auth::lockout`. These knobs let operators tune
/// the thresholds without recompiling — useful for sites with
/// monitoring tools that already do brute-force protection
/// upstream and want a looser per-user lockout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockoutConfig {
    /// Number of consecutive failed-login attempts inside
    /// `window_secs` that trip the lockout. Default: 5.
    #[serde(default = "default_lockout_max_attempts")]
    pub max_attempts: u32,
    /// Sliding window for the attempt counter (seconds).
    /// Default: 900 (15 min).
    #[serde(default = "default_lockout_window_secs")]
    pub window_secs: u32,
    /// Lockout duration once the threshold is tripped (seconds).
    /// Default: 900 (15 min). Admins can clear early via
    /// `POST /api/v1/admin/users/:id/unlock`.
    #[serde(default = "default_lockout_secs")]
    pub lockout_secs: u32,
}

impl Default for LockoutConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_lockout_max_attempts(),
            window_secs: default_lockout_window_secs(),
            lockout_secs: default_lockout_secs(),
        }
    }
}

fn default_lockout_max_attempts() -> u32 {
    5
}

fn default_lockout_window_secs() -> u32 {
    900
}

fn default_lockout_secs() -> u32 {
    900
}

/// M6 audit-log retention. Daily sweeper deletes audit_log rows
/// older than `retention_days`. Reuses the M2.1 retention sweeper
/// plumbing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuditConfig {
    /// How long audit_log rows live before the daily sweeper
    /// deletes them. Default: 365 days. Set to 0 to disable the
    /// sweeper entirely (retain forever — used by operators who
    /// ship audit to an external SIEM and don't want local
    /// expiry).
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u32,
}

impl Default for RuntimeAuditConfig {
    fn default() -> Self {
        Self {
            retention_days: default_audit_retention_days(),
        }
    }
}

fn default_audit_retention_days() -> u32 {
    365
}

/// M7 alert-delivery retention. The `alert_sink_outbox` is an
/// append-only delivery ledger that nothing ever trimmed, so it grew
/// without bound on busy sites (a 29-camera box reached ~82 k rows in
/// eleven days). The daily sweeper now deletes **terminal** rows
/// (`sent` / `suppressed` / `dead`) past the horizon; `pending` rows
/// are live work and are never swept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDeliveryConfig {
    /// How long terminal `alert_sink_outbox` rows live before the
    /// daily sweeper deletes them. Default: 30 days — long enough to
    /// answer "was this alarm delivered?" for any realistic incident
    /// review, short enough to keep the dispatcher's per-second
    /// `outbox_pending` scan cheap. Set to 0 to disable trimming and
    /// retain the full ledger forever.
    #[serde(default = "default_outbox_retention_days")]
    pub outbox_retention_days: u32,
}

impl Default for RuntimeDeliveryConfig {
    fn default() -> Self {
        Self {
            outbox_retention_days: default_outbox_retention_days(),
        }
    }
}

fn default_outbox_retention_days() -> u32 {
    30
}
