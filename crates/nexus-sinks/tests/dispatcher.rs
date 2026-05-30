//! M7 Phase 1 Step 3 — integration tests for the alert-sink dispatcher.
//!
//! These tests exercise [`dispatcher::process_row`] directly against
//! a live `Store` + a hand-rolled `AlertSink` implementation. We
//! drive the row state machine without booting the timer loop in
//! [`dispatcher::run_dispatcher`] so the tests are deterministic
//! and don't depend on wall-clock sleeps. The timer loop itself
//! gets coverage in Step 5 alongside the wiremock-backed
//! `WebhookSink` integration.
//!
//! Coverage matrix:
//!
//! | Test                                       | Branch                          |
//! |--------------------------------------------|---------------------------------|
//! | `delivers_pending_row_marks_sent`          | happy path                      |
//! | `permanent_error_marks_dead`               | SinkError::Permanent → dead     |
//! | `transient_error_schedules_retry`          | SinkError::Transient → failed   |
//! | `exhausted_retries_become_dead`            | retries == MAX_ATTEMPTS         |
//! | `suppressed_by_policy_marks_suppressed`    | policy verdict                  |
//! | `missing_sink_marks_dead`                  | registry miss                   |
//! | `missing_event_marks_dead`                 | events row cascade-deleted      |
//! | `malformed_sink_id_marks_dead`             | poison-pill outbox row          |

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_sinks::backoff::MAX_ATTEMPTS;
use nexus_sinks::dispatcher::{self, AllowAllPolicy, DeliveryPolicy, DeliveryVerdict};
use nexus_sinks::{AlertSink, SinkError, SinkId, SinkRegistry};
use nexus_store::{OutboxRow, OutboxStatus, Store, SuppressionReason};
use nexus_types::{AlertEvent, Artifacts, Severity};
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn fresh_store() -> (Arc<Store>, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db_path.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    let store = Store::open(&cfg).await.expect("Store::open");
    (Arc::new(store), dir)
}

fn sample_camera(id: i64, name: &str) -> CameraConfig {
    CameraConfig {
        id,
        name: name.into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
            enabled: true,
            max_fps: 0,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: nexus_config::CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
        },
        zones: vec![],
    }
}

fn sample_alert(camera_id: i64, rule: &str) -> AlertEvent {
    AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id,
        rule_id: rule.into(),
        track_id: Some(7),
        label: "person".into(),
        severity: Severity::High,
        bbox: None,
        frame_id: 1,
        captured_at: Utc::now(),
        trace_id: "trace-disp".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
    }
}

/// Hand-rolled `AlertSink` that records every `deliver()` call and
/// can be primed to return a sequence of pre-defined outcomes.
struct ScriptedSink {
    kind: &'static str,
    id: SinkId,
    calls: AtomicUsize,
    script: parking_lot::Mutex<Vec<Result<(), SinkError>>>,
}

impl ScriptedSink {
    fn new(id: SinkId, script: Vec<Result<(), SinkError>>) -> Self {
        // The dispatcher uses `id.kind()` for routing; the trait
        // method `kind()` is metadata for logging/health. Match
        // the SinkId's kind by snapshotting it as a literal string
        // (every test uses "webhook").
        Self {
            kind: "webhook",
            id,
            calls: AtomicUsize::new(0),
            script: parking_lot::Mutex::new(script),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AlertSink for ScriptedSink {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn id(&self) -> &SinkId {
        &self.id
    }
    async fn deliver(&self, _event: &AlertEvent) -> Result<(), SinkError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock();
        script
            .pop()
            // No more script entries → assume Ok (test wrote past the
            // intended call count; the assertion in the test body
            // will catch it).
            .unwrap_or(Ok(()))
    }
}

/// Always-suppress policy used by the suppression branch test.
struct SuppressOnlyPolicy;

#[async_trait]
impl DeliveryPolicy for SuppressOnlyPolicy {
    async fn evaluate(
        &self,
        _row: &OutboxRow,
        _event: &AlertEvent,
        _now: DateTime<Utc>,
    ) -> DeliveryVerdict {
        DeliveryVerdict::Suppressed(SuppressionReason::GlobalDisabled)
    }
}

/// Enqueue one event + sink and return the resulting outbox row.
async fn enqueue_one(
    store: &Arc<Store>,
    camera_id: i64,
    rule: &str,
    sink: &str,
) -> (AlertEvent, OutboxRow) {
    let alert = sample_alert(camera_id, rule);
    store
        .record_event_and_enqueue(&alert, &[sink])
        .await
        .expect("enqueue");
    let rows = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .expect("outbox_for_event");
    assert_eq!(rows.len(), 1);
    (alert, rows.into_iter().next().unwrap())
}

// ---------------------------------------------------------------------------
// Happy + error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delivers_pending_row_marks_sent() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "ok").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (_alert, row) = enqueue_one(&store, 1, "rule.ok", id.as_str()).await;
    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    assert_eq!(sink.calls(), 1);
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
    assert_eq!(after.attempts, 1);
    assert!(after.delivered_at.is_some());
    assert!(after.last_error.is_none());
}

#[tokio::test]
async fn permanent_error_marks_dead() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "perm").unwrap();
    let sink = Arc::new(ScriptedSink::new(
        id.clone(),
        vec![Err(SinkError::Permanent("401 unauthorized".into()))],
    ));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (_alert, row) = enqueue_one(&store, 1, "rule.p", id.as_str()).await;
    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    assert_eq!(sink.calls(), 1);
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Dead);
    assert_eq!(after.attempts, 1);
    assert!(after.last_error.as_deref().unwrap().contains("permanent"));
    assert!(after.last_error.as_deref().unwrap().contains("401"));
    assert!(after.next_attempt_at.is_none());
}

#[tokio::test]
async fn transient_error_schedules_retry() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "tr").unwrap();
    let sink = Arc::new(ScriptedSink::new(
        id.clone(),
        vec![Err(SinkError::Transient("503 service unavailable".into()))],
    ));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (_alert, row) = enqueue_one(&store, 1, "rule.t", id.as_str()).await;
    let before = Utc::now();
    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    assert_eq!(sink.calls(), 1);
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    // mark_failed bounces status back to 'pending' for the retry
    // loop — see `nexus_store::Store::outbox_mark_failed`.
    assert_eq!(after.status, OutboxStatus::Pending);
    assert_eq!(after.attempts, 1);
    assert!(after.last_error.as_deref().unwrap().contains("transient"));
    let scheduled = after.next_attempt_at.expect("retry scheduled");
    // First retry is `backoff_for(1)` = 500 ms in the future.
    assert!(scheduled > before);
    assert!(scheduled < before + chrono::Duration::seconds(5));
}

#[tokio::test]
async fn exhausted_retries_become_dead() {
    // Pre-load an outbox row whose attempts is one short of MAX,
    // then deliver one more transient failure — it should flip to
    // `dead` rather than schedule another retry.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "ex").unwrap();
    let sink = Arc::new(ScriptedSink::new(
        id.clone(),
        vec![Err(SinkError::Transient("still down".into()))],
    ));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (_alert, row) = enqueue_one(&store, 1, "rule.x", id.as_str()).await;
    // Backdate attempts so the NEXT failure has next_attempts ==
    // MAX_ATTEMPTS → backoff_for returns None → mark_dead.
    let attempts_before_last = (MAX_ATTEMPTS - 1) as i64;
    sqlx::query("UPDATE alert_sink_outbox SET attempts = ? WHERE id = ?")
        .bind(attempts_before_last)
        .bind(row.id)
        .execute(store.pool())
        .await
        .unwrap();
    let row_after_backdate = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(row_after_backdate.attempts, attempts_before_last);

    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row_after_backdate).await;

    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Dead);
    assert_eq!(after.attempts, MAX_ATTEMPTS as i64);
    assert!(after.last_error.as_deref().unwrap().contains("max retries"));
}

#[tokio::test]
async fn suppressed_by_policy_marks_suppressed() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "supp").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (_alert, row) = enqueue_one(&store, 1, "rule.s", id.as_str()).await;
    dispatcher::process_row(&store, &registry, &SuppressOnlyPolicy, row.clone()).await;

    // Policy short-circuits BEFORE deliver() — sink never called.
    assert_eq!(sink.calls(), 0);
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Suppressed);
    assert_eq!(
        after.suppression_reason,
        Some(SuppressionReason::GlobalDisabled)
    );
    assert_eq!(after.attempts, 0);
}

#[tokio::test]
async fn missing_sink_marks_dead() {
    // Outbox row points at a sink the operator has since deleted.
    // No retry — terminal-dead.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let registry = Arc::new(SinkRegistry::new()); // EMPTY
    let (_alert, row) = enqueue_one(&store, 1, "rule.miss", "webhook:gone").await;
    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Dead);
    assert!(after
        .last_error
        .as_deref()
        .unwrap()
        .contains("no sink registered"));
}

#[tokio::test]
async fn missing_event_marks_dead() {
    // The events row vanished out from under the outbox row (clip
    // eviction cascaded through events.clip_id). Dispatcher must
    // mark the row `dead` rather than spin forever.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "evgone").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![]));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.evgone", id.as_str()).await;

    // Delete the events row directly. The ON DELETE CASCADE from
    // 0006 would normally sweep the outbox too — disable FK pragma
    // for this one delete so we keep the outbox row around to
    // observe the dispatcher's behaviour.
    //
    // SQLite FK enforcement is connection-scoped, and sqlx's pool
    // hands out arbitrary connections. Pin all three statements to
    // a single acquired connection so the PRAGMA actually covers
    // the DELETE.
    {
        use sqlx::Acquire;
        let mut conn = store.pool().acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(conn.acquire().await.unwrap())
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .execute(conn.acquire().await.unwrap())
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(conn.acquire().await.unwrap())
            .await
            .unwrap();
    }

    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    assert_eq!(sink.calls(), 0, "deliver() must not be called");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Dead);
    assert!(after.last_error.as_deref().unwrap().contains("missing"));
}

#[tokio::test]
async fn malformed_sink_id_marks_dead() {
    // A row whose `sink_id` doesn't match `<kind>:<name>` is a
    // poison-pill from a buggy enqueue call. Terminal-dead.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let registry = Arc::new(SinkRegistry::new());
    // Side-channel: bypass record_event_and_enqueue's validation
    // (which today doesn't validate the sink_id format anyway, but
    // belt-and-suspenders).
    let alert = sample_alert(1, "rule.poison");
    store
        .record_event_and_enqueue(&alert, &["this-has-no-colon"])
        .await
        .unwrap();
    let row = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    dispatcher::process_row(&store, &registry, &AllowAllPolicy, row.clone()).await;

    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Dead);
    assert!(after.last_error.as_deref().unwrap().contains("malformed"));
}
