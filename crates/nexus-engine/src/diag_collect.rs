//! Phase 7.0a · Step 2 — edge-side diagnostics collector.
//!
//! When the cloud control plane wants a support bundle from a core it
//! sends a verified `diag_collect` envelope down the tunnel (see
//! `cloud_tunnel::pump_rpc_dispatch`). The envelope carries a pre-minted
//! write SAS URL plus the collection parameters. This module runs the
//! resulting collect → upload → confirm flow on a detached task:
//!
//! 1. GET the engine's own loopback diagnostics export
//!    (`/api/v1/admin/diagnostics/export`), streaming the gzip tarball
//!    into memory with a hard `max_bytes` cap so a runaway export can't
//!    balloon RAM or the upload.
//! 2. PUT those bytes straight to the SAS URL (Azure Blob `BlockBlob`),
//!    with a small bounded retry on transient 5xx / transport errors.
//!    The bytes never transit the cloud gateway — only the SAS URL does
//!    (cloud Hard Rule 7).
//! 3. Emit exactly one terminal `diag_ready` envelope back up the tunnel
//!    so the cloud can flip the `diag_collections` row to `uploaded` or
//!    `failed`.
//!
//! The flow is fail-open (cloud Hard Rule 5 / edge Hard Rule 5): every
//! exit path emits a `diag_ready` if the tunnel is up, and if the tunnel
//! is down at emit time we log and drop rather than block — the operator
//! re-collects.
//!
//! ## Privacy / boundary
//!
//! All Azure I/O is a plain `reqwest` PUT against a SAS URL the cloud
//! minted; this crate links no Azure SDK (edge Hard Rule 1). The tarball
//! contents are whatever the local export endpoint produces; this module
//! does not add identifiers.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use nexus_cloud_client::{build_diag_ready_envelope, TunnelOutbox};
use nexus_cloud_protocol::v1::DiagCollectPayload;
use tracing::{info, warn};

/// Default tarball cap (50 MiB) when the cloud omits `max_bytes`.
const DEFAULT_MAX_BYTES: u64 = 52_428_800;
/// Lower clamp on `max_bytes` (1 MiB) — a smaller cap can't hold even
/// the snapshot metadata, so treat anything below it as a typo.
const MIN_MAX_BYTES: u64 = 1_048_576;
/// Upper clamp on `max_bytes` (512 MiB) — mirrors the cloud-side clamp
/// in `handlers::diagnostics` so the two agree on the ceiling.
const MAX_MAX_BYTES: u64 = 536_870_912;
/// Azure Blob REST API version pinned on the SAS PUT. Matches the clip
/// uploader in `nexus-storage-cloud`.
const AZURE_API_VERSION: &str = "2023-11-03";
/// Maximum SAS PUT attempts before giving up.
const MAX_UPLOAD_ATTEMPTS: u32 = 3;

/// A stable diagnostics failure mode. Each variant maps to one of the
/// cloud's locked `diag_collections.error_code` enum values via
/// [`Self::code`]; [`Self::message`] is the scrubbed, operator-facing
/// string stamped into `diag_ready.error_message`.
enum DiagError {
    /// The loopback export request failed or returned non-2xx.
    TarballFailed(String),
    /// The streamed tarball exceeded the negotiated `max_bytes` cap.
    TarballTooLarge(u64),
    /// The SAS URL returned 403 — the token expired (or was malformed)
    /// before the upload finished. A re-collect (fresh SAS) is the fix.
    SasExpired,
    /// The SAS PUT failed for any other reason after exhausting retries.
    UploadFailed(String),
}

impl DiagError {
    /// Locked `diag_collections.error_code` value for this failure.
    fn code(&self) -> &'static str {
        match self {
            Self::TarballFailed(_) => "tarball_failed",
            Self::TarballTooLarge(_) => "tarball_too_large",
            Self::SasExpired => "sas_expired",
            Self::UploadFailed(_) => "upload_failed",
        }
    }

    /// Operator-facing message for `diag_ready.error_message`.
    fn message(&self) -> String {
        match self {
            Self::TarballFailed(detail) => {
                format!("diagnostics tarball assembly failed: {detail}")
            }
            Self::TarballTooLarge(cap) => {
                format!("diagnostics tarball exceeded the {cap}-byte limit")
            }
            Self::SasExpired => {
                "the upload URL expired before the tarball finished uploading".to_string()
            }
            Self::UploadFailed(detail) => {
                format!("uploading the diagnostics tarball failed: {detail}")
            }
        }
    }
}

/// Run the collect → upload → confirm flow to completion and emit a
/// single terminal `diag_ready` through `outbox`. Spawned detached by
/// [`crate::engine_rpc::EngineRpcHandler::spawn_diag_collect`]; the
/// caller has already verified the `actor_token` and gated on a
/// privileged role.
pub async fn run(
    http: reqwest::Client,
    loopback_admin_base: Arc<ArcSwap<String>>,
    admin_secret: Option<Arc<String>>,
    payload: DiagCollectPayload,
    outbox: Arc<TunnelOutbox>,
) {
    let diag_id = payload.diag_id.clone();
    let include_sqlite = payload.include_sqlite.unwrap_or(false);
    let max_bytes = payload
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(MIN_MAX_BYTES, MAX_MAX_BYTES);

    info!(diag_id = %diag_id, include_sqlite, max_bytes, "starting diag collection");

    let outcome = collect_and_upload(
        &http,
        &loopback_admin_base,
        admin_secret.as_deref().map(String::as_str),
        &payload,
        max_bytes,
    )
    .await;

    let envelope = match outcome {
        Ok(size_bytes) => {
            // Partial success: the loopback export endpoint does not
            // (yet) bundle the SQLite state DB, so an opt-in
            // `include_sqlite` request uploads the tarball WITHOUT it
            // and reports the locked `include_sqlite_unavailable` code.
            let error_code = include_sqlite.then(|| "include_sqlite_unavailable".to_string());
            if error_code.is_some() {
                info!(
                    diag_id = %diag_id,
                    size_bytes,
                    "diag tarball uploaded (sqlite requested but unavailable in this build)"
                );
            } else {
                info!(diag_id = %diag_id, size_bytes, "diag tarball uploaded");
            }
            build_diag_ready_envelope(
                diag_id.clone(),
                "uploaded".to_string(),
                Some(size_bytes),
                error_code,
                None,
            )
        }
        Err(err) => {
            warn!(diag_id = %diag_id, error_code = err.code(), "diag collection failed");
            build_diag_ready_envelope(
                diag_id.clone(),
                "failed".to_string(),
                None,
                Some(err.code().to_string()),
                Some(err.message()),
            )
        }
    };

    if let Err(e) = outbox.send(envelope).await {
        // Tunnel down at emit time. We MUST NOT block (fail-open). The
        // cloud row stays non-terminal until the operator re-collects.
        warn!(
            diag_id = %diag_id,
            error = %e,
            "diag_ready emit failed (tunnel down); cloud will see this run as in-flight until re-collected"
        );
    }
}

/// Assemble the tarball from the loopback export, then upload it to the
/// SAS URL. Returns the uploaded (compressed, on-wire) byte count on
/// success.
async fn collect_and_upload(
    http: &reqwest::Client,
    loopback_admin_base: &Arc<ArcSwap<String>>,
    admin_secret: Option<&str>,
    payload: &DiagCollectPayload,
    max_bytes: u64,
) -> Result<u64, DiagError> {
    let tarball = assemble_tarball(http, loopback_admin_base, admin_secret, max_bytes).await?;
    let size = tarball.len() as u64;
    upload_to_sas(http, &payload.sas_put_url, tarball).await?;
    Ok(size)
}

/// GET the engine's own loopback diagnostics export, streaming the gzip
/// body into memory and aborting if it would exceed `max_bytes`.
async fn assemble_tarball(
    http: &reqwest::Client,
    loopback_admin_base: &Arc<ArcSwap<String>>,
    admin_secret: Option<&str>,
    max_bytes: u64,
) -> Result<Vec<u8>, DiagError> {
    let base = loopback_admin_base.load();
    // Local admin API serves under `/api/v1` (api.rs `.nest("/api", _)`).
    let url = format!(
        "{}/api/v1/admin/diagnostics/export",
        base.trim_end_matches('/')
    );

    let mut req = http.get(&url);
    if let Some(secret) = admin_secret {
        let token = crate::admin_auth::mint_internal_passthrough_bearer(secret).map_err(|e| {
            DiagError::TarballFailed(format!("minting loopback bearer failed: {e}"))
        })?;
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }

    let mut resp = req
        .send()
        .await
        .map_err(|e| DiagError::TarballFailed(format!("loopback export request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(DiagError::TarballFailed(format!(
            "loopback export returned {status}: {}",
            truncate(&body)
        )));
    }

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        DiagError::TarballFailed(format!("reading loopback export body failed: {e}"))
    })? {
        if buf.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(DiagError::TarballTooLarge(max_bytes));
        }
        buf.extend_from_slice(&chunk);
    }

    if buf.is_empty() {
        return Err(DiagError::TarballFailed(
            "loopback export produced an empty tarball".to_string(),
        ));
    }

    Ok(buf)
}

/// PUT `bytes` to the SAS URL as a single `BlockBlob`. Retries up to
/// [`MAX_UPLOAD_ATTEMPTS`] on transient 5xx / transport failures with a
/// bounded backoff; treats 403 as an expired SAS and other 4xx as a
/// hard, non-retryable failure.
async fn upload_to_sas(
    http: &reqwest::Client,
    sas_put_url: &str,
    bytes: Vec<u8>,
) -> Result<(), DiagError> {
    let len = bytes.len();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let result = http
            .put(sas_put_url)
            .header("x-ms-blob-type", "BlockBlob")
            .header("x-ms-version", AZURE_API_VERSION)
            .header(reqwest::header::CONTENT_LENGTH, len.to_string())
            .body(bytes.clone())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let code = resp.status();
                if code == reqwest::StatusCode::FORBIDDEN {
                    return Err(DiagError::SasExpired);
                }
                if code.is_client_error() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(DiagError::UploadFailed(format!(
                        "blob PUT returned {code}: {}",
                        truncate(&body)
                    )));
                }
                if attempt >= MAX_UPLOAD_ATTEMPTS {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(DiagError::UploadFailed(format!(
                        "blob PUT returned {code} after {attempt} attempts: {}",
                        truncate(&body)
                    )));
                }
            }
            Err(e) => {
                if attempt >= MAX_UPLOAD_ATTEMPTS {
                    return Err(DiagError::UploadFailed(format!(
                        "blob PUT transport error after {attempt} attempts: {e}"
                    )));
                }
            }
        }

        // Bounded backoff: 1s, 2s before the next attempt.
        let backoff = Duration::from_secs(1u64 << (attempt - 1).min(1));
        tokio::time::sleep(backoff).await;
    }
}

/// Clip a remote error body to 200 chars so a chatty Azure / admin-API
/// error page can't bloat the log line or the wire `error_message`.
fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}
