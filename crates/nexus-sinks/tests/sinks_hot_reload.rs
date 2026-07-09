//! M7 Phase 1 Step 5 — hot reload flips a sink's behaviour immediately,
//! with no process restart.
//!
//! When an operator edits a sink through `PUT /v1/admin/sinks/config/
//! {kind}/{name}` the api handler upserts the row and publishes the
//! `sink.config.changed` bus topic. The engine's `sinks_reload` task
//! (in the binary-only `nexus-engine` crate, hence not reachable from
//! an external integration test) reacts by re-reading the persisted
//! sink rows and running exactly this sequence:
//!
//! ```text
//! let json = store.alert_sinks_list();        // db rows
//! let sinks = build_effective_sinks(file, &json)?;
//! registry.replace(sinks);                    // atomic ArcSwap
//! ```
//!
//! The dispatcher holds the SAME `Arc<SinkRegistry>` for its whole
//! lifetime and resolves the target sink via `registry.get(&id)` on
//! every row, so swapping the registry contents re-points delivery
//! instantly — no dispatcher, store, or socket is recreated. This
//! test reproduces that swap directly against two live `wiremock`
//! endpoints and proves a row enqueued after the reload is delivered
//! to the NEW endpoint.

#![cfg(feature = "webhook")]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use nexus_config::{CameraConfig, StoreConfig};
use nexus_sinks::build_effective_sinks;
use nexus_sinks::dispatcher::{self, AllowAllPolicy};
use nexus_sinks::{SinkId, SinkRegistry};
use nexus_store::{OutboxRow, OutboxStatus, Store};
use nexus_types::{AlertEvent, Artifacts, Severity};
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
        trace_id: "trace-hot-reload".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
        frame_w: 0,
        frame_h: 0,
    }
}

/// The `config_json` blob a `PUT /v1/admin/sinks/config/webhook/hot`
/// persists — a webhook sink named `hot` pointed at `endpoint`.
fn webhook_json(endpoint: &str) -> String {
    format!(r#"{{"kind":"webhook","name":"hot","url":"{endpoint}/hook","timeout_secs":5}}"#)
}

/// Enqueue one event routed at `sink_id`, deliver it once through the
/// dispatcher, and return the terminal outbox row.
async fn enqueue_and_deliver(
    store: &Arc<Store>,
    registry: &Arc<SinkRegistry>,
    camera_id: i64,
    rule: &str,
    sink_id: &str,
) -> OutboxRow {
    let alert = sample_alert(camera_id, rule);
    store
        .record_event_and_enqueue(&alert, &[sink_id])
        .await
        .expect("enqueue");
    let row = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .expect("outbox_for_event")
        .into_iter()
        .next()
        .expect("one outbox row");
    dispatcher::process_row(store, registry, &AllowAllPolicy, row.clone()).await;
    store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .expect("re-fetch")
        .into_iter()
        .next()
        .expect("row still present")
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hot_reload_repoints_sink_without_restart() {
    // Two operator endpoints. `hot` initially points at A; a reload
    // re-points it at B.
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    for srv in [&server_a, &server_b] {
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(srv)
            .await;
    }

    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let registry = Arc::new(SinkRegistry::new());
    let sink_id = SinkId::new("webhook", "hot").unwrap();

    // --- initial config: webhook:hot → server A -----------------------
    let sinks = build_effective_sinks(&[], &[webhook_json(&server_a.uri())]).unwrap();
    assert_eq!(registry.replace(sinks), 1);

    let first = enqueue_and_deliver(&store, &registry, 1, "rule.a", sink_id.as_str()).await;
    assert_eq!(first.status, OutboxStatus::Sent);
    assert_eq!(server_a.received_requests().await.unwrap().len(), 1);
    assert_eq!(server_b.received_requests().await.unwrap().len(), 0);

    // --- hot reload: webhook:hot → server B (same registry Arc) -------
    // This is the precise sequence the engine's `sinks_reload` task
    // runs when the api handler publishes `sink.config.changed`.
    let sinks = build_effective_sinks(&[], &[webhook_json(&server_b.uri())]).unwrap();
    assert_eq!(registry.replace(sinks), 1);

    let second = enqueue_and_deliver(&store, &registry, 1, "rule.b", sink_id.as_str()).await;
    assert_eq!(second.status, OutboxStatus::Sent);

    // Delivery flipped to B immediately; A saw no further traffic.
    assert_eq!(
        server_a.received_requests().await.unwrap().len(),
        1,
        "old endpoint must not receive post-reload traffic"
    );
    assert_eq!(
        server_b.received_requests().await.unwrap().len(),
        1,
        "new endpoint receives the row enqueued after the reload"
    );
}
