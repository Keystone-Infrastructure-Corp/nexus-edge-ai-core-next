//! Audit-log retention sweeper — M6 Phase 4 Step 4.4.
//!
//! Runs once at boot (so a freshly-restarted engine catches up on
//! overdue retention without a 24h wait) and then every 24h
//! thereafter. Deletes every `audit_log` row whose `created_at` is
//! older than `runtime.audit.retention_days` days.
//!
//! Records its own activity in `audit_log` (action =
//! `audit.retention.sweep`, actor_kind = `system`,
//! resource_kind = `audit_log`, resource_id = `"sweeper"`,
//! after_json = `{"deleted": N, "cutoff": "..."}`) so operators can
//! see in the global feed when retention last ran and how many rows
//! it pruned. A zero-delete sweep is NOT recorded — that would
//! create one row per day forever and defeat the point of retention.
//!
//! Honours `tokio::select!` against the engine's shutdown signal so
//! Ctrl-C between sweep ticks doesn't have to wait the full
//! interval.
//!
//! `retention_days = 0` disables the sweeper entirely — used by
//! operators who ship audit to an external SIEM and want the local
//! audit_log to grow without bound. The task still runs (and logs
//! that retention is disabled at boot) so a config reload can
//! activate it without an engine restart later (config reload is
//! out of scope for M6 but the task shape supports it).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use nexus_store::Store;
use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};
use tokio::time::interval;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct AuditRetentionConfig {
    /// Rows older than this many days are deleted on each sweep.
    /// Zero means "never sweep" (retain forever).
    pub retention_days: u32,
    /// Sweep cadence. Production: 24h. Tests pass shorter values.
    pub interval: Duration,
}

/// Run the audit-log retention sweeper until cancelled. Returns
/// when the shutdown future resolves.
pub async fn run_audit_retention(
    cfg: AuditRetentionConfig,
    store: Arc<Store>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    if cfg.retention_days == 0 {
        info!("audit-log retention disabled (retention_days = 0)");
        // We still hold the task open until shutdown so the lifecycle
        // matches the other sweepers and the main-task join doesn't
        // miss this handle.
        shutdown.await;
        return;
    }

    info!(
        retention_days = cfg.retention_days,
        interval_secs = cfg.interval.as_secs(),
        "audit-log retention sweeper starting"
    );

    tokio::pin!(shutdown);
    let mut tick = interval(cfg.interval);
    // First tick fires immediately so a freshly-booted engine
    // catches up on overdue retention without a full-day wait.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("audit-log retention sweeper shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        match sweep_once(&store, cfg.retention_days).await {
            Ok(0) => debug!("audit retention sweep idle"),
            Ok(n) => info!(deleted = n, "audit retention sweep complete"),
            Err(e) => warn!(error = %e, "audit retention sweep failed"),
        }
    }
}

/// One sweep cycle. Public for unit tests + future ad-hoc API
/// invocation. Returns the row count deleted.
pub async fn sweep_once(
    store: &Arc<Store>,
    retention_days: u32,
) -> Result<u64, nexus_store::StoreError> {
    let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
    let deleted = store.delete_audit_older_than(cutoff).await?;
    if deleted > 0 {
        // Record the sweep itself so operators can see in the global
        // audit feed when retention last ran. Skip the zero-delete
        // case — that'd create one row per day forever and defeat
        // the point of retention. Best-effort: a failure to record
        // the sweep shouldn't roll back the actual deletion.
        let after_json = serde_json::to_string(&serde_json::json!({
            "deleted": deleted,
            "cutoff": cutoff.to_rfc3339(),
            "retention_days": retention_days,
        }))
        .ok();
        let entry = NewAuditEntry {
            actor_kind: Some(AuditActorKind::System),
            actor_id: Some("0"),
            actor_label: "system:audit-retention",
            action: "audit.retention.sweep",
            resource_kind: Some("audit_log"),
            resource_id: Some("sweeper"),
            before_json: None,
            after_json: after_json.as_deref(),
            outcome: AuditOutcome::Success,
            ip: None,
            user_agent: None,
        };
        if let Err(e) = store.record_audit_event_standalone(&entry).await {
            warn!(error = %e, "failed to record audit retention sweep in audit_log");
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChDuration;
    use nexus_config::StoreConfig;
    use nexus_store::audit::AuditFilter;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn fresh_store() -> (Arc<Store>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("audit.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .expect("open"),
        );
        (store, dir)
    }

    async fn insert_audit_row_at(
        store: &Arc<Store>,
        action: &str,
        offset_days: i64,
    ) -> i64 {
        let mut tx = store.pool().begin().await.expect("begin");
        let ts = Utc::now() - ChDuration::days(offset_days);
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO audit_log (
                created_at, actor_kind, actor_id, actor_label, action,
                resource_kind, resource_id, before_json, after_json,
                outcome, ip, user_agent
            ) VALUES (
                ?, 'system', '0', 'system:test', ?,
                'test', '1', NULL, NULL,
                'success', NULL, NULL
            ) RETURNING id
            "#,
        )
        .bind(ts.to_rfc3339())
        .bind(action)
        .fetch_one(&mut *tx)
        .await
        .expect("insert");
        tx.commit().await.expect("commit");
        row.0
    }

    #[tokio::test]
    async fn sweep_once_deletes_only_rows_older_than_cutoff() {
        let (store, _dir) = fresh_store().await;
        // 3 rows: 400 days old (delete), 200 days old (keep),
        // 1 hour old (keep).
        insert_audit_row_at(&store, "old.row", 400).await;
        let mid_id = insert_audit_row_at(&store, "mid.row", 200).await;
        let new_id = insert_audit_row_at(&store, "new.row", 0).await;

        let deleted = sweep_once(&store, 365).await.expect("sweep");
        assert_eq!(deleted, 1, "should delete exactly the 400-day-old row");

        let remaining = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .expect("list");
        let ids: Vec<i64> = remaining.iter().map(|r| r.id).collect();
        assert!(ids.contains(&mid_id), "mid row must survive");
        assert!(ids.contains(&new_id), "new row must survive");
        // And the sweep recorded itself.
        let sweep_rows: Vec<_> = remaining
            .iter()
            .filter(|r| r.action == "audit.retention.sweep")
            .collect();
        assert_eq!(sweep_rows.len(), 1, "sweep should record itself once");
    }

    #[tokio::test]
    async fn sweep_with_zero_deletions_does_not_record_self() {
        let (store, _dir) = fresh_store().await;
        insert_audit_row_at(&store, "fresh.row", 0).await;

        let deleted = sweep_once(&store, 365).await.expect("sweep");
        assert_eq!(deleted, 0);

        let remaining = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .expect("list");
        // Only the fresh row should be present — NO sweep self-record.
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].action, "fresh.row");
    }

    #[tokio::test]
    async fn retention_days_zero_disables_sweeper() {
        let (store, _dir) = fresh_store().await;
        insert_audit_row_at(&store, "ancient.row", 10_000).await;

        // run_audit_retention with retention_days=0 returns
        // immediately once shutdown fires.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let store_for_task = store.clone();
        let handle = tokio::spawn(async move {
            run_audit_retention(
                AuditRetentionConfig {
                    retention_days: 0,
                    interval: Duration::from_millis(10),
                },
                store_for_task,
                async {
                    let _ = rx.await;
                },
            )
            .await;
        });
        // Give the task a tick to enter the disabled branch.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(());
        handle.await.expect("task join");

        // Ancient row must STILL be there — sweeper never ran.
        let remaining = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].action, "ancient.row");
    }
}
