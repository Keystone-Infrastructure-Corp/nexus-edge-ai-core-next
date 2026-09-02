//! Boot-time cloud-tunnel supervisor.
//!
//! Reads `cloud_enrollment` once, and if present spawns a long-running
//! task that maintains the WSS+mTLS tunnel to `edge-gateway`. On
//! connect, the task sends a `Heartbeat` envelope every 30s. On
//! disconnect (any error or close frame), it backs off exponentially
//! and reconnects. The engine continues to serve locally even if the
//! tunnel never connects — Hard Rule 5 (fail-open).
//!
//! Phase 1.8 ships heartbeats only. RPC dispatch lands in the next
//! slice once `nexus-engine` has handlers.
//!
//! Phase 1.14 — this supervisor also owns the trace-uploader consumer
//! task: when the engine boots, `main.rs` calls
//! [`nexus_cloud_client::trace_uploader::TraceUploader::channel`] to
//! get the producer half (handed to the tracing subscriber) and the
//! receiver half (passed in here). Once `cloud_enrollment` is read,
//! the receiver is drained by a [`TraceUploader::run_with_mtls`] task
//! that reuses the same cert / key / CA chain as the tunnel itself.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::VerifyingKey;
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_cloud_client::trace_uploader::{
    Span, TraceUploader, TraceUploaderConfig, DEFAULT_BATCH_SIZE, DEFAULT_FLUSH_INTERVAL,
    DEFAULT_QUEUE_CAPACITY,
};
use nexus_cloud_client::{
    EnvelopeContext, RpcDispatcher, RpcResponseCache, SystemMethodPolicy, TrustedKey, TunnelClient,
    TunnelHandle, VerifiedActor, VerifierBuilder,
};
use nexus_cloud_protocol::v1::{
    BusEventPayload, CameraDecodeCounts, CameraDecodeHealthPayload, EdgeDegradation, EdgeHealth,
    Envelope, EnvelopeBody, EnvelopeMeta, HeartbeatPayload, SinkDeliveryCounts,
    SinkDeliveryHealthPayload, StorageEvictionAggressivePayload, StorageWatermarkPayload,
};
use nexus_storage::Registry;
use nexus_storage_cloud::{AzureBlobBackend, GatewaySasIssuer};
use nexus_store::cloud::CloudEnrollment;
use nexus_store::Store;
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::engine_rpc::{
    build_rpc_response_envelope, engine_rpc_response, EngineAuditSink, EngineRpcHandler,
};
use crate::storage_safety::WatermarkSignal;

/// Heartbeat cadence. Matches the cloud edge-gateway's `liveness_timeout / 2`
/// expectation.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// ADR-075 Tier 2 — everything [`pump_storage_events`] needs to publish the
/// edge's storage watermark level over the new `bus_event` kind.
///
/// Deliberately NOT a bespoke rate limiter or backoff: `bus_event` rides the
/// existing `nexus_cloud_client::tunnel::tier_of()` default (`Tier::Control`)
/// backpressure classification like any other low-volume kind (WIRE_PROTOCOL
/// §6) — the edge's own `WatermarkController` FSM (5-point hysteresis) is
/// already the anti-flap control, so no second one is layered on top here.
#[derive(Clone)]
pub struct StorageWatermarkHandle {
    pub signal: WatermarkSignal,
    pub bus: Arc<dyn Bus>,
    pub low_watermark_pct: u8,
    pub panic_watermark_pct: u8,
}

/// Spawn the tunnel supervisor. The task probes
/// `cloud_enrollment`; if the row is missing it parks on
/// `enrollment_changed` and re-probes on every notification, so a
/// post-boot admin enrollment (`POST /v1/admin/cloud/enroll` or the
/// `nexus-engine enroll` CLI re-using the same store path) activates
/// the WSS tunnel within seconds — no engine restart required. The
/// task only exits when the shutdown signal fires.
///
/// Note: re-enrollment *while the tunnel is already running* still
/// requires a restart to swap the live cert/key material. The
/// notification path is only consulted while the supervisor is in
/// the "no enrollment yet" wait state. Switching cloud hosts /
/// rotating an enrollment is a deliberate operation today; first-
/// time enrollment is the hot path.
///
/// Returns the shutdown sender + join handle pair so the engine
/// shutdown sequence can clean it up the same way it cleans up the
/// other long-running tasks.
///
/// `trace_rx`, when provided, is the consumer half of the
/// boot-time-allocated trace-uploader channel. After enrollment is
/// successfully read, a [`TraceUploader::run_with_mtls`] task is
/// spawned to drain the channel and ship batches to the edge-gateway.
/// While the supervisor is waiting for enrollment the receiver is
/// held but not drained: the bounded channel fills at
/// `queue_capacity` and further pushes from the `TraceLayer` fail
/// silently per the fail-open posture in Hard Rule 5. Once
/// enrollment lands, the drain task takes over and ships any spans
/// the channel could still hold.
///
/// `registry` and `replicator_kick` are wired by Phase 2 Step 2.1b:
/// post-enrollment we construct a [`GatewaySasIssuer`] + [`AzureBlobBackend`]
/// using the same mTLS cert material as the WSS tunnel, install it
/// in the registry under the reserved handle `"cloud"`, upsert a
/// matching `storage_backends` row (so the admin UI lists it), bind
/// `storage_cold_replica.backend_handle = "cloud"` if the singleton
/// is still NULL (first-enrollment default), and `notify_one()` the
/// replicator kick so any pre-enrollment clip backlog drains
/// immediately instead of waiting up to 5 min for the polling
/// backstop. Any error in this block is logged and the supervisor
/// continues — the engine remains fully functional locally (Hard
/// Rule 5 / fail-open).
#[allow(clippy::too_many_arguments)]
pub fn spawn_tunnel(
    store: Arc<Store>,
    registry: Registry,
    replicator_kick: Arc<Notify>,
    enrollment_changed: Arc<Notify>,
    cloud_outbox: Arc<nexus_cloud_client::TunnelOutbox>,
    entitlement_cache: Arc<nexus_cloud_client::entitlements::EntitlementCache>,
    pending_acks: Arc<crate::cloud_alert_sink::PendingAckRegistry>,
    snapshot_uploader_slot: crate::cloud_alert_sink::SnapshotUploaderSlot,
    live_view: Arc<crate::live_view::LiveViewManager>,
    frame_stats: Arc<nexus_pipeline::FrameStatsRegistry>,
    decode_health: Arc<nexus_pipeline::DecodeHealthRegistry>,
    webrtc: Arc<crate::webrtc_bridge::WebRtcBridge>,
    trace_rx: Option<mpsc::Receiver<Span>>,
    loopback_admin_base: Arc<arc_swap::ArcSwap<String>>,
    admin_secret: Option<Arc<String>>,
    remote_access: nexus_config::RemoteAccessConfig,
    liveness: Arc<crate::cloud_liveness::TunnelLiveness>,
    watermark: StorageWatermarkHandle,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        // Shared HTTP client for the admin-passthrough RPC handler.
        // Cheap to clone (internal `Arc`); reusing one client keeps
        // the connection pool alive across every cloud→edge admin
        // call so we're not re-establishing a TCP socket per
        // envelope.
        let admin_http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|e| {
                warn!(
                    error = %e,
                    "failed to build admin-passthrough http client; using default",
                );
                reqwest::Client::new()
            });
        // Outer wait-for-enrollment loop. The Phase 1.8 supervisor
        // exited immediately when no row was present, forcing the
        // operator to restart the engine after enrolling. Phase 1.16:
        // park on `enrollment_changed` so a post-boot enrollment
        // (admin POST or CLI) hot-activates the tunnel within seconds.
        let enrollment = loop {
            match store.get_cloud_enrollment().await {
                Ok(Some(e)) => break e,
                Ok(None) => {
                    info!(
                        "no cloud enrollment present; cloud tunnel idle until admin enrolls (POST /v1/admin/cloud/enroll) or engine restart",
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "could not read cloud_enrollment; will retry on next enrollment notification",
                    );
                }
            }
            tokio::select! {
                biased;
                _ = &mut rx => {
                    info!("cloud tunnel shutdown requested before enrollment");
                    return;
                }
                _ = enrollment_changed.notified() => {
                    info!("enrollment change notification received; re-probing cloud_enrollment");
                }
            }
        };
        info!(
            core_id = %enrollment.core_id,
            gateway_url = %enrollment.gateway_url,
            "starting cloud tunnel supervisor",
        );
        install_cloud_blob_backend(
            &enrollment,
            &store,
            &registry,
            &replicator_kick,
            &snapshot_uploader_slot,
        )
        .await;
        if let Some(trace_rx) = trace_rx {
            spawn_trace_uploader(&enrollment, trace_rx);
        }
        let dispatcher = build_rpc_dispatcher(
            &enrollment,
            &store,
            &replicator_kick,
            &loopback_admin_base,
            &admin_http_client,
            admin_secret.as_ref(),
        );
        run(
            enrollment,
            dispatcher,
            cloud_outbox,
            entitlement_cache,
            pending_acks,
            live_view,
            frame_stats,
            decode_health,
            webrtc,
            store,
            remote_access,
            liveness,
            watermark,
            rx,
        )
        .await;
    });
    (tx, handle)
}

/// Build the inbound `rpc_call` dispatcher from the
/// enrollment-bundled Ed25519 trusted-key PEM. Returns `None` (the
/// supervisor falls back to heartbeat-only mode) when:
///   * the enrollment artefact does not carry a `signing_key_pem`
///     (legacy enrollments minted before Phase 1.7 — should never
///     happen in practice because re-enrollment is a forced
///     migration, but the supervisor is fail-open per Hard Rule 5),
///   * the PEM does not parse as SPKI Ed25519 (corrupted artefact;
///     log + skip and let the operator re-enroll),
///   * `signing_kid` is missing (the cloud always emits one;
///     defensive).
fn build_rpc_dispatcher(
    enrollment: &CloudEnrollment,
    store: &Arc<Store>,
    replicator_kick: &Arc<Notify>,
    loopback_admin_base: &Arc<arc_swap::ArcSwap<String>>,
    http_client: &reqwest::Client,
    admin_secret: Option<&Arc<String>>,
) -> Option<Arc<RpcDispatcher<EngineRpcHandler>>> {
    let signing_pem = enrollment.signing_key_pem.as_deref().or_else(|| {
        warn!(
            core_id = %enrollment.core_id,
            "enrollment artefact missing signing_key_pem; inbound RPC dispatch disabled (heartbeat-only mode)",
        );
        None
    })?;
    let kid = enrollment.signing_kid.as_deref().or_else(|| {
        warn!(
            core_id = %enrollment.core_id,
            "enrollment artefact missing signing_kid; inbound RPC dispatch disabled (heartbeat-only mode)",
        );
        None
    })?;
    let key = match VerifyingKey::from_public_key_pem(signing_pem) {
        Ok(k) => k,
        Err(e) => {
            warn!(
                core_id = %enrollment.core_id,
                error = %e,
                "enrollment signing_key_pem failed to parse as Ed25519 SPKI; inbound RPC dispatch disabled",
            );
            return None;
        }
    };
    let trusted = TrustedKey {
        kid: kid.to_string(),
        key,
    };
    let Some(verifier) = VerifierBuilder::new(enrollment.core_id.clone())
        .trusted_key(trusted)
        .build()
    else {
        warn!(
            core_id = %enrollment.core_id,
            "verifier construction returned None despite a trusted_key present; bug?",
        );
        return None;
    };

    // System-sub policy: `entitlement_update` (existing) plus the Phase 9
    // OTA control verbs the orchestrator issues as
    // `system:update-orchestrator` — auto-halt rollback fan-out and
    // orchestrator-issued forced downgrades (WIRE_PROTOCOL §11.4). The
    // logical path each token binds to (`update_assignment` /
    // `update_cancel` / `update_rollback`) doubles as the whitelist key,
    // consulted out-of-band in `pump_rpc_dispatch` (the OTA handlers are
    // fire-and-forget and outlive the token TTL, so they never travel the
    // reply-bound `RpcCall` dispatch path). Human-actor forced assignments
    // and cancels ride the owner/admin/operator lane and skip this gate.
    let mut policy = SystemMethodPolicy::default();
    policy.permit("update_assignment");
    policy.permit("update_cancel");
    policy.permit("update_rollback");
    let handler = EngineRpcHandler {
        store: store.clone(),
        replicator_kick: replicator_kick.clone(),
        loopback_admin_base: loopback_admin_base.clone(),
        http_client: http_client.clone(),
        admin_secret: admin_secret.cloned(),
    };
    let dispatcher = RpcDispatcher::new(verifier, policy, handler)
        .with_audit_sink(Arc::new(EngineAuditSink {
            store: store.clone(),
        }))
        .with_response_cache(Arc::new(RpcResponseCache::new()));
    info!(
        core_id = %enrollment.core_id,
        kid = %kid,
        "inbound RPC dispatcher ready (Ed25519 verifier + replay cache wired)",
    );
    Some(Arc::new(dispatcher))
}

/// [`SnapshotUploader`](crate::cloud_alert_sink::SnapshotUploader) backed by
/// the gateway SAS issuer: mints a snapshot **write** SAS then PUTs the JPEG
/// straight to blob storage (never through the tunnel — Hard Rule 7).
/// Installed into the shared slot post-enrollment by
/// [`install_cloud_blob_backend`].
#[derive(Debug)]
struct GatewaySnapshotUploader {
    issuer: GatewaySasIssuer,
}

#[async_trait]
impl crate::cloud_alert_sink::SnapshotUploader for GatewaySnapshotUploader {
    async fn upload(&self, event_id: &str, jpeg: Vec<u8>) -> Result<String, String> {
        let sas = self
            .issuer
            .issue_snapshot_put(event_id)
            .await
            .map_err(|e| format!("snapshot SAS issuance: {e}"))?;
        crate::discovery::onvif_snapshot::put_snapshot_to_sas(&sas.url, &jpeg).await?;
        Ok(sas.blob_url_unsigned)
    }
}

/// Build the cloud `AzureBlobBackend` from the enrollment artefact
/// (mTLS cert chain for the SAS-issuance hop, plain HTTPS for direct
/// Azure Blob PUT/GET) and install it into the registry under the
/// reserved handle `"cloud"`. Idempotent — safe to call on every
/// supervisor boot.
///
/// Errors in this block are logged and swallowed (Hard Rule 5):
///   * SAS-issuer HTTP client construction failure → no cloud
///     replication this boot; engine continues serving locally.
///   * `upsert_storage_backend` SQL failure → ditto.
///   * `write_cold_replica` SQL failure → the existing binding (if
///     any) is left as-is; on next boot we try again.
async fn install_cloud_blob_backend(
    enrollment: &CloudEnrollment,
    store: &Arc<Store>,
    registry: &Registry,
    replicator_kick: &Arc<Notify>,
    snapshot_uploader_slot: &crate::cloud_alert_sink::SnapshotUploaderSlot,
) {
    // Reuse the trace-uploader's mTLS recipe verbatim for the SAS
    // issuance hop; the gateway authenticates the edge by client
    // cert just like for traces.
    let mtls_http = match build_mtls_http_client(
        enrollment.cert_pem.as_bytes(),
        enrollment.private_key_pem.as_bytes(),
        enrollment.ca_chain_pem.as_bytes(),
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                error = %e,
                "cloud blob backend: mTLS client build failed; cloud replication disabled this boot",
            );
            return;
        }
    };
    // Direct-to-Azure client. SAS URL carries its own auth; no
    // cert material needed here. We deliberately do NOT pin a tight
    // whole-request timeout: a single Put Blob of a large (but
    // in-bounds) clip over a slow edge uplink can legitimately run
    // for many minutes, and a fixed 5-min ceiling used to abort
    // every such PUT and wedge the cold queue. The per-PUT deadline
    // is now set size-proportionally at the call site
    // (`AzureBlobBackend::put` → `put_timeout_for`); the
    // client-level `timeout` here is only a coarse 2 h backstop so a
    // truly hung socket can't leak a task forever, while
    // `connect_timeout` still fails fast when the box is offline.
    let azure_http = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(2 * 60 * 60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                error = %e,
                "cloud blob backend: Azure direct HTTP client build failed; cloud replication disabled this boot",
            );
            return;
        }
    };
    let gateway_url = derive_https_base(&enrollment.gateway_url);
    let issuer = Arc::new(GatewaySasIssuer::new(mtls_http, gateway_url.clone()));
    // Install the alert-snapshot uploader now that we have an
    // mTLS-authenticated SAS path. The cloud alert sink reads this slot
    // per delivery to PUT the rule-fire thumbnail before sending the alert.
    snapshot_uploader_slot
        .write()
        .replace(Arc::new(GatewaySnapshotUploader {
            issuer: (*issuer).clone(),
        })
            as Arc<dyn crate::cloud_alert_sink::SnapshotUploader>);
    let backend: Arc<dyn nexus_storage::ColdBackend> =
        Arc::new(AzureBlobBackend::new("cloud", issuer, azure_http));

    // Persist a `storage_backends` row so the admin UI surfaces
    // the cloud backend. `build_any_backend` refuses to (re)build
    // this kind from the row (the cloud-tunnel supervisor owns
    // the live impl); `rebuild_registry` skips it with a warn.
    let config_json = format!(r#"{{"gateway_url":"{gateway_url}"}}"#);
    if let Err(e) = store
        .upsert_storage_backend("cloud", "azure_blob", &config_json)
        .await
    {
        warn!(
            error = %e,
            "cloud blob backend: upsert_storage_backend(\"cloud\") failed; admin listing will not show it",
        );
        // Continue anyway — the in-memory registry entry below is
        // what the cold replicator actually consumes.
    }

    // Auto-bind cold replication to the cloud backend on first
    // enrollment so the operator does not have to flip a switch
    // for clips to start uploading. If the operator has already
    // configured a LAN/USB/Drive backend, leave their choice
    // alone — they can switch to "cloud" via the admin UI.
    match store.read_cold_replica().await {
        Ok(cur) => {
            if cur.backend_handle.is_none() {
                if let Err(e) = store
                    .write_cold_replica(Some("cloud"), cur.throttle_bps)
                    .await
                {
                    warn!(
                        error = %e,
                        "cloud blob backend: write_cold_replica(\"cloud\") failed; replication will stay disabled until operator picks one",
                    );
                } else {
                    info!(
                        gateway_url = %gateway_url,
                        "cloud blob backend: auto-bound storage_cold_replica → \"cloud\" (was NULL)",
                    );
                }
            } else {
                info!(
                    current = ?cur.backend_handle,
                    "cloud blob backend: storage_cold_replica already bound; leaving operator choice intact",
                );
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "cloud blob backend: read_cold_replica failed; cannot auto-bind",
            );
        }
    }

    registry.insert_reserved(backend);
    replicator_kick.notify_one();
    info!(
        gateway_url = %gateway_url,
        "cloud blob backend installed under reserved handle \"cloud\"; cold replicator kicked",
    );
}

/// Mirror of [`nexus_cloud_client::trace_uploader::build_mtls_transport`]
/// minus the `BatchTransport` wrapper — we just need the bare
/// `reqwest::Client` for the SAS-issuance POST.
fn build_mtls_http_client(
    cert_pem: &[u8],
    key_pem: &[u8],
    ca_chain_pem: &[u8],
) -> Result<reqwest::Client, String> {
    let identity = reqwest::Identity::from_pem(&[cert_pem, key_pem].concat())
        .map_err(|e| format!("reqwest identity from PEM: {e}"))?;
    let ca = reqwest::Certificate::from_pem(ca_chain_pem)
        .map_err(|e| format!("reqwest ca from PEM: {e}"))?;
    reqwest::Client::builder()
        .use_rustls_tls()
        .identity(identity)
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("reqwest build: {e}"))
}

/// Derive the HTTPS base for cloud APIs from the tunnel URL
/// (`wss://host/v1/tunnel` → `https://host`). Matches the same
/// transform [`derive_trace_endpoint`] does but stops before
/// appending the per-API suffix.
fn derive_https_base(wss_url: &str) -> String {
    let base = wss_url
        .strip_prefix("wss://")
        .map(|s| format!("https://{s}"))
        .or_else(|| wss_url.strip_prefix("ws://").map(|s| format!("http://{s}")))
        .unwrap_or_else(|| wss_url.to_string());
    let trimmed = base.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1/tunnel")
        .unwrap_or(trimmed)
        .to_string()
}

/// Derive the HTTP(S) base URL for cloud APIs from the websocket
/// tunnel URL: replace `wss://` with `https://` (or `ws://` with
/// `http://`), strip any trailing `/v1/tunnel` path, and append
/// `/v1/edge/traces`.
fn derive_trace_endpoint(wss_url: &str) -> String {
    let base = wss_url
        .strip_prefix("wss://")
        .map(|s| format!("https://{s}"))
        .or_else(|| wss_url.strip_prefix("ws://").map(|s| format!("http://{s}")))
        .unwrap_or_else(|| wss_url.to_string());
    let trimmed = base.trim_end_matches('/');
    let stripped = trimmed.strip_suffix("/v1/tunnel").unwrap_or(trimmed);
    format!("{stripped}/v1/edge/traces")
}

fn spawn_trace_uploader(enrollment: &CloudEnrollment, rx: mpsc::Receiver<Span>) {
    let core_id = match Uuid::parse_str(&enrollment.core_id) {
        Ok(id) => id,
        Err(e) => {
            warn!(
                core_id = %enrollment.core_id,
                error = %e,
                "cloud enrollment core_id is not a valid UUID; trace uploader disabled",
            );
            return;
        }
    };
    let endpoint_url = derive_trace_endpoint(&enrollment.gateway_url);
    let cfg = TraceUploaderConfig {
        endpoint_url,
        core_id,
        batch_size: DEFAULT_BATCH_SIZE,
        flush_interval: DEFAULT_FLUSH_INTERVAL,
        queue_capacity: DEFAULT_QUEUE_CAPACITY,
    };
    match TraceUploader::run_with_mtls(
        rx,
        cfg,
        enrollment.cert_pem.as_bytes(),
        enrollment.private_key_pem.as_bytes(),
        enrollment.ca_chain_pem.as_bytes(),
    ) {
        Ok(_join) => {
            info!(
                core_id = %enrollment.core_id,
                "trace uploader spawned; engine spans will ship to edge-gateway",
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                "trace uploader spawn failed; engine spans will not ship",
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    enrollment: CloudEnrollment,
    dispatcher: Option<Arc<RpcDispatcher<EngineRpcHandler>>>,
    cloud_outbox: Arc<nexus_cloud_client::TunnelOutbox>,
    entitlement_cache: Arc<nexus_cloud_client::entitlements::EntitlementCache>,
    pending_acks: Arc<crate::cloud_alert_sink::PendingAckRegistry>,
    live_view: Arc<crate::live_view::LiveViewManager>,
    frame_stats: Arc<nexus_pipeline::FrameStatsRegistry>,
    decode_health: Arc<nexus_pipeline::DecodeHealthRegistry>,
    webrtc: Arc<crate::webrtc_bridge::WebRtcBridge>,
    store: Arc<Store>,
    remote_access: nexus_config::RemoteAccessConfig,
    liveness: Arc<crate::cloud_liveness::TunnelLiveness>,
    watermark: StorageWatermarkHandle,
    mut shutdown: oneshot::Receiver<()>,
) {
    let client = TunnelClient::new(
        enrollment.gateway_url.clone(),
        enrollment.cert_pem.clone(),
        enrollment.private_key_pem.clone(),
        enrollment.ca_chain_pem.clone(),
    );
    // Remote shell. Built once per engine, not per reconnect: sessions
    // ride their own socket, so a control-tunnel blip does not have to
    // kill an operator's shell mid-keystroke. A real disconnect does
    // — see the `close_all` on the reconnect path below — because a
    // session the console can no longer revoke has no business running.
    let remote_shell = Arc::new(crate::remote_shell::RemoteShellManager::new(
        remote_access,
        client.clone(),
        dispatcher.as_ref().map(|d| d.verifier().clone()),
        Arc::clone(&cloud_outbox),
    ));
    // Adopt the cloud's SSH certificate authority. Runs once per engine
    // start, and only when BOTH the box owner opted in locally and the
    // cloud actually shipped a CA in the enrollment bundle. Idempotent, so
    // it also repairs a drop-in an OS-level sshd reconfiguration removed.
    // Deliberately fire-and-forget: a box with no sshd, or a failed
    // adoption, must never delay or block the control tunnel — remote
    // shell just stays unavailable and every later session open reports
    // the failure to the console.
    if remote_shell.is_enabled() {
        if let Some(ca) = enrollment.ssh_ca_public_key.clone() {
            tokio::spawn(async move {
                match crate::ssh_ca::install_ca(&ca).await {
                    Ok(()) => info!("remote access: SSH CA adopted"),
                    Err(e) => warn!(
                        reason = e.code(),
                        "remote access: SSH CA adoption failed; native sessions will be refused"
                    ),
                }
            });
        } else {
            warn!(
                "remote access is enabled locally but this enrollment carries no SSH CA; \
                 re-enroll against a cloud that has one provisioned"
            );
        }
    }
    let mut backoff = BACKOFF_MIN;
    let core_id = enrollment.core_id.clone();
    // Outlives the connection on purpose: this is the decode-health
    // baseline, and rebuilding it per connection would make the census sent
    // on reconnect all-zero (see `pump_camera_decode_health`).
    let mut decode_window = DecodeHealthWindow::default();
    // Same reason, different failure: rebuilding this per connection would
    // reset both the counts and the window deadline, so a core reconnecting
    // faster than `EVICTION_WINDOW` would never reach a boundary and never
    // report (see `pump_eviction_events`).
    let mut eviction_window = EvictionWindow::new();
    loop {
        // Check for shutdown before each connect attempt.
        if shutdown.try_recv().is_ok() {
            info!(core_id = %core_id, "cloud tunnel shutdown requested");
            cloud_outbox.set_handle(None);
            return;
        }
        match client.connect().await {
            Ok(mut conn) => {
                backoff = BACKOFF_MIN;
                // Take the inbound receiver BEFORE Arc-wrapping the
                // Connection — `take_inbound` is `&mut self` so it
                // needs unique access.
                let inbound = conn.take_inbound();
                let conn = Arc::new(conn);
                // Phase 2 · Step 2.8 — publish the live handle into
                // the shared outbox so the cold replicator (and any
                // future publisher) can fire envelopes through this
                // session. Cleared on every disconnect path below.
                cloud_outbox.set_handle(Some(
                    conn.clone() as Arc<dyn nexus_cloud_client::TunnelHandle>
                ));
                let pump = pump_heartbeats(
                    &*conn,
                    &core_id,
                    store.clone(),
                    &liveness,
                    &live_view,
                    &frame_stats,
                );
                let dispatch = pump_rpc_dispatch(
                    &*conn,
                    inbound,
                    dispatcher.as_deref(),
                    &core_id,
                    &cloud_outbox,
                    &entitlement_cache,
                    &pending_acks,
                    &store,
                    &live_view,
                    &webrtc,
                    &remote_shell,
                    &liveness,
                );
                let storage_events = pump_storage_events(&*conn, &watermark);
                let eviction_events =
                    pump_eviction_events(&*conn, &watermark.bus, &mut eviction_window);
                let sink_health = pump_sink_delivery_health(&*conn, &watermark.bus, &store);
                let decode_events =
                    pump_camera_decode_health(&*conn, &decode_health, &mut decode_window);
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        info!(core_id = %core_id, "cloud tunnel shutdown requested");
                        cloud_outbox.set_handle(None);
                        return;
                    }
                    _ = pump => {
                        // pump returns when send fails -> tunnel down -> reconnect.
                        warn!(core_id = %core_id, "cloud tunnel heartbeat pump exited; will reconnect");
                    }
                    _ = dispatch => {
                        // Inbound channel closed (reader task ended) -> tunnel down.
                        // A peer that vanishes without a reset reaches this arm via
                        // the socket's TCP_USER_TIMEOUT (BUG-133), not on its own.
                        warn!(core_id = %core_id, "cloud tunnel inbound dispatch ended; will reconnect");
                    }
                    _ = storage_events => {
                        // ADR-075 Tier 2 — send failure means the tunnel is
                        // down; reconnect (the initial republish on the new
                        // connection is what recovers the sticky level).
                        warn!(core_id = %core_id, "cloud tunnel storage watermark pump exited; will reconnect");
                    }
                    _ = eviction_events => {
                        warn!(core_id = %core_id, "cloud tunnel eviction pump exited; will reconnect");
                    }
                    _ = sink_health => {
                        warn!(core_id = %core_id, "cloud tunnel sink delivery-health pump exited; will reconnect");
                    }
                    _ = decode_events => {
                        warn!(core_id = %core_id, "cloud tunnel camera decode-health pump exited; will reconnect");
                    }
                }
                cloud_outbox.set_handle(None);
                live_view.clear_all();
                webrtc.clear_all();
                remote_shell.close_all();
            }
            Err(e) => {
                warn!(
                    core_id = %core_id,
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "cloud tunnel connect failed; backing off",
                );
            }
        }
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(core_id = %core_id, "cloud tunnel shutdown requested");
                cloud_outbox.set_handle(None);
                return;
            }
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
    }
}

/// Verify the `actor_token` on a Phase 9 OTA control envelope out-of-band.
///
/// The OTA update handlers are fire-and-forget and outlive the token TTL, so
/// they never travel the reply-bound [`RpcDispatcher::dispatch`] path. This
/// mirrors the `diag_collect` gate: verify the signature + claims against a
/// LOGICAL RPC path (there is no HTTP path on an update payload — the cloud
/// binds `path=update_assignment` / `update_cancel` / `update_rollback`),
/// then require EITHER a privileged human role (owner/admin/operator) OR a
/// `system:`-sub token whose logical path is on the dispatcher's
/// system-method whitelist — the same authorisation the dispatcher applies
/// to an inbound `rpc_call`.
fn verify_update_actor(
    disp: &RpcDispatcher<EngineRpcHandler>,
    token: Option<&str>,
    logical_path: &str,
) -> Result<VerifiedActor, String> {
    let token = token.ok_or_else(|| "actor_token missing".to_string())?;
    let actor = disp
        .verifier()
        .verify(
            token,
            EnvelopeContext {
                method: "POST",
                path: logical_path,
            },
        )
        .map_err(|reason| format!("actor_token invalid: {reason}"))?;
    if actor.sub.starts_with("system:") {
        if !disp.policy().allows(logical_path) {
            return Err(format!(
                "system sub `{}` not permitted for `{logical_path}`",
                actor.sub
            ));
        }
    } else if !crate::engine_rpc::is_priviledged_role(&actor.role) {
        return Err(format!("actor role `{}` lacks privilege", actor.role));
    }
    Ok(actor)
}

/// Drain inbound envelopes off the tunnel reader's channel. For
/// every `rpc_call`, build the response envelope (with the
/// `EngineRpcHandler`-derived status code) and send it back through
/// the same `TunnelHandle`. Non-RpcCall envelopes (entitlement_update,
/// clip_replicated_ack, future cloud→edge variants) are debug-logged
/// and skipped — those have their own consumers wired elsewhere or
/// are not yet handled.
///
/// Returns when:
///   * the inbound channel is closed (tunnel reader exited),
///   * we have no dispatcher (heartbeat-only mode — we still drain
///     the channel so `try_send` doesn't backpressure-drop the next
///     non-RpcCall envelope that does have a consumer).
///   * the outbound `handle.send` errors (tunnel writer died) —
///     supervisor reconnects.
#[allow(clippy::too_many_arguments)]
async fn pump_rpc_dispatch<H: TunnelHandle>(
    handle: &H,
    inbound: Option<mpsc::Receiver<Envelope>>,
    dispatcher: Option<&RpcDispatcher<EngineRpcHandler>>,
    core_id: &str,
    outbox: &Arc<nexus_cloud_client::TunnelOutbox>,
    entitlement_cache: &Arc<nexus_cloud_client::entitlements::EntitlementCache>,
    pending_acks: &Arc<crate::cloud_alert_sink::PendingAckRegistry>,
    store: &Arc<Store>,
    live_view: &Arc<crate::live_view::LiveViewManager>,
    webrtc: &Arc<crate::webrtc_bridge::WebRtcBridge>,
    remote_shell: &Arc<crate::remote_shell::RemoteShellManager>,
    liveness: &crate::cloud_liveness::TunnelLiveness,
) {
    let Some(mut rx) = inbound else {
        debug!(core_id = %core_id, "no inbound channel on this connection; pump idle");
        // Park forever so the supervisor's tokio::select! arm
        // doesn't fire spuriously. The select dropping the future
        // on shutdown is fine.
        std::future::pending::<()>().await;
        return;
    };
    while let Some(env) = rx.recv().await {
        match &env.body {
            EnvelopeBody::RpcCall(_) => {
                let Some(disp) = dispatcher else {
                    // No dispatcher (heartbeat-only mode) — reply with
                    // a synthetic 503 so the cloud's send_mutating_rpc
                    // surfaces the misconfiguration cleanly instead of
                    // timing out.
                    let payload = nexus_cloud_protocol::v1::RpcResponsePayload {
                        body: serde_json::json!({
                            "error": "rpc_disabled",
                            "message": "inbound RPC dispatch disabled on this engine (missing enrollment signing key)",
                        }),
                        status: 503,
                    };
                    let resp = build_rpc_response_envelope(&env, payload);
                    if let Err(e) = handle.send(resp).await {
                        warn!(
                            core_id = %core_id,
                            error = %e,
                            "rpc dispatch (no-op) send failed; tunnel writer down",
                        );
                        return;
                    }
                    continue;
                };
                let payload = engine_rpc_response(disp, &env).await;
                let resp = build_rpc_response_envelope(&env, payload);
                if let Err(e) = handle.send(resp).await {
                    warn!(
                        core_id = %core_id,
                        error = %e,
                        "rpc dispatch send failed; tunnel writer down",
                    );
                    return;
                }
            }
            EnvelopeBody::EntitlementUpdate(payload) => {
                // Phase 3 / cloud ARCHITECTURE §12.4 — cache the latest
                // entitlement JWT so the M7 `CloudAwarePolicy` can suppress
                // the always-on `cloud:console` audit sink when the org is
                // suspended for non-payment. Claims are re-decoded
                // (unverified) on each read; we just store the compact JWS.
                let previous = entitlement_cache.store(payload.jwt.clone());
                if previous.as_deref() != Some(payload.jwt.as_str()) {
                    debug!(
                        core_id = %core_id,
                        suspended = entitlement_cache.is_suspended(),
                        "entitlement_update cached",
                    );
                }
            }
            EnvelopeBody::AlertAck(ack) => {
                // Cloud confirmation for an alert we forwarded. Correlate it
                // back to the waiting `deliver()` via `in_reply_to` (the sent
                // envelope's meta.id) and fire the outcome so the M7 outbox
                // row transitions correctly: stored → sent, permanent_failure
                // → dead. (No ack → deliver() times out → transient retry.)
                let outcome = match ack.status.as_str() {
                    "stored" | "received" => crate::cloud_alert_sink::AckOutcome::Stored,
                    "permanent_failure" => {
                        crate::cloud_alert_sink::AckOutcome::PermanentFailure(ack.reason.clone())
                    }
                    other => {
                        debug!(core_id = %core_id, status = %other, "alert_ack: unknown status, treating as stored");
                        crate::cloud_alert_sink::AckOutcome::Stored
                    }
                };
                match &env.meta.in_reply_to {
                    Some(reply_id) => {
                        let fired = pending_acks.fire(reply_id, outcome);
                        debug!(core_id = %core_id, in_reply_to = %reply_id, status = %ack.status, fired, "alert_ack correlated");
                    }
                    None => {
                        warn!(core_id = %core_id, status = %ack.status, "alert_ack missing in_reply_to; cannot correlate");
                    }
                }
            }
            EnvelopeBody::DiagCollect(payload) => {
                // Phase 7.0a — verified out-of-band here (NOT through the
                // RpcCall dispatch path, which is reply-bound) because the
                // collector outlives the request: tarball assembly + upload
                // routinely exceed the actor_token TTL, so we spawn a
                // detached task and confirm later via a `diag_ready`.
                let Some(disp) = dispatcher else {
                    // Heartbeat-only mode: no signing key, so we cannot
                    // authenticate the actor_token. Drop — the cloud sees
                    // the run stay non-terminal and the operator re-collects.
                    warn!(
                        core_id = %core_id,
                        "diag_collect received but inbound dispatch is disabled (missing enrollment signing key); dropping",
                    );
                    continue;
                };
                match disp.verifier().verify(
                    &payload.actor_token,
                    EnvelopeContext {
                        method: "POST",
                        path: "diag_collect",
                    },
                ) {
                    Ok(actor) if crate::engine_rpc::is_priviledged_role(&actor.role) => {
                        disp.handler()
                            .spawn_diag_collect(payload.clone(), Arc::clone(outbox));
                    }
                    Ok(actor) => {
                        warn!(
                            core_id = %core_id,
                            role = %actor.role,
                            "diag_collect actor lacks a privileged role; dropping",
                        );
                    }
                    Err(reason) => {
                        warn!(
                            core_id = %core_id,
                            reason = %reason,
                            "diag_collect actor_token verification failed; dropping",
                        );
                    }
                }
            }
            // Phase 10 Live View — LBR pump lifecycle. The cloud LiveHub
            // ref-counts browser subscribers and sends exactly one subscribe
            // per (core, camera) (re-sent on tile/tier change) and one
            // unsubscribe when the last viewer leaves; the manager keeps a
            // single encode task per camera (encode-once fan-out).
            EnvelopeBody::LbrSubscribe(payload) => {
                live_view.on_subscribe(payload);
            }
            EnvelopeBody::LbrUnsubscribe(payload) => {
                live_view.on_unsubscribe(payload);
            }
            // Phase 2 dual-transport — SFU HD publish signalling. The cloud
            // sends live_hd_start for the single expanded camera; the bridge
            // builds a send-only publisher webrtcbin, gathers ICE, and emits
            // live_hd_offer. live_hd_answer carries the SFU's answer;
            // live_hd_stop tears the publisher down. No-op (logged) without
            // the gstreamer-webrtc feature — the heartbeat never advertised
            // `hd_sfu` then, so this is defence in depth.
            EnvelopeBody::LiveHdStart(payload) => {
                webrtc.on_live_hd_start(payload, outbox);
            }
            EnvelopeBody::LiveHdAnswer(payload) => {
                webrtc.on_live_hd_answer(payload);
            }
            EnvelopeBody::LiveHdStop(payload) => {
                webrtc.on_live_hd_stop(payload);
            }
            // Cloud-computed downlink hint: clamp the running publisher's
            // encoder to the slowest browser viewer's measured receive path.
            // Closes the end-to-end congestion loop the raw SFU doesn't relay.
            EnvelopeBody::LiveHdBitrate(payload) => {
                webrtc.on_live_hd_bitrate(payload);
            }
            // Remote shell. Verified out-of-band here rather than on the
            // reply-bound RpcCall path for the same reason `diag_collect`
            // is: a session outlives by orders of magnitude the 30-second
            // token that authorised opening it.
            EnvelopeBody::ShellSessionOpen(payload) => {
                remote_shell.on_open(payload.clone());
            }
            EnvelopeBody::ShellSessionClose(payload) => {
                remote_shell.on_close(payload);
            }
            other => {
                if let EnvelopeBody::HeartbeatAck(ack) = other {
                    // The only proof the cloud is still on the other end of
                    // this socket. Everything else the engine can observe
                    // stays healthy across a half-open connection.
                    liveness.mark_ack();
                    outbox.update_caps(ack.cloud_capabilities.as_deref());
                    debug!(
                        core_id = %core_id,
                        cap_count = ack.cloud_capabilities.as_ref().map_or(0, Vec::len),
                        "cloud heartbeat_ack capabilities refreshed",
                    );
                    continue;
                }
                // Phase 9 (M_OTA) — OTA update envelopes. Dispatch is
                // fire-and-forget: the handlers persist state + emit
                // `update_progress` asynchronously, never a sync reply.
                // Every state-mutating verb re-verifies its `actor_token`
                // out-of-band here (REPO_BOUNDARY R4c) — the same posture
                // as `diag_collect` — because the apply outlives the token
                // TTL and never travels the reply-bound RpcCall path.
                match other {
                    EnvelopeBody::UpdateAssignment(p) => {
                        // Routine cohort assignments trust the Ed25519
                        // manifest signature (re-verified inside the
                        // handler) and carry no actor_token. Operator
                        // `force` and orchestrator-issued downgrades DO
                        // carry one and MUST verify before we apply.
                        let forced = p.force.unwrap_or(false) || p.allow_downgrade.unwrap_or(false);
                        if forced {
                            let Some(disp) = dispatcher else {
                                warn!(
                                    core_id = %core_id,
                                    "forced update_assignment received but inbound dispatch is disabled (missing enrollment signing key); dropping",
                                );
                                continue;
                            };
                            match verify_update_actor(
                                disp,
                                p.actor_token.as_deref(),
                                "update_assignment",
                            ) {
                                Ok(actor) => {
                                    info!(
                                        core_id = %core_id,
                                        sub = %actor.sub,
                                        assignment_id = %p.assignment_id,
                                        "forced update_assignment actor_token verified",
                                    );
                                }
                                Err(reason) => {
                                    warn!(
                                        core_id = %core_id,
                                        reason = %reason,
                                        "forced update_assignment actor_token rejected; dropping",
                                    );
                                    continue;
                                }
                            }
                        }
                        crate::cloud_update::handle_assignment(
                            Arc::clone(store),
                            Arc::clone(outbox),
                            p.clone(),
                        )
                        .await;
                        continue;
                    }
                    EnvelopeBody::UpdateRollback(p) => {
                        // Rollback ALWAYS carries a verified actor_token
                        // (system:update-orchestrator on auto-halt, or a
                        // human operator token) — there is no manifest to
                        // anchor trust, so the token is the only gate.
                        let Some(disp) = dispatcher else {
                            warn!(
                                core_id = %core_id,
                                "update_rollback received but inbound dispatch is disabled (missing enrollment signing key); dropping",
                            );
                            continue;
                        };
                        match verify_update_actor(disp, p.actor_token.as_deref(), "update_rollback")
                        {
                            Ok(actor) => {
                                info!(
                                    core_id = %core_id,
                                    sub = %actor.sub,
                                    reason = %p.reason,
                                    "update_rollback actor_token verified",
                                );
                                crate::cloud_update::handle_rollback(
                                    Arc::clone(store),
                                    Arc::clone(outbox),
                                    p.clone(),
                                )
                                .await;
                            }
                            Err(reason) => {
                                warn!(
                                    core_id = %core_id,
                                    reason = %reason,
                                    "update_rollback actor_token rejected; dropping",
                                );
                            }
                        }
                        continue;
                    }
                    EnvelopeBody::UpdateCancel(p) => {
                        // Cancel is honoured only before the restart is
                        // committed; once the symlink flip + restart
                        // fires the new version simply heartbeats and the
                        // orchestrator reconciles. The apply task runs
                        // detached, so a best-effort log is the contract
                        // here — mid-flight cancellation of an in-process
                        // download is a post-v1 refinement. We still
                        // verify the actor_token so a spoofed cancel
                        // cannot even reach that best-effort path.
                        let Some(disp) = dispatcher else {
                            warn!(
                                core_id = %core_id,
                                "update_cancel received but inbound dispatch is disabled (missing enrollment signing key); dropping",
                            );
                            continue;
                        };
                        match verify_update_actor(disp, p.actor_token.as_deref(), "update_cancel") {
                            Ok(actor) => {
                                debug!(
                                    core_id = %core_id,
                                    sub = %actor.sub,
                                    assignment_id = %p.assignment_id,
                                    "update_cancel verified (best-effort; ignored once restart committed)",
                                );
                            }
                            Err(reason) => {
                                warn!(
                                    core_id = %core_id,
                                    reason = %reason,
                                    "update_cancel actor_token rejected; dropping",
                                );
                            }
                        }
                        continue;
                    }
                    EnvelopeBody::UpdateProgress(_) => {
                        warn!(
                            core_id = %core_id,
                            "refused inbound update_progress (edge→cloud only)",
                        );
                        continue;
                    }
                    _ => {}
                }
                debug!(
                    core_id = %core_id,
                    kind = ?std::mem::discriminant(other),
                    "inbound envelope is not rpc_call; no engine consumer wired",
                );
            }
        }
    }
    debug!(core_id = %core_id, "inbound channel closed; dispatch pump exiting");
}

/// The heartbeat `caps` tags this engine advertises.
///
/// Dual-transport live view: the always-on LBR pump (`live_view`) plus the
/// per-core configured HD transport (`hd_sfu` / `hd_moq`), so the cloud routes
/// an expanding operator to the matching client adapter. No back-compat — the
/// old single `webrtc` tag is gone. Additive on wire `v=1`.
///
/// `talkdown_webrtc` is deliberately **not** advertised. It used to be pushed
/// whenever `feature = "gstreamer-webrtc"` was on, but that feature gates the
/// HD *publish* bridge ([`crate::webrtc_bridge`]); this engine has no
/// receive-side talk-down sub-pipeline at all, so the tag claimed a capability
/// nothing implements. Nothing cloud-side routes on it today, which is the only
/// reason that was latent rather than an operator holding a dead mic. Restore
/// it in the same change that lands the sub-pipeline (webrtcbin recvonly →
/// Opus decode → PCMU/PCMA → the camera's RTSP backchannel, whose URL and codec
/// [`nexus_types::CameraTalkDown`] already discovers), gated on the camera
/// actually having one.
fn heartbeat_caps(hd_transport: nexus_types::HdTransport) -> Vec<String> {
    vec!["live_view".to_string(), hd_transport.cap_tag().to_string()]
}

async fn pump_heartbeats<H: TunnelHandle>(
    handle: &H,
    _core_id: &str,
    store: Arc<Store>,
    liveness: &crate::cloud_liveness::TunnelLiveness,
    live_view: &crate::live_view::LiveViewManager,
    frame_stats: &nexus_pipeline::FrameStatsRegistry,
) {
    // Reaching this function at all means the WSS handshake and the mTLS
    // client-certificate check both succeeded.
    liveness.mark_authenticated();
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let start = std::time::Instant::now();
    let mut seq: u64 = 0;
    loop {
        interval.tick().await;
        seq = seq.wrapping_add(1);
        // Phase C: stamp the operator-set display name on every
        // heartbeat so the cloud gateway can refresh its
        // `cores.name` cache. Read on each tick — operators can
        // change the name in the local UI and the cloud sees it
        // within ~30 s with no engine restart. A failure here is
        // logged but never blocks the heartbeat itself.
        let name = crate::admin_runtime::read_display_name(&store).await;
        // Dual-transport live view — read the per-core configured HD transport
        // each tick so a cloud fleet flip is reflected in the advertised caps
        // within one heartbeat, no restart.
        let hd_transport = crate::admin_runtime::read_hd_transport(&store).await;
        // Phase 9 (M_OTA) — report the OTA status block so the
        // orchestrator can drive its rollout state machine + reconcile
        // committed versions. `recording_active` is best-effort false
        // here; the SIGTERM drain is the real recording-safety guarantee.
        let release = Some(crate::cloud_update::release_status_for_heartbeat(&store, false).await);
        // Fail-loud health roll-up. A detector that could not load its
        // model leaves the engine fully "online" from the tunnel's point
        // of view — it records, streams, and answers RPC — while
        // producing zero detections. Without this field the cloud sees a
        // green core and an operator sees only silence, which is
        // indistinguishable from a genuinely quiet site. Recomputed each
        // tick so a repaired detector clears within one heartbeat.
        let health = Some(edge_health(
            &live_view.stalled_cameras(),
            crate::system_metrics::snapshot().decode_capacity.as_ref(),
        ));
        // Camera-liveness rollup — same `FrameStatsRegistry` source of
        // truth as `roster::build_envelope`'s per-camera `online` field,
        // so the two can never disagree. Deliberately NOT
        // `live_view.stalled_cameras()`: that registry only tracks
        // cameras with an active live-view subscriber, which would
        // misreport every unwatched camera as offline. Cheap: an
        // in-memory map read, safe at the 30s heartbeat cadence.
        let now = chrono::Utc::now();
        let online_cameras = frame_stats
            .snapshot_all()
            .values()
            .filter(|s| s.is_online(now))
            .count() as u64;
        // Durable outbox depth — rows still pending delivery, not the
        // count ever enqueued. A query failure is logged and reported
        // as 0 rather than blocking the heartbeat; a transient DB hiccup
        // should not stall the tunnel's only liveness signal.
        let queued_alerts = match store.outbox_queued_count().await {
            Ok(n) => n.try_into().unwrap_or(0),
            Err(e) => {
                warn!(error = %e, "heartbeat: outbox queued-count query failed");
                0
            }
        };
        let env = Envelope {
            meta: EnvelopeMeta {
                id: uuid::Uuid::now_v7().to_string(),
                in_reply_to: None,
                seq: Some(seq),
                trace: None,
                ts: chrono::Utc::now().to_rfc3339(),
                v: 1,
            },
            body: EnvelopeBody::Heartbeat(HeartbeatPayload {
                edge_ts_unix_ms: Some(now_unix_ms()),
                name,
                caps: Some(heartbeat_caps(hd_transport)),
                online_cameras,
                queued_alerts,
                release,
                health,
                // Optional cloud-side capability diagnostic (wire `v=1`,
                // repurposed in place). Populating it with the engine's real
                // probed capability profile (from config `ep_priority` / the
                // device manifest) is a follow-up; the cloud treats the
                // omitted field as "unknown" until then.
                capability_profile: None,
                uptime_s: start.elapsed().as_secs(),
                // See `build.rs` — release-tag at CI build-time, falls
                // back to `CARGO_PKG_VERSION` for local dev builds.
                version: env!("NEXUS_BUILD_VERSION").to_string(),
            }),
        };
        if let Err(e) = handle.send(env).await {
            warn!(error = %e, "heartbeat send failed; pump exiting");
            return;
        }
        // Deliberately no liveness update here. `send` resolving means the
        // uplink queue accepted the frame, which a half-open socket does
        // forever; only an inbound `heartbeat_ack` proves the cloud is
        // there, and `pump_rpc_dispatch` records that (BUG-133).
    }
}

/// ADR-075 Tier 2 — publish the edge's storage watermark level over the new
/// `bus_event` kind.
///
/// # Stickiness / reconnect self-heal
/// Sends the CURRENT level immediately on entry — i.e. on every fresh
/// connection, not only on a threshold crossing. This is the property that
/// makes Tier 2 acceptable for a lossy tunnel per ADR-075: `TunnelOutbox`
/// does not persist envelopes, so anything in flight during a disconnect is
/// gone, but because the level is a sticky STATE rather than a one-shot
/// EDGE, the very next publish (this one, right after reconnect) always
/// carries the box's true current level regardless of what was lost. A
/// duplicate publish of the same level the cloud already knows about is
/// harmless by construction — the cloud does not key anything off
/// occurrence count, only the level value (see `edge-gateway::bus_event`'s
/// module doc for the cloud-side idempotency argument).
///
/// After the initial publish, subscribes fresh to `topic::STORAGE_PANIC`
/// and forwards every subsequent transition for the life of this
/// connection. `BroadcastBus` subscribers only see FUTURE messages — a
/// transition that happened while this pump wasn't running (i.e. while
/// disconnected) is NOT replayed by the bus itself; it is instead covered
/// by the initial `signal`-read publish on the NEXT connection, which is
/// exactly the reconnect-recovers-a-dropped-transition property this
/// function exists to provide.
///
/// Returns (letting the supervisor reconnect) on send failure, exactly like
/// [`pump_heartbeats`], or if the `storage.panic` subscription itself
/// fails (extremely unlikely — the in-process bus channel is created
/// lazily and never errs in practice, but fail-open here rather than
/// panic: no more storage updates this connection, next reconnect retries).
async fn pump_storage_events<H: TunnelHandle>(handle: &H, watermark: &StorageWatermarkHandle) {
    // Subscribe *before* reading the current level. `BroadcastBus` only
    // delivers messages published after a subscription is installed, so if
    // we read-then-subscribed, a transition landing in the gap would be
    // missed for the rest of this connection (the whole point of Tier 2's
    // sticky-republish guarantee). Subscribing first means any transition
    // is captured either by the immediate snapshot below (if it already
    // happened) or by the bus stream (if it happens afterward) — the two
    // can race and double-publish, which is fine because duplicate
    // publishes of the same level are harmless on the cloud side.
    let mut events = match watermark
        .bus
        .subscribe::<crate::storage_safety::StoragePanicEvent>(topic::STORAGE_PANIC)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to subscribe to storage.panic; no further watermark updates will be sent on this connection");
            std::future::pending::<()>().await;
            return;
        }
    };

    let env = storage_watermark_envelope(
        watermark.signal.level(),
        watermark.signal.free_pct(),
        watermark.low_watermark_pct,
        watermark.panic_watermark_pct,
    );
    if let Err(e) = handle.send(env).await {
        warn!(error = %e, "storage watermark republish failed; pump exiting");
        return;
    }

    while let Some(msg) = events.next().await {
        match msg {
            Ok(event) => {
                let env = storage_watermark_envelope(
                    event.level,
                    event.free_pct,
                    event.low_pct,
                    event.panic_pct,
                );
                if let Err(e) = handle.send(env).await {
                    warn!(error = %e, "storage watermark send failed; pump exiting");
                    return;
                }
            }
            Err(e) => {
                debug!(error = %e, "storage.panic bus stream error; skipping this message");
            }
        }
    }
    // Stream ended (bus dropped, which happens only on process shutdown) —
    // park so the outer `select!` keeps waiting on the other pumps instead
    // of spinning.
    std::future::pending::<()>().await;
}

fn storage_watermark_envelope(
    level: crate::storage_safety::WatermarkLevel,
    free_pct: f32,
    low_watermark_pct: u8,
    panic_watermark_pct: u8,
) -> Envelope {
    use crate::storage_safety::WatermarkLevel;
    let level_str = match level {
        WatermarkLevel::Ok => "ok",
        WatermarkLevel::Low => "low",
        WatermarkLevel::Panic => "panic",
    };
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: chrono::Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::BusEvent(BusEventPayload {
            topic: "storage.watermark".to_string(),
            core_id: None,
            payload: serde_json::json!(StorageWatermarkPayload {
                level: level_str.to_string(),
                free_pct: free_pct as f64,
                low_watermark_pct: u64::from(low_watermark_pct),
                panic_watermark_pct: u64::from(panic_watermark_pct),
            }),
        }),
    }
}

/// Rolling window the eviction counts below are accumulated over.
const EVICTION_WINDOW: Duration = Duration::from_secs(300);

/// Multiple of the window's clip closures that evictions must exceed for
/// eviction to count as "aggressive".
///
/// The verdict is a RATIO rather than a count, and that is the whole point.
/// Eviction only runs at Low or Panic
/// ([`storage_safety`](crate::storage_safety) gates the reclaim ladder on
/// the watermark level), which is exactly where a healthy retention-limited
/// recorder permanently lives once the disk has filled — so an absolute
/// count of evictions per window measures how many cameras the site has,
/// not whether anything is wrong. A 4-camera site never reaches any
/// count-based threshold and a 32-camera site clears it continuously, and
/// both are fine. What is *not* fine at either size is deleting footage
/// materially faster than it is being written: at steady state a full disk
/// evicts roughly one clip per clip closed, so a ratio above two means
/// retention depth is collapsing rather than holding.
const EVICTION_AGGRESSIVE_RATIO: u64 = 2;

/// Evictions below which a window is too small to draw a ratio from.
///
/// Only the degenerate end needs suppressing: with zero clips closed *any*
/// eviction count beats the ratio, so a near-idle box that tidied up three
/// clips would report. One full reclaim batch
/// ([`MAX_RECLAIM_STEPS_PER_TICK`](crate::storage_safety::MAX_RECLAIM_STEPS_PER_TICK))
/// is the floor because it is a real statement about the mechanism — at
/// least one tick spent its entire reclaim budget — rather than an invented
/// number. It costs sensitivity on a small site evicting less than a batch
/// per window, which is the right way round for a warning nobody can act on
/// twice.
const EVICTION_MIN_SAMPLE: u64 = crate::storage_safety::MAX_RECLAIM_STEPS_PER_TICK as u64;

/// Eviction and clip-closure counts for the window currently being filled.
///
/// Unlike a lazily-advanced window, this one is closed by a timer (see
/// [`pump_eviction_events`]) so that both counts cover the same interval.
/// Judging on eviction arrival instead would compare a full reclaim batch —
/// which lands all at once, on a watermark tick — against however few clips
/// happened to close before it, and report a healthy site every window.
#[derive(Debug)]
struct EvictionWindow {
    evictions: u64,
    hard_evictions: u64,
    clips_closed: u64,
    freed_bytes: u64,
    /// False while an episode is already being reported. One sustained
    /// episode notifies once; a window that does not trip re-arms it.
    armed: bool,
    /// When the window currently being filled closes. Owned by the window
    /// rather than by the pump because the window outlives the connection:
    /// a core reconnecting faster than [`EVICTION_WINDOW`] — a core under
    /// duress, which is the population this report exists for — would never
    /// reach a boundary if the deadline restarted on every connect.
    deadline: tokio::time::Instant,
}

impl EvictionWindow {
    fn new() -> Self {
        Self {
            evictions: 0,
            hard_evictions: 0,
            clips_closed: 0,
            freed_bytes: 0,
            armed: true,
            deadline: tokio::time::Instant::now() + EVICTION_WINDOW,
        }
    }

    /// Fold one eviction into the open window.
    fn observe_eviction(&mut self, event: &serde_json::Value, is_hard: bool) {
        self.evictions += 1;
        if is_hard {
            self.hard_evictions += 1;
        }
        // Only `freed_bytes` is read off the bus payload: the sibling fields
        // (`clip_id`, `camera_id`, `cold_handle`, and above all `cold_path`,
        // a local filesystem path) stay on the box.
        self.freed_bytes += event
            .get("freed_bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
    }

    /// Fold one finished recording into the open window — the denominator.
    fn observe_clip_closed(&mut self) {
        self.clips_closed += 1;
    }

    /// True when this window's counts describe eviction outrunning
    /// recording, independent of how many cameras the site runs.
    fn is_aggressive(&self) -> bool {
        self.evictions >= EVICTION_MIN_SAMPLE
            && self.evictions > EVICTION_AGGRESSIVE_RATIO.saturating_mul(self.clips_closed)
    }

    /// Close the window: return a payload iff it tripped *and* this is the
    /// first window of the episode, then reset for the next one.
    fn close(&mut self) -> Option<StorageEvictionAggressivePayload> {
        let tripped = self.is_aggressive();
        let payload = (tripped && self.armed).then(|| StorageEvictionAggressivePayload {
            window_secs: EVICTION_WINDOW.as_secs(),
            evictions: self.evictions,
            hard_evictions: self.hard_evictions,
            clips_closed: Some(self.clips_closed),
            freed_bytes: self.freed_bytes,
        });
        // Re-arm only once pressure has actually let up, so a core that
        // stays aggressive for an hour reports at the start of the episode
        // and then stays quiet.
        self.armed = !tripped;
        self.evictions = 0;
        self.hard_evictions = 0;
        self.clips_closed = 0;
        self.freed_bytes = 0;
        self.deadline = tokio::time::Instant::now() + EVICTION_WINDOW;
        payload
    }
}

/// ADR-075 Tier 2 — publish `storage.eviction.aggressive` when the reclaim
/// ladder is deleting footage faster than the recorder is writing it.
///
/// Unlike [`pump_storage_events`] this carries a one-shot EDGE, not a sticky
/// STATE, so there is deliberately no republish-on-entry: there is no
/// "current value" to resync, and an envelope lost to the tunnel is simply
/// lost. That is acceptable because the catalogue types it
/// `is_stateful = false`, so the cloud never leaves a condition open on the
/// strength of one that got dropped, and a still-worsening episode
/// re-detects after the next window that does not trip.
///
/// Windows are closed by a timer rather than on eviction arrival so that
/// evictions and clip closures cover the same interval — see
/// [`EvictionWindow`] for why judging per-eviction reports healthy sites.
///
/// Best-effort like every other pump: returns on send failure so the
/// supervisor reconnects, and parks (rather than spinning) if any
/// subscription fails.
///
/// `window` is owned by the supervisor and outlives the connection, so a
/// tunnel that flaps inside one [`EVICTION_WINDOW`] still accumulates
/// towards a verdict instead of resetting its counts and its deadline on
/// every connect.
async fn pump_eviction_events<H: TunnelHandle>(
    handle: &H,
    bus: &Arc<dyn Bus>,
    window: &mut EvictionWindow,
) {
    let (mut hot, mut hard, mut closed) = match (
        bus.subscribe::<serde_json::Value>(topic::CLIP_HOT_EVICTED)
            .await,
        bus.subscribe::<serde_json::Value>(topic::CLIP_HARD_EVICTED)
            .await,
        bus.subscribe::<serde_json::Value>(topic::CLIP_CLOSED).await,
    ) {
        (Ok(hot), Ok(hard), Ok(closed)) => (hot, hard, closed),
        _ => {
            warn!("failed to subscribe to clip eviction topics; no aggressive-eviction reports on this connection");
            std::future::pending::<()>().await;
            return;
        }
    };

    loop {
        // Read the deadline out before the `select!` so the sleep future
        // does not hold a borrow the observe arms need.
        let deadline = window.deadline;
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {
                if let Some(payload) = window.close() {
                    if let Err(e) = handle.send(eviction_aggressive_envelope(&payload)).await {
                        warn!(error = %e, "aggressive eviction send failed; pump exiting");
                        return;
                    }
                }
            }
            Some(msg) = hot.next() => match msg {
                Ok(event) => window.observe_eviction(&event, false),
                Err(e) => debug!(error = %e, "clip eviction bus stream error; skipping this message"),
            },
            Some(msg) = hard.next() => match msg {
                Ok(event) => window.observe_eviction(&event, true),
                Err(e) => debug!(error = %e, "clip eviction bus stream error; skipping this message"),
            },
            Some(msg) = closed.next() => match msg {
                Ok(_) => window.observe_clip_closed(),
                Err(e) => debug!(error = %e, "clip closed bus stream error; skipping this message"),
            },
        }
    }
}

fn eviction_aggressive_envelope(payload: &StorageEvictionAggressivePayload) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: chrono::Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::BusEvent(BusEventPayload {
            topic: "storage.eviction.aggressive".to_string(),
            core_id: None,
            payload: serde_json::json!(payload),
        }),
    }
}

/// How often the delivery-health sample below is taken and sent.
const SINK_HEALTH_WINDOW: Duration = Duration::from_secs(60);

/// Most sinks reported in one sample. `BusEventPayload.payload` is capped
/// at 4 KiB by the gateway and the schema caps `sinks` at 32; a site with
/// more configured sinks than this reports the busiest ones and drops the
/// tail rather than having the whole envelope rejected.
const SINK_HEALTH_MAX_SINKS: usize = 32;

/// Rolling per-sink delivery counters between two samples.
#[derive(Debug, Default)]
struct SinkHealthWindow {
    /// `sink_id` → (first failures, dead-letters) since the last send.
    counts: std::collections::BTreeMap<String, (u64, u64)>,
}

impl SinkHealthWindow {
    fn observe(&mut self, event: &nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent) {
        let entry = self.counts.entry(event.sink_id.clone()).or_default();
        match event.outcome {
            nexus_sinks::dispatcher::SinkDeliveryOutcome::FirstFailure => entry.0 += 1,
            nexus_sinks::dispatcher::SinkDeliveryOutcome::Dead => entry.1 += 1,
        }
    }

    /// Drain the window into a wire payload. Sinks are ordered busiest
    /// first so the `SINK_HEALTH_MAX_SINKS` truncation drops the quietest.
    fn drain(&mut self, queued: u64) -> SinkDeliveryHealthPayload {
        let mut sinks: Vec<SinkDeliveryCounts> = std::mem::take(&mut self.counts)
            .into_iter()
            .map(|(sink_id, (first_failures, dead))| SinkDeliveryCounts {
                sink_id,
                first_failures,
                dead,
            })
            .collect();
        sinks.sort_by_key(|s| std::cmp::Reverse(s.first_failures + s.dead));
        let truncated = sinks.len() > SINK_HEALTH_MAX_SINKS;
        sinks.truncate(SINK_HEALTH_MAX_SINKS);
        SinkDeliveryHealthPayload {
            window_secs: SINK_HEALTH_WINDOW.as_secs(),
            queued,
            queue_threshold: nexus_sinks::dispatcher::BATCH_SIZE.max(1) as u64,
            // The cloud resolves `sink.delivery.dead` on a sample with no
            // dead-lettering sink. Say so when that absence is the cap
            // rather than health, so it can decline to.
            truncated: truncated.then_some(true),
            sinks,
        }
    }

    /// The gauge-only sample sent immediately on connect.
    ///
    /// `queued` is a live point-in-time read of the outbox and is the whole
    /// value of this sample: it resyncs `sink.outbox.backlogged`, which
    /// otherwise stays open on the cloud for a full window after every
    /// reconnect — or forever, on a tunnel that keeps flapping before the
    /// first tick lands.
    ///
    /// The per-sink counters deliberately ride empty and `window_secs` is
    /// ZERO to say so. They come off a bus subscription that only exists
    /// while connected, so at t=0 nothing has been observed — and an empty
    /// `sinks` array read as "no sink dead-lettered" would resolve a
    /// condition this pump has no evidence about, which is the one thing a
    /// resync must not do.
    fn gauge_only(queued: u64) -> SinkDeliveryHealthPayload {
        SinkDeliveryHealthPayload {
            window_secs: 0,
            queued,
            queue_threshold: nexus_sinks::dispatcher::BATCH_SIZE.max(1) as u64,
            truncated: None,
            sinks: Vec::new(),
        }
    }
}

/// True when a sample says nothing an operator would act on: the queue is
/// under one sweep's worth and no sink failed or dead-lettered.
fn sink_health_is_quiet(p: &SinkDeliveryHealthPayload) -> bool {
    p.queued <= p.queue_threshold && p.sinks.iter().all(|s| s.first_failures == 0 && s.dead == 0)
}

/// ADR-075 Tier 2 — publish a periodic sample of the alert-delivery
/// outbox, feeding `sink.outbox.backlogged`, `sink.delivery.dead` and
/// `sink.delivery.failed`.
///
/// Sticky like [`pump_storage_events`], not one-shot like
/// [`pump_eviction_events`]: two of the three types it feeds are
/// `is_stateful`, so an envelope lost to the tunnel must not be able to
/// strand an open condition. Two mechanisms guarantee the resolve is
/// always reachable — a gauge-only sample goes out IMMEDIATELY on entry,
/// before the first window has elapsed, and afterwards a clean sample is
/// still sent whenever the previous one was not, which is the transition
/// back to healthy.
///
/// The immediate sample matters most on a flapping tunnel, which is
/// precisely when the cloud is out of date: a connection that dies inside
/// 60 s used to send nothing at all, so an outbox that had already drained
/// stayed backlogged on the console indefinitely. It carries only the
/// `queued` gauge and says so with `window_secs = 0` — see
/// [`SinkHealthWindow::gauge_only`] for why the counters cannot honestly
/// ride along with it.
///
/// The counters come off the bus rather than out of `alert_sink_outbox`
/// because neither transition survives sampling: `outbox_mark_failed`
/// writes a retrying row back as `pending`, and `outbox_counts_since`
/// buckets on `created_at` rather than on when a row died. `queued` is a
/// point-in-time gauge, so it is read from the store at each sample.
///
/// Best-effort like every other pump: returns on send failure so the
/// supervisor reconnects, and parks (rather than spinning) if the
/// subscription fails.
async fn pump_sink_delivery_health<H: TunnelHandle>(
    handle: &H,
    bus: &Arc<dyn Bus>,
    store: &Arc<Store>,
) {
    let mut outcomes = match bus
        .subscribe::<nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent>(
            topic::SINK_DELIVERY_OUTCOME,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to subscribe to sink delivery outcomes; no delivery-health reports on this connection");
            std::future::pending::<()>().await;
            return;
        }
    };

    // Resync the outbox depth before waiting on anything. The counters have
    // to wait a window, but this gauge does not, and it is the one the cloud
    // can be stale about for as long as the tunnel keeps flapping.
    match store.outbox_queued_count().await {
        Ok(n) => {
            let payload = SinkHealthWindow::gauge_only(u64::try_from(n).unwrap_or(0));
            if let Err(e) = handle.send(sink_delivery_health_envelope(&payload)).await {
                warn!(error = %e, "sink delivery health resync failed; pump exiting");
                return;
            }
        }
        Err(e) => {
            warn!(error = %e, "sink delivery health: outbox depth query failed; skipping the connect resync");
        }
    }

    let mut window = SinkHealthWindow::default();
    let mut ticker = tokio::time::interval(SINK_HEALTH_WINDOW);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick (tokio fires `interval` at t=0). The
    // resync above has already gone out; a second sample here would report a
    // counter window that has observed nothing and resolve conditions that
    // are still true.
    ticker.tick().await;
    // Starts `false` so the first real sample always goes out, however
    // healthy: that is this connection's resync of the sticky state.
    let mut last_was_quiet = false;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let queued = match store.outbox_queued_count().await {
                    Ok(n) => u64::try_from(n).unwrap_or(0),
                    Err(e) => {
                        warn!(error = %e, "sink delivery health: outbox depth query failed; skipping this sample");
                        continue;
                    }
                };
                let payload = window.drain(queued);
                let quiet = sink_health_is_quiet(&payload);
                if quiet && last_was_quiet {
                    continue;
                }
                last_was_quiet = quiet;
                if let Err(e) = handle.send(sink_delivery_health_envelope(&payload)).await {
                    warn!(error = %e, "sink delivery health send failed; pump exiting");
                    return;
                }
            }
            msg = outcomes.next() => {
                match msg {
                    Some(Ok(event)) => window.observe(&event),
                    Some(Err(e)) => {
                        debug!(error = %e, "sink delivery outcome bus stream error; skipping this message");
                    }
                    // Stream ended (bus dropped, i.e. process shutdown) — park
                    // so the outer `select!` keeps waiting on the other pumps.
                    None => {
                        std::future::pending::<()>().await;
                        return;
                    }
                }
            }
        }
    }
}

fn sink_delivery_health_envelope(payload: &SinkDeliveryHealthPayload) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: chrono::Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::BusEvent(BusEventPayload {
            topic: "sink.delivery.health".to_string(),
            core_id: None,
            payload: serde_json::json!(payload),
        }),
    }
}

/// How often the decode-health census below is taken and sent.
const DECODE_HEALTH_WINDOW: Duration = Duration::from_secs(60);

/// Most cameras reported in one census. Matches the schema's `maxItems` on
/// `CameraDecodeHealthPayload.cameras`; a core with more cameras than this
/// reports the worst offenders and drops the tail rather than having the
/// whole envelope rejected for size.
const DECODE_HEALTH_MAX_CAMERAS: usize = 64;

/// Access-unit drops inside one window above which a camera's stream counts
/// as degraded. Zero (i.e. any loss at all) rather than an invented number,
/// because [`crate::decode_verdict::compute_decode_verdict`] already treats
/// a non-zero `decoder_input_drops` as `over`: one lost access unit corrupts
/// every picture until the next IDR.
const DECODE_HEALTH_DROP_THRESHOLD: u64 = 0;

/// Last census's cumulative `decoder_input_drops` per camera, so the wire
/// can carry a per-window delta. The registry's counters only ever climb,
/// and a condition that can only climb can never be seen to end.
#[derive(Debug, Default)]
struct DecodeHealthWindow {
    previous: std::collections::BTreeMap<nexus_types::CameraId, u64>,
}

impl DecodeHealthWindow {
    /// Take one census from the registry, folding each camera's cumulative
    /// counter into this window's delta.
    ///
    /// A camera whose counter went *backwards* has been cleared and
    /// respawned (`DecodeHealthRegistry::clear`, called by the reconciler on
    /// every camera restart, REMOVES the entry so the next spawn counts up
    /// from zero), so its running total restarts: the new absolute value IS
    /// the window's delta. Subtracting the stale baseline would saturate to
    /// zero and report a camera that has just been restarted *because it was
    /// failing* as clean — resolving `camera.stream.degraded` on the strength
    /// of a window whose drops were all discarded.
    fn census(
        &mut self,
        health: &nexus_pipeline::DecodeHealthRegistry,
    ) -> CameraDecodeHealthPayload {
        let observed = health.snapshot_all();
        let mut cameras: Vec<CameraDecodeCounts> = observed
            .iter()
            // The schema's `edge_camera_id` is unsigned; a camera id that
            // will not convert cannot be named on the wire, so it is left
            // out of the census and the cloud resolves it rather than
            // reporting it under someone else's id.
            .filter_map(|(&camera_id, snap)| {
                let edge_camera_id = u64::try_from(camera_id).ok()?;
                let previous = self.previous.get(&camera_id).copied().unwrap_or(0);
                Some(CameraDecodeCounts {
                    edge_camera_id,
                    input_drops: if snap.decoder_input_drops < previous {
                        snap.decoder_input_drops
                    } else {
                        snap.decoder_input_drops - previous
                    },
                })
            })
            .collect();
        self.previous = observed
            .iter()
            .map(|(&camera_id, snap)| (camera_id, snap.decoder_input_drops))
            .collect();
        // Worst-first, so the truncation below drops the least-degraded
        // cameras. Not truncating is not an option — the census would
        // exceed the schema's `maxItems` and be rejected whole, closing
        // nothing — but the cloud resolves any camera a census omits, so a
        // truncated census that stayed silent about it would assert health
        // about cameras this core never reported on. `truncated` says the
        // omission is the cap, and the cloud skips its resolve sweep.
        cameras.sort_by_key(|c| std::cmp::Reverse(c.input_drops));
        let truncated = cameras.len() > DECODE_HEALTH_MAX_CAMERAS;
        cameras.truncate(DECODE_HEALTH_MAX_CAMERAS);
        CameraDecodeHealthPayload {
            window_secs: DECODE_HEALTH_WINDOW.as_secs(),
            drop_threshold: DECODE_HEALTH_DROP_THRESHOLD,
            truncated: truncated.then_some(true),
            cameras,
        }
    }
}

/// True when a census says nothing an operator would act on: no camera lost
/// an access unit inside the window.
fn decode_health_is_quiet(p: &CameraDecodeHealthPayload) -> bool {
    p.cameras.iter().all(|c| c.input_drops <= p.drop_threshold)
}

/// ADR-075 Tier 2 — publish a periodic census of every camera's decode
/// health, feeding `camera.stream.degraded`.
///
/// Sticky like [`pump_sink_delivery_health`], not one-shot like
/// [`pump_eviction_events`]: the type it feeds is `is_stateful`, so an
/// envelope lost to the tunnel must not be able to strand an open
/// condition. The same two mechanisms guarantee the resolve is reachable —
/// a census goes out IMMEDIATELY on entry, before the first window has
/// elapsed, and afterwards a clean census is still sent whenever the
/// previous one was not, which is the transition back to healthy.
///
/// The on-entry census is a real measurement, not an empty one, because
/// `window` is owned by the supervisor and outlives the connection: its
/// per-camera baseline is whatever the last census read, so the deltas it
/// reports on connect cover the disconnect gap. Rebuilding the baseline per
/// connection would make that census all-zero and resolve every degraded
/// camera on the strength of a window nobody measured.
///
/// The census is COMPLETE, not a list of offenders: every camera the
/// registry knows rides in it, including those at zero. That is what lets
/// the cloud resolve a camera which drops out of the registry entirely
/// (pipeline torn down, camera removed) instead of leaving it degraded
/// forever — except when `truncated` says the census hit its cap, where
/// absence means nothing and the cloud declines to resolve.
///
/// Reads the registry directly rather than subscribing to a bus topic,
/// unlike the two pumps above: `decoder_input_drops` is already a shared
/// gauge written by the ingester threads, so a bus topic would only be a
/// second copy of it.
///
/// Best-effort like every other pump: returns on send failure so the
/// supervisor reconnects.
async fn pump_camera_decode_health<H: TunnelHandle>(
    handle: &H,
    health: &Arc<nexus_pipeline::DecodeHealthRegistry>,
    window: &mut DecodeHealthWindow,
) {
    let mut ticker = tokio::time::interval(DECODE_HEALTH_WINDOW);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The immediate first tick (tokio fires `interval` at t=0) is kept, not
    // consumed: it IS this connection's resync.
    // Starts `false` so the first real census always goes out, however
    // healthy: that is this connection's resync of the sticky state.
    let mut last_was_quiet = false;
    loop {
        ticker.tick().await;
        let payload = window.census(health);
        let quiet = decode_health_is_quiet(&payload);
        if quiet && last_was_quiet {
            continue;
        }
        last_was_quiet = quiet;
        if let Err(e) = handle.send(camera_decode_health_envelope(&payload)).await {
            warn!(error = %e, "camera decode health send failed; pump exiting");
            return;
        }
    }
}

fn camera_decode_health_envelope(payload: &CameraDecodeHealthPayload) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: chrono::Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::BusEvent(BusEventPayload {
            topic: "camera.decode.health".to_string(),
            core_id: None,
            payload: serde_json::json!(payload),
        }),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wire cap on `EdgeDegradation.detail` (`proto/v1.json`). Resolver
/// diagnostics name every shape present in the model pack and can run long,
/// so truncate here rather than letting the gateway reject the envelope.
const HEALTH_DETAIL_MAX: usize = 512;

/// Wire cap on `EdgeHealth.issues` (`proto/v1.json`). The registry holds
/// distinct conditions, not per-occurrence events, so overflow is not
/// expected; the clamp exists so a future subsystem that registers many
/// conditions cannot make the heartbeat unserialisable.
const HEALTH_ISSUES_MAX: usize = 16;

/// Build the heartbeat's health roll-up from the process-wide degradation
/// registry, plus any live-view sources that have stopped producing frames.
///
/// `status` is `degraded` iff at least one issue is open, matching the
/// schema's stated invariant. The cloud renders unknown `code`s verbatim, so
/// new subsystems append here without a wire change.
///
/// Live-view coverage is limited to *subscribed* cameras by construction —
/// only a camera someone is watching has a pump — but that is exactly the
/// population whose staleness an operator can see, and the case BUG-057 was
/// filed against.
fn edge_health(
    stalled_cameras: &[nexus_types::CameraId],
    decode_capacity: Option<&crate::system_metrics::DecodeCapacity>,
) -> EdgeHealth {
    let mut issues: Vec<EdgeDegradation> = nexus_inference::health::degradations()
        .into_iter()
        .map(|d| EdgeDegradation {
            component: "detector".to_string(),
            code: "detector_unavailable".to_string(),
            detail: truncate_detail(&format!("{}: {}", d.kind, d.reason)),
        })
        .collect();
    if !stalled_cameras.is_empty() {
        let ids = stalled_cameras
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        issues.push(EdgeDegradation {
            component: "live_view".to_string(),
            code: "camera_source_stalled".to_string(),
            detail: truncate_detail(&format!(
                "{} subscribed camera(s) have produced no new frame for >{}s: {ids}",
                stalled_cameras.len(),
                crate::live_view::STALL_AFTER.as_secs(),
            )),
        });
    }
    // SPEC-069 Phase 1 — a fixed-function video engine oversubscribed is a
    // fleet-visible degradation in its own right: it explains a core that
    // looks otherwise healthy (still recording, still streaming) but is
    // quietly dropping/duplicating frames across every camera sharing that
    // engine, which is exactly the state a 53-camera Radeon 680M / Intel
    // iHD box is in before this instrumentation existed.
    if let Some(cap) = decode_capacity {
        if cap.oversubscribed {
            issues.push(EdgeDegradation {
                component: "decode".to_string(),
                code: "decode_oversubscribed".to_string(),
                detail: truncate_detail(&format!(
                    "{} at {:.1}% \u{2014} this fixed-function video engine is oversubscribed \
                     and shared by every camera decoding on it",
                    cap.binding_engine, cap.binding_engine_pct,
                )),
            });
        }
    }
    issues.truncate(HEALTH_ISSUES_MAX);
    EdgeHealth {
        status: if issues.is_empty() { "ok" } else { "degraded" }.to_string(),
        issues: Some(issues),
    }
}

/// Truncate to at most [`HEALTH_DETAIL_MAX`] **bytes** without splitting a
/// UTF-8 code point. `String::truncate` panics on a non-boundary index, and
/// resolver diagnostics can contain non-ASCII (operator-set model paths), so
/// walk back to the nearest boundary first.
fn truncate_detail(s: &str) -> String {
    if s.len() <= HEALTH_DETAIL_MAX {
        return s.to_string();
    }
    let mut end = HEALTH_DETAIL_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod health_tests {
    use super::*;

    /// The engine must not advertise a capability it cannot perform. Talk-down
    /// audio has no receive-side pipeline here, so no transport may leak the
    /// `talkdown_webrtc` tag onto the heartbeat — a cloud that routed an
    /// operator on it would hand them a mic wired to nothing.
    #[test]
    fn no_transport_advertises_talk_down() {
        for t in nexus_types::HdTransport::all() {
            let caps = heartbeat_caps(t);
            assert!(
                caps.contains(&"live_view".to_string()),
                "{t} must still advertise the LBR pump: {caps:?}"
            );
            assert!(
                !caps.iter().any(|c| c.contains("talkdown")),
                "{t} advertised talk-down with no sub-pipeline behind it: {caps:?}"
            );
        }
    }

    #[test]
    fn short_detail_is_passed_through_unchanged() {
        assert_eq!(truncate_detail("boom"), "boom");
    }

    #[test]
    fn long_detail_is_clamped_to_the_wire_cap() {
        let long = "x".repeat(HEALTH_DETAIL_MAX * 2);
        assert_eq!(truncate_detail(&long).len(), HEALTH_DETAIL_MAX);
    }

    /// A multi-byte code point straddling the cap must not panic and must
    /// not produce invalid UTF-8. Resolver diagnostics echo operator-set
    /// model paths, which are not guaranteed ASCII.
    #[test]
    fn truncation_never_splits_a_code_point() {
        // 'é' is 2 bytes, so the 512-byte boundary lands mid-character.
        let s = format!("{}é", "x".repeat(HEALTH_DETAIL_MAX - 1));
        let out = truncate_detail(&s);
        assert_eq!(out.len(), HEALTH_DETAIL_MAX - 1);
        assert!(out.chars().all(|c| c == 'x'));
    }

    /// The registry is process-global, so assert on presence of our own
    /// kind rather than on an exact issue count — a sibling test in the
    /// same binary may have registered its own degradation.
    #[test]
    fn degradations_map_to_wire_issues_and_flip_the_status() {
        let kind = "cloud_tunnel_health_test";
        nexus_inference::health::record_degraded(kind, "model pack has no 640x640 export");

        let health = edge_health(&[], None);
        assert_eq!(health.status, "degraded");
        let issue = health
            .issues
            .as_ref()
            .expect("issues present when degraded")
            .iter()
            .find(|i| i.detail.starts_with(kind))
            .expect("our degradation is on the wire");
        assert_eq!(issue.component, "detector");
        assert_eq!(issue.code, "detector_unavailable");
        assert!(issue.detail.contains("no 640x640 export"));

        nexus_inference::health::clear_degraded(kind);
    }

    /// BUG-057 — a stalled live-view source must reach the cloud. It rides
    /// the existing `EdgeHealth.issues` shape, so no wire bump is needed.
    #[test]
    fn stalled_live_view_sources_become_a_wire_issue() {
        let health = edge_health(&[4, 11], None);
        assert_eq!(health.status, "degraded");
        let issue = health
            .issues
            .as_ref()
            .expect("issues present when degraded")
            .iter()
            .find(|i| i.component == "live_view")
            .expect("the stall is on the wire");
        assert_eq!(issue.code, "camera_source_stalled");
        assert!(issue.detail.contains("4,11"), "detail: {}", issue.detail);
    }

    /// SPEC-069 Phase 1 — a fixed-function engine oversubscribed must
    /// surface as its own wire issue, naming the binding engine.
    #[test]
    fn oversubscribed_decode_capacity_becomes_a_wire_issue() {
        let cap = crate::system_metrics::DecodeCapacity {
            binding_engine: "video-enhance".to_string(),
            binding_engine_pct: 99.1,
            oversubscribed: true,
        };
        let health = edge_health(&[], Some(&cap));
        assert_eq!(health.status, "degraded");
        let issue = health
            .issues
            .as_ref()
            .expect("issues present when degraded")
            .iter()
            .find(|i| i.component == "decode")
            .expect("oversubscription is on the wire");
        assert_eq!(issue.code, "decode_oversubscribed");
        assert!(issue.detail.contains("video-enhance"), "{}", issue.detail);
        assert!(issue.detail.contains("99.1"), "{}", issue.detail);
    }

    /// A binding engine below the threshold must not raise an issue —
    /// otherwise every core with any GPU decode load at all would read
    /// permanently degraded.
    ///
    /// Asserts the absence of a `decode` issue rather than
    /// `status == "ok"`: status folds in the process-global detector
    /// registry, which a sibling test in this binary writes and never
    /// clears, so the stronger assertion only passed by winning a race
    /// (BUG-155).
    #[test]
    fn healthy_decode_capacity_does_not_raise_an_issue() {
        let cap = crate::system_metrics::DecodeCapacity {
            binding_engine: "video-decode".to_string(),
            binding_engine_pct: 12.0,
            oversubscribed: false,
        };
        let health = edge_health(&[], Some(&cap));
        assert!(
            !health
                .issues
                .unwrap_or_default()
                .iter()
                .any(|i| i.component == "decode"),
            "an under-threshold binding engine must not be reported",
        );
    }
}

#[cfg(test)]
mod storage_watermark_tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use nexus_bus::{topic, BroadcastBus, BusExt};
    use nexus_cloud_client::tunnel::{TunnelError, TunnelHandle};
    use nexus_cloud_protocol::v1::EnvelopeBody;

    use super::*;
    use crate::storage_safety::{StoragePanicEvent, WatermarkLevel};

    /// Captures every envelope handed to `send` into a shared list. Mirrors
    /// the `CapturingTunnel` convention used by `cloud_sighting.rs` /
    /// `outbox.rs` tests. `fail_after` optionally makes the Nth-and-later
    /// send fail with `TunnelError::Disconnected`, simulating the tunnel
    /// going down mid-pump so the reconnect-republish path can be exercised.
    struct CapturingTunnel {
        sent: parking_lot::Mutex<Vec<Envelope>>,
        fail_after: Option<usize>,
    }

    impl CapturingTunnel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: parking_lot::Mutex::new(Vec::new()),
                fail_after: None,
            })
        }

        fn failing_after(n: usize) -> Arc<Self> {
            Arc::new(Self {
                sent: parking_lot::Mutex::new(Vec::new()),
                fail_after: Some(n),
            })
        }

        fn watermark_payloads(&self) -> Vec<StorageWatermarkPayload> {
            self.sent
                .lock()
                .iter()
                .filter_map(|env| match &env.body {
                    EnvelopeBody::BusEvent(p) => {
                        serde_json::from_value::<StorageWatermarkPayload>(p.payload.clone()).ok()
                    }
                    _ => None,
                })
                .collect()
        }

        /// Every `bus_event` envelope under `topic`, with its inner payload
        /// still typed — so a test can assert on the WIRE topic name, not
        /// just on a shape that happens to deserialize.
        fn bus_events_on(&self, topic: &str) -> Vec<serde_json::Value> {
            self.sent
                .lock()
                .iter()
                .filter_map(|env| match &env.body {
                    EnvelopeBody::BusEvent(p) if p.topic == topic => Some(p.payload.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl TunnelHandle for CapturingTunnel {
        async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
            let mut sent = self.sent.lock();
            if let Some(n) = self.fail_after {
                if sent.len() >= n {
                    return Err(TunnelError::Disconnected);
                }
            }
            sent.push(envelope);
            Ok(())
        }
    }

    fn handle_with(signal: WatermarkSignal, bus: Arc<dyn Bus>) -> StorageWatermarkHandle {
        StorageWatermarkHandle {
            signal,
            bus,
            low_watermark_pct: 15,
            panic_watermark_pct: 5,
        }
    }

    /// Core stickiness property: connecting (or reconnecting) sends the
    /// CURRENT level immediately, before any bus transition arrives. This
    /// is what makes a dropped transition self-heal on the very next
    /// publish per ADR-075's Tier 2 durability argument.
    #[tokio::test]
    async fn publishes_current_state_immediately_on_entry() {
        let signal = WatermarkSignal::new();
        signal.set(WatermarkLevel::Low);
        signal.set_free_pct(12.5);
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
        let watermark = handle_with(signal, bus);
        let tunnel = CapturingTunnel::failing_after(1);

        // pump_storage_events never returns on its own once the initial
        // send succeeds (it then awaits the bus stream), so race it
        // against a short timeout: by the time the timeout fires the
        // initial republish must already be captured.
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            pump_storage_events(&*tunnel, &watermark),
        )
        .await;

        let payloads = tunnel.watermark_payloads();
        assert_eq!(payloads.len(), 1, "exactly the initial republish, no more");
        assert_eq!(payloads[0].level, "low");
        assert!((payloads[0].free_pct - 12.5).abs() < 1e-3);
        assert_eq!(payloads[0].low_watermark_pct, 15);
        assert_eq!(payloads[0].panic_watermark_pct, 5);
    }

    /// The regression this whole feature exists to prevent: a transition
    /// that occurs while the tunnel is down (no pump running to observe the
    /// bus) is NOT lost — it is recovered because the NEXT connection's
    /// pump reads the CURRENT signal state on entry, not just future bus
    /// messages (which `BroadcastBus` never replays anyway).
    #[tokio::test]
    async fn reconnect_recovers_a_transition_missed_while_disconnected() {
        let signal = WatermarkSignal::new();
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));

        // "Connection 1": starts Ok. Race the pump against a short timeout
        // so we observe the initial republish without hanging forever on
        // the (never-arriving, in this test) bus subscription.
        let watermark1 = handle_with(signal.clone(), bus.clone());
        let tunnel1 = CapturingTunnel::new();
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            pump_storage_events(&*tunnel1, &watermark1),
        )
        .await;
        assert_eq!(tunnel1.watermark_payloads()[0].level, "ok");

        // Tunnel drops. While it is down, the FSM crosses to Panic — but
        // there is no pump running to see it on `topic::STORAGE_PANIC`,
        // exactly like a real disconnect. Only the shared signal is
        // updated (as `run_storage_safety` does on every tick regardless
        // of tunnel state).
        signal.set(WatermarkLevel::Panic);
        signal.set_free_pct(2.0);

        // "Reconnect": a fresh pump on a fresh connection must publish the
        // CURRENT level (Panic) immediately, recovering the missed
        // transition from the signal alone.
        let watermark2 = handle_with(signal.clone(), bus.clone());
        let tunnel2 = CapturingTunnel::new();
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            pump_storage_events(&*tunnel2, &watermark2),
        )
        .await;
        let payloads2 = tunnel2.watermark_payloads();
        assert_eq!(payloads2.len(), 1);
        assert_eq!(payloads2[0].level, "panic");
        assert!((payloads2[0].free_pct - 2.0).abs() < 1e-3);
    }

    /// Publishing the same level twice in a row (e.g. two consecutive
    /// reconnects with no real change in between) must be harmless — the
    /// edge never suppresses the republish, and the resulting envelopes
    /// are structurally identical modulo `meta.id`/`meta.ts`, which is what
    /// makes the cloud side's duplicate-tolerant handling ("nothing keyed
    /// off occurrence count") safe.
    #[tokio::test]
    async fn duplicate_republish_of_the_same_level_is_harmless() {
        let signal = WatermarkSignal::new();
        signal.set(WatermarkLevel::Low);
        signal.set_free_pct(11.0);
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));

        for _ in 0..2 {
            let watermark = handle_with(signal.clone(), bus.clone());
            let tunnel = CapturingTunnel::new();
            let _ = tokio::time::timeout(
                Duration::from_millis(50),
                pump_storage_events(&*tunnel, &watermark),
            )
            .await;
            let payloads = tunnel.watermark_payloads();
            assert_eq!(payloads.len(), 1);
            assert_eq!(payloads[0].level, "low");
            assert!((payloads[0].free_pct - 11.0).abs() < 1e-3);
        }
    }

    /// A live transition published on `topic::STORAGE_PANIC` while the
    /// pump is connected must be forwarded (not just the initial
    /// republish), proving the second half of `pump_storage_events` — the
    /// bus-forwarding loop — actually works end-to-end.
    #[tokio::test]
    async fn live_transition_on_the_bus_is_forwarded_while_connected() {
        let signal = WatermarkSignal::new();
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
        let watermark = handle_with(signal, bus.clone());
        let tunnel = CapturingTunnel::new();

        let pump = tokio::spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                pump_storage_events(&*tunnel, &watermark),
            )
            .await;
            tunnel
        });
        // Give the pump time to send its initial republish and subscribe.
        tokio::time::sleep(Duration::from_millis(20)).await;
        bus.publish(
            topic::STORAGE_PANIC,
            &StoragePanicEvent {
                level: WatermarkLevel::Panic,
                free_pct: 1.0,
                low_pct: 15,
                panic_pct: 5,
                clips_dir: std::path::PathBuf::from("/clips"),
            },
        )
        .await
        .expect("publish storage.panic");

        let tunnel = pump.await.expect("pump task join");
        let payloads = tunnel.watermark_payloads();
        assert_eq!(
            payloads.len(),
            2,
            "initial republish + forwarded transition"
        );
        assert_eq!(payloads[0].level, "ok");
        assert_eq!(payloads[1].level, "panic");
    }

    /// Publish `n` clip closures, the denominator of the eviction ratio.
    async fn publish_clip_closures(bus: &Arc<dyn Bus>, n: u64) {
        for _ in 0..n {
            bus.publish(topic::CLIP_CLOSED, &serde_json::json!({ "clip_id": 1 }))
                .await
                .expect("publish");
        }
    }

    /// Publish `n` evictions on `topic` and wait for the pump to drain them.
    async fn publish_evictions(bus: &Arc<dyn Bus>, topic: &str, n: u64, freed_bytes: u64) {
        for _ in 0..n {
            bus.publish(
                topic,
                &serde_json::json!({
                    "clip_id": 1,
                    "camera_id": 1,
                    "cold_path": "usb/NEXUS_ACME_HQ/cam1/clip.mp4",
                    "freed_bytes": freed_bytes,
                }),
            )
            .await
            .expect("publish");
        }
    }

    /// A full disk at steady state evicts roughly one clip per clip closed,
    /// and that is a healthy retention-limited recorder — at four cameras or
    /// at thirty-two. The volume here is deliberately far above any absolute
    /// floor: if the verdict were a count rather than a ratio, this is the
    /// site that would notify forever.
    #[tokio::test(start_paused = true)]
    async fn steady_state_eviction_is_not_aggressive_at_any_site_size() {
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(256));
        let tunnel = CapturingTunnel::new();
        let mut window = EvictionWindow::new();
        let pump = pump_eviction_events(&*tunnel, &bus, &mut window);
        let drive = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let busy = EVICTION_MIN_SAMPLE * 8;
            publish_clip_closures(&bus, busy).await;
            publish_evictions(&bus, topic::CLIP_HOT_EVICTED, busy, 1).await;
            // The verdict lands on the window ticker, not on the eviction.
            tokio::time::sleep(EVICTION_WINDOW + Duration::from_secs(1)).await;
        };
        tokio::select! {
            () = pump => {}
            () = drive => {}
        }

        assert!(
            tunnel
                .bus_events_on("storage.eviction.aggressive")
                .is_empty(),
            "evicting one clip per clip written is retention working, not failing",
        );
    }

    /// Deleting materially faster than recording IS the condition: evictions
    /// past twice the clips closed in the same window means retention depth is
    /// collapsing rather than holding. It must reach the cloud as a
    /// `bus_event` on the wire topic the gateway allow-lists.
    #[tokio::test(start_paused = true)]
    async fn eviction_outrunning_recording_reports_on_the_wire_topic() {
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(256));
        let tunnel = CapturingTunnel::new();
        let mut window = EvictionWindow::new();
        let pump = pump_eviction_events(&*tunnel, &bus, &mut window);
        let drive = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            // Two clips written, five deleted: past the 2x ratio and above the
            // small-sample floor.
            publish_clip_closures(&bus, 2).await;
            let hot = (EVICTION_MIN_SAMPLE.max(5)) - 1;
            publish_evictions(&bus, topic::CLIP_HOT_EVICTED, hot, 1024).await;
            // The one that tips it over, and the only hard eviction.
            publish_evictions(&bus, topic::CLIP_HARD_EVICTED, 1, 2048).await;
            // The verdict lands on the window ticker, not on the eviction.
            tokio::time::sleep(EVICTION_WINDOW + Duration::from_secs(1)).await;
        };
        tokio::select! {
            () = pump => {}
            () = drive => {}
        }

        let sent = tunnel.bus_events_on("storage.eviction.aggressive");
        assert_eq!(
            sent.len(),
            1,
            "the window resets on report, so sustained pressure sends once per window",
        );
        let ev: StorageEvictionAggressivePayload =
            serde_json::from_value(sent[0].clone()).expect("payload matches the wire schema");
        let hot = (EVICTION_MIN_SAMPLE.max(5)) - 1;
        assert_eq!(ev.evictions, hot + 1);
        assert_eq!(ev.hard_evictions, 1);
        assert_eq!(ev.freed_bytes, hot * 1024 + 2048);
        assert_eq!(ev.window_secs, EVICTION_WINDOW.as_secs());

        // AGENTS.md payload hygiene: the bus payload carries `cold_path`, a
        // local filesystem path that can embed a customer's name. It must
        // not survive the bridge.
        let serialized = serde_json::to_string(&sent[0]).expect("serialize");
        assert!(
            !serialized.contains("cold_path") && !serialized.contains("NEXUS_ACME_HQ"),
            "no filesystem paths on the wire, got {serialized}",
        );
    }

    /// A core under duress is a core whose tunnel flaps, and it is exactly
    /// the core this report exists for. The window (counts, `armed` and the
    /// boundary deadline) therefore has to outlive the connection: rebuilt
    /// per connect, a core reconnecting inside one `EVICTION_WINDOW` would
    /// restart the clock every time and never reach a boundary.
    #[tokio::test(start_paused = true)]
    async fn a_tunnel_flapping_inside_one_window_still_reaches_a_boundary() {
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(256));
        let tunnel = CapturingTunnel::new();
        let mut window = EvictionWindow::new();
        let per_connection = EVICTION_MIN_SAMPLE.max(5);

        // Two connections, each barely over half a window: neither is long
        // enough to close a window of its own.
        for _ in 0..2 {
            let pump = pump_eviction_events(&*tunnel, &bus, &mut window);
            let drive = async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                publish_clip_closures(&bus, 1).await;
                publish_evictions(&bus, topic::CLIP_HOT_EVICTED, per_connection, 512).await;
                tokio::time::sleep(EVICTION_WINDOW / 2 + Duration::from_secs(1)).await;
            };
            tokio::select! {
                () = pump => {}
                () = drive => {}
            }
        }

        let sent = tunnel.bus_events_on("storage.eviction.aggressive");
        assert_eq!(
            sent.len(),
            1,
            "a window that survives the reconnect reports; one rebuilt per \
             connection never would",
        );
        let ev: StorageEvictionAggressivePayload =
            serde_json::from_value(sent[0].clone()).expect("payload matches the wire schema");
        assert_eq!(
            ev.evictions,
            per_connection * 2,
            "the counts span both connections",
        );
    }

    /// One sample's worth of outcomes is reported once each, under the
    /// sink's id — the counters the cloud turns into `sink.delivery.failed`
    /// and `sink.delivery.dead`.
    #[test]
    fn a_window_reports_each_outcome_once_per_sink() {
        let mut window = SinkHealthWindow::default();
        for outcome in [
            nexus_sinks::dispatcher::SinkDeliveryOutcome::FirstFailure,
            nexus_sinks::dispatcher::SinkDeliveryOutcome::FirstFailure,
            nexus_sinks::dispatcher::SinkDeliveryOutcome::Dead,
        ] {
            window.observe(&nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent {
                sink_id: "webhook:primary".to_string(),
                outcome,
            });
        }
        window.observe(&nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent {
            sink_id: "sureview:b".to_string(),
            outcome: nexus_sinks::dispatcher::SinkDeliveryOutcome::Dead,
        });

        let payload = window.drain(0);
        assert_eq!(payload.sinks.len(), 2, "one entry per sink");
        let primary = payload
            .sinks
            .iter()
            .find(|s| s.sink_id == "webhook:primary")
            .expect("the busy sink is reported");
        assert_eq!(primary.first_failures, 2);
        assert_eq!(primary.dead, 1);

        // Draining is what bounds the report: the next window starts empty,
        // so one outage is not re-reported every minute for as long as it
        // lasts.
        assert!(
            window.drain(0).sinks.is_empty(),
            "the window resets on drain"
        );
    }

    /// The edge, not the cloud, decides what "backlogged" means, because
    /// the threshold is the dispatcher's own sweep size.
    #[test]
    fn the_queue_threshold_is_the_dispatcher_batch_size() {
        let payload = SinkHealthWindow::default().drain(0);
        assert_eq!(
            payload.queue_threshold,
            nexus_sinks::dispatcher::BATCH_SIZE as u64,
        );
        assert_eq!(payload.window_secs, SINK_HEALTH_WINDOW.as_secs());
    }

    /// `sink_health_is_quiet` is what stops a healthy core sending a sample
    /// a minute forever — and, paired with the pump's `last_was_quiet`,
    /// what guarantees the ONE sample that resolves a stateful condition
    /// still goes out. A depth exactly at the threshold is under one
    /// sweep's work and must not count as backlogged.
    #[test]
    fn quiet_means_nothing_failed_and_the_queue_is_within_one_sweep() {
        let threshold = SinkHealthWindow::default().drain(0).queue_threshold;
        assert!(sink_health_is_quiet(
            &SinkHealthWindow::default().drain(threshold)
        ));
        assert!(!sink_health_is_quiet(
            &SinkHealthWindow::default().drain(threshold + 1)
        ));

        let mut window = SinkHealthWindow::default();
        window.observe(&nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent {
            sink_id: "webhook:primary".to_string(),
            outcome: nexus_sinks::dispatcher::SinkDeliveryOutcome::Dead,
        });
        assert!(
            !sink_health_is_quiet(&window.drain(0)),
            "a dead-lettering sink is never quiet, however short the queue",
        );
    }

    /// The bridge is only useful if the cloud recognises the topic, and the
    /// payload must carry ids and counts — never the sink's endpoint.
    #[test]
    fn the_sample_rides_its_own_wire_topic_and_carries_ids_only() {
        let mut window = SinkHealthWindow::default();
        window.observe(&nexus_sinks::dispatcher::SinkDeliveryOutcomeEvent {
            sink_id: "webhook:primary".to_string(),
            outcome: nexus_sinks::dispatcher::SinkDeliveryOutcome::Dead,
        });
        let env = sink_delivery_health_envelope(&window.drain(9));
        let EnvelopeBody::BusEvent(body) = &env.body else {
            panic!("expected a bus_event envelope, got {:?}", env.body);
        };
        assert_eq!(body.topic, "sink.delivery.health");
        assert_eq!(body.core_id, None, "scope comes from the cert, not here");
        assert_eq!(body.payload["sinks"][0]["sink_id"], "webhook:primary");

        let serialized = serde_json::to_string(&env).expect("serialize");
        for forbidden in ["last_error", "http", "@"] {
            assert!(
                !serialized.contains(forbidden),
                "{forbidden:?} must not reach the wire, got {serialized}",
            );
        }
    }

    /// The connect-time resync is the whole reason a flapping tunnel does
    /// not strand `sink.outbox.backlogged` open forever, so it has to go out
    /// BEFORE the first window elapses — and it has to say, with
    /// `window_secs = 0`, that its empty `sinks` array is silence rather
    /// than a clean window. A consumer that read that array as "no sink
    /// dead-lettered" would resolve a condition on every reconnect.
    #[tokio::test]
    async fn the_connect_resync_is_gauge_only_and_precedes_the_first_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.path().join("n.db").display()),
                ..nexus_config::StoreConfig::default()
            })
            .await
            .expect("open store"),
        );
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));
        let tunnel = CapturingTunnel::new();

        // Real clock (the pump reads SQLite), and far short of one window.
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            pump_sink_delivery_health(&*tunnel, &bus, &store),
        )
        .await;

        let sent = tunnel.bus_events_on("sink.delivery.health");
        assert_eq!(
            sent.len(),
            1,
            "exactly one sample before the first window closes: the resync",
        );
        let sh: SinkDeliveryHealthPayload =
            serde_json::from_value(sent[0].clone()).expect("payload matches the wire schema");
        assert_eq!(
            sh.window_secs, 0,
            "a zero window is how the resync says its counters observed nothing",
        );
        assert!(
            sh.sinks.is_empty(),
            "the resync carries the gauge only, got {:?}",
            sh.sinks,
        );
        assert!(sh.queue_threshold > 0, "the threshold must stay meaningful");
    }

    /// The registry's counters only ever climb, so the wire has to carry a
    /// per-window delta: a cumulative total can never be seen to fall back
    /// under the threshold, which would strand the stateful condition the
    /// cloud opens from it.
    #[test]
    fn a_census_reports_the_windows_drops_not_the_running_total() {
        let registry = nexus_pipeline::DecodeHealthRegistry::new();
        let mut window = DecodeHealthWindow::default();

        for _ in 0..3 {
            registry.observe_decoder_input_drop(7);
        }
        let first = window.census(&registry);
        assert_eq!(drops_for(&first, 7), 3, "first window sees all three");

        // Two more drops on a cumulative total of five.
        for _ in 0..2 {
            registry.observe_decoder_input_drop(7);
        }
        let second = window.census(&registry);
        assert_eq!(drops_for(&second, 7), 2, "second window sees only its own");

        // A window with no new drops is what resolves the condition.
        let third = window.census(&registry);
        assert_eq!(drops_for(&third, 7), 0, "a clean window reports zero");
        assert!(decode_health_is_quiet(&third));
    }

    /// A camera restarted mid-window must report the drops it took SINCE the
    /// restart, not zero.
    ///
    /// `DecodeHealthRegistry::clear` removes the entry (the reconciler calls
    /// it on every camera teardown), so the next spawn counts up from zero
    /// while this window still holds the pre-restart baseline. Subtracting
    /// that baseline saturates to zero, and a zero census is exactly what
    /// resolves `camera.stream.degraded` — so a camera restarted *because it
    /// is failing* would report clean on the very window that proves it is
    /// not.
    #[test]
    fn a_census_after_a_camera_restart_reports_the_new_totals_drops() {
        let registry = nexus_pipeline::DecodeHealthRegistry::new();
        let mut window = DecodeHealthWindow::default();

        for _ in 0..500 {
            registry.observe_decoder_input_drop(7);
        }
        let first = window.census(&registry);
        assert_eq!(drops_for(&first, 7), 500, "baseline window sees all 500");

        // The reconciler tears the camera down and respawns it; the counter
        // restarts at zero while `window.previous` still holds 500.
        registry.clear(7);
        for _ in 0..30 {
            registry.observe_decoder_input_drop(7);
        }

        let second = window.census(&registry);
        assert_eq!(
            drops_for(&second, 7),
            30,
            "the post-restart total IS the window's delta",
        );
        assert!(
            !decode_health_is_quiet(&second),
            "a camera still dropping after a restart is not healthy",
        );

        // And the new baseline is the post-restart total, not the old one:
        // the next clean window must still resolve.
        let third = window.census(&registry);
        assert_eq!(drops_for(&third, 7), 0, "a clean window still reports zero");
        assert!(decode_health_is_quiet(&third));
    }

    /// The census sent on reconnect is a real measurement of the gap, not an
    /// empty one: the baseline is owned by the supervisor and survives the
    /// connection. Rebuilt per connection it would report all-zero and
    /// resolve every degraded camera on the strength of a window nobody
    /// measured.
    #[tokio::test]
    async fn a_reconnect_census_reports_the_drops_taken_while_disconnected() {
        let registry = Arc::new(nexus_pipeline::DecodeHealthRegistry::new());
        let tunnel = CapturingTunnel::new();
        let mut window = DecodeHealthWindow::default();

        registry.observe_decoder_output(5);
        // First connection: establishes the baseline and disconnects.
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            pump_camera_decode_health(&*tunnel, &registry, &mut window),
        )
        .await;

        // Four access units lost while the tunnel was down.
        for _ in 0..4 {
            registry.observe_decoder_input_drop(5);
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            pump_camera_decode_health(&*tunnel, &registry, &mut window),
        )
        .await;

        let sent = tunnel.bus_events_on("camera.decode.health");
        assert_eq!(sent.len(), 2, "one census per connection");
        let first: CameraDecodeHealthPayload =
            serde_json::from_value(sent[0].clone()).expect("payload matches the wire schema");
        let second: CameraDecodeHealthPayload =
            serde_json::from_value(sent[1].clone()).expect("payload matches the wire schema");
        assert_eq!(drops_for(&first, 5), 0, "nothing lost before the baseline");
        assert_eq!(
            drops_for(&second, 5),
            4,
            "the reconnect census covers the disconnect gap",
        );
    }

    /// The census is what makes the cloud's resolve total, so it must name
    /// every camera the registry knows — including the healthy ones, whose
    /// absence the cloud would otherwise be unable to tell apart from a
    /// camera that stopped being decoded.
    #[test]
    fn a_census_names_healthy_cameras_too() {
        let registry = nexus_pipeline::DecodeHealthRegistry::new();
        registry.observe_decoder_output(1);
        registry.observe_decoder_output(2);
        registry.observe_decoder_input_drop(2);

        let census = DecodeHealthWindow::default().census(&registry);
        assert_eq!(census.cameras.len(), 2, "both cameras are in the census");
        assert_eq!(drops_for(&census, 1), 0, "the healthy camera rides at zero");
        assert_eq!(drops_for(&census, 2), 1);
        assert!(
            !decode_health_is_quiet(&census),
            "one losing camera makes the whole census worth sending",
        );
    }

    /// The threshold is the edge's call and is deliberately zero — it is the
    /// same rule `decode_verdict` already applies, not an invented number.
    #[test]
    fn any_lost_access_unit_clears_the_threshold() {
        let registry = nexus_pipeline::DecodeHealthRegistry::new();
        registry.observe_decoder_output(4);
        let clean = DecodeHealthWindow::default().census(&registry);
        assert_eq!(clean.drop_threshold, 0);
        assert_eq!(clean.window_secs, DECODE_HEALTH_WINDOW.as_secs());
        assert!(decode_health_is_quiet(&clean));

        registry.observe_decoder_input_drop(4);
        assert!(
            !decode_health_is_quiet(&DecodeHealthWindow::default().census(&registry)),
            "a single dropped access unit is already damage",
        );
    }

    /// The bridge is only useful if the cloud recognises the topic. The
    /// hygiene half is asserted on an envelope built from a REAL registry
    /// census rather than a hand-written payload: a test that constructs
    /// the thing it inspects cannot catch a publisher that starts copying
    /// a URL or an error string into it.
    #[test]
    fn the_census_rides_its_own_wire_topic_and_carries_counts_only() {
        let registry = nexus_pipeline::DecodeHealthRegistry::new();
        registry.observe_decoder_geometry(3, 1920, 1080);
        registry.observe_decoder_input_drop(3);

        let census = DecodeHealthWindow::default().census(&registry);
        let env = camera_decode_health_envelope(&census);
        let EnvelopeBody::BusEvent(body) = &env.body else {
            panic!("expected a bus_event envelope, got {:?}", env.body);
        };
        assert_eq!(body.topic, "camera.decode.health");
        assert_eq!(body.core_id, None, "scope comes from the cert, not here");
        assert_eq!(body.payload["cameras"][0]["edge_camera_id"], 3);
        assert_eq!(body.payload["cameras"][0]["input_drops"], 1);

        let keys: Vec<&str> = body.payload["cameras"][0]
            .as_object()
            .expect("a camera entry is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["edge_camera_id", "input_drops"],
            "a camera entry carries an id and a count, nothing else",
        );
    }

    fn drops_for(payload: &CameraDecodeHealthPayload, edge_camera_id: u64) -> u64 {
        payload
            .cameras
            .iter()
            .find(|c| c.edge_camera_id == edge_camera_id)
            .unwrap_or_else(|| panic!("camera {edge_camera_id} missing from the census"))
            .input_drops
    }
}

/// BUG-133 — a half-open tunnel accepts heartbeats forever, so "the send
/// succeeded" cannot stand in for "the cloud is still there".
#[cfg(test)]
mod heartbeat_ack_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use nexus_cloud_client::tunnel::{TunnelError, TunnelHandle};
    use nexus_cloud_client::TunnelOutbox;
    use nexus_config::StoreConfig;
    use nexus_pipeline::{FrameStatsRegistry, LatestFrameCache};

    use super::*;
    use crate::cloud_liveness::TunnelLiveness;
    use crate::live_view::LiveViewManager;

    /// Accepts every frame and never fails — what the engine sees when the
    /// peer is gone but the kernel still holds the socket open. The box
    /// that prompted BUG-133 sat here for 29 minutes with 29 KB queued.
    #[derive(Default)]
    struct HalfOpenTunnel {
        sent: AtomicUsize,
    }

    #[async_trait]
    impl TunnelHandle for HalfOpenTunnel {
        async fn send(&self, _envelope: Envelope) -> Result<(), TunnelError> {
            self.sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    async fn test_store() -> (Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("nexus.db");
        let store = Store::open(&StoreConfig {
            url: format!("sqlite://{}?mode=rwc", db_path.display()),
            ..StoreConfig::default()
        })
        .await
        .expect("open store");
        (Arc::new(store), dir)
    }

    fn live_view_manager() -> Arc<LiveViewManager> {
        LiveViewManager::new(
            Arc::new(LatestFrameCache::new()),
            Arc::new(TunnelOutbox::new()),
        )
    }

    /// The go-dark watchdog reflips an appliance to its previous release
    /// on this signal. Enqueueing a heartbeat onto a local channel proves
    /// the channel had room, not that the cloud is reachable — a half-open
    /// socket accepts every one of them.
    ///
    /// Real clock, not a paused one: the pump reads SQLite every tick and
    /// a virtual clock races sqlx's own acquire timeout. One heartbeat is
    /// enough, and `tokio::interval` fires its first tick immediately.
    #[tokio::test]
    async fn sending_a_heartbeat_is_not_proof_the_cloud_received_it() {
        let (store, _dir) = test_store().await;
        let live_view = live_view_manager();
        let frame_stats = FrameStatsRegistry::new();
        let liveness = TunnelLiveness::new();
        let tunnel = HalfOpenTunnel::default();

        let pump = pump_heartbeats(
            &tunnel,
            "core-1",
            store,
            &liveness,
            &live_view,
            &frame_stats,
        );
        tokio::pin!(pump);
        // Poll for the condition rather than paying a fixed wait: the pump
        // never returns on its own, which is the point.
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    () = &mut pump => return,
                    () = tokio::time::sleep(Duration::from_millis(10)) => {
                        if tunnel.sent.load(Ordering::Relaxed) > 0 {
                            return;
                        }
                    }
                }
            }
        })
        .await;

        assert!(observed.is_ok(), "fixture never sent a heartbeat");
        assert!(tunnel.sent.load(Ordering::Relaxed) > 0);
        assert!(
            !liveness.heartbeat_since_boot(),
            "no heartbeat was ever acknowledged, so nothing proved this \
             binary can reach the cloud",
        );
    }

    /// The other half of the same invariant, and the line everything now
    /// depends on: the OTA success gate and the go-dark watchdog both read
    /// this signal, and after this change the *only* thing that sets it is
    /// an inbound `heartbeat_ack`. Without this test that call site could
    /// be deleted and the suite would stay green — more emphatically, in
    /// fact, since the sibling test asserts the signal stays unset.
    #[tokio::test]
    async fn an_inbound_heartbeat_ack_is_what_records_cloud_liveness() {
        use nexus_cloud_protocol::v1::HeartbeatAckPayload;

        let (store, _dir) = test_store().await;
        let live_view = live_view_manager();
        let outbox = Arc::new(TunnelOutbox::new());
        let liveness = TunnelLiveness::new();
        // The supervisor marks this on connect; the round trip is the part
        // under test.
        liveness.mark_authenticated();
        let tunnel = HalfOpenTunnel::default();

        let (tx, rx) = mpsc::channel::<Envelope>(4);
        tx.send(Envelope {
            meta: EnvelopeMeta {
                id: uuid::Uuid::now_v7().to_string(),
                in_reply_to: None,
                seq: None,
                trace: None,
                ts: chrono::Utc::now().to_rfc3339(),
                v: 1,
            },
            body: EnvelopeBody::HeartbeatAck(HeartbeatAckPayload {
                cert_rotate: None,
                cloud_capabilities: None,
                server_ts: chrono::Utc::now().to_rfc3339(),
            }),
        })
        .await
        .expect("queue the ack");
        // Closing the sender lets the pump drain and return.
        drop(tx);

        assert!(
            !liveness.heartbeat_since_boot(),
            "nothing has been acknowledged yet",
        );

        let webrtc = crate::webrtc_bridge::WebRtcBridge::disabled();
        let remote_shell = Arc::new(crate::remote_shell::RemoteShellManager::new(
            nexus_config::RemoteAccessConfig::default(),
            nexus_cloud_client::tunnel::TunnelClient::new(
                "wss://cloud.invalid/v1/tunnel",
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            None,
            Arc::clone(&outbox),
        ));
        let entitlements = Arc::new(nexus_cloud_client::entitlements::EntitlementCache::new());
        let pending_acks = Arc::new(crate::cloud_alert_sink::PendingAckRegistry::new());

        pump_rpc_dispatch(
            &tunnel,
            Some(rx),
            None,
            "core-1",
            &outbox,
            &entitlements,
            &pending_acks,
            &store,
            &live_view,
            &webrtc,
            &remote_shell,
            &liveness,
        )
        .await;

        assert!(
            liveness.heartbeat_since_boot(),
            "an acknowledged heartbeat is the only proof the cloud is on the \
             other end of this socket",
        );
    }
}
