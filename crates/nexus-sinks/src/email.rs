//! Generic SMTP email `AlertSink` — "mail this alert to these people".
//!
//! This is the operator-facing email sink. It is deliberately NOT the
//! same thing as [`crate::sureview_email`], which is hard-wired to
//! SureView's alarm-point model (the destination address *is* the
//! alarm point, so there is exactly one recipient, a fixed subject
//! shape, and no HTML part). Here the operator supplies a real `to` /
//! `cc` list, an optional subject prefix they can build mailbox rules
//! on, and gets a readable HTML body with a plain-text alternative.
//!
//! The relay is whatever the site already runs — Microsoft 365,
//! Google Workspace, an Exchange connector, or an on-prem MTA.
//! Credentials never leave the appliance: they are entered through the
//! admin API, stored in the edge-resident `alert_sinks` table, and
//! redacted by `SinkConfig::redact_secrets` on every read-back.
//!
//! Contract (one logical exchange per `deliver()` call; the dispatcher
//! owns retry/backoff):
//!   * Connect to `smtp_host:smtp_port`, optionally upgrade with
//!     STARTTLS, optionally AUTH, then send one message.
//!   * Outcome mapping: a permanent SMTP reply (5xx) → `Permanent`;
//!     everything else (4xx, timeout, TLS, network) → `Transient`.
//!
//! Behind the `email` cargo feature so deployments that don't send
//! mail skip the lettre + rustls SMTP stack.

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, trace, warn};

use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{Address, AsyncTransport, Message, Tokio1Executor};

use nexus_config::EmailSinkConfig;
use nexus_types::AlertEvent;

use crate::mail_attach::{read_attachment, MAX_CLIP_BYTES, MAX_SNAPSHOT_BYTES};
use crate::{AlertSink, SinkError, SinkHealth, SinkId};

/// Discriminator string for `SinkId::kind()`. Stable wire value
/// stored in every `alert_sink_outbox.sink_id` column — DO NOT
/// rename without a migration that rewrites historical rows.
pub const KIND: &str = "email";

/// Message-shape helpers, isolated so the subject / body stay in one
/// place and stay unit-testable without an SMTP server.
pub mod body {
    use nexus_types::{AlertEvent, Severity};

    /// Human-readable severity word.
    pub fn severity_word(severity: Severity) -> &'static str {
        match severity {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }

    /// Subject line, optionally prefixed with the operator's literal
    /// tag so one relay can serve several sites and still be
    /// filterable (`"[North Yard] Nexus alert: person on camera 4 …"`).
    pub fn subject(event: &AlertEvent, prefix: Option<&str>) -> String {
        let core = format!(
            "Nexus alert: {} on camera {} ({} severity)",
            event.label,
            event.camera_id,
            severity_word(event.severity),
        );
        match prefix.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => format!("{p} {core}"),
            None => core,
        }
    }

    /// Plain-text alternative. Also the whole body on mail clients
    /// that refuse HTML.
    pub fn text(event: &AlertEvent) -> String {
        let mut out = String::new();
        out.push_str("Nexus Edge AI detected an alert.\n\n");
        out.push_str(&format!("Camera:   {}\n", event.camera_id));
        out.push_str(&format!("Detected: {}\n", event.label));
        out.push_str(&format!("Severity: {}\n", severity_word(event.severity)));
        out.push_str(&format!("Time:     {}\n", event.captured_at.to_rfc3339()));
        out.push_str(&format!("Rule:     {}\n", event.rule_id));
        out.push_str(&format!("Event ID: {}\n", event.event_id));
        out.push_str("\nThis message was sent by a Nexus Edge AI appliance on your network.\n");
        out
    }

    /// HTML alternative. Table-based and inline-styled because mail
    /// clients strip `<style>` blocks and ignore most modern CSS.
    pub fn html(event: &AlertEvent) -> String {
        let row = |k: &str, v: String| {
            format!(
                "<tr><td style=\"padding:4px 12px 4px 0;color:#666;\">{}</td>\
                 <td style=\"padding:4px 0;font-weight:600;\">{}</td></tr>",
                escape(k),
                escape(&v)
            )
        };
        format!(
            "<div style=\"font-family:-apple-system,Segoe UI,Roboto,sans-serif;font-size:14px;color:#111;\">\
             <p style=\"margin:0 0 12px;\">Nexus Edge AI detected an alert.</p>\
             <table style=\"border-collapse:collapse;\">{}{}{}{}{}{}</table>\
             <p style=\"margin:16px 0 0;color:#666;font-size:12px;\">\
             This message was sent by a Nexus Edge AI appliance on your network.</p></div>",
            row("Camera", event.camera_id.to_string()),
            row("Detected", event.label.clone()),
            row("Severity", severity_word(event.severity).to_string()),
            row("Time", event.captured_at.to_rfc3339()),
            row("Rule", event.rule_id.clone()),
            row("Event ID", event.event_id.to_string()),
        )
    }

    /// Minimal HTML entity escaping. Detector labels and rule ids are
    /// operator-supplied strings that land in an HTML document, so
    /// they must not be able to close a tag or open one.
    pub fn escape(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for c in raw.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(c),
            }
        }
        out
    }
}

/// Generic SMTP email sink. One message per `deliver()` call; the
/// dispatcher owns retry + backoff.
pub struct EmailSink {
    id: SinkId,
    cfg: EmailSinkConfig,
    /// Pre-parsed header `From` mailbox (with optional display name).
    from: Mailbox,
    /// Pre-parsed recipients, validated at construction so a typo
    /// fails at boot rather than on the first alert.
    to: Vec<Mailbox>,
    cc: Vec<Mailbox>,
    reply_to: Option<Mailbox>,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl EmailSink {
    /// Build an email sink from its config. Returns `Permanent` on
    /// misconfiguration (bad name, unparseable address, invalid TLS
    /// builder); the caller surfaces this at engine boot before the
    /// dispatcher ever spins.
    pub fn new(cfg: &EmailSinkConfig) -> Result<Self, SinkError> {
        let id = SinkId::new(KIND, &cfg.name).ok_or_else(|| {
            SinkError::Permanent(format!(
                "invalid email sink name '{}' (empty or contains ':')",
                cfg.name
            ))
        })?;
        let from = Self::mailbox(
            cfg,
            "from_address",
            &cfg.from_address,
            cfg.from_name.clone(),
        )?;
        let to = cfg
            .to
            .iter()
            .map(|a| Self::mailbox(cfg, "to", a, None))
            .collect::<Result<Vec<_>, _>>()?;
        if to.is_empty() {
            return Err(SinkError::Permanent(format!(
                "email sink '{}' has no 'to' recipients",
                cfg.name
            )));
        }
        let cc = cfg
            .cc
            .iter()
            .map(|a| Self::mailbox(cfg, "cc", a, None))
            .collect::<Result<Vec<_>, _>>()?;
        let reply_to = cfg
            .reply_to
            .as_deref()
            .map(|a| Self::mailbox(cfg, "reply_to", a, None))
            .transpose()?;
        let transport = Self::build_transport(cfg)?;
        Ok(Self {
            id,
            cfg: cfg.clone(),
            from,
            to,
            cc,
            reply_to,
            transport,
        })
    }

    /// Parse one address into a `Mailbox`, attributing a parse failure
    /// to the config field it came from so the operator can find it.
    fn mailbox(
        cfg: &EmailSinkConfig,
        field: &str,
        addr: &str,
        display_name: Option<String>,
    ) -> Result<Mailbox, SinkError> {
        let parsed: Address = addr.trim().parse().map_err(|e| {
            SinkError::Permanent(format!("email sink '{}' {field} '{addr}': {e}", cfg.name))
        })?;
        Ok(Mailbox::new(
            display_name.filter(|n| !n.trim().is_empty()),
            parsed,
        ))
    }

    /// Construct the lettre SMTP transport: STARTTLS relay by default,
    /// plaintext (`builder_dangerous`) only when the operator opts out
    /// of TLS; optional AUTH.
    fn build_transport(
        cfg: &EmailSinkConfig,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, SinkError> {
        let host = cfg.smtp_host.trim();
        let mut builder = if cfg.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| {
                SinkError::Permanent(format!(
                    "email sink '{}' starttls relay '{host}': {e}",
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
        let mut builder = Message::builder()
            .from(self.from.clone())
            .subject(body::subject(event, self.cfg.subject_prefix.as_deref()));
        for m in &self.to {
            builder = builder.to(m.clone());
        }
        for m in &self.cc {
            builder = builder.cc(m.clone());
        }
        if let Some(m) = &self.reply_to {
            builder = builder.reply_to(m.clone());
        }

        let alternative = MultiPart::alternative_plain_html(body::text(event), body::html(event));
        let message = if attachments.is_empty() {
            builder.multipart(alternative)
        } else {
            let mut mixed = MultiPart::mixed().multipart(alternative);
            for part in attachments {
                mixed = mixed.singlepart(part);
            }
            builder.multipart(mixed)
        }
        .map_err(|e| SinkError::Permanent(format!("build message: {e}")))?;
        Ok(message)
    }

    /// Read the configured attachments best-effort. A missing,
    /// unreadable, or oversized file is logged and omitted — it never
    /// fails the delivery.
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
    /// e.g. a rejected recipient or a refused sender) is
    /// operator-actionable; everything else (transient 4xx, timeout,
    /// TLS, network) is retryable, which is what the dispatcher's
    /// exponential backoff is for.
    fn classify_send_err(cfg_name: &str, err: lettre::transport::smtp::Error) -> SinkError {
        if err.is_permanent() {
            SinkError::Permanent(format!("email '{cfg_name}' send: {err}"))
        } else {
            SinkError::Transient(format!("email '{cfg_name}' send: {err}"))
        }
    }
}

#[async_trait]
impl AlertSink for EmailSink {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn id(&self) -> &SinkId {
        &self.id
    }

    fn wants_clip(&self) -> bool {
        self.cfg.attach_clip
    }

    async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError> {
        let attachments = self.collect_attachments(event).await;
        let message = self.build_message(event, attachments)?;
        trace!(sink = %self.id, event = %event.event_id, "email send");

        match self.transport.send(message).await {
            Ok(resp) => {
                debug!(sink = %self.id, event = %event.event_id,
                       code = ?resp.code(), "delivered");
                Ok(())
            }
            Err(e) => {
                let err = Self::classify_send_err(&self.cfg.name, e);
                warn!(sink = %self.id, event = %event.event_id,
                      transient = err.is_transient(), "email send failed");
                Err(err)
            }
        }
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
    use nexus_types::Severity;

    fn cfg(name: &str) -> EmailSinkConfig {
        EmailSinkConfig {
            name: name.to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            starttls: true,
            from_address: "nexus@example.com".to_string(),
            from_name: Some("Nexus Edge AI".to_string()),
            to: vec!["ops@example.com".to_string()],
            cc: Vec::new(),
            reply_to: None,
            subject_prefix: None,
            attach_snapshot: true,
            attach_clip: false,
            username: None,
            password: None,
            timeout_secs: 15,
        }
    }

    fn sink(name: &str) -> EmailSink {
        EmailSink::new(&cfg(name)).unwrap()
    }

    fn sample_event() -> AlertEvent {
        AlertEvent {
            event_id: uuid::Uuid::nil(),
            camera_id: 4,
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
            frame_w: 0,
            frame_h: 0,
        }
    }

    #[test]
    fn sink_id_is_kind_qualified() {
        assert_eq!(sink("site-ops").id().as_str(), "email:site-ops");
        assert_eq!(sink("site-ops").id().kind(), "email");
        assert_eq!(sink("site-ops").id().name(), "site-ops");
    }

    #[test]
    fn new_rejects_name_with_separator() {
        let mut c = cfg("ok");
        c.name = "bad:name".to_string();
        assert!(EmailSink::new(&c).is_err());
    }

    #[test]
    fn new_rejects_bad_addresses() {
        let mut c = cfg("ok");
        c.from_address = "not-an-email".to_string();
        assert!(EmailSink::new(&c).is_err());

        let mut c = cfg("ok");
        c.to = vec!["also-not-an-email".to_string()];
        assert!(EmailSink::new(&c).is_err());
    }

    #[test]
    fn new_rejects_empty_recipient_list() {
        let mut c = cfg("ok");
        c.to = Vec::new();
        assert!(EmailSink::new(&c).is_err());
    }

    #[test]
    fn plaintext_relay_builds_without_tls() {
        let mut c = cfg("plain");
        c.starttls = false;
        assert!(EmailSink::new(&c).is_ok());
    }

    #[test]
    fn subject_carries_label_camera_and_severity() {
        let s = body::subject(&sample_event(), None);
        assert!(s.contains("person"), "{s}");
        assert!(s.contains('4'), "{s}");
        assert!(s.contains("high"), "{s}");
    }

    #[test]
    fn subject_prefix_is_prepended_and_blank_prefix_ignored() {
        let e = sample_event();
        assert!(body::subject(&e, Some("[North Yard]")).starts_with("[North Yard] Nexus alert:"));
        assert!(body::subject(&e, Some("   ")).starts_with("Nexus alert:"));
    }

    #[test]
    fn html_body_escapes_operator_supplied_strings() {
        let mut e = sample_event();
        e.label = "<img src=x onerror=alert(1)>".to_string();
        let html = body::html(&e);
        assert!(!html.contains("<img"), "label was not escaped: {html}");
        assert!(html.contains("&lt;img"), "{html}");
    }

    #[test]
    fn text_body_carries_the_identifying_fields() {
        let t = body::text(&sample_event());
        for needle in ["person", "rule-7", "high"] {
            assert!(t.contains(needle), "missing {needle} in {t}");
        }
    }

    #[test]
    fn message_builds_with_cc_and_reply_to() {
        let mut c = cfg("multi");
        c.to = vec!["a@example.com".to_string(), "b@example.com".to_string()];
        c.cc = vec!["c@example.com".to_string()];
        c.reply_to = Some("replies@example.com".to_string());
        let s = EmailSink::new(&c).unwrap();
        let msg = s.build_message(&sample_event(), Vec::new()).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("a@example.com"), "{raw}");
        assert!(raw.contains("b@example.com"), "{raw}");
        assert!(raw.contains("Cc: c@example.com"), "{raw}");
        assert!(raw.contains("Reply-To: replies@example.com"), "{raw}");
        // Both alternatives ride in the same message.
        assert!(raw.contains("text/plain"), "{raw}");
        assert!(raw.contains("text/html"), "{raw}");
    }

    #[test]
    fn from_display_name_is_used_when_set() {
        let msg = sink("named")
            .build_message(&sample_event(), Vec::new())
            .unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("Nexus Edge AI"), "{raw}");
    }

    #[test]
    fn wants_clip_follows_config() {
        assert!(!sink("no-clip").wants_clip());
        let mut c = cfg("clip");
        c.attach_clip = true;
        assert!(EmailSink::new(&c).unwrap().wants_clip());
    }
}
