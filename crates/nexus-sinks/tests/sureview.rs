//! End-to-end SureView Ops "HTTP Alarms" sink tests against a real
//! HTTP server.
//!
//! These pin the on-the-wire contract `nexus-engine` makes with a
//! SureView Ops receiver (per
//! <https://help.sureviewops.com/hc/en-us/articles/13213264758557-Http-Alarms>):
//!
//!   * `POST {receiver}` with body `{ "systemIdentifier", "text",
//!     "location"? }` and header `Authorization: <base64(api_key)>`.
//!   * 200 → `deliver()` returns `Ok(())`.
//!   * 500 / 408 / 429 → `Transient` (dispatcher backs off).
//!   * 401 / 404 → `Permanent` (operator must fix the API key / URL).
//!   * Per-camera `system_identifiers` override addresses the right
//!     alarm point.
//!   * Optional `location` rides along when configured.
//!
//! The dispatcher's retry / backoff / dead-letter behaviour is
//! covered separately in `tests/dispatcher.rs`.

#![cfg(feature = "sureview")]

use chrono::Utc;
use nexus_config::{SureViewRegion, SureViewSinkConfig};
use nexus_sinks::sureview::{api, SureViewSink};
use nexus_sinks::{AlertSink, SinkError};
use nexus_types::{AlertEvent, Artifacts, Severity};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const API_KEY: &str = "acct-key-xyz";
const SYSTEM_ID: &str = "9z5138b9ak";

fn alert_on_camera(camera_id: i64) -> AlertEvent {
    AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id,
        rule_id: "rule.test".into(),
        track_id: None,
        label: "person".into(),
        severity: Severity::Medium,
        bbox: None,
        frame_id: 42,
        captured_at: Utc::now(),
        trace_id: Uuid::now_v7().to_string(),
        artifacts: Artifacts::default(),
        context: Default::default(),
    }
}

fn sample_alert() -> AlertEvent {
    alert_on_camera(1)
}

fn sureview_cfg(name: &str, endpoint: Url) -> SureViewSinkConfig {
    SureViewSinkConfig {
        name: name.into(),
        region: SureViewRegion::Us,
        endpoint: Some(endpoint),
        api_key: API_KEY.into(),
        system_identifier: SYSTEM_ID.into(),
        system_identifiers: Default::default(),
        location: None,
        timeout_secs: 5,
    }
}

fn receiver(server: &MockServer) -> Url {
    Url::parse(&format!("{}/receiver", server.uri())).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deliver_200_posts_authorized_alarm() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .and(header("content-type", "application/json"))
        .and(header("authorization", api::auth_header(API_KEY).as_str()))
        .and(body_partial_json(
            serde_json::json!({ "systemIdentifier": SYSTEM_ID }),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn deliver_500_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    match sink.deliver(&sample_alert()).await {
        Err(SinkError::Transient(_)) => {}
        other => panic!("expected Transient, got {other:?}"),
    }
}

#[tokio::test]
async fn deliver_408_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .respond_with(ResponseTemplate::new(408))
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    match sink.deliver(&sample_alert()).await {
        Err(SinkError::Transient(_)) => {}
        other => panic!("expected Transient, got {other:?}"),
    }
}

#[tokio::test]
async fn deliver_429_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    match sink.deliver(&sample_alert()).await {
        Err(SinkError::Transient(_)) => {}
        other => panic!("expected Transient, got {other:?}"),
    }
}

#[tokio::test]
async fn deliver_401_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    match sink.deliver(&sample_alert()).await {
        Err(SinkError::Permanent(_)) => {}
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn deliver_404_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let sink = SureViewSink::new(&sureview_cfg("primary", receiver(&server))).unwrap();
    match sink.deliver(&sample_alert()).await {
        Err(SinkError::Permanent(_)) => {}
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn per_camera_override_addresses_its_alarm_point() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .and(body_partial_json(
            serde_json::json!({ "systemIdentifier": "cam7-point" }),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut cfg = sureview_cfg("primary", receiver(&server));
    cfg.system_identifiers
        .insert("7".into(), "cam7-point".into());
    let sink = SureViewSink::new(&cfg).unwrap();
    sink.deliver(&alert_on_camera(7)).await.expect("ok");
}

#[tokio::test]
async fn location_is_sent_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/receiver"))
        .and(body_partial_json(
            serde_json::json!({ "location": "27.947380,-82.460741" }),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut cfg = sureview_cfg("primary", receiver(&server));
    cfg.location = Some("27.947380,-82.460741".into());
    let sink = SureViewSink::new(&cfg).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}
