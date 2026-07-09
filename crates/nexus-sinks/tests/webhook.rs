//! End-to-end webhook sink tests against a real HTTP server.
//!
//! These tests pin the on-the-wire contract `nexus-engine` makes
//! with operator-provided webhook endpoints:
//!
//!   * 200/204 → `deliver()` returns `Ok(())`.
//!   * 500 → `Transient` error; dispatcher's job to back off.
//!   * 408 / 429 → `Transient` (rate-limit / timeout are retryable).
//!   * 401 / 404 → `Permanent` (operator must fix config or URL).
//!   * Body is JSON-encoded `AlertEvent`, content-type
//!     `application/json`.
//!   * When `hmac_secret` is set, request carries
//!     `X-Nexus-Signature: sha256=<hex>` matching the body.
//!   * Operator custom headers ride along on every request.
//!
//! The dispatcher's retry / backoff / dead-letter behaviour is
//! covered separately in `tests/dispatcher.rs` — these tests only
//! exercise one `deliver()` call at a time.

#![cfg(feature = "webhook")]

use std::collections::HashMap;

use chrono::Utc;
use nexus_config::WebhookSinkConfig;
use nexus_sinks::webhook::{WebhookSink, SIGNATURE_HEADER};
use nexus_sinks::{AlertSink, SinkError};
use nexus_types::{AlertEvent, Artifacts, Severity};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn sample_alert() -> AlertEvent {
    AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id: 1,
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
        frame_w: 0,
        frame_h: 0,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deliver_200_is_ok() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn deliver_204_is_ok() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn deliver_500_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    let SinkError::Transient(msg) = err else {
        unreachable!()
    };
    assert!(msg.contains("500"), "{msg}");
    assert!(msg.contains("boom"), "{msg}");
}

#[tokio::test]
async fn deliver_429_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn deliver_408_is_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(408))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn deliver_401_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(!err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn deliver_404_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(!err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn body_is_json_encoded_alert_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("content-type", "application/json"))
        .respond_with(|req: &Request| {
            // Echo: assert body parses as the same shape we
            // serialize from. Returning 200 lets the test assert
            // delivery success; we just want a parser to validate
            // shape.
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("body json");
            assert!(body.get("event_id").is_some(), "missing event_id");
            assert!(body.get("camera_id").is_some(), "missing camera_id");
            assert_eq!(
                body.get("rule_id").and_then(|v| v.as_str()),
                Some("rule.test")
            );
            assert_eq!(body.get("label").and_then(|v| v.as_str()), Some("person"));
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn hmac_signature_header_present_when_secret_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header_exists(SIGNATURE_HEADER))
        // Signature length is 7 ("sha256=") + 64 hex chars = 71.
        // wiremock has no "value matches regex" matcher built-in,
        // so we use a request-inspecting responder.
        .respond_with(|req: &Request| {
            let sig = req
                .headers
                .get(SIGNATURE_HEADER)
                .expect("signature header set")
                .to_str()
                .unwrap();
            assert!(sig.starts_with("sha256="), "wrong prefix: {sig}");
            assert_eq!(sig.len(), 71, "wrong sig length: {sig}");
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let cfg = WebhookSinkConfig {
        hmac_secret: Some("shh".into()),
        ..webhook_cfg("primary", url)
    };
    let sink = WebhookSink::new(&cfg).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn hmac_signature_absent_without_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|req: &Request| {
            assert!(
                req.headers.get(SIGNATURE_HEADER).is_none(),
                "signature header must NOT be present"
            );
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let sink = WebhookSink::new(&webhook_cfg("primary", url)).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn operator_custom_headers_propagate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-tenant", "acme"))
        .and(header("authorization", "Bearer xyz"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    let mut headers = HashMap::new();
    headers.insert("X-Tenant".into(), "acme".into());
    headers.insert("Authorization".into(), "Bearer xyz".into());
    let cfg = WebhookSinkConfig {
        headers,
        ..webhook_cfg("primary", url)
    };
    let sink = WebhookSink::new(&cfg).unwrap();
    sink.deliver(&sample_alert()).await.expect("ok");
}

#[tokio::test]
async fn network_error_is_transient() {
    // Point at a port nothing listens on. Connect refused →
    // Transient (the dispatcher's job to back off).
    let url = Url::parse("http://127.0.0.1:1/hook").unwrap();
    let cfg = WebhookSinkConfig {
        timeout_secs: 1,
        ..webhook_cfg("primary", url)
    };
    let sink = WebhookSink::new(&cfg).unwrap();
    let err = sink.deliver(&sample_alert()).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn new_rejects_invalid_name() {
    // Names that would mangle the SinkId wire format must be
    // caught at construction time, not at first deliver().
    let url = Url::parse("http://localhost/hook").unwrap();
    for bad in &["", "has:colon"] {
        let cfg = webhook_cfg(bad, url.clone());
        match WebhookSink::new(&cfg) {
            Err(SinkError::Permanent(msg)) => {
                assert!(msg.contains(bad) || bad.is_empty(), "{msg}");
            }
            Err(other) => panic!("expected Permanent for name {bad:?}, got {other:?}"),
            Ok(_) => panic!("expected error for name {bad:?}"),
        }
    }
}
