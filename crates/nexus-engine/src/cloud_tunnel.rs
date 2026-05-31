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

use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::VerifyingKey;
use nexus_cloud_client::trace_uploader::{
    Span, TraceUploader, TraceUploaderConfig, DEFAULT_BATCH_SIZE, DEFAULT_FLUSH_INTERVAL,
    DEFAULT_QUEUE_CAPACITY,
};
use nexus_cloud_client::{
    RpcDispatcher, RpcResponseCache, SystemMethodPolicy, TrustedKey, TunnelClient, TunnelHandle,
    VerifierBuilder,
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
        install_cloud_blob_backend(&enrollment, &store, &registry, &replicator_kick).await;
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
        run(enrollment, dispatcher, cloud_outbox, rx).await;
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

    // System-sub policy: the only `system:` lane we admit today is
    // the existing `entitlement_update`. The Phase 2 Expedite path
    // is on the human-actor lane (owner/admin/operator).
    let policy = SystemMethodPolicy::default();
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
    // cert material needed here. 5 min total timeout is generous
    // for a single PUT of a typical 30-60 s clip MP4 (a few MB).
    let azure_http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5 * 60))
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

async fn run(
    enrollment: CloudEnrollment,
    dispatcher: Option<Arc<RpcDispatcher<EngineRpcHandler>>>,
    cloud_outbox: Arc<nexus_cloud_client::TunnelOutbox>,
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
                let pump = pump_heartbeats(&*conn, &core_id);
                let dispatch = pump_rpc_dispatch(&*conn, inbound, dispatcher.as_deref(), &core_id);
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
async fn pump_rpc_dispatch<H: TunnelHandle>(
    handle: &H,
    inbound: Option<mpsc::Receiver<Envelope>>,
    dispatcher: Option<&RpcDispatcher<EngineRpcHandler>>,
    core_id: &str,
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
            other => {
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

async fn pump_heartbeats<H: TunnelHandle>(handle: &H, _core_id: &str) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let start = std::time::Instant::now();
    let mut seq: u64 = 0;
    loop {
        interval.tick().await;
        seq = seq.wrapping_add(1);
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
                online_cameras: 0,
                queued_alerts: 0,
                release: None,
                tier: "dev".to_string(),
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
