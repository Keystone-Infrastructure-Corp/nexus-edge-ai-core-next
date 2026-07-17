//! Cloud-console alert delivery — the always-on audit-trail sink.
//!
//! Wires the engine's recorded alerts to the cloud Alert Console over the
//! WSS tunnel by implementing the M7 [`nexus_sinks::AlertSink`] contract,
//! so alerts ride the existing DURABLE `alert_sink_outbox` + dispatcher
//! (retry/backoff, survives reboot) rather than the in-memory
//! [`TunnelOutbox`] alone.
//!
//! Two pieces live here:
//!
//! * [`CloudConsoleAlertSink`] — the sink. Registered as a RESERVED sink
//!   ([`nexus_sinks::SinkRegistry::insert_reserved`]) under the id
//!   `cloud:console` so an operator config edit (`sink.config.changed`,
//!   which rebuilds the config-managed set) never wipes it. `deliver`
//!   projects the [`AlertEvent`] into a wire envelope and hands it to the
//!   shared [`TunnelOutbox`]; a disconnected tunnel maps to
//!   [`SinkError::Transient`] so the dispatcher redelivers from the
//!   durable outbox when the tunnel returns — the audit trail never drops
//!   an alert to a transient outage.
//!
//! * [`CloudAwarePolicy`] — a [`DeliveryPolicy`] wrapper that makes the
//!   cloud sink the always-on **audit trail**: `cloud:*` rows BYPASS the
//!   operator delivery policy (schedule / global-disable / per-rule), so
//!   the console captures every alert. It DOES still suppress when the
//!   org's cloud entitlement is suspended for non-payment (cloud
//!   `ARCHITECTURE.md` §12.4). Every other sink delegates to the inner
//!   [`CascadingPolicy`](nexus_sinks::policy::CascadingPolicy) unchanged.
//!
//! Notification quiet-hours ("don't page an operator outside a defined
//! window") is deliberately NOT handled here — the cloud console is the
//! record, not a notifier. That gate lives at the cloud notification layer
//! (notify-svc / CMS); see `docs/cloud-console/PHASES.md` step 11.11.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use nexus_cloud_client::entitlements::EntitlementCache;
use nexus_cloud_client::{build_alert_envelope, AlertProjection, TunnelOutbox};
use nexus_cloud_protocol::v1::VerificationState;
use nexus_sinks::dispatcher::{DeliveryPolicy, DeliveryVerdict};
use nexus_sinks::{AlertSink, SinkError, SinkId};
use nexus_store::{OutboxRow, SuppressionReason};
use nexus_types::{AlertEvent, Severity};

/// `<kind>` half of the reserved cloud sink id.
const CLOUD_SINK_KIND: &str = "cloud";
/// `<name>` half of the reserved cloud sink id.
const CLOUD_SINK_NAME: &str = "console";
/// Sink-id prefix (`"cloud:"`) that [`CloudAwarePolicy`] uses to recognise
/// cloud-destined outbox rows.
pub const CLOUD_SINK_PREFIX: &str = "cloud:";

/// How long `deliver()` waits for the cloud's `alert_ack` before treating
/// the delivery as unconfirmed (→ `SinkError::Transient`, retried from the
/// durable outbox). Well under the dispatcher's ~127 s / 8-attempt budget;
/// the happy path acks in one tunnel round-trip (~tens of ms).
const ACK_TIMEOUT: Duration = Duration::from_secs(8);

/// Outcome of an inbound `alert_ack`, delivered to the waiting `deliver()`.
#[derive(Debug, Clone)]
pub enum AckOutcome {
    /// Cloud accepted + stored the alert (`status = "stored"`).
    Stored,
    /// Cloud permanently rejected it (`status = "permanent_failure"`); the
    /// dispatcher marks the row dead (no retry).
    PermanentFailure(Option<String>),
}

/// In-memory correlation between a sent alert envelope's `meta.id` and the
/// `deliver()` call awaiting its `alert_ack`. Shared (via `Arc`) between the
/// [`CloudConsoleAlertSink`] (registers a waiter before sending) and the
/// cloud-tunnel dispatch (fires the waiter when the matching `alert_ack`
/// arrives, keyed by `in_reply_to`). Entries live only for the duration of a
/// single `deliver()` — no persistence, no schema.
#[derive(Default)]
pub struct PendingAckRegistry {
    inner: Mutex<HashMap<String, oneshot::Sender<AckOutcome>>>,
}

impl PendingAckRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for `envelope_id`; returns the receiver `deliver()`
    /// awaits.
    fn register(&self, envelope_id: String) -> oneshot::Receiver<AckOutcome> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().insert(envelope_id, tx);
        rx
    }

    /// Deliver `outcome` to the waiter for `envelope_id` (the `alert_ack`'s
    /// `in_reply_to`). No-op if the waiter already timed out / was cancelled.
    /// Returns `true` when a waiter was found.
    pub fn fire(&self, envelope_id: &str, outcome: AckOutcome) -> bool {
        match self.inner.lock().remove(envelope_id) {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Drop the waiter for `envelope_id` without firing (send failed / timed
    /// out) so the map doesn't leak.
    fn cancel(&self, envelope_id: &str) {
        self.inner.lock().remove(envelope_id);
    }
}

/// Uploads an alert snapshot JPEG to cloud blob storage and returns the
/// UNSIGNED blob URL to persist on the alert projection (the short-TTL SAS
/// token never rides the wire / never lands in Postgres — Hard Rule 7).
///
/// Implemented over the gateway SAS issuer once enrollment completes
/// (`cloud_tunnel::install_cloud_blob_backend`); a mock stands in for the
/// sink's unit tests. Kept as a narrow engine-local trait so the sink
/// doesn't depend on `nexus-storage-cloud` internals and stays testable.
#[async_trait]
pub trait SnapshotUploader: Send + Sync {
    /// Upload `jpeg` for the alert `event_id`; return the unsigned blob URL.
    async fn upload(&self, event_id: &str, jpeg: Vec<u8>) -> Result<String, String>;
}

/// Shared, late-bound slot holding the [`SnapshotUploader`]. Empty (`None`)
/// until the cloud tunnel enrolls and installs a gateway-backed uploader;
/// the sink reads it per delivery. Writes happen once at enrollment, reads
/// are per-alert (infrequent), so a plain `RwLock` is ample.
pub type SnapshotUploaderSlot = Arc<RwLock<Option<Arc<dyn SnapshotUploader>>>>;

/// Construct an empty (not-yet-enrolled) [`SnapshotUploaderSlot`]. The cloud
/// tunnel fills it post-enrollment (`install_cloud_blob_backend`).
#[must_use]
pub fn new_uploader_slot() -> SnapshotUploaderSlot {
    Arc::new(RwLock::new(None))
}

/// Map the engine's [`Severity`] enum onto the wire `severity` u64 the
/// cloud console bins into visual severities (`AlertConsolePage.tsx`:
/// `>= 3` red, `== 2` amber, else default).
const fn severity_to_u64(s: Severity) -> u64 {
    match s {
        Severity::Low => 1,
        Severity::Medium => 2,
        Severity::High => 3,
        Severity::Critical => 4,
    }
}

/// Project a local [`AlertEvent`] into the wire-shaped [`AlertProjection`]
/// the cloud alert-ingest expects.
///
/// Mapping notes:
/// * `confidence` is read from the `context["confidence"]` value the rule
///   evaluator already stamps — no extra `AlertEvent` field needed.
/// * `bbox` is normalised from analysis-frame pixels to `[x, y, w, h]` in
///   0..1 via [`normalize_bbox`] (using `frame_w`/`frame_h`); omitted when
///   frame dims are unknown.
/// * `trace_id` (W3C 32-hex) rides into the envelope `meta.trace` so the
///   cloud can wire the alert to its end-to-end trace.
/// * `verification_state` = `Candidate` when the rule opted in via
///   `RuleConfig.verify` (stamped as `context["verify"]`) so the cloud VLM
///   adjudicates it; otherwise `Verified` (appears immediately).
/// * `snapshot_blob_url` / `clip_blob_url` are `None`: no snapshot is
///   captured at alert-fire and the clip is not yet cold-replicated, and
///   the cloud UI does not render these fields (it links evidence via
///   entity sightings). A real alert-thumbnail feature is a deliberate
///   future scope spanning edge capture + cloud rendering.
#[must_use]
pub fn project_alert(event: &AlertEvent) -> AlertProjection {
    // Verify-opt-in rules stamp `context["verify"]=true` so the cloud VLM
    // behavior-verifier adjudicates them; everything else fires `verified`.
    let verification_state = if event
        .context
        .get("verify")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        Some(VerificationState::Candidate)
    } else {
        Some(VerificationState::Verified)
    };
    AlertProjection {
        edge_event_id: event.event_id.to_string(),
        ts: event.captured_at,
        camera_id: event.camera_id.max(0) as u64,
        severity: severity_to_u64(event.severity),
        edge_rule_id: Some(event.rule_id.clone()),
        matched_label: Some(event.label.clone()),
        confidence: event
            .context
            .get("confidence")
            .and_then(serde_json::Value::as_f64),
        bbox: normalize_bbox(event),
        snapshot_blob_url: None,
        clip_blob_url: None,
        attached_history: None,
        verification_state,
        trace_id: Some(event.trace_id.clone()).filter(|t| !t.is_empty()),
    }
}

/// Normalise the pixel `AlertEvent.bbox` (top-left origin; `x1,y1,x2,y2` in
/// analysis-frame pixels) into the wire `[x, y, w, h]` in 0..1 the cloud
/// expects. Returns `None` when there is no bbox or the frame dims are
/// unknown (`0` — e.g. an event persisted before `frame_w`/`frame_h`
/// existed): the cloud column is nullable and its entity-context matcher
/// falls back to time, so omitting is safe and never sends wrong coords.
fn normalize_bbox(event: &AlertEvent) -> Option<Vec<f64>> {
    let b = event.bbox.as_ref()?;
    if event.frame_w == 0 || event.frame_h == 0 {
        return None;
    }
    let fw = f64::from(event.frame_w);
    let fh = f64::from(event.frame_h);
    let x = (f64::from(b.x1) / fw).clamp(0.0, 1.0);
    let y = (f64::from(b.y1) / fh).clamp(0.0, 1.0);
    let w = (f64::from(b.x2 - b.x1) / fw).clamp(0.0, 1.0);
    let h = (f64::from(b.y2 - b.y1) / fh).clamp(0.0, 1.0);
    Some(vec![x, y, w, h])
}

/// The always-on cloud-console alert sink. See the module docs.
pub struct CloudConsoleAlertSink {
    id: SinkId,
    outbox: Arc<TunnelOutbox>,
    pending_acks: Arc<PendingAckRegistry>,
    /// `<state_dir>/snapshots` — where the supervisor wrote
    /// `<event_id>.jpg` at rule-fire. The sink reads it back here to
    /// upload the thumbnail on delivery.
    snapshots_dir: PathBuf,
    /// Late-bound snapshot uploader (installed post-enrollment). `None`
    /// until then — alerts still deliver, just without a thumbnail.
    uploader_slot: SnapshotUploaderSlot,
}

impl CloudConsoleAlertSink {
    /// Build the sink around the shared cloud tunnel outbox + the shared
    /// pending-ack registry (the cloud-tunnel dispatch fires acks into the
    /// same registry). `snapshots_dir` is `<state_dir>/snapshots`;
    /// `uploader_slot` is filled once the tunnel enrolls.
    #[must_use]
    pub fn new(
        outbox: Arc<TunnelOutbox>,
        pending_acks: Arc<PendingAckRegistry>,
        snapshots_dir: PathBuf,
        uploader_slot: SnapshotUploaderSlot,
    ) -> Self {
        Self {
            id: SinkId::new(CLOUD_SINK_KIND, CLOUD_SINK_NAME)
                .expect("cloud:console is a valid sink id"),
            outbox,
            pending_acks,
            snapshots_dir,
            uploader_slot,
        }
    }

    /// Best-effort thumbnail upload. If the supervisor wrote a snapshot for
    /// `event_id` and an uploader is installed, PUT the JPEG to blob storage
    /// and return the unsigned blob URL for the projection. `None` on any
    /// miss/failure — a thumbnail is never allowed to fail an alert.
    async fn maybe_upload_snapshot(&self, event_id: &str) -> Option<String> {
        // Clone the Arc out and release the lock before any await.
        let uploader = self.uploader_slot.read().clone()?;
        let path = self.snapshots_dir.join(format!("{event_id}.jpg"));
        let jpeg = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            // Missing file is common + benign (snapshots disabled for the
            // camera, or an alert recorded before this feature shipped) —
            // debug, not warn.
            Err(e) => {
                debug!(event = %event_id, "no alert snapshot to upload: {e}");
                return None;
            }
        };
        match uploader.upload(event_id, jpeg).await {
            Ok(url) => Some(url),
            Err(e) => {
                warn!(event = %event_id, "alert snapshot upload failed: {e}");
                None
            }
        }
    }
}

#[async_trait]
impl AlertSink for CloudConsoleAlertSink {
    fn kind(&self) -> &'static str {
        CLOUD_SINK_KIND
    }

    fn id(&self) -> &SinkId {
        &self.id
    }

    async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError> {
        let event_id = event.event_id.to_string();
        let mut projection = project_alert(event);
        // Best-effort thumbnail: upload the rule-fire snapshot (if any) and
        // stamp its unsigned blob URL so the console renders the alert
        // thumbnail. Never fails the delivery — see `maybe_upload_snapshot`.
        if let Some(url) = self.maybe_upload_snapshot(&event_id).await {
            projection.snapshot_blob_url = Some(url);
        }
        let (envelope, envelope_id) = build_alert_envelope(projection);
        // Register the pending-ack waiter BEFORE sending so a fast ack can't
        // race the registration.
        let ack_rx = self.pending_acks.register(envelope_id.clone());
        // A send failure (tunnel down / not yet up) is transient: the
        // dispatcher retries from the durable outbox once the tunnel returns.
        if let Err(e) = self.outbox.send(envelope).await {
            self.pending_acks.cancel(&envelope_id);
            return Err(SinkError::Transient(e.to_string()));
        }
        // The outbox row is `sent` only once the cloud CONFIRMS via
        // `alert_ack` (in_reply_to == envelope_id) — so `sent` means "cloud
        // stored it", not merely "written to the socket". Timeout → transient
        // retry (the cloud INSERT is idempotent on (core_id, edge_event_id),
        // so a resend is safe). permanent_failure → dead (no retry).
        let outcome = match tokio::time::timeout(ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(AckOutcome::Stored)) => Ok(()),
            Ok(Ok(AckOutcome::PermanentFailure(reason))) => {
                Err(SinkError::Permanent(reason.unwrap_or_else(|| {
                    "cloud reported permanent_failure".to_string()
                })))
            }
            // Sender dropped without firing (should not happen) — treat the
            // WSS write as delivered rather than spuriously retrying.
            Ok(Err(_cancelled)) => Ok(()),
            Err(_elapsed) => {
                self.pending_acks.cancel(&envelope_id);
                Err(SinkError::Transient(
                    "alert_ack not received before timeout".to_string(),
                ))
            }
        };
        // NOTE: the local snapshot at `<state_dir>/snapshots/<event_id>.jpg`
        // is a SHARED resource — external sinks (e.g. SureView email with
        // `attach_snapshot`) attach the same file. Reclaiming it here, per
        // this sink's terminal outcome, races a slower sink that has not yet
        // delivered (the email sink defers until the motion clip finalises,
        // which can be seconds after the cloud ack). So reclaim is
        // centralised in the dispatcher, which deletes the JPEG only once
        // NO outbox row for the event is still `pending`. See
        // `nexus_sinks::dispatcher::reclaim_snapshot_if_drained`.
        outcome
    }
}

/// [`DeliveryPolicy`] wrapper making the cloud audit sink always-on while
/// leaving external sinks on the operator delivery policy. See module docs.
pub struct CloudAwarePolicy {
    inner: Arc<dyn DeliveryPolicy>,
    entitlements: Arc<EntitlementCache>,
}

impl CloudAwarePolicy {
    /// Wrap the operator delivery policy with the cloud-sink carve-out.
    #[must_use]
    pub fn new(inner: Arc<dyn DeliveryPolicy>, entitlements: Arc<EntitlementCache>) -> Self {
        Self {
            inner,
            entitlements,
        }
    }
}

#[async_trait]
impl DeliveryPolicy for CloudAwarePolicy {
    async fn evaluate(
        &self,
        row: &OutboxRow,
        event: &AlertEvent,
        now: DateTime<Utc>,
    ) -> DeliveryVerdict {
        if row.sink_id.starts_with(CLOUD_SINK_PREFIX) {
            // Always-on audit trail — bypass the operator schedule /
            // global-disable / per-rule policy, but respect entitlement
            // suspension (billing / non-payment, §12.4).
            if self.entitlements.is_suspended() {
                return DeliveryVerdict::Suppressed(SuppressionReason::EntitlementSuspended);
            }
            return DeliveryVerdict::Deliver;
        }
        self.inner.evaluate(row, event, now).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use nexus_cloud_client::{TunnelError, TunnelHandle};
    use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody};
    use nexus_sinks::dispatcher::AllowAllPolicy;
    use nexus_store::OutboxStatus;
    use nexus_types::Artifacts;

    fn sample_event() -> AlertEvent {
        let mut context = serde_json::Map::new();
        context.insert("confidence".into(), serde_json::json!(0.87));
        AlertEvent {
            event_id: uuid::Uuid::now_v7(),
            camera_id: 7,
            rule_id: "rule_person".into(),
            track_id: None,
            label: "person".into(),
            severity: Severity::High,
            bbox: Some(nexus_types::BBox {
                x1: 96.0,
                y1: 54.0,
                x2: 192.0,
                y2: 162.0,
            }),
            frame_id: 1,
            captured_at: Utc::now(),
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            frame_w: 960,
            frame_h: 540,
            artifacts: Artifacts::default(),
            context,
        }
    }

    fn outbox_row(sink_id: &str) -> OutboxRow {
        OutboxRow {
            id: 1,
            event_id: "evt".into(),
            sink_id: sink_id.into(),
            status: OutboxStatus::Pending,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            suppression_reason: None,
            created_at: Utc::now(),
            delivered_at: None,
        }
    }

    /// A suspended-org entitlement JWT (unverified claims — the cache only
    /// reads them). Header + signature are placeholders.
    fn suspended_jwt() -> String {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"plan":"suspended","max_cameras":0}"#);
        format!("aGRy.{payload}.c2ln")
    }

    /// Mock tunnel handle that captures the sent envelope AND simulates the
    /// cloud by firing an `alert_ack` (keyed by the envelope's meta.id) into
    /// the shared registry so `deliver()`'s ack wait resolves.
    struct AckingHandle {
        last: Mutex<Option<Envelope>>,
        acks: Arc<PendingAckRegistry>,
        outcome: AckOutcome,
    }

    #[async_trait]
    impl TunnelHandle for AckingHandle {
        async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
            let id = envelope.meta.id.clone();
            *self.last.lock().unwrap() = Some(envelope);
            self.acks.fire(&id, self.outcome.clone());
            Ok(())
        }
    }

    /// Inner policy that suppresses everything — stands in for the operator
    /// delivery schedule being off, so a cloud-row `Deliver` proves bypass.
    struct DenyAllPolicy;

    #[async_trait]
    impl DeliveryPolicy for DenyAllPolicy {
        async fn evaluate(
            &self,
            _row: &OutboxRow,
            _event: &AlertEvent,
            _now: DateTime<Utc>,
        ) -> DeliveryVerdict {
            DeliveryVerdict::Suppressed(SuppressionReason::GlobalDisabled)
        }
    }

    #[test]
    fn project_alert_maps_core_fields() {
        let ev = sample_event();
        let p = project_alert(&ev);
        assert_eq!(p.edge_event_id, ev.event_id.to_string());
        assert_eq!(p.camera_id, 7);
        assert_eq!(p.severity, 3); // High
        assert_eq!(p.matched_label.as_deref(), Some("person"));
        assert_eq!(p.edge_rule_id.as_deref(), Some("rule_person"));
        assert_eq!(p.confidence, Some(0.87));
        assert_eq!(p.verification_state, Some(VerificationState::Verified));
        assert_eq!(
            p.trace_id.as_deref(),
            Some("0af7651916cd43dd8448eb211c80319c")
        );
        // 960x540 frame, bbox (96,54)-(192,162) -> [0.1, 0.1, 0.1, 0.2].
        let bbox = p.bbox.expect("normalised bbox");
        assert_eq!(bbox.len(), 4);
        for (got, want) in bbox.iter().zip([0.1, 0.1, 0.1, 0.2]) {
            assert!((got - want).abs() < 1e-6, "bbox {got} != {want}");
        }
    }

    #[test]
    fn verify_rule_projects_candidate_state() {
        let mut ev = sample_event();
        ev.context.insert("verify".into(), serde_json::json!(true));
        assert_eq!(
            project_alert(&ev).verification_state,
            Some(VerificationState::Candidate)
        );
    }

    #[test]
    fn bbox_omitted_when_frame_dims_unknown() {
        let mut ev = sample_event();
        ev.frame_w = 0;
        ev.frame_h = 0;
        assert!(project_alert(&ev).bbox.is_none());
    }

    #[tokio::test]
    async fn deliver_sends_alert_envelope_and_confirms_on_ack() {
        let acks = Arc::new(PendingAckRegistry::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let handle = Arc::new(AckingHandle {
            last: Mutex::new(None),
            acks: acks.clone(),
            outcome: AckOutcome::Stored,
        });
        outbox.set_handle(Some(handle.clone() as Arc<dyn TunnelHandle>));

        let sink = CloudConsoleAlertSink::new(outbox, acks, snapshots_dir(), empty_uploader_slot());
        sink.deliver(&sample_event()).await.expect("deliver ok");

        let captured = handle.last.lock().unwrap().clone().expect("envelope sent");
        assert_eq!(captured.meta.v, 1);
        assert!(matches!(captured.body, EnvelopeBody::Alert(_)));
    }

    #[tokio::test]
    async fn deliver_permanent_failure_ack_maps_to_permanent() {
        let acks = Arc::new(PendingAckRegistry::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let handle = Arc::new(AckingHandle {
            last: Mutex::new(None),
            acks: acks.clone(),
            outcome: AckOutcome::PermanentFailure(Some("camera_not_found".into())),
        });
        outbox.set_handle(Some(handle as Arc<dyn TunnelHandle>));

        let sink = CloudConsoleAlertSink::new(outbox, acks, snapshots_dir(), empty_uploader_slot());
        let err = sink.deliver(&sample_event()).await.expect_err("permanent");
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn deliver_is_transient_when_tunnel_disconnected() {
        // No handle installed → TunnelOutbox::send returns Disconnected before
        // the ack wait, so deliver fails fast (transient, retryable).
        let sink = CloudConsoleAlertSink::new(
            Arc::new(TunnelOutbox::new()),
            Arc::new(PendingAckRegistry::new()),
            snapshots_dir(),
            empty_uploader_slot(),
        );
        let err = sink
            .deliver(&sample_event())
            .await
            .expect_err("should fail");
        assert!(err.is_transient());
    }

    /// A snapshot dir path for tests whose slot is empty (the fs is never
    /// touched because `maybe_upload_snapshot` short-circuits on `None`).
    fn snapshots_dir() -> PathBuf {
        PathBuf::from("/nonexistent-nexus-snapshots")
    }

    /// An empty (not-yet-enrolled) uploader slot.
    fn empty_uploader_slot() -> SnapshotUploaderSlot {
        Arc::new(RwLock::new(None))
    }

    /// Records the `(event_id, jpeg_len)` it was asked to upload and returns
    /// a fixed unsigned blob URL.
    struct RecordingUploader {
        seen: Arc<Mutex<Option<(String, usize)>>>,
    }

    #[async_trait]
    impl SnapshotUploader for RecordingUploader {
        async fn upload(&self, event_id: &str, jpeg: Vec<u8>) -> Result<String, String> {
            *self.seen.lock().unwrap() = Some((event_id.to_string(), jpeg.len()));
            Ok("https://acct.blob.core.windows.net/snapshots/x.jpg".to_string())
        }
    }

    #[tokio::test]
    async fn deliver_uploads_snapshot_and_stamps_url() {
        let dir = tempfile::tempdir().unwrap();
        let ev = sample_event();
        let event_id = ev.event_id.to_string();
        // The supervisor would have written <dir>/<event_id>.jpg at fire.
        std::fs::write(
            dir.path().join(format!("{event_id}.jpg")),
            b"\xff\xd8\xff\xd9",
        )
        .unwrap();

        let seen: Arc<Mutex<Option<(String, usize)>>> = Arc::new(Mutex::new(None));
        let uploader = Arc::new(RecordingUploader { seen: seen.clone() });
        let slot: SnapshotUploaderSlot =
            Arc::new(RwLock::new(Some(uploader as Arc<dyn SnapshotUploader>)));

        let acks = Arc::new(PendingAckRegistry::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let handle = Arc::new(AckingHandle {
            last: Mutex::new(None),
            acks: acks.clone(),
            outcome: AckOutcome::Stored,
        });
        outbox.set_handle(Some(handle.clone() as Arc<dyn TunnelHandle>));

        let sink = CloudConsoleAlertSink::new(outbox, acks, dir.path().to_path_buf(), slot);
        sink.deliver(&ev).await.expect("deliver ok");

        // The uploader saw the JPEG bytes for this event.
        let (seen_id, seen_len) = seen.lock().unwrap().clone().expect("uploaded");
        assert_eq!(seen_id, event_id);
        assert_eq!(seen_len, 4);

        // The sent envelope carries the unsigned URL the uploader returned.
        let captured = handle.last.lock().unwrap().clone().expect("sent");
        match captured.body {
            EnvelopeBody::Alert(p) => assert_eq!(
                p.snapshot_blob_url.as_deref(),
                Some("https://acct.blob.core.windows.net/snapshots/x.jpg")
            ),
            _ => panic!("expected alert body"),
        }

        // Terminal (stored) outcome reclaims the local snapshot file.
        assert!(
            !dir.path().join(format!("{event_id}.jpg")).exists(),
            "snapshot file should be removed after a stored delivery"
        );
    }

    #[tokio::test]
    async fn deliver_keeps_snapshot_on_transient_failure() {
        // No tunnel handle → send returns Disconnected (transient) before the
        // ack wait, so the local snapshot MUST survive for the retry.
        let dir = tempfile::tempdir().unwrap();
        let ev = sample_event();
        let event_id = ev.event_id.to_string();
        let snap = dir.path().join(format!("{event_id}.jpg"));
        std::fs::write(&snap, b"\xff\xd8\xff\xd9").unwrap();

        // Uploader installed but the tunnel is down: upload may run, but the
        // send fails transiently, so we keep the file.
        let seen: Arc<Mutex<Option<(String, usize)>>> = Arc::new(Mutex::new(None));
        let uploader = Arc::new(RecordingUploader { seen });
        let slot: SnapshotUploaderSlot =
            Arc::new(RwLock::new(Some(uploader as Arc<dyn SnapshotUploader>)));

        let sink = CloudConsoleAlertSink::new(
            Arc::new(TunnelOutbox::new()),
            Arc::new(PendingAckRegistry::new()),
            dir.path().to_path_buf(),
            slot,
        );
        let err = sink.deliver(&ev).await.expect_err("transient");
        assert!(err.is_transient());
        assert!(snap.exists(), "snapshot must survive a transient failure");
    }

    #[test]
    fn pending_ack_registry_fires_registered_waiter_once() {
        let reg = PendingAckRegistry::new();
        let _rx = reg.register("id-1".into());
        assert!(reg.fire("id-1", AckOutcome::Stored));
        // Second fire finds no waiter.
        assert!(!reg.fire("id-1", AckOutcome::Stored));
        // Unknown id: no waiter.
        assert!(!reg.fire("id-2", AckOutcome::Stored));
    }

    #[tokio::test]
    async fn cloud_row_bypasses_inner_policy_when_active() {
        let pol = CloudAwarePolicy::new(Arc::new(DenyAllPolicy), Arc::new(EntitlementCache::new()));
        let v = pol
            .evaluate(&outbox_row("cloud:console"), &sample_event(), Utc::now())
            .await;
        assert_eq!(v, DeliveryVerdict::Deliver);
    }

    #[tokio::test]
    async fn cloud_row_suppressed_when_entitlement_suspended() {
        let cache = Arc::new(EntitlementCache::new());
        cache.store(suspended_jwt());
        let pol = CloudAwarePolicy::new(Arc::new(AllowAllPolicy), cache);
        let v = pol
            .evaluate(&outbox_row("cloud:console"), &sample_event(), Utc::now())
            .await;
        assert_eq!(
            v,
            DeliveryVerdict::Suppressed(SuppressionReason::EntitlementSuspended)
        );
    }

    #[tokio::test]
    async fn external_row_delegates_to_inner_policy() {
        let pol = CloudAwarePolicy::new(Arc::new(DenyAllPolicy), Arc::new(EntitlementCache::new()));
        let v = pol
            .evaluate(&outbox_row("webhook:primary"), &sample_event(), Utc::now())
            .await;
        assert_eq!(
            v,
            DeliveryVerdict::Suppressed(SuppressionReason::GlobalDisabled)
        );
    }
}
