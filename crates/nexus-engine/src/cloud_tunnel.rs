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
use nexus_cloud_client::trace_uploader::{
    Span, TraceUploader, TraceUploaderConfig, DEFAULT_BATCH_SIZE, DEFAULT_FLUSH_INTERVAL,
    DEFAULT_QUEUE_CAPACITY,
};
use nexus_cloud_client::{
    EnvelopeContext, RpcDispatcher, RpcResponseCache, SystemMethodPolicy, TrustedKey, TunnelClient,
    TunnelHandle, VerifiedActor, VerifierBuilder,
};
use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody, EnvelopeMeta, HeartbeatPayload};
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

/// Heartbeat cadence. Matches the cloud edge-gateway's `liveness_timeout / 2`
/// expectation.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

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
    webrtc: Arc<crate::webrtc_bridge::WebRtcBridge>,
    trace_rx: Option<mpsc::Receiver<Span>>,
    loopback_admin_base: Arc<arc_swap::ArcSwap<String>>,
    admin_secret: Option<Arc<String>>,
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
            webrtc,
            store,
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
    webrtc: Arc<crate::webrtc_bridge::WebRtcBridge>,
    store: Arc<Store>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let client = TunnelClient::new(
        enrollment.gateway_url.clone(),
        enrollment.cert_pem.clone(),
        enrollment.private_key_pem.clone(),
        enrollment.ca_chain_pem.clone(),
    );
    let mut backoff = BACKOFF_MIN;
    let core_id = enrollment.core_id.clone();
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
                let pump = pump_heartbeats(&*conn, &core_id, store.clone());
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
                );
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
                        warn!(core_id = %core_id, "cloud tunnel inbound dispatch ended; will reconnect");
                    }
                }
                cloud_outbox.set_handle(None);
                live_view.clear_all();
                webrtc.clear_all();
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
            other => {
                if let EnvelopeBody::HeartbeatAck(ack) = other {
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

async fn pump_heartbeats<H: TunnelHandle>(handle: &H, _core_id: &str, store: Arc<Store>) {
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
                // Dual-transport live view — advertise the always-on LBR pump
                // (`live_view`) plus the per-core configured HD transport
                // (`hd_sfu` / `hd_moq`) so the cloud routes an expanding
                // operator to the matching client adapter. `talkdown_webrtc`
                // is advertised whenever the WebRTC/Opus talk-down
                // sub-pipeline is compiled in. No back-compat: the old single
                // `webrtc` tag is gone. Additive on wire `v=1`.
                caps: Some({
                    // `mut` is only exercised when the talk-down sub-pipeline
                    // is compiled in; suppress the unused-mut lint otherwise.
                    #[cfg_attr(not(feature = "gstreamer-webrtc"), allow(unused_mut))]
                    let mut caps =
                        vec!["live_view".to_string(), hd_transport.cap_tag().to_string()];
                    #[cfg(feature = "gstreamer-webrtc")]
                    caps.push("talkdown_webrtc".to_string());
                    caps
                }),
                online_cameras: 0,
                queued_alerts: 0,
                release,
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
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
