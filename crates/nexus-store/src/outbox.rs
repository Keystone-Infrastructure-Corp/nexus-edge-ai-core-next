//! M7 alert delivery: outbox row types and status enums.
//!
//! The shapes here cross the store boundary into `nexus-engine`'s
//! dispatcher. They do NOT cross the bus or the HTTP API directly —
//! when the API needs to surface delivery state (per-event badges,
//! `/api/v1/admin/sinks/health`), the engine projects from these
//! rows into the wire types in `nexus-types`.
//!
//! Both enums are stored as SQLite `TEXT` and CHECK-constrained at
//! the schema level (see migration `0006_alert_sink_outbox.sql`).
//! The Rust-side enums and the SQL CHECK list MUST stay in lockstep
//! — if you add a new variant here, add the matching value to the
//! CHECK in 0006 *and* re-run the migration on a fresh DB to
//! confirm.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Terminal state of an outbox row.
///
/// State machine:
///
/// ```text
///   pending ──deliver()─Ok──> sent          (terminal)
///       │
///       ├──deliver()─Err──> failed ──retry──> pending
///       │                       └──max attempts──> dead   (terminal)
///       │
///       └──policy─Suppressed──> suppressed   (terminal)
/// ```
///
/// `failed` is the only non-terminal state other than `pending`;
/// the dispatcher rewrites `failed → pending` once
/// `next_attempt_at` arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxStatus {
    Pending,
    Sent,
    Failed,
    Dead,
    Suppressed,
}

impl OutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxStatus::Pending => "pending",
            OutboxStatus::Sent => "sent",
            OutboxStatus::Failed => "failed",
            OutboxStatus::Dead => "dead",
            OutboxStatus::Suppressed => "suppressed",
        }
    }
}

impl TryFrom<&str> for OutboxStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "pending" => Ok(OutboxStatus::Pending),
            "sent" => Ok(OutboxStatus::Sent),
            "failed" => Ok(OutboxStatus::Failed),
            "dead" => Ok(OutboxStatus::Dead),
            "suppressed" => Ok(OutboxStatus::Suppressed),
            other => Err(format!("unknown OutboxStatus: {other:?}")),
        }
    }
}

/// Why a row was terminally suppressed by the delivery policy.
///
/// Set in lockstep with `status = Suppressed`; the migration's
/// paired CHECK constraint enforces that a non-suppressed row
/// always has `suppression_reason = NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionReason {
    /// `delivery_settings.enabled = 0` (master kill switch).
    GlobalDisabled,
    /// Rule's per-rule policy carries `enabled = false`.
    RuleDisabled,
    /// Global schedule is in a `false` slot AND no rule schedule
    /// override applied.
    OffScheduleGlobal,
    /// Rule schedule (which fully *replaces* the global schedule)
    /// is in a `false` slot.
    OffScheduleRule,
    /// The org's cloud entitlement is suspended (non-payment /
    /// lifecycle `suspended`). Set by the engine's `CloudAwarePolicy`
    /// for the always-on `cloud:console` audit sink ONLY — external
    /// sinks are unaffected. See cloud `ARCHITECTURE.md` §12.4.
    EntitlementSuspended,
}

impl SuppressionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressionReason::GlobalDisabled => "global_disabled",
            SuppressionReason::RuleDisabled => "rule_disabled",
            SuppressionReason::OffScheduleGlobal => "off_schedule_global",
            SuppressionReason::OffScheduleRule => "off_schedule_rule",
            SuppressionReason::EntitlementSuspended => "entitlement_suspended",
        }
    }
}

impl TryFrom<&str> for SuppressionReason {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "global_disabled" => Ok(SuppressionReason::GlobalDisabled),
            "rule_disabled" => Ok(SuppressionReason::RuleDisabled),
            "off_schedule_global" => Ok(SuppressionReason::OffScheduleGlobal),
            "off_schedule_rule" => Ok(SuppressionReason::OffScheduleRule),
            "entitlement_suspended" => Ok(SuppressionReason::EntitlementSuspended),
            other => Err(format!("unknown SuppressionReason: {other:?}")),
        }
    }
}

/// A single `alert_sink_outbox` row as the dispatcher sees it.
///
/// `sink_id` is the wire-format string (`<kind>:<name>`) the
/// dispatcher passes to `SinkRegistry::get`; the store stays
/// agnostic of the sink-impl shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRow {
    pub id: i64,
    pub event_id: String,
    pub sink_id: String,
    pub status: OutboxStatus,
    pub attempts: i64,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub suppression_reason: Option<SuppressionReason>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// Aggregate counts for one sink over a single time window.
///
/// Produced by `Store::outbox_counts_since` and consumed by the
/// admin `GET /api/v1/admin/sinks/health` handler. Default is
/// all-zero — the admin handler uses it to fill in a row for a
/// configured-but-quiet sink (so the UI can still render a card
/// for it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxSinkCounts {
    pub sink_id: String,
    pub sent: i64,
    pub failed: i64,
    pub dead: i64,
    pub suppressed: i64,
    pub pending: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip() {
        for s in [
            OutboxStatus::Pending,
            OutboxStatus::Sent,
            OutboxStatus::Failed,
            OutboxStatus::Dead,
            OutboxStatus::Suppressed,
        ] {
            assert_eq!(OutboxStatus::try_from(s.as_str()).unwrap(), s);
        }
        assert!(OutboxStatus::try_from("nope").is_err());
    }

    #[test]
    fn reason_round_trip() {
        for r in [
            SuppressionReason::GlobalDisabled,
            SuppressionReason::RuleDisabled,
            SuppressionReason::OffScheduleGlobal,
            SuppressionReason::OffScheduleRule,
        ] {
            assert_eq!(SuppressionReason::try_from(r.as_str()).unwrap(), r);
        }
        assert!(SuppressionReason::try_from("nope").is_err());
    }
}
