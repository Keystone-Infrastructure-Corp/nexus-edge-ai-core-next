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
//! | `alert_clip_wait_does_not_consume_retry_budget` | clip wait ≠ delivery attempt |
//! | `future_dated_event_does_not_wait_forever` | clock-skew wait bound           |

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_sinks::backoff::MAX_ATTEMPTS;
use nexus_sinks::dispatcher::{self, AllowAllPolicy, DeliveryPolicy, DeliveryVerdict};
use nexus_sinks::{AlertSink, SinkError, SinkId, SinkRegistry};
use nexus_store::{ClipClose, NewAlertClip, NewClip};
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
            analysis_url: None,
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
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
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
        frame_w: 0,
        frame_h: 0,
    }
}

/// Hand-rolled `AlertSink` that records every `deliver()` call and
/// can be primed to return a sequence of pre-defined outcomes.
struct ScriptedSink {
    kind: &'static str,
    id: SinkId,
    calls: AtomicUsize,
    wants_clip: bool,
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
            wants_clip: false,
            script: parking_lot::Mutex::new(script),
        }
    }

    /// Builder toggle: make this sink attach the surrounding clip, so
    /// the dispatcher runs its clip-resolution / grace-window path.
    fn wanting_clip(mut self) -> Self {
        self.wants_clip = true;
        self
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
    fn wants_clip(&self) -> bool {
        self.wants_clip
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
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row_after_backdate,
    )
    .await;

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
    dispatcher::process_row(
        &store,
        &registry,
        &SuppressOnlyPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

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

// ---------------------------------------------------------------------------
// Clip-link grace window (intermittent "SureView alert missing its clip")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_clip_linked_within_grace_schedules_retry() {
    // A clip-attaching sink processes a *fresh* alert whose clip has
    // not been linked yet (the supervisor links it a few frames after
    // it enqueues the outbox row). The dispatcher must treat this as
    // "link still pending" and schedule a retry rather than shipping a
    // clip-less alarm.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "clip-young").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    // sample_alert's captured_at is Utc::now() → inside the grace.
    let (_alert, row) = enqueue_one(&store, 1, "rule.clip", id.as_str()).await;
    let before = Utc::now();
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 0, "must not deliver while clip link pending");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Pending);
    assert_eq!(
        after.attempts, 0,
        "waiting on the clip link must not spend the delivery retry budget"
    );
    let scheduled = after.next_attempt_at.expect("retry scheduled");
    assert!(scheduled > before);
    assert!(after
        .last_error
        .as_deref()
        .unwrap()
        .contains("clip link pending"));
}

#[tokio::test]
async fn no_clip_linked_after_grace_delivers() {
    // Same clip-attaching sink, but the alert is old enough that the
    // clip would certainly have been linked by now if one existed.
    // The dispatcher gives up waiting and delivers clip-less.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "clip-old").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    // Backdate the alert well past CLIP_LINK_GRACE_SECS (10 s).
    let mut alert = sample_alert(1, "rule.clip.old");
    alert.captured_at = Utc::now() - chrono::Duration::seconds(30);
    store
        .record_event_and_enqueue(&alert, &[id.as_str()])
        .await
        .expect("enqueue");
    let row = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 1, "delivers clip-less after grace elapses");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

/// Open + close a clip, link it to the given event, and return the
/// clip's relative hot path. `hot_present` controls whether the row
/// keeps a hot pointer (soft-evicted clips have `hot_path = NULL`).
async fn link_closed_clip(store: &Arc<Store>, event_id: &str, hot_present: bool) -> String {
    let rel = format!("cam1/{event_id}.mp4");
    let clip_id = store
        .open_clip(&NewClip {
            camera_id: 1,
            started_at: Utc::now() - chrono::Duration::seconds(5),
            hot_path: rel.clone(),
            hot_handle: "local".into(),
            codec: "h264".into(),
            container: "mp4".into(),
            frame_width: 960,
            frame_height: 540,
        })
        .await
        .expect("open_clip");
    store
        .close_clip(
            clip_id,
            &ClipClose {
                ended_at: Utc::now(),
                duration_ms: 4000,
                size_bytes: 1024,
                hot_path: Some(rel.clone()),
                sha256: Some("deadbeef".into()),
            },
        )
        .await
        .expect("close_clip");
    if !hot_present {
        // Simulate a soft-eviction: the cold replicator uploaded the
        // clip and the hot copy was then reclaimed under disk pressure.
        // The row schema requires at least one handle and, when the hot
        // handle is present, a hot_path — so clearing the hot side must
        // go hand-in-hand with populating the cold side to satisfy the
        // `hot_handle IS NOT NULL OR cold_handle IS NOT NULL` and
        // `cold_handle IS NULL OR (cold_path IS NOT NULL AND
        // cold_uploaded_at IS NOT NULL)` CHECK constraints.
        sqlx::query(
            "UPDATE motion_clips \
             SET hot_path = NULL, hot_handle = NULL, \
                 cold_handle = 'local', cold_path = ?, \
                 cold_uploaded_at = ? \
             WHERE id = ?",
        )
        .bind(format!("cold/{event_id}.mp4"))
        .bind(Utc::now())
        .bind(clip_id)
        .execute(store.pool())
        .await
        .unwrap();
    }
    store
        .link_event_to_clip(event_id, clip_id)
        .await
        .expect("link_event_to_clip");
    rel
}

#[tokio::test]
async fn clip_hot_file_present_delivers_with_clip() {
    // A closed clip with a hot pointer whose MP4 is actually on disk
    // resolves and delivers.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "clip-hot").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.clip.hot", id.as_str()).await;
    let rel = link_closed_clip(&store, &alert.event_id.to_string(), true).await;
    // Materialise the file on disk so the dispatcher's existence
    // check passes.
    let abs = clips_dir.path().join(&rel);
    tokio::fs::create_dir_all(abs.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&abs, b"fake mp4 bytes").await.unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 1, "delivers with the resolved clip");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

#[tokio::test]
async fn clip_hot_file_missing_within_grace_retries() {
    // The DB names a hot_path but the MP4 isn't on disk (recorder
    // still flushing, or a racing eviction) and the alert is fresh —
    // retry rather than ship a clip-less alarm.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "clip-missing").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.clip.missing", id.as_str()).await;
    // Link a closed clip but DO NOT create the file on disk.
    link_closed_clip(&store, &alert.event_id.to_string(), true).await;

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 0, "must not deliver while hot file absent");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Pending);
    assert!(after
        .last_error
        .as_deref()
        .unwrap()
        .contains("not yet on disk"));
}

#[tokio::test]
async fn clip_soft_evicted_within_grace_retries() {
    // Clip closed but soft-evicted (hot_path NULL); a fresh alert
    // retries in case the eviction raced the close.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "clip-evicted").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.clip.evicted", id.as_str()).await;
    link_closed_clip(&store, &alert.event_id.to_string(), false).await;

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(
        sink.calls(),
        0,
        "must not deliver while soft-evict may be racing"
    );
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Pending);
    assert!(after.last_error.as_deref().unwrap().contains("no hot path"));
}

#[tokio::test]
async fn clip_soft_evicted_after_grace_delivers_clipless() {
    // Same soft-evicted clip, but the alert is old enough that the
    // hot copy is gone for good — deliver clip-less rather than block.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "clip-evicted-old").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let mut alert = sample_alert(1, "rule.clip.evicted.old");
    alert.captured_at = Utc::now() - chrono::Duration::seconds(30);
    store
        .record_event_and_enqueue(&alert, &[id.as_str()])
        .await
        .expect("enqueue");
    let row = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    link_closed_clip(&store, &alert.event_id.to_string(), false).await;

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 1, "delivers clip-less once hot copy is gone");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

// ---------------------------------------------------------------------------
// M-Alert-Clip: the dispatcher resolves the short alert clip (ready in
// ~post_secs) instead of waiting on the up-to-5-min motion clip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alert_clip_building_within_grace_retries() {
    // The event carries an alert_clip_id whose clip is still building. A
    // fresh alert must WAIT (retry), not ship clip-less or fall back to
    // the motion clip.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "ac-building").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.ac.building", id.as_str()).await;
    let acid = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/x/1.mp4".into(),
        })
        .await
        .unwrap();
    store
        .link_event_alert_clip(&alert.event_id.to_string(), acid)
        .await
        .unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(
        sink.calls(),
        0,
        "must not deliver while the alert clip builds"
    );
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Pending);
    assert!(after
        .last_error
        .as_deref()
        .unwrap()
        .contains("still building"));
}

#[tokio::test]
async fn alert_clip_wait_does_not_consume_retry_budget() {
    // Regression: a slow alert-clip build used to burn the SAME
    // `attempts` counter that delivery retries draw from. Field data
    // showed 75-92% of *successful* SureView deliveries arriving with
    // 5+ of the 8 attempts already spent on waiting, so one transient
    // SMTP blip after a slow build killed an alarm that should have
    // been retried.
    //
    // Waiting is not a delivery attempt: no `deliver()` call happens,
    // so `attempts` must stay put no matter how many sweeps observe
    // the still-building clip. The wait is bounded by the wall-clock
    // grace window instead.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "ac-budget").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.ac.budget", id.as_str()).await;
    let acid = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/x/budget.mp4".into(),
        })
        .await
        .unwrap();
    store
        .link_event_alert_clip(&alert.event_id.to_string(), acid)
        .await
        .unwrap();

    // Far more sweeps than MAX_ATTEMPTS — under the old shared-budget
    // behaviour the row would have been marked `dead` partway through.
    for _ in 0..(MAX_ATTEMPTS + 4) {
        dispatcher::process_row(
            &store,
            &registry,
            &AllowAllPolicy,
            Some(clips_dir.path()),
            None,
            None,
            row.clone(),
        )
        .await;
    }

    assert_eq!(sink.calls(), 0, "clip never became ready");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        after.status,
        OutboxStatus::Pending,
        "waiting must never mark the alarm dead"
    );
    assert_eq!(
        after.attempts, 0,
        "the full retry budget must remain available for delivery"
    );
}

#[tokio::test]
async fn future_dated_event_does_not_wait_forever() {
    // Clip waits are bounded ONLY by wall-clock age now, so a
    // future-dated `captured_at` (clock step / NTP correction) must not
    // read as "forever young" and wedge the alarm. A negative age is
    // treated as past the grace window: deliver clip-less rather than
    // never deliver.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "ac-future").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let mut alert = sample_alert(1, "rule.ac.future");
    alert.captured_at = Utc::now() + chrono::Duration::seconds(3600);
    store
        .record_event_and_enqueue(&alert, &[id.as_str()])
        .await
        .expect("enqueue");
    let row = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let acid = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/x/future.mp4".into(),
        })
        .await
        .unwrap();
    store
        .link_event_alert_clip(&alert.event_id.to_string(), acid)
        .await
        .unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(
        sink.calls(),
        1,
        "a future-dated alert must still be delivered, clip-less"
    );
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

#[tokio::test]
async fn alert_clip_ready_delivers_with_clip() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "ac-ready").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.ac.ready", id.as_str()).await;
    let rel = "alert/1/x/2.mp4";
    let acid = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: rel.into(),
        })
        .await
        .unwrap();
    store
        .link_event_alert_clip(&alert.event_id.to_string(), acid)
        .await
        .unwrap();
    store
        .mark_alert_clip_ready(acid, 8_000, 1_234, None)
        .await
        .unwrap();
    // Materialise the file so the existence check passes.
    let abs = clips_dir.path().join(rel);
    tokio::fs::create_dir_all(abs.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&abs, b"fake alert mp4").await.unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 1, "delivers with the resolved alert clip");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

#[tokio::test]
async fn alert_clip_failed_delivers_clipless() {
    // A failed build must NOT hold the alarm — deliver clip-less at once.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clips_dir = tempfile::tempdir().expect("clips tmp");

    let id = SinkId::new("webhook", "ac-failed").unwrap();
    let sink = Arc::new(ScriptedSink::new(id.clone(), vec![Ok(())]).wanting_clip());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let (alert, row) = enqueue_one(&store, 1, "rule.ac.failed", id.as_str()).await;
    let acid = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/x/3.mp4".into(),
        })
        .await
        .unwrap();
    store
        .link_event_alert_clip(&alert.event_id.to_string(), acid)
        .await
        .unwrap();
    store.mark_alert_clip_failed(acid).await.unwrap();

    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        Some(clips_dir.path()),
        None,
        None,
        row.clone(),
    )
    .await;

    assert_eq!(sink.calls(), 1, "delivers clip-less when the build failed");
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.status, OutboxStatus::Sent);
}

/// The delivery-outcome bridge carries an id and an enum, and nothing
/// else. The endpoint's own error text is the thing most likely to leak
/// a URL, a mailbox or a token, and `alert_sink_outbox.last_error` keeps
/// it verbatim — so the assertion is on the published payload, taken
/// from a failure whose message contains all three.
#[tokio::test]
async fn a_delivery_outcome_publishes_an_id_and_an_enum_only() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let id = SinkId::new("webhook", "leaky").unwrap();
    let sink = Arc::new(ScriptedSink::new(
        id.clone(),
        vec![Err(SinkError::Transient(
            "POST https://hook.example/t/abc123 failed for ops@example.com".into(),
        ))],
    ));
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink.clone()]);

    let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
    let mut outcomes = bus
        .subscribe::<serde_json::Value>(topic::SINK_DELIVERY_OUTCOME)
        .await
        .expect("subscribe");

    let (_alert, row) = enqueue_one(&store, 1, "rule.leak", id.as_str()).await;
    dispatcher::process_row(
        &store,
        &registry,
        &AllowAllPolicy,
        None,
        None,
        Some(&bus),
        row.clone(),
    )
    .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), outcomes.next())
        .await
        .expect("an outcome is published")
        .expect("stream open")
        .expect("well-formed");
    assert_eq!(event["sink_id"], "webhook:leaky");
    assert_eq!(event["outcome"], "first_failure");

    let serialized = serde_json::to_string(&event).unwrap();
    for forbidden in ["https", "hook.example", "abc123", "@", "last_error"] {
        assert!(
            !serialized.contains(forbidden),
            "{forbidden:?} must not reach the delivery-outcome bridge, got {serialized}",
        );
    }
    // The row still records the error locally — the boundary is the bus,
    // not the outbox.
    let after = store
        .outbox_for_event(&row.event_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(after
        .last_error
        .as_deref()
        .unwrap()
        .contains("hook.example"));
}
