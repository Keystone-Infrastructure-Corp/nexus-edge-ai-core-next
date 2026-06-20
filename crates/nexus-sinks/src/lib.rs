//! M7 alert delivery — sink trait + registry.
//!
//! This crate is the engine's egress contract. Every alert that
//! fires lands in `events` locally (M2.1 invariant), and zero or
//! more `alert_sink_outbox` rows enqueue a delivery attempt against
//! each [`AlertSink`] configured for that rule. The dispatcher
//! (in `nexus-engine`) drains the outbox and calls
//! [`AlertSink::deliver`] exactly once per attempt — retry/backoff
//! is the dispatcher's job, *not* the sink's.
//!
//! Design split, in three pieces that each have one reason to
//! change:
//!
//!   * [`AlertSink`] — the trait every delivery target implements.
//!     Stable async surface; one method (`deliver`) plus a
//!     synchronous health probe.
//!   * [`SinkId`] — the stable `<kind>:<name>` identifier every
//!     `alert_sink_outbox` row references. The pair survives sink
//!     config edits; renaming a sink is forbidden in M7 to keep
//!     historical outbox rows resolvable.
//!   * [`SinkRegistry`] — lock-protected map the dispatcher reads
//!     on every outbox row. Admin mutations swap the full map in
//!     one `RwLock::write` so readers never observe a half-applied
//!     reconfiguration.
//!
//! Concrete impls (`WebhookSink`, `SureViewSink`) land in follow-up
//! commits behind cargo features in this same crate.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use nexus_types::AlertEvent;

pub mod backoff;
pub mod dispatcher;
pub mod policy;
#[cfg(feature = "sureview")]
pub mod sureview;
#[cfg(feature = "sureview-email")]
pub mod sureview_email;
#[cfg(feature = "webhook")]
pub mod webhook;
pub use backoff::{backoff_for, backoff_for_with};

// ---------------------------------------------------------------------------
// SinkId
// ---------------------------------------------------------------------------

/// Stable identifier for one configured sink instance.
///
/// Wire format: `"<kind>:<name>"` — e.g. `"webhook:primary"`,
/// `"sureview:siteX"`. `kind` matches [`AlertSink::kind`]; `name`
/// is operator-chosen and must be stable across config reloads
/// because every `alert_sink_outbox` row references it.
///
/// Renaming a sink is forbidden in M7 — the engine rejects
/// `PUT /api/v1/admin/sinks/:id` requests that change `kind` or
/// `name` (operator must delete + re-add).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SinkId(String);

impl SinkId {
    /// Build a new [`SinkId`] from its component pieces. Both
    /// `kind` and `name` must be non-empty and free of the `:`
    /// separator; otherwise returns `None`. Wire-format parsing
    /// goes through [`SinkId::parse`].
    pub fn new(kind: &str, name: &str) -> Option<Self> {
        if kind.is_empty() || name.is_empty() || kind.contains(':') || name.contains(':') {
            return None;
        }
        Some(Self(format!("{kind}:{name}")))
    }

    /// Parse a wire-format `"<kind>:<name>"` string. Returns `None`
    /// if either half is empty or the separator is missing.
    pub fn parse(raw: &str) -> Option<Self> {
        let (kind, name) = raw.split_once(':')?;
        Self::new(kind, name)
    }

    /// Full wire form — `"<kind>:<name>"`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Discriminator half — `"webhook"`, `"sureview"`, …
    pub fn kind(&self) -> &str {
        self.0.split(':').next().unwrap_or("")
    }

    /// Operator-chosen half.
    pub fn name(&self) -> &str {
        self.0.split_once(':').map(|(_, n)| n).unwrap_or("")
    }
}

impl std::fmt::Display for SinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Outcome of a single [`AlertSink::deliver`] call.
///
/// The dispatcher uses the variant to choose between "retry on the
/// normal backoff schedule" (`Transient`) and "fail loudly because
/// the operator must intervene" (`Permanent`).
///
/// `Permanent` does NOT short-circuit retries entirely — the
/// dispatcher still attempts each row up to its `attempts` ceiling
/// — but it bumps the row to `dead` faster and surfaces a louder
/// signal on `/admin/sinks/health`. Use it for misconfiguration
/// (bad credentials, 401, 404 on the configured URL) where another
/// network-level retry will not help.
#[derive(Debug, Error)]
pub enum SinkError {
    /// Network blip, 5xx, timeout, rate-limit. Dispatcher retries
    /// per the standard exp-backoff schedule.
    #[error("transient: {0}")]
    Transient(String),

    /// 4xx auth/config error or unparseable response. Dispatcher
    /// counts the attempt and accelerates the row toward `dead`.
    #[error("permanent: {0}")]
    Permanent(String),
}

impl SinkError {
    /// True when the dispatcher should retry this row on the
    /// normal backoff schedule rather than accelerating to `dead`.
    pub fn is_transient(&self) -> bool {
        matches!(self, SinkError::Transient(_))
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Cheap snapshot reported by `/api/v1/admin/sinks/health`.
///
/// Implementations may cache the last result internally; the
/// dispatcher does NOT call [`AlertSink::health`] inside the
/// delivery loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SinkHealth {
    Up,
    Degraded,
    Down,
    Unknown,
}

// ---------------------------------------------------------------------------
// AlertSink trait
// ---------------------------------------------------------------------------

/// One configured delivery target.
///
/// Implementations are async and must be cancellation-safe — the
/// dispatcher may abort a slow `deliver` if the engine is shutting
/// down.
///
/// Hard contract:
///
///   * `deliver` returns `Ok(())` only when the remote system has
///     accepted ownership of the alert (200/201/204 for HTTP, etc.).
///   * `deliver` MUST NOT internally retry — that's the
///     dispatcher's job.
///   * `deliver` MUST be safe to call concurrently for different
///     events. Per-sink serialization (if any rate-limiting
///     requires it) is the dispatcher's responsibility.
///   * `kind` is a compile-time constant string used as the
///     `<kind>` half of every [`SinkId`] this impl issues.
#[async_trait]
pub trait AlertSink: Send + Sync {
    /// Stable discriminator (`"webhook"`, `"sureview"`, …).
    fn kind(&self) -> &'static str;

    /// Full identifier — `<kind>:<operator-chosen-name>`.
    fn id(&self) -> &SinkId;

    /// Ship one alert. See trait-level contract for retry / error
    /// semantics.
    async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError>;

    /// Synchronous health probe. Default returns
    /// [`SinkHealth::Unknown`]; impls override when they maintain a
    /// running success/failure window.
    fn health(&self) -> SinkHealth {
        SinkHealth::Unknown
    }
}

// ---------------------------------------------------------------------------
// SinkRegistry
// ---------------------------------------------------------------------------

type SinkMap = HashMap<SinkId, Arc<dyn AlertSink>>;

/// Thread-safe registry of every active sink.
///
/// The dispatcher resolves each `alert_sink_outbox.sink_id` via
/// [`SinkRegistry::get`] on every drain iteration. Admin mutations
/// (`PUT /api/v1/admin/sinks/:id`, the `sink.config.changed` bus
/// event) call [`SinkRegistry::replace`] with the full new set so
/// readers never observe a half-applied reconfiguration.
#[derive(Default)]
pub struct SinkRegistry {
    inner: RwLock<SinkMap>,
}

impl SinkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire active set in one atomic swap.
    ///
    /// Returns the number of sinks now registered.
    pub fn replace(&self, sinks: Vec<Arc<dyn AlertSink>>) -> usize {
        let map: SinkMap = sinks.into_iter().map(|s| (s.id().clone(), s)).collect();
        let n = map.len();
        *self.inner.write() = map;
        n
    }

    /// Look up by ID. Returns `None` if no sink is registered
    /// under that identifier — the dispatcher must treat this as a
    /// `Permanent` failure (most likely a stale `alert_sink_outbox`
    /// row that survived a sink deletion).
    pub fn get(&self, id: &SinkId) -> Option<Arc<dyn AlertSink>> {
        self.inner.read().get(id).cloned()
    }

    /// All currently registered IDs. Cheap snapshot; ordering is
    /// unspecified.
    pub fn ids(&self) -> Vec<SinkId> {
        self.inner.read().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Config → Vec<Arc<dyn AlertSink>>
// ---------------------------------------------------------------------------

/// Build the concrete sink set from a parsed `[[sinks]]` config
/// list. Called once at engine boot before the dispatcher spins,
/// then handed to `SinkRegistry::replace`.
///
/// Each variant of `nexus_config::SinkConfig` is gated on its own
/// cargo feature in this crate — a binary that opts out of
/// `--features webhook` will get a `Permanent` error if the
/// operator's config contains a `kind = "webhook"` entry, so the
/// misconfiguration surfaces at boot rather than at the first
/// alert's first delivery attempt.
pub fn build_sinks_from_config(
    sinks: &[nexus_config::SinkConfig],
) -> Result<Vec<Arc<dyn AlertSink>>, SinkError> {
    // The `mut` is only consumed by feature-gated branches that
    // push into the vec; suppress the unused-mut warning under
    // every feature combination that has no enabled sink kinds.
    #[cfg_attr(
        not(any(feature = "webhook", feature = "sureview", feature = "sureview-email")),
        allow(unused_mut)
    )]
    let mut out: Vec<Arc<dyn AlertSink>> = Vec::with_capacity(sinks.len());
    for cfg in sinks {
        match cfg {
            #[cfg(feature = "webhook")]
            nexus_config::SinkConfig::Webhook(w) => {
                out.push(Arc::new(webhook::WebhookSink::new(w)?));
            }
            #[cfg(not(feature = "webhook"))]
            nexus_config::SinkConfig::Webhook(w) => {
                return Err(SinkError::Permanent(format!(
                    "webhook sink '{}' configured but binary was built without --features webhook",
                    w.name
                )));
            }
            #[cfg(feature = "sureview")]
            nexus_config::SinkConfig::SureView(s) => {
                out.push(Arc::new(sureview::SureViewSink::new(s)?));
            }
            #[cfg(not(feature = "sureview"))]
            nexus_config::SinkConfig::SureView(s) => {
                return Err(SinkError::Permanent(format!(
                    "sureview sink '{}' configured but binary was built without --features sureview",
                    s.name
                )));
            }
            #[cfg(feature = "sureview-email")]
            nexus_config::SinkConfig::SureViewEmail(s) => {
                out.push(Arc::new(sureview_email::SureViewEmailSink::new(s)?));
            }
            #[cfg(not(feature = "sureview-email"))]
            nexus_config::SinkConfig::SureViewEmail(s) => {
                return Err(SinkError::Permanent(format!(
                    "sureview_email sink '{}' configured but binary was built without --features sureview-email",
                    s.name
                )));
            }
        }
    }
    Ok(out)
}

/// Merge file-defined sinks (`nexus.toml` `[[sinks]]`) with the
/// db-persisted sink configs the cloud console / admin UI manage at
/// runtime. db sinks WIN on `<kind>:<name>` collision; file order is
/// preserved and db-only sinks are appended in input order. Returns
/// a flat config list ready for [`build_sinks_from_config`].
///
/// `db_sink_json` is the raw `config_json` blob from each
/// `alert_sinks` row (see `nexus-store::alert_sinks_list`); a blob
/// that fails to deserialise is a `Permanent` error so a corrupt row
/// surfaces at boot / reload rather than silently dropping a sink.
pub fn merge_sink_configs(
    file_sinks: &[nexus_config::SinkConfig],
    db_sink_json: &[String],
) -> Result<Vec<nexus_config::SinkConfig>, SinkError> {
    let mut out: Vec<nexus_config::SinkConfig> = Vec::with_capacity(file_sinks.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for cfg in file_sinks {
        let id = format!("{}:{}", cfg.kind(), cfg.name());
        index.insert(id, out.len());
        out.push(cfg.clone());
    }
    for json in db_sink_json {
        let cfg: nexus_config::SinkConfig = serde_json::from_str(json).map_err(|e| {
            SinkError::Permanent(format!("persisted sink config is not valid JSON: {e}"))
        })?;
        let id = format!("{}:{}", cfg.kind(), cfg.name());
        match index.get(&id) {
            Some(&i) => out[i] = cfg,
            None => {
                index.insert(id, out.len());
                out.push(cfg);
            }
        }
    }
    Ok(out)
}

/// Build the live sink set from the UNION of file sinks + db sinks.
/// Convenience wrapper over [`merge_sink_configs`] +
/// [`build_sinks_from_config`] used by the engine at boot and on
/// each `sink.config.changed` bus signal.
pub fn build_effective_sinks(
    file_sinks: &[nexus_config::SinkConfig],
    db_sink_json: &[String],
) -> Result<Vec<Arc<dyn AlertSink>>, SinkError> {
    let merged = merge_sink_configs(file_sinks, db_sink_json)?;
    build_sinks_from_config(&merged)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink {
        id: SinkId,
        calls: AtomicUsize,
    }

    impl CountingSink {
        fn new(kind: &'static str, name: &str) -> Self {
            Self {
                id: SinkId::new(kind, name).unwrap(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl AlertSink for CountingSink {
        fn kind(&self) -> &'static str {
            "test"
        }
        fn id(&self) -> &SinkId {
            &self.id
        }
        async fn deliver(&self, _event: &AlertEvent) -> Result<(), SinkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn sink_id_round_trip() {
        let id = SinkId::new("webhook", "primary").unwrap();
        assert_eq!(id.as_str(), "webhook:primary");
        assert_eq!(id.kind(), "webhook");
        assert_eq!(id.name(), "primary");

        let parsed = SinkId::parse("sureview:siteX").unwrap();
        assert_eq!(parsed.kind(), "sureview");
        assert_eq!(parsed.name(), "siteX");
    }

    #[test]
    fn merge_db_wins_and_appends_db_only() {
        // One file sink; db re-points it and adds a second.
        let file: Vec<nexus_config::SinkConfig> = vec![serde_json::from_str(
            r#"{"kind":"webhook","name":"a","url":"https://file.example/a"}"#,
        )
        .unwrap()];
        let db = vec![
            r#"{"kind":"webhook","name":"a","url":"https://db.example/a"}"#.to_string(),
            r#"{"kind":"webhook","name":"b","url":"https://db.example/b"}"#.to_string(),
        ];
        let merged = merge_sink_configs(&file, &db).unwrap();
        assert_eq!(merged.len(), 2);
        // File order preserved: "a" first (db override), then db-only "b".
        assert_eq!(merged[0].name(), "a");
        assert_eq!(merged[1].name(), "b");
        if let nexus_config::SinkConfig::Webhook(w) = &merged[0] {
            assert_eq!(w.url.as_str(), "https://db.example/a");
        } else {
            panic!("expected webhook");
        }
    }

    #[test]
    fn merge_rejects_corrupt_db_json() {
        let file: Vec<nexus_config::SinkConfig> = vec![];
        let db = vec!["not json".to_string()];
        assert!(merge_sink_configs(&file, &db).is_err());
    }

    #[test]
    fn sink_id_rejects_malformed() {
        assert!(SinkId::parse("nosep").is_none());
        assert!(SinkId::parse(":noname").is_none());
        assert!(SinkId::parse("nokind:").is_none());
        // Embedded ':' in name half is intentionally allowed by
        // `parse` (only the *first* ':' splits) — that case is
        // dropped via `new`'s contains-check, but only when the
        // operator constructs it programmatically.
    }

    #[test]
    fn registry_replace_round_trip() {
        let reg = SinkRegistry::new();
        assert!(reg.is_empty());

        let a: Arc<dyn AlertSink> = Arc::new(CountingSink::new("test", "a"));
        let b: Arc<dyn AlertSink> = Arc::new(CountingSink::new("test", "b"));
        let n = reg.replace(vec![a, b]);
        assert_eq!(n, 2);
        assert_eq!(reg.len(), 2);

        let id_a = SinkId::new("test", "a").unwrap();
        assert!(reg.get(&id_a).is_some());
        assert!(reg.get(&SinkId::new("test", "ghost").unwrap()).is_none());

        // Replace shrinks the set atomically.
        let c: Arc<dyn AlertSink> = Arc::new(CountingSink::new("test", "c"));
        reg.replace(vec![c]);
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&id_a).is_none());
    }

    #[test]
    fn registry_replace_deduplicates_by_id() {
        // Two sinks with the same ID — second one wins (last-write).
        let reg = SinkRegistry::new();
        let a1: Arc<dyn AlertSink> = Arc::new(CountingSink::new("test", "same"));
        let a2: Arc<dyn AlertSink> = Arc::new(CountingSink::new("test", "same"));
        let n = reg.replace(vec![a1, a2]);
        assert_eq!(n, 1);
    }

    #[test]
    fn sink_error_classification() {
        let t = SinkError::Transient("conn reset".into());
        assert!(t.is_transient());
        let p = SinkError::Permanent("401".into());
        assert!(!p.is_transient());
    }
}
