//! Sinks (M7 alert delivery)

use crate::ConfigError;
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

/// One configured alert-delivery sink. Tagged by `kind` so the
/// engine knows which `nexus_sinks::AlertSink` to build at boot;
/// `name` is operator-chosen and is the half of the `<kind>:<name>`
/// SinkId every `alert_sink_outbox` row references.
///
/// Wire shape:
///
/// ```toml
/// [[sinks]]
/// kind = "webhook"
/// name = "primary"
/// url  = "https://example.com/nexus"
/// hmac_secret = "shared-secret"  # optional
/// timeout_secs = 10              # optional, default 10
///
/// [sinks.headers]                # optional
/// "X-Tenant" = "acme"
/// ```
///
/// Renaming a sink (changing `name` while keeping `kind`) is
/// forbidden in M7 because outbox rows reference the historical
/// id by string; the engine rejects validation if two entries
/// share `(kind, name)`. Operators MUST delete + re-add to rename.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SinkConfig {
    /// Generic HTTP webhook with optional HMAC-SHA256 signature.
    /// v1 parity port of `webhook_retry_queue.cpp`.
    Webhook(WebhookSinkConfig),
    /// SureView Ops central-monitoring sink. Triggers a SureView
    /// alarm point via a single JSON POST to the `/receiver`
    /// endpoint ("HTTP Alarms"); video stays on the SureView device.
    ///
    /// The serde discriminator is pinned to `"sureview"` (NOT the
    /// snake_case default `"sure_view"`) so the JSON / TOML `kind`
    /// tag matches [`SinkConfig::kind`] and the `<kind>:<name>`
    /// SinkId the dispatcher + outbox use.
    #[serde(rename = "sureview")]
    SureView(SureViewSinkConfig),
    /// SureView Ops central-monitoring sink over **SMTP / Email
    /// Alarms**. Sends one email per alert to the alarm point's
    /// unique receiver address; the operator-facing message rides the
    /// `Subject`, optional GPS map-plot coordinates ride the body, and
    /// the clip / snapshot can ride as attachments. No account API key
    /// is required — the destination address identifies the alarm
    /// point (per the SureView *SMTP Alarms* reference).
    ///
    /// The serde discriminator is pinned to `"sureview_email"` (the
    /// snake_case default already matches, but it is spelled
    /// explicitly to keep the wire tag stable alongside the
    /// `<kind>:<name>` SinkId).
    #[serde(rename = "sureview_email")]
    SureViewEmail(SureViewEmailSinkConfig),
    /// Generic SMTP email sink — one message per alert to an
    /// operator-chosen recipient list, through the site's own relay
    /// (Microsoft 365, Google Workspace, Exchange connector, on-prem
    /// MTA). Unlike [`SinkConfig::SureViewEmail`] this is not tied to
    /// any monitoring vendor's alarm-point semantics: it carries a
    /// real `to` / `cc` list, an operator-filterable subject prefix,
    /// and an HTML body alongside the plain-text alternative.
    Email(EmailSinkConfig),
}

impl SinkConfig {
    /// Discriminator — matches `nexus_sinks::AlertSink::kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            SinkConfig::Webhook(_) => "webhook",
            SinkConfig::SureView(_) => "sureview",
            SinkConfig::SureViewEmail(_) => "sureview_email",
            SinkConfig::Email(_) => "email",
        }
    }

    /// Operator-chosen identifier — the `<name>` half of the
    /// `<kind>:<name>` SinkId every outbox row references.
    pub fn name(&self) -> &str {
        match self {
            SinkConfig::Webhook(cfg) => &cfg.name,
            SinkConfig::SureView(cfg) => &cfg.name,
            SinkConfig::SureViewEmail(cfg) => &cfg.name,
            SinkConfig::Email(cfg) => &cfg.name,
        }
    }

    /// Per-kind validation invoked from `Config::validate`. Cheap
    /// structural checks only — the sink crate does protocol-level
    /// validation lazily on first `deliver()` call.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            SinkConfig::Webhook(cfg) => cfg.validate(),
            SinkConfig::SureView(cfg) => cfg.validate(),
            SinkConfig::SureViewEmail(cfg) => cfg.validate(),
            SinkConfig::Email(cfg) => cfg.validate(),
        }
    }

    /// Sentinel the admin GET surface substitutes for any secret
    /// field so a configured secret never leaves the box. A PUT
    /// that echoes this value back means "keep the stored secret
    /// unchanged" — see [`SinkConfig::restore_redacted_secrets_from`].
    pub const REDACTED_SECRET: &'static str = "__nexus_secret_redacted__";

    /// Replace every secret field with [`Self::REDACTED_SECRET`] in
    /// place. Called before a sink config is serialised into an
    /// admin GET response so the live secret never leaves the edge.
    /// Covers SureView `api_key` and webhook `hmac_secret`. Custom
    /// webhook `headers` are NOT redacted — operators must not place
    /// secrets there when configuring from the cloud.
    pub fn redact_secrets(&mut self) {
        match self {
            SinkConfig::Webhook(w) => {
                if w.hmac_secret.is_some() {
                    w.hmac_secret = Some(Self::REDACTED_SECRET.to_string());
                }
            }
            SinkConfig::SureView(s) => {
                s.api_key = Self::REDACTED_SECRET.to_string();
            }
            SinkConfig::SureViewEmail(s) => {
                if s.password.is_some() {
                    s.password = Some(Self::REDACTED_SECRET.to_string());
                }
            }
            SinkConfig::Email(s) => {
                if s.password.is_some() {
                    s.password = Some(Self::REDACTED_SECRET.to_string());
                }
            }
        }
    }

    /// For an incoming PUT body, restore any secret left as the
    /// redaction sentinel from the previously-stored config. Lets
    /// the cloud console round-trip a sink edit without ever
    /// handling the live secret: it echoes [`Self::REDACTED_SECRET`]
    /// for an unchanged field and the edge re-fills it from the db.
    /// `existing` is the prior stored config for the same `sink_id`;
    /// a kind mismatch is a no-op (the caller validates kind).
    pub fn restore_redacted_secrets_from(&mut self, existing: &SinkConfig) {
        match (self, existing) {
            (SinkConfig::Webhook(new), SinkConfig::Webhook(old))
                if new.hmac_secret.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.hmac_secret = old.hmac_secret.clone();
            }
            (SinkConfig::SureView(new), SinkConfig::SureView(old))
                if new.api_key == Self::REDACTED_SECRET =>
            {
                new.api_key = old.api_key.clone();
            }
            (SinkConfig::SureViewEmail(new), SinkConfig::SureViewEmail(old))
                if new.password.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.password = old.password.clone();
            }
            (SinkConfig::Email(new), SinkConfig::Email(old))
                if new.password.as_deref() == Some(Self::REDACTED_SECRET) =>
            {
                new.password = old.password.clone();
            }
            _ => {}
        }
    }

    /// `true` iff any secret field still holds the redaction
    /// sentinel. After [`Self::restore_redacted_secrets_from`] this
    /// signals a brand-new sink whose secret the operator never
    /// supplied (the sentinel had nothing to restore from) — the
    /// admin handler rejects such a PUT with a 400.
    pub fn has_redacted_secret(&self) -> bool {
        match self {
            SinkConfig::Webhook(w) => w.hmac_secret.as_deref() == Some(Self::REDACTED_SECRET),
            SinkConfig::SureView(s) => s.api_key == Self::REDACTED_SECRET,
            SinkConfig::SureViewEmail(s) => s.password.as_deref() == Some(Self::REDACTED_SECRET),
            SinkConfig::Email(s) => s.password.as_deref() == Some(Self::REDACTED_SECRET),
        }
    }
}

/// HTTP webhook sink configuration. JSON POST of the `AlertEvent`
/// payload, optional shared-secret HMAC-SHA256 signature shipped
/// in the `X-Nexus-Signature: sha256=<hex>` header (GitHub style),
/// optional custom headers fan-out.
///
/// Retry + backoff lives in the dispatcher
/// (`nexus_sinks::dispatcher`), not the sink — the sink does at
/// most one HTTP attempt per `deliver()` call and classifies the
/// outcome as `Transient` (5xx, 408, 429, network) or `Permanent`
/// (other 4xx).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// Target HTTP(S) endpoint. The webhook sink POSTs the alert
    /// JSON to this URL on every delivery attempt.
    pub url: Url,
    /// Optional custom request headers. Common use: tenant tags,
    /// auth bearer tokens (set the `Authorization` header here).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional shared secret. When set, the sink computes
    /// `hex(hmac_sha256(secret, body))` and ships it in the
    /// `X-Nexus-Signature: sha256=<hex>` header.
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Per-attempt HTTP timeout in seconds. The dispatcher's
    /// retry backoff (500ms → 60s, 8 attempts) wraps this.
    #[serde(default = "default_webhook_timeout_secs")]
    pub timeout_secs: u64,
}

impl WebhookSinkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "webhook sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "webhook sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if self.url.scheme() != "http" && self.url.scheme() != "https" {
            return Err(ConfigError::Validation(format!(
                "webhook sink '{}' url scheme '{}' is not http(s)",
                self.name,
                self.url.scheme()
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "webhook sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

fn default_webhook_timeout_secs() -> u64 {
    10
}

/// SureView Ops "HTTP Alarms" sink configuration. Triggers a SureView
/// alarm point with a single JSON POST to the regional `/receiver`
/// endpoint, per the SureView Ops *HTTP Alarms* reference
/// (<https://help.sureviewops.com/hc/en-us/articles/13213264758557-Http-Alarms>).
///
/// SureView Ops is an alarm receiver: the POST only *triggers* an
/// alarm point identified by its **System Identifier** (configured in
/// SureView's *Alarm Setup → HTTP Alarms* tab). Video is NOT carried
/// in the payload — the operator pulls live/recorded video from the
/// camera the customer has set up as a SureView device — so this sink
/// has no media-upload step.
///
/// The account API Key is a per-customer secret — it lives in
/// `nexus.toml` on the edge box, never in this repo — and is sent
/// base64-encoded in the `Authorization` header (per the docs).
/// Retry/backoff is the dispatcher's job; the sink does one HTTP POST
/// per `deliver()` call and classifies the outcome as `Transient`
/// (5xx, 408, 429, network) or `Permanent` (other 4xx, e.g. a bad
/// API key).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SureViewSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SureView Ops SaaS hosting region. Resolves to the documented
    /// receiver endpoint unless `endpoint` overrides it. Defaults to
    /// `us`.
    #[serde(default)]
    pub region: SureViewRegion,
    /// Explicit receiver endpoint override. Set this only for on-prem
    /// SureView installs or integration testing; production SaaS
    /// deployments should set `region` instead.
    #[serde(default)]
    pub endpoint: Option<Url>,
    /// Per-customer account API Key from SureView's *Alarm Setup →
    /// HTTP Alarms* tab. Secret — sourced from `nexus.toml` on the
    /// box, never committed. Sent base64-encoded in `Authorization`.
    pub api_key: String,
    /// Default SureView alarm-point **System Identifier** to trigger.
    /// Used for any camera without a `system_identifiers` override.
    pub system_identifier: String,
    /// Optional per-camera override of `system_identifier`, keyed by
    /// the nexus camera id (as a string). Lets each camera trigger
    /// its own SureView alarm point / zone.
    #[serde(default)]
    pub system_identifiers: std::collections::HashMap<String, String>,
    /// Optional static `"Latitude,Longitude"` reported as the SureView
    /// `location` field on every alarm from this sink.
    #[serde(default)]
    pub location: Option<String>,
    /// Per-attempt HTTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff (500ms → 60s, 8 attempts) wraps it.
    #[serde(default = "default_sureview_timeout_secs")]
    pub timeout_secs: u64,
}

/// SureView Ops SaaS hosting region — selects the documented
/// `/receiver` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SureViewRegion {
    /// `https://us.sureviewops.com/receiver`
    #[default]
    Us,
    /// `https://eu.sureviewops.com/receiver`
    Eu,
}

impl SureViewRegion {
    /// The documented regional receiver URL.
    pub fn receiver_url(self) -> &'static str {
        match self {
            SureViewRegion::Us => "https://us.sureviewops.com/receiver",
            SureViewRegion::Eu => "https://eu.sureviewops.com/receiver",
        }
    }

    /// The documented regional SMTP relay host for *SMTP / Email
    /// Alarms*. AMER routes through `us-smtp`; EMEA / APAC through
    /// `eu-smtp`.
    pub fn smtp_host(self) -> &'static str {
        match self {
            SureViewRegion::Us => "us-smtp.sureviewops.com",
            SureViewRegion::Eu => "eu-smtp.sureviewops.com",
        }
    }
}

impl SureViewSinkConfig {
    /// The receiver URL this sink POSTs to — the explicit `endpoint`
    /// override when set, else the region's documented receiver URL.
    pub fn resolved_endpoint(&self) -> Result<Url, ConfigError> {
        match &self.endpoint {
            Some(u) => Ok(u.clone()),
            None => Url::parse(self.region.receiver_url())
                .map_err(|e| ConfigError::Validation(format!("sureview region url: {e}"))),
        }
    }

    /// Resolve the SureView System Identifier for a given camera —
    /// the per-camera override if present, else the default.
    pub fn system_identifier_for(&self, camera_id: CameraId) -> &str {
        self.system_identifiers
            .get(&camera_id.to_string())
            .map(String::as_str)
            .unwrap_or(&self.system_identifier)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "sureview sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "sureview sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if let Some(endpoint) = &self.endpoint {
            if endpoint.scheme() != "http" && endpoint.scheme() != "https" {
                return Err(ConfigError::Validation(format!(
                    "sureview sink '{}' endpoint scheme '{}' is not http(s)",
                    self.name,
                    endpoint.scheme()
                )));
            }
        }
        if self.api_key.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' api_key must be non-empty",
                self.name
            )));
        }
        if self.system_identifier.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' system_identifier must be non-empty",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

fn default_sureview_timeout_secs() -> u64 {
    15
}

/// SureView Ops "SMTP / Email Alarms" sink configuration. Triggers a
/// SureView alarm point by sending one email per alert to the alarm
/// point's unique receiver address, per the SureView Ops *SMTP Alarms*
/// reference
/// (<https://help.sureviewops.com/hc/en-us/articles/13211794487837-Smtp-alarms-Email-Alarms>).
///
/// Unlike the HTTP Alarms sink there is **no account API key** — the
/// destination email address itself identifies the alarm point.
/// SureView reads the email as follows:
///   * `To` — the alarm point's unique address (e.g.
///     `8nrawg1sxc@us.sureviewops.com`). Per-camera overrides live in
///     [`Self::alarm_emails`].
///   * `From` — any syntactically valid address (SureView ignores it).
///   * `Subject` — the operator-facing alarm message.
///   * body — optional; a decimal `"latitude,longitude"` pair auto-
///     plots the alarm on the SureView map ([`Self::location`]).
///   * attachments — optional clip (MP4) / snapshot (JPG).
///
/// SureView's relay requires no authentication, but some networks
/// front it with an authenticating relay, so optional
/// [`Self::username`] / [`Self::password`] (a redacted secret) are
/// supported. Retry / backoff is the dispatcher's job; the sink sends
/// at most one message per `deliver()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SureViewEmailSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SureView Ops SaaS hosting region. Selects the documented SMTP
    /// relay host unless `smtp_host` overrides it. Defaults to `us`.
    #[serde(default)]
    pub region: SureViewRegion,
    /// Explicit SMTP relay host override. Set this only for on-prem
    /// SureView installs or integration testing; production SaaS
    /// deployments should set `region` instead.
    #[serde(default)]
    pub smtp_host: Option<String>,
    /// SMTP submission port. Defaults to 587 (SureView also accepts
    /// 25).
    #[serde(default = "default_sureview_smtp_port")]
    pub smtp_port: u16,
    /// Negotiate STARTTLS on the connection. Defaults to `true`
    /// (SureView supports but does not require TLS); set `false` only
    /// for a plaintext test relay.
    #[serde(default = "crate::inference::default_true")]
    pub starttls: bool,
    /// Envelope / header `From` address. SureView ignores it, but SMTP
    /// requires a syntactically valid sender. Defaults to
    /// `nexus-edge@localhost`.
    #[serde(default = "default_sureview_from")]
    pub from_address: String,
    /// Default SureView alarm-point receiver address (the `To`). Used
    /// for any camera without an `alarm_emails` override.
    pub alarm_email: String,
    /// Optional per-camera override of `alarm_email`, keyed by the
    /// nexus camera id (as a string). Lets each camera trigger its own
    /// SureView alarm point.
    #[serde(default)]
    pub alarm_emails: std::collections::HashMap<String, String>,
    /// Optional static `"Latitude,Longitude"` placed in the email body
    /// so SureView auto-plots the alarm on its map.
    #[serde(default)]
    pub location: Option<String>,
    /// Attach the alert's annotated snapshot (JPG) when one is present
    /// on the event. Best-effort: a missing / unreadable file is
    /// logged and the alarm is still sent. Defaults to `false`.
    #[serde(default)]
    pub attach_snapshot: bool,
    /// Attach the alert's motion clip (MP4) when one is present on the
    /// event. Best-effort and size-capped; a missing / oversized file
    /// is logged and the alarm is still sent. Defaults to `false`.
    #[serde(default)]
    pub attach_clip: bool,
    /// Optional SMTP AUTH username for a fronting authenticating relay.
    /// Leave unset for SureView's unauthenticated relay.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional SMTP AUTH password (paired with `username`). Secret —
    /// sourced from `nexus.toml` on the box, never committed; redacted
    /// on the admin GET surface.
    #[serde(default)]
    pub password: Option<String>,
    /// Per-attempt SMTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff wraps it.
    #[serde(default = "default_sureview_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_sureview_smtp_port() -> u16 {
    587
}

fn default_sureview_from() -> String {
    "nexus-edge@localhost".to_string()
}

impl SureViewEmailSinkConfig {
    /// The SMTP relay host this sink connects to — the explicit
    /// `smtp_host` override when set, else the region's documented
    /// relay host.
    pub fn resolved_smtp_host(&self) -> &str {
        match &self.smtp_host {
            Some(h) => h.as_str(),
            None => self.region.smtp_host(),
        }
    }

    /// Resolve the SureView alarm-point address for a given camera —
    /// the per-camera override if present, else the default.
    pub fn alarm_email_for(&self, camera_id: CameraId) -> &str {
        self.alarm_emails
            .get(&camera_id.to_string())
            .map(String::as_str)
            .unwrap_or(&self.alarm_email)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "sureview_email sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if !self.from_address.contains('@') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' from_address '{}' is not a valid email",
                self.name, self.from_address
            )));
        }
        if !self.alarm_email.contains('@') {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' alarm_email '{}' is not a valid email",
                self.name, self.alarm_email
            )));
        }
        for (cam, addr) in &self.alarm_emails {
            if !addr.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "sureview_email sink '{}' alarm_emails['{cam}'] '{addr}' is not a valid email",
                    self.name
                )));
            }
        }
        if self.resolved_smtp_host().is_empty() {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' smtp_host must be non-empty",
                self.name
            )));
        }
        if self.smtp_port == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' smtp_port must be > 0",
                self.name
            )));
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' username and password must be set together",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "sureview_email sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}

/// Generic SMTP email sink configuration — "email this alert to
/// these people".
///
/// Distinct from [`SureViewEmailSinkConfig`], which is hard-wired to
/// SureView's alarm-point model (the destination address *is* the
/// alarm point, so there is one recipient, a fixed subject shape, and
/// no HTML part). This sink is the operator-facing one: an explicit
/// recipient list, a subject an operator can filter their mailbox on,
/// and a readable HTML body alongside the plain-text alternative.
///
/// The relay is whatever the site already uses — Microsoft 365,
/// Google Workspace, a corporate Exchange connector, or an on-prem
/// MTA. Credentials stay on the box: they are entered through the
/// admin API, persisted in the edge-resident `alert_sinks` table, and
/// redacted by [`SinkConfig::redact_secrets`] before any GET response
/// leaves the appliance.
///
/// ```toml
/// [[sinks]]
/// kind = "email"
/// name = "site-ops"
/// smtp_host = "smtp.example.com"
/// from_address = "nexus@example.com"
/// from_name = "Nexus Edge AI"
/// to = ["ops@example.com", "security@example.com"]
/// subject_prefix = "[North Yard]"
/// username = "nexus@example.com"
/// password = "app-password"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailSinkConfig {
    /// Operator-chosen identifier (the `<name>` of the SinkId).
    /// Must be unique across the `[[sinks]]` list. Stable across
    /// config reloads — outbox rows reference it by string.
    pub name: String,
    /// SMTP relay hostname. Required — unlike the SureView sink
    /// there is no vendor default to fall back on.
    pub smtp_host: String,
    /// SMTP submission port. Defaults to 587 (STARTTLS submission).
    #[serde(default = "default_email_smtp_port")]
    pub smtp_port: u16,
    /// Negotiate STARTTLS on the connection. Defaults to `true`;
    /// set `false` only for a plaintext relay on a trusted LAN.
    #[serde(default = "crate::inference::default_true")]
    pub starttls: bool,
    /// Envelope / header `From` address. Most relays require this to
    /// be an address they are authorised to send as.
    pub from_address: String,
    /// Optional display name shown beside `from_address` in a mail
    /// client (`Nexus Edge AI <nexus@example.com>`).
    #[serde(default)]
    pub from_name: Option<String>,
    /// Recipients. Must be non-empty — a sink with nobody to mail is
    /// a silent no-op, so it is rejected at validation instead.
    pub to: Vec<String>,
    /// Optional carbon-copy recipients.
    #[serde(default)]
    pub cc: Vec<String>,
    /// Optional `Reply-To`. Useful when `from_address` is a no-reply
    /// mailbox but replies should reach a monitored inbox.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Optional literal prefix for the subject line, e.g.
    /// `"[North Yard]"`. Gives operators a stable string to build
    /// mailbox rules on when one relay serves several sites.
    #[serde(default)]
    pub subject_prefix: Option<String>,
    /// Attach the alert's annotated snapshot (JPG) when the event
    /// carries one. Defaults to `true` — a still frame is the whole
    /// point of an alert email. Best-effort: a missing or unreadable
    /// file is logged and the mail still goes out.
    #[serde(default = "crate::inference::default_true")]
    pub attach_snapshot: bool,
    /// Attach the alert's motion clip (MP4) when the event carries
    /// one. Defaults to `false` because clips routinely exceed relay
    /// message-size limits; best-effort and size-capped when enabled.
    #[serde(default)]
    pub attach_clip: bool,
    /// Optional SMTP AUTH username. Most hosted relays require it.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional SMTP AUTH password (paired with `username`). Secret —
    /// redacted on the admin GET surface, never leaves the box.
    #[serde(default)]
    pub password: Option<String>,
    /// Per-attempt SMTP timeout in seconds. Defaults to 15. The
    /// dispatcher's retry backoff wraps it.
    #[serde(default = "default_email_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_email_smtp_port() -> u16 {
    587
}

fn default_email_timeout_secs() -> u64 {
    15
}

impl EmailSinkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "email sink name must be non-empty".into(),
            ));
        }
        if self.name.contains(':') {
            return Err(ConfigError::Validation(format!(
                "email sink name '{}' must not contain ':' (reserved as SinkId separator)",
                self.name
            )));
        }
        if self.smtp_host.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' smtp_host must be non-empty",
                self.name
            )));
        }
        if self.smtp_port == 0 {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' smtp_port must be > 0",
                self.name
            )));
        }
        if !self.from_address.contains('@') {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' from_address '{}' is not a valid email",
                self.name, self.from_address
            )));
        }
        // A sink with no recipients would accept every outbox row and
        // deliver nothing, which is indistinguishable from working.
        if self.to.is_empty() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' must have at least one 'to' recipient",
                self.name
            )));
        }
        for addr in self.to.iter().chain(self.cc.iter()) {
            if !addr.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "email sink '{}' recipient '{addr}' is not a valid email",
                    self.name
                )));
            }
        }
        if let Some(reply_to) = &self.reply_to {
            if !reply_to.contains('@') {
                return Err(ConfigError::Validation(format!(
                    "email sink '{}' reply_to '{reply_to}' is not a valid email",
                    self.name
                )));
            }
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' username and password must be set together",
                self.name
            )));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::Validation(format!(
                "email sink '{}' timeout_secs must be > 0",
                self.name
            )));
        }
        Ok(())
    }
}
