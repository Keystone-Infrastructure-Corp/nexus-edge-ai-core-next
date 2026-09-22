//! BUG-054 regression — per-sink isolation and drain-loop liveness.
//!
//! These boot the real [`dispatcher::run_dispatcher`] timer loop (the
//! other dispatcher test files drive `process_row` directly), because
//! the defects being guarded against are properties of the *loop*, not
//! of any single row transition:
//!
//! | Test                                          | Guards                        |
//! |-----------------------------------------------|-------------------------------|
//! | `slow_sink_does_not_block_other_sinks`        | one blocked sink stalled all  |
//! | `dispatcher_health_reports_liveness`          | a wedged loop was invisible   |
//! | `fresh_health_is_not_live`                    | absent ≠ healthy              |
//! | `hung_sink_does_not_wedge_later_ticks`        | a hung sink stopped the loop  |
//!
//! Production shape being reproduced: an alert fans out to several
//! sinks, and one of them (the SureView email sink, uploading multi-MB
//! clip attachments over STARTTLS) blocks for a long time — or, as in
//! the incident itself, forever. Before the fix the dispatcher walked
//! the batch on a single task in `id` order, so that one sink held every
//! other sink's rows behind it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use nexus_config::{CameraConfig, StoreConfig};
use nexus_sinks::dispatcher::{self, AllowAllPolicy, DeliveryPolicy, DispatcherHealth};
use nexus_sinks::{AlertSink, SinkError, SinkId, SinkRegistry};
use nexus_store::{OutboxRow, OutboxStatus, Store};
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
        trace_id: "trace-iso".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
        frame_w: 0,
        frame_h: 0,
    }
}

/// An `AlertSink` whose `deliver()` parks until the test hands out a
/// permit. Models a sink that is slow (big attachment, slow relay) or
/// wedged (the stalled tunnel writer of BUG-054) without any sleeps, so
/// the test stays deterministic.
struct GatedSink {
    id: SinkId,
    gate: Arc<tokio::sync::Semaphore>,
    calls: AtomicUsize,
}

impl GatedSink {
    fn new(id: SinkId, permits: usize) -> Self {
        Self {
            id,
            gate: Arc::new(tokio::sync::Semaphore::new(permits)),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AlertSink for GatedSink {
    fn kind(&self) -> &'static str {
        "webhook"
    }
    fn id(&self) -> &SinkId {
        &self.id
    }
    async fn deliver(&self, _event: &AlertEvent) -> Result<(), SinkError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let permit = self
            .gate
            .acquire()
            .await
            .expect("gate semaphore is never closed");
        permit.forget();
        Ok(())
    }
}

/// Poll `f` until it returns true or `within` elapses.
async fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn row_for(store: &Arc<Store>, event_id: &str, sink_id: &str) -> OutboxRow {
    store
        .outbox_for_event(event_id)
        .await
        .expect("outbox_for_event")
        .into_iter()
        .find(|r| r.sink_id == sink_id)
        .expect("row for sink")
}

async fn status_of(store: &Arc<Store>, event_id: &str, sink_id: &str) -> OutboxStatus {
    row_for(store, event_id, sink_id).await.status
}

/// Poll until an outbox row reaches `want`, or `within` elapses.
async fn wait_for_status(
    store: &Arc<Store>,
    event_id: &str,
    sink_id: &str,
    want: OutboxStatus,
    within: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if status_of(store, event_id, sink_id).await == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn test_cfg() -> dispatcher::SinkDispatcherConfig {
    dispatcher::SinkDispatcherConfig {
        tick_interval: Duration::from_millis(25),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A sink that blocks inside `deliver()` must not hold up any *other*
/// sink's rows.
///
/// The blocked sink is enqueued FIRST (lower outbox `id`) **and** named
/// so it sorts first among sinks. Both orderings matter: the drain is
/// `ORDER BY id`, and the per-sink grouping iterates in sink-id order,
/// so a blocking sink that came second under either ordering would let
/// this test pass while still being serialised. With the slow sink at
/// the head of both, any serialisation of the batch stalls the fast
/// sink — exactly how one wedged sink stalled every sink for 17.7h.
#[tokio::test(flavor = "multi_thread")]
async fn slow_sink_does_not_block_other_sinks() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    // Names chosen so `slow` < `fast` in sink-id order.
    let slow_id = SinkId::new("webhook", "aslow").unwrap();
    let fast_id = SinkId::new("webhook", "zfast").unwrap();

    // 0 permits — parks forever until the test releases it.
    let slow = Arc::new(GatedSink::new(slow_id.clone(), 0));
    // Ample permits — never blocks.
    let fast = Arc::new(GatedSink::new(fast_id.clone(), 128));

    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![slow.clone(), fast.clone()]);

    let alert = sample_alert(1, "rule.iso");
    // Slow sink first => lower outbox id => head of the batch.
    store
        .record_event_and_enqueue(&alert, &[slow_id.as_str(), fast_id.as_str()])
        .await
        .expect("enqueue");
    let event_id = alert.event_id.to_string();
    assert!(
        slow_id.as_str() < fast_id.as_str(),
        "fixture invariant: the blocking sink must sort first, else \
         a serialised dispatcher would still pass this test",
    );

    let health = Arc::new(DispatcherHealth::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let policy: Arc<dyn DeliveryPolicy> = Arc::new(AllowAllPolicy);
    let loop_handle = tokio::spawn(dispatcher::run_dispatcher(
        test_cfg(),
        store.clone(),
        registry.clone(),
        policy,
        health.clone(),
        None,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    // The fast sink must complete while the slow sink is still parked.
    let fast_sent = wait_for_status(
        &store,
        &event_id,
        fast_id.as_str(),
        OutboxStatus::Sent,
        Duration::from_secs(10),
    )
    .await;

    assert!(
        fast_sent,
        "fast sink must deliver while the slow sink is blocked; \
         slow.calls={}, fast.calls={}",
        slow.calls(),
        fast.calls()
    );

    // And the slow sink really is still mid-delivery, not skipped.
    assert_eq!(slow.calls(), 1, "slow sink should be parked inside deliver");
    assert_eq!(
        status_of(&store, &event_id, slow_id.as_str()).await,
        OutboxStatus::Pending,
        "slow sink's row must still be pending while it blocks",
    );

    // Release the slow sink and confirm it also lands.
    slow.gate.add_permits(1);
    assert!(
        wait_for_status(
            &store,
            &event_id,
            slow_id.as_str(),
            OutboxStatus::Sent,
            Duration::from_secs(10),
        )
        .await,
        "slow sink must finish once unblocked",
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), loop_handle).await;
}

/// The drain loop must publish a liveness signal. During BUG-054 the
/// engine looked healthy from every angle — it logged, served the API,
/// and reported its sinks as configured — because nothing reported
/// whether the loop was still completing passes.
#[tokio::test(flavor = "multi_thread")]
async fn dispatcher_health_reports_liveness() {
    let (store, _tmp) = fresh_store().await;
    let registry = Arc::new(SinkRegistry::new());
    let health = Arc::new(DispatcherHealth::default());

    assert!(
        !health.is_live(Utc::now(), Duration::from_secs(60)),
        "a dispatcher that has never completed a tick is not live",
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let policy: Arc<dyn DeliveryPolicy> = Arc::new(AllowAllPolicy);
    let loop_handle = tokio::spawn(dispatcher::run_dispatcher(
        test_cfg(),
        store.clone(),
        registry,
        policy,
        health.clone(),
        None,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    let ticked = {
        let h = health.clone();
        wait_until(Duration::from_secs(10), move || h.ticks_completed() > 0).await
    };
    assert!(ticked, "dispatcher should complete at least one tick");

    let now = Utc::now();
    assert!(
        health.is_live(now, Duration::from_secs(60)),
        "a ticking dispatcher must read as live",
    );
    assert!(
        health.age_ms(now).is_some_and(|ms| ms >= 0),
        "age should be reportable once a tick has completed",
    );
    assert!(health.last_tick_started_ms().is_some());
    assert!(health.last_tick_completed_ms().is_some());

    // An idle loop drains nothing, so the row counters stay at zero
    // while the tick counter climbs — that distinction is the whole
    // point: "no work" must look different from "not running".
    assert_eq!(health.rows_processed(), 0);
    assert_eq!(health.last_batch_rows(), 0);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), loop_handle).await;
}

/// A stale clock must read as not-live rather than defaulting to
/// healthy — the failure mode being guarded against is silence.
#[tokio::test]
async fn fresh_health_is_not_live() {
    let health = DispatcherHealth::default();
    let now = Utc::now();
    assert!(!health.is_live(now, Duration::from_secs(60)));
    assert_eq!(health.age_ms(now), None);
    assert_eq!(health.last_tick_completed_ms(), None);
    assert_eq!(health.last_tick_started_ms(), None);
    assert_eq!(health.ticks_completed(), 0);
}

/// Issue #338 regression — a sink whose `deliver()` never returns must not
/// stop the drain loop, nor the shutdown branch with it.
///
/// `slow_sink_does_not_block_other_sinks` above passes against this
/// defect, which is exactly why it shipped: it fans ONE alert out to both
/// sinks, so both rows land in the SAME batch and the per-sink `JoinSet`
/// gives the fast sink its concurrency *within* tick 1 — then it releases
/// the gate before asserting anything further. But the defect is not
/// inside a tick, it is at the batch boundary: `tick` joins every sink
/// group before `outbox_pending` is called again, so a parked `deliver()`
/// means there is never a tick 2, for any sink. Isolation *within* a
/// batch and liveness *across* batches are different properties and only
/// the first was ever asserted.
///
/// So the assertion here is the one that separates them: an alert
/// enqueued AFTER the hang has begun, for the healthy sink, with the gate
/// still held. Reaching `Sent` requires a second `outbox_pending`.
#[tokio::test(flavor = "multi_thread")]
async fn hung_sink_does_not_wedge_later_ticks() {
    // Every test in the tree supplies its own short ceiling, so nothing
    // else would notice if the shipped default were changed to something
    // that times out every production delivery.
    assert_eq!(
        dispatcher::SinkDispatcherConfig::default().deliver_timeout,
        dispatcher::DELIVER_TIMEOUT,
        "the shipped dispatcher must default to the documented ceiling",
    );

    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let hung_id = SinkId::new("webhook", "ahung").unwrap();
    let fast_id = SinkId::new("webhook", "zfast").unwrap();
    // 0 permits — parks forever, and this test never releases it.
    let hung = Arc::new(GatedSink::new(hung_id.clone(), 0));
    let fast = Arc::new(GatedSink::new(fast_id.clone(), 128));

    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![hung.clone(), fast.clone()]);

    let first = sample_alert(1, "rule.hang");
    store
        .record_event_and_enqueue(&first, &[hung_id.as_str(), fast_id.as_str()])
        .await
        .expect("enqueue");
    let first_id = first.event_id.to_string();

    // Long enough that the second alert is provably enqueued while the
    // first tick is still parked inside `deliver()`, short enough to keep
    // the test quick. Production uses `dispatcher::DELIVER_TIMEOUT`.
    let cfg = dispatcher::SinkDispatcherConfig {
        deliver_timeout: Duration::from_secs(2),
        ..test_cfg()
    };

    let health = Arc::new(DispatcherHealth::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let policy: Arc<dyn DeliveryPolicy> = Arc::new(AllowAllPolicy);
    let loop_handle = tokio::spawn(dispatcher::run_dispatcher(
        cfg,
        store.clone(),
        registry.clone(),
        policy,
        health.clone(),
        None,
        async {
            let _ = shutdown_rx.await;
        },
    ));

    // Wait until the hung sink is genuinely parked inside `deliver()`, so
    // the row enqueued below cannot have been in the batch this tick is
    // already draining.
    assert!(
        wait_until(Duration::from_secs(10), {
            let hung = hung.clone();
            move || hung.calls() >= 1
        })
        .await,
        "the hung sink should have been called at least once",
    );

    let second = sample_alert(1, "rule.after");
    store
        .record_event_and_enqueue(&second, &[fast_id.as_str()])
        .await
        .expect("enqueue");
    let second_id = second.event_id.to_string();

    // THE assertion. Pre-fix this times out: tick 1 never returns, so
    // `outbox_pending` is never called again and this row is never seen.
    assert!(
        wait_for_status(
            &store,
            &second_id,
            fast_id.as_str(),
            OutboxStatus::Sent,
            Duration::from_secs(20),
        )
        .await,
        "an alert enqueued while another sink is hung must still be \
         delivered; hung.calls={}, fast.calls={}",
        hung.calls(),
        fast.calls(),
    );

    // And the hung row must carry the evidence. The production signature
    // of this bug was `attempts = 0, last_error IS NULL` on every pending
    // row: status is written only after `deliver()` returns, so a hang
    // left no trace in the outbox, in the logs, or anywhere else.
    let hung_row = row_for(&store, &first_id, hung_id.as_str()).await;
    assert!(
        hung_row.attempts >= 1,
        "a timed-out delivery must consume an attempt, got {}",
        hung_row.attempts,
    );
    assert!(
        hung_row
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("timed out")),
        "a timed-out delivery must record why, got {:?}",
        hung_row.last_error,
    );

    // Graceful shutdown was wedged by the same join: the `select!` arm
    // that observes `shutdown` cannot be polled while `tick` is awaited.
    let _ = shutdown_tx.send(());
    assert!(
        tokio::time::timeout(Duration::from_secs(10), loop_handle)
            .await
            .is_ok(),
        "the dispatcher must still shut down while a sink is hung",
    );
}
