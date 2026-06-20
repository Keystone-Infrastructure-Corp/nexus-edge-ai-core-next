//! SureView Ops "HTTP Alarms" `AlertSink`.
//!
//! Triggers a SureView alarm point with a single JSON POST to the
//! regional `/receiver` endpoint, per the SureView Ops *HTTP Alarms*
//! reference:
//! <https://help.sureviewops.com/hc/en-us/articles/13213264758557-Http-Alarms>.
//!
//! SureView Ops is an alarm receiver, not a media store: the POST
//! only *triggers* an alarm point (addressed by its **System
//! Identifier**). Video is NOT attached — the operator pulls
//! live/recorded video from the camera the customer has configured as
//! a SureView device — so there is no media-upload step here.
//!
//! Contract (one logical exchange per `deliver()` call; the
//! dispatcher owns retry/backoff):
//!   * `POST {receiver}` with header `Authorization: <base64(api_key)>`
//!     and body `{ "systemIdentifier", "text", "location"? }`.
//!   * Status mapping: 2xx → `Ok`, 5xx/408/429/network → `Transient`,
//!     other 4xx → `Permanent` (auth/config — e.g. a bad API key).
//!
//! Behind the `sureview` cargo feature so deployments that don't talk
//! to SureView skip reqwest + base64.

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, trace, warn};

use nexus_config::SureViewSinkConfig;
use nexus_types::AlertEvent;

use crate::{AlertSink, SinkError, SinkHealth, SinkId};

/// Discriminator string for `SinkId::kind()`. Stable wire value
/// stored in every `alert_sink_outbox.sink_id` column — DO NOT
/// rename without a migration that rewrites historical rows.
pub const KIND: &str = "sureview";

/// SureView "HTTP Alarms" wire details, isolated so the request shape
/// stays in one place. The field names and the base64 `Authorization`
/// scheme are taken verbatim from the SureView Ops HTTP Alarms docs.
pub mod api {
    use base64::Engine as _;
    use nexus_types::{AlertEvent, Severity};

    /// JSON key: the alarm point's System Identifier (required).
    pub const FIELD_SYSTEM_IDENTIFIER: &str = "systemIdentifier";
    /// JSON key: operator-facing alarm description (optional).
    pub const FIELD_TEXT: &str = "text";
    /// JSON key: `"Latitude,Longitude"` of the alarm (optional).
    pub const FIELD_LOCATION: &str = "location";

    /// Base64-encode the account API Key for the `Authorization`
    /// header, as the HTTP Alarms docs require.
    pub fn auth_header(api_key: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(api_key.as_bytes())
    }

    /// Operator-facing alarm `text` built from the alert.
    pub fn alarm_text(event: &AlertEvent) -> String {
        format!(
            "Nexus: {} on camera {} ({} priority)",
            event.label,
            event.camera_id,
            severity_word(event.severity),
        )
    }

    /// Human-readable severity word for the alarm text.
    pub fn severity_word(severity: Severity) -> &'static str {
        match severity {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

/// SureView Ops "HTTP Alarms" sink. One JSON POST per `deliver()`
/// call to the receiver endpoint; the dispatcher owns retry + backoff.
pub struct SureViewSink {
    id: SinkId,
    cfg: SureViewSinkConfig,
    /// Resolved receiver URL (region default or explicit override).
    endpoint: url::Url,
    /// Precomputed `Authorization` header value: `base64(api_key)`.
    auth_header: String,
    http: reqwest::Client,
}

impl SureViewSink {
    /// Build a SureView sink from its TOML config. Returns
    /// `Permanent` on misconfiguration (bad name, bad endpoint,
    /// invalid client builder); the caller surfaces this at engine
    /// boot before the dispatcher ever spins.
    pub fn new(cfg: &SureViewSinkConfig) -> Result<Self, SinkError> {
        let id = SinkId::new(KIND, &cfg.name).ok_or_else(|| {
            SinkError::Permanent(format!(
                "invalid sureview sink name '{}' (empty or contains ':')",
                cfg.name
            ))
        })?;
        let endpoint = cfg
            .resolved_endpoint()
            .map_err(|e| SinkError::Permanent(format!("sureview endpoint: {e}")))?;
        let auth_header = api::auth_header(&cfg.api_key);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!("nexus-edge/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| SinkError::Permanent(format!("reqwest client build: {e}")))?;
        Ok(Self {
            id,
            cfg: cfg.clone(),
            endpoint,
            auth_header,
            http,
        })
    }

    /// Build the SureView HTTP Alarms JSON body for one alert. The
    /// camera's System Identifier (per-camera override or the sink
    /// default) addresses the alarm point. Pulled out for direct
    /// unit-testing without a server.
    pub(crate) fn build_alarm_payload(&self, event: &AlertEvent) -> serde_json::Value {
        let mut body = serde_json::json!({
            api::FIELD_SYSTEM_IDENTIFIER: self.cfg.system_identifier_for(event.camera_id),
            api::FIELD_TEXT: api::alarm_text(event),
        });
        if let Some(location) = &self.cfg.location {
            body[api::FIELD_LOCATION] = serde_json::Value::String(location.clone());
        }
        body
    }

    /// Classify a reqwest transport error. Network-level failures are
    /// always transient — the dispatcher's exponential backoff is
    /// designed for exactly this class of fault.
    fn classify_send_err(err: reqwest::Error) -> SinkError {
        if err.is_builder() {
            SinkError::Permanent(format!("request build: {err}"))
        } else {
            SinkError::Transient(format!("send: {err}"))
        }
    }

    /// HTTP status → SinkError variant. Same mapping as the webhook
    /// sink: 5xx/408/429 retryable, other 4xx operator-actionable.
    pub(crate) fn classify_status(status: reqwest::StatusCode, body_preview: &str) -> SinkError {
        if status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            SinkError::Transient(format!("HTTP {status}: {body_preview}"))
        } else {
            SinkError::Permanent(format!("HTTP {status}: {body_preview}"))
        }
    }

    /// Read at most 256 chars of a response body for diagnostics.
    async fn body_preview(resp: reqwest::Response) -> String {
        resp.text()
            .await
            .map(|t| t.chars().take(256).collect::<String>())
            .unwrap_or_default()
    }
}

#[async_trait]
impl AlertSink for SureViewSink {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn id(&self) -> &SinkId {
        &self.id
    }

    async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError> {
        let payload = self.build_alarm_payload(event);
        trace!(sink = %self.id, event = %event.event_id, "sureview alarm POST");

        let resp = self
            .http
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .json(&payload)
            .send()
            .await
            .map_err(Self::classify_send_err)?;

        let status = resp.status();
        if status.is_success() {
            debug!(sink = %self.id, event = %event.event_id, %status, "delivered");
            return Ok(());
        }

        let preview = Self::body_preview(resp).await;
        let err = Self::classify_status(status, &preview);
        warn!(sink = %self.id, event = %event.event_id, %status,
              transient = err.is_transient(), "sureview alarm POST failed");
        Err(err)
    }

    fn health(&self) -> SinkHealth {
        SinkHealth::Unknown
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::SureViewRegion;
    use nexus_types::Severity;

    fn cfg(name: &str) -> SureViewSinkConfig {
        SureViewSinkConfig {
            name: name.to_string(),
            region: SureViewRegion::Us,
            endpoint: None,
            api_key: "tok-123".to_string(),
            system_identifier: "9z5138b9ak".to_string(),
            system_identifiers: Default::default(),
            location: None,
            timeout_secs: 15,
        }
    }

    fn sink(name: &str) -> SureViewSink {
        SureViewSink::new(&cfg(name)).unwrap()
    }

    fn sample_event() -> AlertEvent {
        AlertEvent {
            event_id: uuid::Uuid::nil(),
            camera_id: 42,
            rule_id: "rule-7".to_string(),
            track_id: None,
            label: "person".to_string(),
            severity: Severity::High,
            bbox: None,
            frame_id: 0,
            captured_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            trace_id: nexus_types::TraceId::default(),
            artifacts: Default::default(),
            context: serde_json::Map::new(),
        }
    }

    #[test]
    fn new_rejects_name_with_separator() {
        let mut c = cfg("ok");
        c.name = "bad:name".to_string();
        assert!(SureViewSink::new(&c).is_err());
    }

    #[test]
    fn sink_id_is_kind_qualified() {
        assert_eq!(sink("siteX").id().as_str(), "sureview:siteX");
        assert_eq!(sink("siteX").id().kind(), "sureview");
        assert_eq!(sink("siteX").id().name(), "siteX");
    }

    #[test]
    fn default_endpoint_is_region_receiver() {
        assert_eq!(
            sink("siteX").endpoint.as_str(),
            "https://us.sureviewops.com/receiver"
        );
        let mut c = cfg("eu");
        c.region = SureViewRegion::Eu;
        let s = SureViewSink::new(&c).unwrap();
        assert_eq!(s.endpoint.as_str(), "https://eu.sureviewops.com/receiver");
    }

    #[test]
    fn explicit_endpoint_overrides_region() {
        let mut c = cfg("siteX");
        c.endpoint = Some(url::Url::parse("https://mock.local/receiver").unwrap());
        let s = SureViewSink::new(&c).unwrap();
        assert_eq!(s.endpoint.as_str(), "https://mock.local/receiver");
    }

    #[test]
    fn auth_header_is_base64_of_api_key() {
        // base64("tok-123") == "dG9rLTEyMw=="
        assert_eq!(sink("siteX").auth_header, "dG9rLTEyMw==");
    }

    #[test]
    fn payload_carries_system_identifier_and_text() {
        let s = sink("siteX");
        let payload = s.build_alarm_payload(&sample_event());
        assert_eq!(payload["systemIdentifier"], "9z5138b9ak");
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("person"));
        assert!(text.contains("camera 42"));
        // No location configured → field omitted.
        assert!(payload.get("location").is_none());
    }

    #[test]
    fn per_camera_system_identifier_override_wins() {
        let mut c = cfg("siteX");
        c.system_identifiers
            .insert("42".to_string(), "cam42-point".to_string());
        let s = SureViewSink::new(&c).unwrap();
        let payload = s.build_alarm_payload(&sample_event());
        assert_eq!(payload["systemIdentifier"], "cam42-point");
    }

    #[test]
    fn location_included_when_configured() {
        let mut c = cfg("siteX");
        c.location = Some("27.947380,-82.460741".to_string());
        let s = SureViewSink::new(&c).unwrap();
        let payload = s.build_alarm_payload(&sample_event());
        assert_eq!(payload["location"], "27.947380,-82.460741");
    }

    #[test]
    fn status_5xx_is_transient_4xx_is_permanent() {
        assert!(SureViewSink::classify_status(reqwest::StatusCode::BAD_GATEWAY, "").is_transient());
        assert!(
            SureViewSink::classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "")
                .is_transient()
        );
        assert!(
            !SureViewSink::classify_status(reqwest::StatusCode::UNAUTHORIZED, "").is_transient()
        );
    }
}
