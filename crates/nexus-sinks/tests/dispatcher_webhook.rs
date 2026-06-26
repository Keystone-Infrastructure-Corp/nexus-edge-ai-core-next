//! M7 Phase 1 Step 5 — end-to-end retry / backoff / dead-letter against
//! a flaky HTTP endpoint, driven through the real [`dispatcher`] state
//! machine and a real [`WebhookSink`].
//!
//! Where `tests/dispatcher.rs` exercises the state machine against a
//! hand-rolled `ScriptedSink`, and `tests/webhook.rs` exercises one
//! `WebhookSink::deliver()` call at a time, this suite wires the two
//! together: a live `wiremock` server stands in for an operator's
//! webhook endpoint, the genuine `WebhookSink` POSTs to it, and the
//! dispatcher decides retry-vs-dead based on the HTTP status the
//! server returns.
//!
//! We drive [`dispatcher::process_row`] directly in a loop rather than
//! booting [`dispatcher::run_dispatcher`], re-fetching the outbox row
//! between passes. Because `process_row` acts on whatever row it is
//! handed (the `next_attempt_at` backoff gate lives in
//! `outbox_pending`, the timer-loop's query), this reproduces the full
//! retry sequence with zero wall-clock sleeps — the same property the
//! `ScriptedSink` tests rely on.
//!
//! | Test                          | Branch                                    |
//! |-------------------------------|-------------------------------------------|
//! | `flaky_then_recovers`         | 503, 503, 200 → Sent after 3 attempts     |
//! | `permanently_flaky_dead_letters` | always 503 → Dead at MAX_ATTEMPTS      |

#![cfg(feature = "webhook")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::Utc;
use nexus_config::{CameraConfig, StoreConfig, WebhookSinkConfig};
use nexus_sinks::backoff::MAX_ATTEMPTS;
use nexus_sinks::dispatcher::{self, AllowAllPolicy};
use nexus_sinks::webhook::WebhookSink;
use nexus_sinks::{SinkId, SinkRegistry};
use nexus_store::{OutboxRow, OutboxStatus, Store};
use nexus_types::{AlertEvent, Artifacts, Severity};
use std::collections::HashMap;
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures (kept in sync with tests/dispatcher.rs)
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
            ..Default::default()
        },
        onvif: Default::default(),
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
        trace_id: "trace-disp-webhook".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
    }
}

fn webhook_cfg(name: &str, url: Url) -> WebhookSinkConfig {
    WebhookSinkConfig {
        name: name.into(),
        url,
        headers: HashMap::new(),
        hmac_secret: None,
        timeout_secs: 5,
    }
}

/// Enqueue one event routed at `sink_id`, returning the pending row.
async fn enqueue_one(store: &Arc<Store>, camera_id: i64, rule: &str, sink_id: &str) -> OutboxRow {
    let alert = sample_alert(camera_id, rule);
    store
        .record_event_and_enqueue(&alert, &[sink_id])
        .await
        .expect("enqueue");
    store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .expect("outbox_for_event")
        .into_iter()
        .next()
        .expect("one outbox row")
}

/// Drive `process_row` until the row leaves `Pending`, re-fetching
/// between passes. Bounded well above `MAX_ATTEMPTS` so a genuine
/// non-terminating loop fails the test instead of hanging.
async fn drain(store: &Arc<Store>, registry: &Arc<SinkRegistry>, mut row: OutboxRow) -> OutboxRow {
    for _ in 0..(MAX_ATTEMPTS + 4) {
        if row.status != OutboxStatus::Pending {
            break;
        }
        dispatcher::process_row(store, registry, &AllowAllPolicy, row.clone()).await;
        row = store
            .outbox_for_event(&row.event_id)
            .await
            .expect("re-fetch")
            .into_iter()
            .next()
            .expect("row still present");
    }
    row
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A flaky endpoint that 503s on the first two POSTs then 200s.
/// The dispatcher should treat the 503s as transient, retry, and
/// land on `Sent` with `attempts == 3` once the endpoint recovers.
#[tokio::test]
async fn flaky_then_recovers() {
    let server = MockServer::start().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_resp = hits.clone();
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(move |_req: &Request| {
            let n = hits_resp.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&server)
        .await;

    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let id = SinkId::new("webhook", "flaky").unwrap();
    let sink = Arc::new(WebhookSink::new(&webhook_cfg("flaky", url)).unwrap());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink]);

    let row = enqueue_one(&store, 1, "rule.flaky", id.as_str()).await;
    let after = drain(&store, &registry, row).await;

    assert_eq!(after.status, OutboxStatus::Sent);
    assert_eq!(after.attempts, 3);
    assert!(after.delivered_at.is_some());
    assert!(after.last_error.is_none());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "endpoint hit once per attempt"
    );
}

/// A permanently-broken endpoint (always 503). Each 503 is transient,
/// so the dispatcher retries until `attempts == MAX_ATTEMPTS`, at which
/// point `backoff_for` returns `None` and the row is dead-lettered.
#[tokio::test]
async fn permanently_flaky_dead_letters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let id = SinkId::new("webhook", "down").unwrap();
    let sink = Arc::new(WebhookSink::new(&webhook_cfg("down", url)).unwrap());
    let registry = Arc::new(SinkRegistry::new());
    registry.replace(vec![sink]);

    let row = enqueue_one(&store, 1, "rule.down", id.as_str()).await;
    let after = drain(&store, &registry, row).await;

    assert_eq!(after.status, OutboxStatus::Dead);
    assert_eq!(after.attempts, MAX_ATTEMPTS as i64);
    assert!(after.next_attempt_at.is_none());
    assert!(after.last_error.as_deref().unwrap().contains("transient"));
    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        MAX_ATTEMPTS as usize,
        "endpoint hit once per attempt up to the dead-letter threshold"
    );
}
