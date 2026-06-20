//! SureView Ops "SMTP / Email Alarms" `AlertSink`.
//!
//! Triggers a SureView alarm point by sending one email per alert to
//! the alarm point's unique receiver address, per the SureView Ops
//! *SMTP Alarms* reference:
//! <https://help.sureviewops.com/hc/en-us/articles/13211794487837-Smtp-alarms-Email-Alarms>.
//!
//! Unlike the HTTP Alarms sink there is **no account API key** — the
//! destination address itself identifies the alarm point. SureView
//! reads each message as:
//!   * `To` — the alarm point's unique address (per-camera override or
//!     the sink default).
//!   * `From` — any syntactically valid address (SureView ignores it;
//!     SMTP just requires one).
//!   * `Subject` — the operator-facing alarm message.
//!   * body — optional; a decimal `"latitude,longitude"` pair
//!     auto-plots the alarm on the SureView map.
//!   * parts — optional clip (MP4) / snapshot (JPG) attachments.
//!
//! Contract (one logical exchange per `deliver()` call; the
//! dispatcher owns retry/backoff):
//!   * Connect to the regional relay (`us-smtp` / `eu-smtp`) on the
//!     submission port (default 587), optionally upgrade with
//!     STARTTLS, optionally AUTH, then send one message.
//!   * Outcome mapping: a permanent SMTP reply (5xx) → `Permanent`;
//!     everything else (4xx, timeout, TLS, network) → `Transient`.
//!
//! Behind the `sureview-email` cargo feature so deployments that
//! don't talk to SureView over SMTP skip the lettre + rustls SMTP
//! stack. Independent of the HTTP `sureview` feature.

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, trace, warn};

use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};

use nexus_config::SureViewEmailSinkConfig;
use nexus_types::AlertEvent;

use crate::{AlertSink, SinkError, SinkHealth, SinkId};

/// Discriminator string for `SinkId::kind()`. Stable wire value
/// stored in every `alert_sink_outbox.sink_id` column — DO NOT
/// rename without a migration that rewrites historical rows.
pub const KIND: &str = "sureview_email";

/// Cap on an attached snapshot (JPG). Snapshots are small; anything
/// larger is almost certainly the wrong file, so skip it rather than
/// risk a 552 "message too large" reply.
const MAX_SNAPSHOT_BYTES: u64 = 5 * 1024 * 1024;

/// Cap on an attached motion clip (MP4). SMTP relays commonly reject
/// messages over ~25 MB; we stay well under and skip oversized clips
/// (the alarm still fires, just without the clip attached).
const MAX_CLIP_BYTES: u64 = 20 * 1024 * 1024;

/// SureView "SMTP / Email Alarms" wire details, isolated so the
/// message shape stays in one place and stays unit-testable without
/// an SMTP server.
pub mod api {
    use nexus_types::{AlertEvent, Severity};

    /// Operator-facing alarm `Subject` built from the alert.
    pub fn alarm_subject(event: &AlertEvent) -> String {
        format!(
            "Nexus: {} on camera {} ({} priority)",
            event.label,
            event.camera_id,
            severity_word(event.severity),
        )
    }

    /// Plain-text alarm body. When `location` is set its decimal
    /// `"latitude,longitude"` rides the first line so SureView
    /// auto-plots the alarm on its map; human-readable detail follows.
    pub fn alarm_body(event: &AlertEvent, location: Option<&str>) -> String {
        let mut body = String::new();
        if let Some(loc) = location {
            body.push_str(loc);
            body.push_str("\n\n");
        }
        body.push_str("Nexus Edge AI alert\n");
        body.push_str(&format!("Camera: {}\n", event.camera_id));
        body.push_str(&format!("Label: {}\n", event.label));
        body.push_str(&format!("Severity: {}\n", severity_word(event.severity)));
        body.push_str(&format!("Time: {}\n", event.captured_at.to_rfc3339()));
        body
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

/// SureView Ops "SMTP / Email Alarms" sink. One email per `deliver()`
/// call to the alarm point's receiver address; the dispatcher owns
/// retry + backoff.
pub struct SureViewEmailSink {
    id: SinkId,
    cfg: SureViewEmailSinkConfig,
    /// Pre-parsed envelope `From` mailbox.
    from: Mailbox,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SureViewEmailSink {
    /// Build a SureView email sink from its TOML config. Returns
    /// `Permanent` on misconfiguration (bad name, bad sender address,
    /// invalid TLS builder); the caller surfaces this at engine boot
    /// before the dispatcher ever spins.
    pub fn new(cfg: &SureViewEmailSinkConfig) -> Result<Self, SinkError> {
        let id = SinkId::new(KIND, &cfg.name).ok_or_else(|| {
            SinkError::Permanent(format!(
                "invalid sureview_email sink name '{}' (empty or contains ':')",
                cfg.name
            ))
        })?;
        let from: Mailbox = cfg.from_address.parse().map_err(|e| {
            SinkError::Permanent(format!(
                "sureview_email sink '{}' from_address '{}': {e}",
                cfg.name, cfg.from_address
            ))
        })?;
        let transport = Self::build_transport(cfg)?;
        Ok(Self {
            id,
            cfg: cfg.clone(),
            from,
            transport,
        })
    }

    /// Construct the lettre SMTP transport from the config: STARTTLS
    /// relay by default, plaintext (`builder_dangerous`) only when the
    /// operator opts out of TLS; optional AUTH for a fronting relay.
    fn build_transport(
        cfg: &SureViewEmailSinkConfig,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, SinkError> {
        let host = cfg.resolved_smtp_host();
        let mut builder = if cfg.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| {
                SinkError::Permanent(format!(
                    "sureview_email sink '{}' starttls relay '{host}': {e}",
                    cfg.name
                ))
            })?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        };
        builder = builder
            .port(cfg.smtp_port)
            .timeout(Some(Duration::from_secs(cfg.timeout_secs)));
        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        Ok(builder.build())
    }

    /// Build the outgoing message for one alert. Pulled out so the
    /// header / body shape is testable without an SMTP server; the
    /// async attachment reads happen in `deliver` and are passed in.
    fn build_message(
        &self,
        event: &AlertEvent,
        attachments: Vec<SinglePart>,
    ) -> Result<Message, SinkError> {
        let to: Mailbox = self
            .cfg
            .alarm_email_for(event.camera_id)
            .parse()
            .map_err(|e| {
                SinkError::Permanent(format!(
                    "sureview_email sink '{}' alarm address '{}': {e}",
                    self.cfg.name,
                    self.cfg.alarm_email_for(event.camera_id)
                ))
            })?;
        let builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(api::alarm_subject(event));
        let body = api::alarm_body(event, self.cfg.location.as_deref());
        let message = if attachments.is_empty() {
            builder
                .header(ContentType::TEXT_PLAIN)
                .body(body)
                .map_err(|e| SinkError::Permanent(format!("build message: {e}")))?
        } else {
            let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(body));
            for part in attachments {
                multipart = multipart.singlepart(part);
            }
            builder
                .multipart(multipart)
                .map_err(|e| SinkError::Permanent(format!("build message: {e}")))?
        };
        Ok(message)
    }

    /// Read the alert's configured attachments best-effort. A missing,
    /// unreadable, or oversized file is logged and omitted — it never
    /// fails the alarm.
    async fn collect_attachments(&self, event: &AlertEvent) -> Vec<SinglePart> {
        let mut parts = Vec::new();
        if self.cfg.attach_snapshot {
            if let Some(path) = &event.artifacts.snapshot {
                if let Some(part) = read_attachment(path, MAX_SNAPSHOT_BYTES).await {
                    parts.push(part);
                }
            }
        }
        if self.cfg.attach_clip {
            if let Some(path) = &event.artifacts.clip {
                if let Some(part) = read_attachment(path, MAX_CLIP_BYTES).await {
                    parts.push(part);
                }
            }
        }
        parts
    }

    /// Classify a lettre SMTP error. A permanent SMTP reply (5xx —
    /// e.g. a rejected recipient) is operator-actionable; everything
    /// else (transient 4xx, timeout, TLS, network) is retryable, which
    /// is exactly what the dispatcher's exponential backoff is for.
    fn classify_send_err(cfg_name: &str, err: lettre::transport::smtp::Error) -> SinkError {
        if err.is_permanent() {
            SinkError::Permanent(format!("sureview_email '{cfg_name}' send: {err}"))
        } else {
            SinkError::Transient(format!("sureview_email '{cfg_name}' send: {err}"))
        }
    }
}

#[async_trait]
impl AlertSink for SureViewEmailSink {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn id(&self) -> &SinkId {
        &self.id
    }

    async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError> {
        let attachments = self.collect_attachments(event).await;
        let message = self.build_message(event, attachments)?;
        trace!(sink = %self.id, event = %event.event_id, "sureview email alarm send");

        match self.transport.send(message).await {
            Ok(resp) => {
                debug!(sink = %self.id, event = %event.event_id,
                       code = ?resp.code(), "delivered");
                Ok(())
            }
            Err(e) => {
                let err = Self::classify_send_err(&self.cfg.name, e);
                warn!(sink = %self.id, event = %event.event_id,
                      transient = err.is_transient(), "sureview email alarm send failed");
                Err(err)
            }
        }
    }

    fn health(&self) -> SinkHealth {
        SinkHealth::Unknown
    }
}

/// MIME content type for an attachment, inferred from its file
/// extension. Falls back to `text/plain` for unknown types (lettre
/// requires a parseable content type; the static strings here always
/// parse).
fn content_type_for(path: &str) -> ContentType {
    let lower = path.to_ascii_lowercase();
    let mime = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    };
    ContentType::parse(mime).unwrap_or(ContentType::TEXT_PLAIN)
}

/// Read one attachment from disk, capped at `max_bytes`. Returns
/// `None` (with a log line) on any problem so the caller can send the
/// alarm without it.
async fn read_attachment(path: &str, max_bytes: u64) -> Option<SinglePart> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > max_bytes => {
            warn!(
                path,
                size = meta.len(),
                max = max_bytes,
                "attachment too large, skipping"
            );
            return None;
        }
        Ok(_) => {}
        Err(e) => {
            debug!(path, error = %e, "attachment not available, skipping");
            return None;
        }
    }
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path, error = %e, "attachment read failed, skipping");
            return None;
        }
    };
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("attachment")
        .to_string();
    Some(Attachment::new(filename).body(bytes, content_type_for(path)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::SureViewRegion;
    use nexus_types::Severity;

    fn cfg(name: &str) -> SureViewEmailSinkConfig {
        SureViewEmailSinkConfig {
            name: name.to_string(),
            region: SureViewRegion::Us,
            smtp_host: None,
            smtp_port: 587,
            starttls: true,
            from_address: "nexus-edge@localhost".to_string(),
            alarm_email: "8nrawg1sxc@us.sureviewops.com".to_string(),
            alarm_emails: Default::default(),
            location: None,
            attach_snapshot: false,
            attach_clip: false,
            username: None,
            password: None,
            timeout_secs: 15,
        }
    }

    fn sink(name: &str) -> SureViewEmailSink {
        SureViewEmailSink::new(&cfg(name)).unwrap()
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
        assert!(SureViewEmailSink::new(&c).is_err());
    }

    #[test]
    fn new_rejects_bad_from_address() {
        let mut c = cfg("ok");
        c.from_address = "not-an-email".to_string();
        assert!(SureViewEmailSink::new(&c).is_err());
    }

    #[test]
    fn sink_id_is_kind_qualified() {
        assert_eq!(sink("siteX").id().as_str(), "sureview_email:siteX");
        assert_eq!(sink("siteX").id().kind(), "sureview_email");
        assert_eq!(sink("siteX").id().name(), "siteX");
    }

    #[test]
    fn plaintext_relay_builds_without_tls() {
        let mut c = cfg("plain");
        c.starttls = false;
        assert!(SureViewEmailSink::new(&c).is_ok());
    }

    #[test]
    fn subject_carries_label_and_camera() {
        let s = api::alarm_subject(&sample_event());
        assert!(s.contains("person"));
        assert!(s.contains("camera 42"));
        assert!(s.contains("high"));
    }

    #[test]
    fn body_without_location_omits_coords() {
        let body = api::alarm_body(&sample_event(), None);
        assert!(body.contains("Camera: 42"));
        assert!(body.contains("Label: person"));
        // No leading coordinate line.
        assert!(body.starts_with("Nexus Edge AI alert"));
    }

    #[test]
    fn body_with_location_leads_with_coords() {
        let body = api::alarm_body(&sample_event(), Some("51.650646,-3.914983"));
        // Decimal lat,lng on the first line so SureView auto-plots it.
        assert!(body.starts_with("51.650646,-3.914983\n"));
        assert!(body.contains("Camera: 42"));
    }

    #[test]
    fn message_to_is_default_alarm_address() {
        let s = sink("siteX");
        let msg = s.build_message(&sample_event(), Vec::new()).unwrap();
        let formatted = String::from_utf8(msg.formatted()).unwrap();
        assert!(formatted.contains("8nrawg1sxc@us.sureviewops.com"));
        assert!(formatted.contains("Subject: Nexus: person on camera 42"));
    }

    #[test]
    fn message_to_honours_per_camera_override() {
        let mut c = cfg("siteX");
        c.alarm_emails.insert(
            "42".to_string(),
            "cam42point@us.sureviewops.com".to_string(),
        );
        let s = SureViewEmailSink::new(&c).unwrap();
        let msg = s.build_message(&sample_event(), Vec::new()).unwrap();
        let formatted = String::from_utf8(msg.formatted()).unwrap();
        assert!(formatted.contains("cam42point@us.sureviewops.com"));
    }

    #[test]
    fn content_type_inference() {
        assert_eq!(
            content_type_for("/x/a.jpg"),
            ContentType::parse("image/jpeg").unwrap()
        );
        assert_eq!(
            content_type_for("/x/a.JPEG"),
            ContentType::parse("image/jpeg").unwrap()
        );
        assert_eq!(
            content_type_for("/x/a.mp4"),
            ContentType::parse("video/mp4").unwrap()
        );
        assert_eq!(
            content_type_for("/x/a.bin"),
            ContentType::parse("application/octet-stream").unwrap()
        );
    }

    #[tokio::test]
    async fn read_attachment_skips_missing_file() {
        assert!(read_attachment("/no/such/file.jpg", MAX_SNAPSHOT_BYTES)
            .await
            .is_none());
    }
}
