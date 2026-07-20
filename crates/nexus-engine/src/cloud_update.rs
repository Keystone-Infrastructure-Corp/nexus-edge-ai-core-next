//! Phase 9 (milestone M_OTA) — edge-side OTA update handler.
//!
//! The cloud `update-orchestrator-svc` dispatches a signed
//! `update_assignment` envelope when a core's deterministic cohort
//! bucket is reached for an active rollout (or immediately on operator
//! Force). This module owns the edge half of that contract:
//!
//! 1. Re-verify the cloud's Ed25519 manifest `signature` against the
//!    public key embedded in this binary
//!    ([`NEXUS_RELEASE_SIGNING_PUBKEY_V1`]) BEFORE any download.
//! 2. Download the release tarball with `reqwest` (bytes never transit
//!    the gateway) and re-verify `artifact_sha256` over the downloaded
//!    bytes.
//! 3. Extract into `/opt/nexus/releases/<version>/`, flip the
//!    `/opt/nexus/current` symlink, and `systemctl restart
//!    nexus-engine` — all three privileged steps gated by the pinned
//!    `/etc/sudoers.d/nexus-update` rules.
//! 4. Emit an `update_progress` envelope at every phase transition.
//!    Dispatch is fire-and-forget; progress is routed by the cloud on
//!    `assignment_id`, never as a sync RPC reply.
//!
//! Privileged work (`tar`, `ln`, `systemctl`) is Linux-only and gated
//! behind `cfg(target_os = "linux")`; on every other platform the apply
//! path fails closed with `failed:unsupported_platform` so the engine
//! still compiles and the dev workstation never tries to self-update.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::Utc;
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, VerifyingKey};
use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{
    Envelope, EnvelopeBody, EnvelopeMeta, ReleaseStatus, UpdateAssignmentPayload,
    UpdateProgressPayload, UpdateRollbackPayload,
};
use nexus_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;

/// Hardcoded Ed25519 release-signing public key (SPKI PEM).
///
/// This is the public half of the `release-ed25519-v1` signing key whose
/// private half lives in Key Vault (`release-signing-key-pem-<env>`) and
/// is file-mounted into the cloud `update-orchestrator-svc`. The edge
/// re-verifies every `update_assignment` manifest signature against this
/// key BEFORE downloading; a mismatch fails closed with
/// `signature_invalid`. A dev/CI override is read from the
/// `NEXUS_RELEASE_SIGNING_PUBKEY_PEM` environment variable — that env
/// path is ONLY for synthetic-edge tests and never set in a real
/// deployment. Rotate via the docs/cloud-console signing-key runbook
/// (bump `signing_key_id` + ship a new edge release with the new key).
const NEXUS_RELEASE_SIGNING_PUBKEY_V1: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAN+D4ubLSbOUPt1muO03GRn4nD5OHpsUthVxR2W449OY=\n-----END PUBLIC KEY-----\n";

/// `engine_runtime_settings` key holding the JSON-serialised
/// [`UpdateState`] single-row snapshot.
const KEY_UPDATE_STATE: &str = "update.state";

/// Fixed on-disk paths for the staged tarball + release tree. Pinned so
/// the `/etc/sudoers.d/nexus-update` command rules can be exact.
#[cfg(target_os = "linux")]
const STAGED_TARBALL: &str = "/opt/nexus/staging/update.tar.gz";
#[cfg(target_os = "linux")]
const RELEASES_DIR: &str = "/opt/nexus/releases";
#[cfg(target_os = "linux")]
const CURRENT_SYMLINK: &str = "/opt/nexus/current";

/// Update phase names — mirror the `update_progress.phase` enum in
/// WIRE_PROTOCOL §4.
mod phase {
    pub const VERIFYING_SIGNATURE: &str = "verifying_signature";
    pub const FETCHING_ARTIFACT: &str = "fetching_artifact";
    pub const DRAINING: &str = "draining";
    pub const RESTARTING: &str = "restarting";
    pub const VERIFYING_HEALTH: &str = "verifying_health";
    pub const SUCCESS: &str = "success";
    pub const FAILED: &str = "failed";
}

/// Persisted single-row update state. Survives the engine restart that an
/// update triggers so the NEW binary can finalise the in-flight attempt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateState {
    /// Channel the core is currently assigned to (`dev|beta|stable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_channel: Option<String>,
    /// Last version the edge committed (flipped the symlink to).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    /// Version known-good before the active attempt — the rollback target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_good: Option<String>,
    /// The assignment currently being applied, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_assignment_id: Option<String>,
    /// Last phase emitted by the OLD binary before the restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_phase: Option<String>,
    /// RFC3339 timestamp of the last update attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<String>,
    /// Terminal result of the last attempt (`success` / `failed:<code>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    /// Crash counter for the crash-loop auto-rollback guard.
    #[serde(default)]
    pub crash_count: u32,
}

/// Read the persisted update state, defaulting to an empty record.
pub async fn read_state(store: &Store) -> UpdateState {
    match store.read_runtime_setting(KEY_UPDATE_STATE).await {
        Ok(Some(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => UpdateState::default(),
    }
}

/// Persist the update state as JSON in `engine_runtime_settings`.
async fn write_state(store: &Store, state: &UpdateState) {
    let Ok(json) = serde_json::to_string(state) else {
        warn!("update: failed to serialise update state");
        return;
    };
    if let Err(e) = store
        .write_runtime_setting(KEY_UPDATE_STATE, Some(&json))
        .await
    {
        warn!(error = %e, "update: failed to persist update state");
    }
}

/// Build the `heartbeat.release` block from persisted state.
///
/// `recording_active` is supplied by the caller (the heartbeat pump) so
/// the orchestrator can defer an update while a clip is mid-write.
pub async fn release_status_for_heartbeat(store: &Store, recording_active: bool) -> ReleaseStatus {
    let state = read_state(store).await;
    ReleaseStatus {
        channel: state
            .assigned_channel
            .unwrap_or_else(|| "stable".to_string()),
        current_version: state
            .current_version
            .unwrap_or_else(|| env!("NEXUS_BUILD_VERSION").to_string()),
        last_update_attempt_at: state.last_attempt_at,
        last_update_result: state.last_result,
        recording_active,
    }
}

/// Canonical manifest bytes — MUST be byte-identical to the
/// orchestrator's `manifest.rs::canonical_bytes`: a compact,
/// sorted-key JSON object. v1 fixes `min_wire_v = max_wire_v = 1` and
/// `min_engine_version_to_apply = null`, matching the orchestrator's v1
/// release defaults (the assignment payload does not carry those fields).
fn canonical_bytes(payload: &UpdateAssignmentPayload) -> Vec<u8> {
    let mut map: BTreeMap<&'static str, Value> = BTreeMap::new();
    map.insert(
        "artifact_sha256",
        Value::from(payload.artifact_sha256.clone()),
    );
    map.insert("artifact_url", Value::from(payload.artifact_url.clone()));
    map.insert("channel", Value::from(payload.channel.clone()));
    map.insert("max_wire_v", Value::from(1u16));
    map.insert("min_engine_version_to_apply", Value::Null);
    map.insert("min_wire_v", Value::from(1u16));
    map.insert("version", Value::from(payload.target_version.clone()));
    serde_json::to_vec(&map).unwrap_or_default()
}

/// Verify the Ed25519 manifest signature against the embedded public key.
fn verify_signature(payload: &UpdateAssignmentPayload) -> Result<(), &'static str> {
    let pem = std::env::var("NEXUS_RELEASE_SIGNING_PUBKEY_PEM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| NEXUS_RELEASE_SIGNING_PUBKEY_V1.to_string());
    if pem.trim().is_empty() {
        return Err("signature_invalid");
    }
    let key = VerifyingKey::from_public_key_pem(&pem).map_err(|_| "signature_invalid")?;
    let sig_bytes = B64
        .decode(payload.signature.trim())
        .map_err(|_| "signature_invalid")?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "signature_invalid")?;
    key.verify_strict(&canonical_bytes(payload), &sig)
        .map_err(|_| "signature_invalid")
}

/// Build an `update_progress` envelope.
fn progress_envelope(
    assignment_id: &str,
    phase: &str,
    pct: Option<u64>,
    error: Option<String>,
) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            id: Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::UpdateProgress(UpdateProgressPayload {
            assignment_id: assignment_id.to_string(),
            error,
            pct,
            phase: phase.to_string(),
        }),
    }
}

/// Fire one `update_progress` envelope (best-effort).
async fn emit_progress(
    outbox: &TunnelOutbox,
    assignment_id: &str,
    phase: &str,
    pct: Option<u64>,
    error: Option<String>,
) {
    let env = progress_envelope(assignment_id, phase, pct, error);
    if let Err(e) = outbox.send(env).await {
        warn!(error = %e, phase, "update: progress send failed (cloud will reconcile from heartbeat)");
    }
}

/// Entry point for an inbound `update_assignment`. Persists the assigned
/// channel and spawns the apply task (fire-and-forget — the caller does
/// not await the long-running download/restart).
pub async fn handle_assignment(
    store: Arc<Store>,
    outbox: Arc<TunnelOutbox>,
    payload: UpdateAssignmentPayload,
) {
    // Idempotent re-delivery: already running the target version.
    let running = env!("NEXUS_BUILD_VERSION");
    let mut state = read_state(&store).await;
    state.assigned_channel = Some(payload.channel.clone());
    if running == payload.target_version {
        info!(
            assignment_id = %payload.assignment_id,
            version = %payload.target_version,
            "update: already running target version; acking idempotently",
        );
        state.last_result = Some(phase::SUCCESS.to_string());
        write_state(&store, &state).await;
        emit_progress(&outbox, &payload.assignment_id, phase::SUCCESS, None, None).await;
        return;
    }
    write_state(&store, &state).await;

    tokio::spawn(async move {
        apply_assignment(&store, &outbox, &payload).await;
    });
}

/// Run the full apply state machine for one assignment.
async fn apply_assignment(store: &Store, outbox: &TunnelOutbox, payload: &UpdateAssignmentPayload) {
    let id = payload.assignment_id.as_str();

    let mut state = read_state(store).await;
    state.active_assignment_id = Some(id.to_string());
    state.last_attempt_at = Some(Utc::now().to_rfc3339());
    state.last_phase = Some(phase::VERIFYING_SIGNATURE.to_string());
    write_state(store, &state).await;

    // 1) Verify the manifest signature BEFORE any download.
    emit_progress(outbox, id, phase::VERIFYING_SIGNATURE, None, None).await;
    if let Err(code) = verify_signature(payload) {
        fail(store, outbox, &mut state, id, code).await;
        return;
    }

    // 2) Download the artifact.
    emit_progress(outbox, id, phase::FETCHING_ARTIFACT, Some(0), None).await;
    let bytes = match download_artifact(&payload.artifact_url).await {
        Ok(b) => b,
        Err(_) => {
            fail(store, outbox, &mut state, id, "artifact_unavailable").await;
            return;
        }
    };

    // 3) Verify sha256 over the downloaded bytes.
    if !sha256_matches(&bytes, &payload.artifact_sha256) {
        fail(store, outbox, &mut state, id, "digest_mismatch").await;
        return;
    }
    emit_progress(outbox, id, phase::FETCHING_ARTIFACT, Some(100), None).await;

    // 4) Install: extract → flip symlink → restart. The restart sends
    //    SIGTERM to this process; the drain handler in `main.rs`
    //    finalises in-flight recordings before exit.
    emit_progress(outbox, id, phase::DRAINING, None, None).await;
    state.previous_good = Some(
        state
            .current_version
            .clone()
            .unwrap_or_else(|| env!("NEXUS_BUILD_VERSION").to_string()),
    );
    state.last_phase = Some(phase::RESTARTING.to_string());
    state.current_version = Some(payload.target_version.clone());
    write_state(store, &state).await;

    emit_progress(outbox, id, phase::RESTARTING, None, None).await;
    if let Err(code) = install_release(&bytes, &payload.target_version).await {
        // Roll the persisted version back so the heartbeat keeps
        // reporting the real running version.
        state.current_version = state.previous_good.clone();
        state.last_phase = Some(phase::FAILED.to_string());
        fail(store, outbox, &mut state, id, code).await;
        return;
    }
    // If `install_release` returns Ok the restart has been requested and
    // SIGTERM is imminent; the NEW binary emits `verifying_health` +
    // `success` from `finalize_pending_update`.
    info!(assignment_id = %id, "update: restart requested; awaiting SIGTERM");
}

/// Mark the attempt failed, persist + emit `failed:<code>`.
async fn fail(
    store: &Store,
    outbox: &TunnelOutbox,
    state: &mut UpdateState,
    assignment_id: &str,
    code: &str,
) {
    let result = format!("{}:{code}", phase::FAILED);
    warn!(assignment_id, code, "update: assignment failed");
    state.active_assignment_id = None;
    state.last_phase = Some(phase::FAILED.to_string());
    state.last_result = Some(result.clone());
    write_state(store, state).await;
    emit_progress(outbox, assignment_id, phase::FAILED, None, Some(result)).await;
}

/// Download the release tarball into memory.
async fn download_artifact(url: &str) -> Result<Vec<u8>, ()> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| warn!(error = %e, "update: http client build failed"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| warn!(error = %e, "update: artifact GET failed"))?;
    if !resp.status().is_success() {
        warn!(status = %resp.status(), "update: artifact GET non-200");
        return Err(());
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| warn!(error = %e, "update: artifact body read failed"))?;
    Ok(bytes.to_vec())
}

/// Constant-time-ish lower-hex SHA-256 comparison.
fn sha256_matches(bytes: &[u8], expected_hex: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let got = hasher.finalize();
    let got_hex = hex::encode(got);
    got_hex.eq_ignore_ascii_case(expected_hex.trim())
}

/// Handle an inbound `update_rollback`: re-install the locally-cached
/// `previous_good` release (already on disk + Ed25519-verified at first
/// install) by flipping the symlink back and restarting.
pub async fn handle_rollback(
    store: Arc<Store>,
    outbox: Arc<TunnelOutbox>,
    payload: UpdateRollbackPayload,
) {
    let mut state = read_state(&store).await;
    let Some(target) = state.previous_good.clone() else {
        warn!(reason = %payload.reason, "update: rollback requested but no previous_good on disk");
        return;
    };
    info!(reason = %payload.reason, target = %target, "update: rolling back to previous_good");
    let assignment_id = state
        .active_assignment_id
        .clone()
        .unwrap_or_else(|| format!("rollback-{}", Uuid::now_v7()));
    state.current_version = Some(target.clone());
    state.last_phase = Some(phase::RESTARTING.to_string());
    state.last_result = Some("rolled_back".to_string());
    state.last_attempt_at = Some(Utc::now().to_rfc3339());
    write_state(&store, &state).await;
    emit_progress(&outbox, &assignment_id, phase::RESTARTING, None, None).await;
    if let Err(code) = flip_and_restart(&target).await {
        warn!(code, "update: rollback failed");
        state.last_result = Some(format!("{}:rollback_also_failed", phase::FAILED));
        write_state(&store, &state).await;
        emit_progress(
            &outbox,
            &assignment_id,
            phase::FAILED,
            None,
            Some(format!("{}:rollback_also_failed", phase::FAILED)),
        )
        .await;
    }
}

/// Boot-time finalisation of an in-flight update. Called early in
/// `main`. If the previous boot committed an update (`last_phase ==
/// restarting`) and the running version now matches the target, emit
/// `verifying_health` + `success`. Otherwise apply the crash-loop guard.
pub async fn finalize_pending_update(store: Arc<Store>, outbox: Arc<TunnelOutbox>) {
    let mut state = read_state(&store).await;
    if state.last_phase.as_deref() != Some(phase::RESTARTING) {
        return;
    }
    let running = env!("NEXUS_BUILD_VERSION");
    let assignment_id = state
        .active_assignment_id
        .clone()
        .unwrap_or_else(|| format!("recover-{}", Uuid::now_v7()));

    if state.current_version.as_deref() == Some(running) {
        // The new version is up — success.
        info!(
            version = running,
            "update: post-restart health OK; finalising success"
        );
        emit_progress(&outbox, &assignment_id, phase::VERIFYING_HEALTH, None, None).await;
        emit_progress(&outbox, &assignment_id, phase::SUCCESS, None, None).await;
        state.active_assignment_id = None;
        state.last_phase = Some(phase::SUCCESS.to_string());
        state.last_result = Some(phase::SUCCESS.to_string());
        state.crash_count = 0;
        write_state(&store, &state).await;

        // Reclaim disk now that the new release has booted healthy: prune
        // every release tree except the running one and the rollback target.
        // Best-effort + non-fatal — release hygiene must never affect the
        // update outcome (already reported success above).
        prune_old_releases(&state).await;
        return;
    }

    // Running version does NOT match the target after a restart attempt:
    // count it as a crash. Crash-loop auto-rollback fires at 3.
    state.crash_count = state.crash_count.saturating_add(1);
    warn!(
        crash_count = state.crash_count,
        running, "update: post-restart version mismatch (crash-loop guard)",
    );
    write_state(&store, &state).await;
    if state.crash_count >= 3 {
        if let Some(target) = state.previous_good.clone() {
            warn!(target = %target, "update: crash-loop threshold reached; auto-rolling back");
            emit_progress(
                &outbox,
                &assignment_id,
                phase::FAILED,
                None,
                Some(format!("{}:health_check_failed", phase::FAILED)),
            )
            .await;
            let store2 = Arc::clone(&store);
            let outbox2 = Arc::clone(&outbox);
            tokio::spawn(async move {
                handle_rollback(
                    store2,
                    outbox2,
                    UpdateRollbackPayload {
                        // Locally self-issued crash-loop rollback: calls
                        // the handler directly, never crosses the tunnel,
                        // so there is no actor_token to verify.
                        actor_token: None,
                        reason: "crash-loop auto-rollback".to_string(),
                    },
                )
                .await;
            });
        }
    }
}

// ---------------------------------------------------------------------
// Privileged install steps — Linux only.
// ---------------------------------------------------------------------

/// Extract the tarball, flip the `current` symlink, and restart.
///
/// The release tarball carries a single top-level directory named after
/// the **bare** version (e.g. `0.1.7/`) — the same `<version>` the cloud
/// dispatches (`== NEXUS_BUILD_VERSION`; see the `package tarball` step in
/// `.github/workflows/release.yml`). Extracting into `RELEASES_DIR` (no
/// `--strip-components`) therefore lands the tree at
/// `RELEASES_DIR/<version>/`, exactly where `flip_and_restart` points
/// `current`.
#[cfg(target_os = "linux")]
async fn install_release(bytes: &[u8], version: &str) -> Result<(), &'static str> {
    use std::io::Write as _;
    // Stage the tarball at the pinned path (matches the sudoers tar rule).
    if let Some(parent) = std::path::Path::new(STAGED_TARBALL).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(error = %e, dir = %parent.display(), "update: staging dir create failed");
        }
    }
    let mut f = std::fs::File::create(STAGED_TARBALL).map_err(|e| {
        warn!(error = %e, path = STAGED_TARBALL, "update: staged tarball create failed");
        "artifact_unavailable"
    })?;
    f.write_all(bytes).map_err(|e| {
        warn!(error = %e, path = STAGED_TARBALL, "update: staged tarball write failed");
        "artifact_unavailable"
    })?;
    drop(f);

    // sudo tar -xzf <staged> -C /opt/nexus/releases. The tarball's top-level
    // dir is the bare <version>, so this lands the tree at
    // /opt/nexus/releases/<version>/.
    let extract = std::process::Command::new("sudo")
        .args(["/usr/bin/tar", "-xzf", STAGED_TARBALL, "-C", RELEASES_DIR])
        .status();
    match extract {
        Ok(s) if s.success() => {}
        Ok(s) => {
            warn!(code = ?s.code(), "update: `sudo tar` extract exited non-zero");
            return Err("artifact_unavailable");
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `sudo tar` extract");
            return Err("artifact_unavailable");
        }
    }

    // Sanity-check that the expected release dir materialised BEFORE flipping
    // the live symlink. A mismatch here (e.g. a tarball whose top-level dir
    // is not the bare <version>) would otherwise leave `current` dangling and
    // the unit failing status=203/EXEC after the restart — a silent brick.
    let release_path = format!("{RELEASES_DIR}/{version}");
    if !std::path::Path::new(&release_path).is_dir() {
        warn!(
            expected = %release_path,
            "update: extracted release dir missing after tar (tarball top-level dir != bare version?)"
        );
        return Err("artifact_unavailable");
    }

    // Install any runtime system dependencies this release declares
    // (`share/apt-requirements.txt`) via the pinned, root-owned wrapper at
    // `/usr/local/sbin/nexus-apply-deps` (installed by `scripts/install.sh`,
    // whitelisted in `deploy/sudoers.d/nexus-update`). This is what lets an
    // OTA pull in a NEW system package (e.g. `gstreamer1.0-nice` for HD live
    // view) without an operator hand-touching the box. Best-effort and
    // NON-FATAL: a missing or failed dep install must never block the version
    // flip — the engine fail-opens locally and only optional features degrade.
    // The wrapper enforces its own package allowlist, so a tampered manifest
    // cannot coerce it into installing arbitrary packages.
    match std::process::Command::new("sudo")
        .args(["/usr/local/sbin/nexus-apply-deps", &release_path])
        .status()
    {
        Ok(s) if s.success() => {
            info!(release = %release_path, "update: runtime dependencies applied");
        }
        Ok(s) => {
            warn!(
                code = ?s.code(),
                "update: nexus-apply-deps exited non-zero; proceeding with flip (optional features may degrade)"
            );
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn nexus-apply-deps; proceeding with flip");
        }
    }

    flip_and_restart(version).await
}

/// Flip `/opt/nexus/current` to the named release and restart the unit.
#[cfg(target_os = "linux")]
async fn flip_and_restart(version: &str) -> Result<(), &'static str> {
    let release_path = format!("{RELEASES_DIR}/{version}");
    let link = std::process::Command::new("sudo")
        .args(["/usr/bin/ln", "-sfn", &release_path, CURRENT_SYMLINK])
        .status();
    match link {
        Ok(s) if s.success() => {}
        Ok(s) => {
            warn!(code = ?s.code(), target = %release_path, "update: `sudo ln` symlink flip exited non-zero");
            return Err("rollback_also_failed");
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `sudo ln` symlink flip");
            return Err("rollback_also_failed");
        }
    }
    let restart = std::process::Command::new("sudo")
        .args(["/usr/bin/systemctl", "restart", "nexus-engine"])
        .status();
    match restart {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            warn!(code = ?s.code(), "update: `sudo systemctl restart` exited non-zero");
            Err("rollback_also_failed")
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `sudo systemctl restart`");
            Err("rollback_also_failed")
        }
    }
}

/// Prune stale release trees under `/opt/nexus/releases`, keeping only the
/// running version and the rollback target (`previous_good`). Invoked from
/// `finalize_pending_update` after an OTA has booted healthy, so we never
/// delete a release we might still roll back to. Best-effort + non-fatal —
/// release hygiene must never block or fail an update.
///
/// Root-gated by the pinned `/usr/local/sbin/nexus-prune-releases` wrapper
/// (the fifth `nexus-update` sudoers rule), which itself always preserves the
/// live `current` symlink target and fail-closes (deletes nothing) on an empty
/// keep set — so even a bad invocation can never delete the running release.
#[cfg(target_os = "linux")]
async fn prune_old_releases(state: &UpdateState) {
    let mut keep: Vec<&str> = Vec::new();
    if let Some(v) = state.current_version.as_deref() {
        keep.push(v);
    }
    if let Some(v) = state.previous_good.as_deref() {
        if !keep.contains(&v) {
            keep.push(v);
        }
    }
    if keep.is_empty() {
        return;
    }
    let mut cmd = std::process::Command::new("sudo");
    cmd.arg("/usr/local/sbin/nexus-prune-releases");
    cmd.args(&keep);
    match cmd.status() {
        Ok(s) if s.success() => {
            info!("update: pruned old releases (kept running + previous_good)")
        }
        Ok(s) => {
            warn!(code = ?s.code(), "update: nexus-prune-releases exited non-zero (non-fatal)")
        }
        Err(e) => warn!(error = %e, "update: failed to spawn nexus-prune-releases (non-fatal)"),
    }
}

/// Non-Linux stub — the dev workstation never prunes releases.
#[cfg(not(target_os = "linux"))]
async fn prune_old_releases(_state: &UpdateState) {}

/// Non-Linux stub — the dev workstation never self-updates.
#[cfg(not(target_os = "linux"))]
async fn install_release(_bytes: &[u8], _version: &str) -> Result<(), &'static str> {
    Err("unsupported_platform")
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
async fn flip_and_restart(_version: &str) -> Result<(), &'static str> {
    Err("unsupported_platform")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::{Signer, SigningKey};

    fn sample_payload() -> UpdateAssignmentPayload {
        UpdateAssignmentPayload {
            actor_token: None,
            allow_downgrade: None,
            artifact_sha256: "a".repeat(64),
            artifact_url: "https://example.test/engine.tar.gz".to_string(),
            assignment_id: "11111111-1111-1111-1111-111111111111".to_string(),
            channel: "stable".to_string(),
            deferral_until: None,
            force: None,
            manifest_url: None,
            signature: String::new(),
            target_version: "0.6.0".to_string(),
        }
    }

    #[test]
    fn canonical_bytes_are_sorted_compact_json() {
        let p = sample_payload();
        let got = String::from_utf8(canonical_bytes(&p)).unwrap();
        assert_eq!(
            got,
            "{\"artifact_sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"artifact_url\":\"https://example.test/engine.tar.gz\",\"channel\":\"stable\",\"max_wire_v\":1,\"min_engine_version_to_apply\":null,\"min_wire_v\":1,\"version\":\"0.6.0\"}"
        );
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let pem = sk
            .verifying_key()
            .to_public_key_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .unwrap();
        std::env::set_var("NEXUS_RELEASE_SIGNING_PUBKEY_PEM", &pem);

        let mut p = sample_payload();
        let sig = sk.sign(&canonical_bytes(&p));
        p.signature = B64.encode(sig.to_bytes());
        assert!(verify_signature(&p).is_ok());

        // Tamper → fails.
        p.target_version = "9.9.9".to_string();
        assert!(verify_signature(&p).is_err());
        std::env::remove_var("NEXUS_RELEASE_SIGNING_PUBKEY_PEM");
    }

    #[test]
    fn sha256_matches_is_case_insensitive() {
        let data = b"hello";
        let hex_lower = hex::encode(Sha256::digest(data));
        assert!(sha256_matches(data, &hex_lower));
        assert!(sha256_matches(data, &hex_lower.to_uppercase()));
        assert!(!sha256_matches(data, &"0".repeat(64)));
    }
}
