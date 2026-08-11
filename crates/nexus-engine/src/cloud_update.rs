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
//! 3. Stage the verified tarball and hand the entire privileged sequence
//!    (extract → deps/journald → flip `/opt/nexus/current` → `systemctl
//!    restart nexus-engine`) to the single root-owned applier
//!    `/usr/local/sbin/nexus-apply-release`, the ONLY command the pinned
//!    `/etc/sudoers.d/nexus-update` rule grants. The sudoers grant is a
//!    stable, argv-independent wildcard on that one path, so the engine's
//!    privileged behaviour can evolve without ever editing sudoers again —
//!    the argv-drift that once bricked an OTA is designed out.
//! 4. Emit an `update_progress` envelope at every phase transition.
//!    Dispatch is fire-and-forget; progress is routed by the cloud on
//!    `assignment_id`, never as a sync RPC reply.
//!
//! Privileged work is Linux-only and gated behind `cfg(target_os =
//! "linux")`; on every other platform the apply path fails closed with
//! `failed:unsupported_platform` so the engine still compiles and the dev
//! workstation never tries to self-update.

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
use tracing::{error, info, warn};
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

/// Fixed on-disk path the engine stages the verified tarball to before
/// invoking the applier. Pinned so the root-owned applier can read it from a
/// known location; the applier owns the release-tree + symlink layout.
#[cfg(target_os = "linux")]
const STAGED_TARBALL: &str = "/opt/nexus/staging/update.tar.gz";

/// The symlink the applier flips to activate a release. World-readable, so
/// the unprivileged engine can read it back to confirm an apply landed even
/// when the applier could not report its own exit status.
#[cfg(target_os = "linux")]
const CURRENT_LINK: &str = "/opt/nexus/current";

/// The single root-owned OTA applier — the ONLY command the pinned
/// `/etc/sudoers.d/nexus-update` rule grants. Every privileged step
/// (extract, deps/journald, symlink flip, restart, rollback reflip,
/// post-health prune) goes through it via `sudo <path> <mode> <version>`.
/// The sudoers grant is a stable wildcard on this fixed path, so this argv
/// can change freely without any sudoers edit.
#[cfg(target_os = "linux")]
const APPLY_RELEASE_WRAPPER: &str = "/usr/local/sbin/nexus-apply-release";

/// How long a freshly-installed release gets to re-establish the cloud
/// tunnel before it is judged unreachable and rolled back.
///
/// Ten minutes is chosen against the reconnect backoff ceiling (60 s) and
/// the heartbeat cadence (30 s): a release that is going to work will
/// prove it inside the first minute or two, so this window is almost
/// entirely slack for a site whose uplink comes back slowly after a power
/// event. Erring long is cheap — the box is up and working locally the
/// whole time — whereas erring short means rolling back a good release.
const TUNNEL_PROOF_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// Poll cadence while waiting for that proof.
const TUNNEL_PROOF_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait for the tunnel to complete an authenticated connect plus one
/// heartbeat. Returns `false` on timeout.
async fn await_tunnel_proof(liveness: &crate::cloud_liveness::TunnelLiveness) -> bool {
    let deadline = tokio::time::Instant::now() + TUNNEL_PROOF_WINDOW;
    loop {
        if liveness.heartbeat_since_boot() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(TUNNEL_PROOF_POLL).await;
    }
}

/// The release this appliance would fall back to, if any.
pub async fn previous_good_version(store: &Arc<Store>) -> Option<String> {
    read_state(store).await.previous_good
}

/// Flip `/opt/nexus/current` back to `version` and restart the unit.
///
/// Shared by the OTA failure paths and the go-dark watchdog. The
/// persisted state is updated BEFORE the flip, because the flip ends this
/// process — anything written after it is a write that may never happen.
pub async fn reflip_to(store: &Arc<Store>, version: &str) {
    let mut state = read_state(store).await;
    state.current_version = Some(version.to_string());
    state.last_phase = Some(phase::RESTARTING.to_string());
    state.last_result = Some("rolled_back".to_string());
    state.last_attempt_at = Some(Utc::now().to_rfc3339());
    write_state(store, &state).await;
    if let Err(code) = flip_and_restart(version).await {
        error!(
            code,
            version, "update: reflip to the previous-good release failed"
        );
    }
}

/// Build the argv (minus the leading `sudo`) for one applier invocation.
/// Pure + total so it can be unit-tested without shelling out. `mode` is one
/// of `apply` | `reflip` | `prune`.
#[cfg(target_os = "linux")]
fn apply_release_argv<'a>(mode: &'a str, version: &'a str) -> [&'a str; 3] {
    [APPLY_RELEASE_WRAPPER, mode, version]
}

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

    // 1b) Preflight the privileged surface BEFORE downloading a single byte.
    //     Confirm sudo actually authorises the pinned applier; if the box's
    //     sudoers has drifted (e.g. an old per-argv file left behind), abort
    //     early with a distinct, actionable status instead of downloading,
    //     staging, and half-applying an update that can never restart.
    if let Err(code) = preflight_privileged() {
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
        discard_staged_tarball();
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
/// restarting`) and the running version now matches the target, wait for
/// the cloud tunnel to prove itself and then emit `verifying_health` +
/// `success`. Otherwise apply the crash-loop guard.
///
/// ## Why success waits for the tunnel
///
/// "The new binary is running" is a much weaker claim than it looks. An
/// engine can come up, serve its local UI, record, and evaluate rules
/// while being completely unable to re-establish the cloud tunnel — and
/// that is precisely the release we must never bless, because blessing it
/// is what makes the appliance permanently unreachable. So the success
/// path additionally requires an authenticated connect plus one heartbeat
/// on THIS binary. If that does not happen inside
/// [`TUNNEL_PROOF_WINDOW`], the release is rolled back even though
/// nothing crashed.
///
/// This is deliberately spawned rather than awaited by `main`: the proof
/// takes minutes, and nothing else about bringing the engine up should
/// wait on the cloud (Hard Rule 5 — fail open locally).
pub async fn finalize_pending_update(
    store: Arc<Store>,
    outbox: Arc<TunnelOutbox>,
    liveness: Arc<crate::cloud_liveness::TunnelLiveness>,
) {
    let mut state = read_state(&store).await;
    if state.last_phase.as_deref() != Some(phase::RESTARTING) {
        return;
    }
    // The applier has already extracted it, so whatever this attempt ends up
    // deciding, the staged tarball is dead weight from here on. Rollback does
    // not need it either — `reflip` points `current` at an already-extracted
    // release tree.
    discard_staged_tarball();
    let running = env!("NEXUS_BUILD_VERSION");
    let assignment_id = state
        .active_assignment_id
        .clone()
        .unwrap_or_else(|| format!("recover-{}", Uuid::now_v7()));

    if state.current_version.as_deref() == Some(running) {
        // The binary took. Now make it prove it can still be reached.
        if !await_tunnel_proof(&liveness).await {
            warn!(
                signal = "ota_tunnel_proof_failed",
                version = running,
                window_s = TUNNEL_PROOF_WINDOW.as_secs(),
                "update: the new release is running but never re-established the \
                 cloud tunnel; rolling back rather than blessing an unreachable \
                 appliance"
            );
            emit_progress(
                &outbox,
                &assignment_id,
                phase::FAILED,
                None,
                Some(format!("{}:tunnel_not_reestablished", phase::FAILED)),
            )
            .await;
            if let Some(target) = state.previous_good.clone() {
                state.crash_count = 0;
                state.last_result = Some(format!("{}:tunnel_not_reestablished", phase::FAILED));
                write_state(&store, &state).await;
                reflip_to(&store, &target).await;
            } else {
                error!(
                    signal = "ota_tunnel_proof_no_rollback_target",
                    "update: no previous-good release to fall back to; this \
                     appliance is running an unreachable version and needs local \
                     intervention"
                );
                state.last_phase = Some(phase::FAILED.to_string());
                state.last_result = Some(format!("{}:tunnel_not_reestablished", phase::FAILED));
                write_state(&store, &state).await;
            }
            return;
        }
        // The new version is up and reachable — success.
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

        // Now — and only now, with the new version proven healthy — reap
        // stale release trees, keeping the running version and the rollback
        // target (`previous_good`). Deferring the prune until here is what
        // guarantees a failed apply can never delete the version we would roll
        // back to. Best-effort; a prune failure never disturbs the success.
        if let Some(prev) = state.previous_good.clone() {
            tokio::task::spawn_blocking(move || prune_stale_releases(&prev));
        }
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

/// Delete the staged tarball if it is there.
///
/// Nobody else does: the applier only reads the path, so before BUG-043 a
/// half-gigabyte release image sat in `/opt/nexus/staging` from one update to
/// the next, on the same filesystem the storage-safety loop is trying to keep
/// above its panic threshold.
#[cfg(target_os = "linux")]
fn discard_staged_tarball() {
    if let Err(e) = std::fs::remove_file(STAGED_TARBALL) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(error = %e, path = STAGED_TARBALL, "update: staged tarball discard failed");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn discard_staged_tarball() {}

/// Write `bytes` to `path`, replacing whatever is already there.
///
/// Deliberately **not** `cfg`-gated on Linux even though its only caller is:
/// the staging bug this exists to prevent (BUG-043) could not be regression-
/// tested at all while it lived inside a Linux-only function, because a
/// macOS `cargo check` never compiles that branch.
///
/// Unlink-then-write-then-rename, in that order, for two separate reasons:
/// `File::create` is `O_TRUNC` on an existing inode rather than a replace, so
/// without the unlink a leftover owned by another user (the applier runs as
/// root) fails `EACCES` forever on this fixed path; and the applier `tar
/// -xzf`s whatever it finds, so publishing by rename is what stops it ever
/// seeing a half-written archive.
// Only Linux calls it outside tests; the point of hoisting it out of the
// Linux-gated `install_release` is that everyone else still compiles it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn stage_tarball(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let part = path.with_extension("part");
    let _ = std::fs::remove_file(&part);
    let mut f = std::fs::File::create(&part)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&part, path)
}

/// Stage the verified tarball, then hand the entire privileged apply
/// sequence to the single root-owned applier.
///
/// The engine (as the unprivileged `nexus` user) only writes the staged
/// tarball; everything root — extract, deps/journald, symlink flip, restart —
/// happens inside `/usr/local/sbin/nexus-apply-release apply <version>`, the
/// one command the pinned sudoers rule grants. That release tarball carries a
/// single top-level directory named after the **bare** version (e.g.
/// `0.1.7/`), so the wrapper's `tar -C /opt/nexus/releases` lands the tree at
/// `RELEASES_DIR/<version>/`, exactly where it then points `current`.
#[cfg(target_os = "linux")]
async fn install_release(bytes: &[u8], version: &str) -> Result<(), &'static str> {
    // `staging_failed`, not `artifact_unavailable`: the bytes are already
    // downloaded and digest-verified by this point, so reusing the download
    // code here sends whoever reads the failure to the wrong side of the
    // tunnel entirely. That misdirection is half of BUG-043.
    stage_tarball(std::path::Path::new(STAGED_TARBALL), bytes).map_err(|e| {
        warn!(error = %e, path = STAGED_TARBALL, "update: staging the release tarball failed");
        "staging_failed"
    })?;

    // Hand the whole privileged sequence to the single applier. It extracts,
    // applies the release's declared apt deps + journald cap (best-effort,
    // WITHOUT pruning — the rollback target must survive a failed apply),
    // flips `current`, and restarts the unit. A non-zero exit means the
    // version did NOT take effect; the caller rolls the persisted version back.
    let status = std::process::Command::new("sudo")
        .arg("-n")
        .args(apply_release_argv("apply", version))
        .status();
    match status {
        Ok(s) if s.success() => {
            info!(
                version,
                "update: applier `apply` requested restart; awaiting SIGTERM"
            );
            Ok(())
        }
        // `ExitStatus::code() == None` means the applier died by signal. Its
        // last act is `systemctl restart nexus-engine`, and the applier runs
        // inside THIS unit's cgroup (we spawned it), so systemd's stop job
        // routinely SIGTERMs it before it can exit 0 — a successful apply
        // that merely could not report itself. `current` already pointing at
        // the target is proof the flip landed.
        Ok(s) if s.code().is_none() && current_release_is(version) => {
            info!(
                version,
                "update: applier signalled by the restart it requested; \
                 `current` already points at the target — treating as applied"
            );
            Ok(())
        }
        Ok(s) => {
            warn!(code = ?s.code(), "update: `nexus-apply-release apply` exited non-zero");
            Err("apply_failed")
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `nexus-apply-release apply`");
            Err("apply_failed")
        }
    }
}

/// True when `/opt/nexus/current` resolves to a release directory named
/// exactly `version`.
///
/// The symlink and its parents are world-readable, so the unprivileged engine
/// can check the applier's work without any privileged help.
#[cfg(target_os = "linux")]
fn current_release_is(version: &str) -> bool {
    std::fs::read_link(CURRENT_LINK).is_ok_and(|target| release_dir_matches(&target, version))
}

/// True when the final component of a `current` symlink target names exactly
/// `version` (the applier lays releases out as `<releases_dir>/<version>/`).
#[cfg(any(target_os = "linux", test))]
fn release_dir_matches(link_target: &std::path::Path, version: &str) -> bool {
    link_target
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == version)
}

/// Re-point `/opt/nexus/current` to the named (already-on-disk) release and
/// restart the unit — the rollback path. Delegates to the same applier in
/// `reflip` mode (no tarball, no deps).
#[cfg(target_os = "linux")]
async fn flip_and_restart(version: &str) -> Result<(), &'static str> {
    let status = std::process::Command::new("sudo")
        .arg("-n")
        .args(apply_release_argv("reflip", version))
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        // Same self-inflicted signal as the `apply` path: the reflip's own
        // `systemctl restart` tears down the cgroup the applier runs in.
        Ok(s) if s.code().is_none() && current_release_is(version) => {
            info!(
                version,
                "update: reflip applier signalled by the restart it requested; \
                 `current` already points at the target — treating as applied"
            );
            Ok(())
        }
        Ok(s) => {
            warn!(code = ?s.code(), "update: `nexus-apply-release reflip` exited non-zero");
            Err("rollback_also_failed")
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `nexus-apply-release reflip`");
            Err("rollback_also_failed")
        }
    }
}

/// Preflight the privileged surface: confirm sudo authorises the pinned
/// applier NOPASSWD before we download or stage anything. `sudo -n -l <cmd>`
/// exits 0 iff the invoking user may run `<cmd>` without a password; a drifted
/// or missing sudoers file makes it non-zero (and, with `-n`, it never blocks
/// on a password prompt). Returns `privsurface_drift` on any failure so the
/// terminal status names the real, operator-actionable cause.
#[cfg(target_os = "linux")]
fn preflight_privileged() -> Result<(), &'static str> {
    // Pass representative args so the probe matches the sudoers pattern, which
    // is `nexus-apply-release *` (a trailing arg is required). `sudo -l` only
    // CHECKS authorisation against sudoers — it never executes the wrapper —
    // so the placeholder mode/version are inert.
    let status = std::process::Command::new("sudo")
        .args(["-n", "-l", APPLY_RELEASE_WRAPPER, "preflight", "probe"])
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            warn!(
                code = ?s.code(),
                wrapper = APPLY_RELEASE_WRAPPER,
                "update: preflight failed — sudo does not authorise the OTA applier (sudoers drift?)"
            );
            Err("privsurface_drift")
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn `sudo -n -l` preflight");
            Err("privsurface_drift")
        }
    }
}

/// Best-effort post-health reap of stale release trees, keeping only the
/// running version and the rollback target (`previous_good`, passed as the
/// argument). Deferred until AFTER the new version boots healthy so a failed
/// apply can never delete the rollback target. Never fails the caller.
#[cfg(target_os = "linux")]
fn prune_stale_releases(keep_version: &str) {
    match std::process::Command::new("sudo")
        .arg("-n")
        .args(apply_release_argv("prune", keep_version))
        .status()
    {
        Ok(s) if s.success() => {
            info!(keep = keep_version, "update: post-health prune complete");
        }
        Ok(s) => {
            warn!(code = ?s.code(), "update: post-health prune exited non-zero (non-fatal)");
        }
        Err(e) => {
            warn!(error = %e, "update: failed to spawn post-health prune (non-fatal)");
        }
    }
}

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

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
fn preflight_privileged() -> Result<(), &'static str> {
    Err("unsupported_platform")
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
fn prune_stale_releases(_keep_version: &str) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn release_dir_matches_exact_version_component() {
        let link = std::path::Path::new("/opt/nexus/releases/0.1.185");
        assert!(release_dir_matches(link, "0.1.185"));
        // A prefix is not a match — 0.1.18 must not satisfy 0.1.185.
        assert!(!release_dir_matches(link, "0.1.18"));
        assert!(!release_dir_matches(link, "0.1.180"));
    }

    #[test]
    fn release_dir_matches_tolerates_trailing_slash_and_rejects_junk() {
        assert!(release_dir_matches(
            std::path::Path::new("/opt/nexus/releases/0.1.185/"),
            "0.1.185"
        ));
        assert!(!release_dir_matches(std::path::Path::new("/"), "0.1.185"));
        assert!(!release_dir_matches(std::path::Path::new(""), "0.1.185"));
    }

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

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_release_argv_is_stable_and_pinned() {
        // The applier path is pinned outside the nexus-writable tree, and the
        // argv is exactly [wrapper, mode, version] for every mode — this is
        // what the frozen sudoers wildcard (`nexus-apply-release *`) matches.
        assert_eq!(
            apply_release_argv("apply", "0.1.171"),
            ["/usr/local/sbin/nexus-apply-release", "apply", "0.1.171"]
        );
        assert_eq!(
            apply_release_argv("reflip", "0.1.170"),
            ["/usr/local/sbin/nexus-apply-release", "reflip", "0.1.170"]
        );
        assert_eq!(
            apply_release_argv("prune", "0.1.170"),
            ["/usr/local/sbin/nexus-apply-release", "prune", "0.1.170"]
        );
    }

    /// BUG-043. A staged tarball left behind by a previous update is owned by
    /// whoever wrote it — and the applier runs as root, so that is not always
    /// the `nexus` user the engine runs as. `File::create` opens the existing
    /// inode (`O_TRUNC`) instead of replacing it, so before the fix this
    /// failed `EACCES` on a fixed path, forever, and reported it as
    /// `artifact_unavailable`.
    ///
    /// Mode `0o444` stands in for "not writable by this process". It does not
    /// constrain root, so a root test runner cannot set the scenario up at all
    /// and the test bails out rather than passing for the wrong reason.
    #[cfg(unix)]
    #[test]
    fn stage_tarball_replaces_a_target_this_process_cannot_write() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");

        // Control: confirm the mode really does block a plain create for this
        // runner, so a green result below means the unlink did the work.
        let decoy = dir.path().join("decoy");
        std::fs::write(&decoy, b"x").unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::File::create(&decoy).is_ok() {
            return; // running as root; 0o444 does not deny us anything
        }

        let target = dir.path().join("update.tar.gz");
        std::fs::write(&target, b"stale release from the last update").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();

        stage_tarball(&target, b"the new release").expect("staging must replace the leftover");
        assert_eq!(std::fs::read(&target).unwrap(), b"the new release");
    }

    /// The applier `tar -xzf`s whatever sits at the pinned path, so staging
    /// must never leave its scratch file behind for a later run to trip over.
    #[test]
    fn stage_tarball_creates_the_parent_and_leaves_no_scratch_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("staging").join("update.tar.gz");

        stage_tarball(&target, b"release bytes").expect("staging into a fresh dir must work");

        assert_eq!(std::fs::read(&target).unwrap(), b"release bytes");
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "update.tar.gz")
            .collect();
        assert!(
            leftovers.is_empty(),
            "scratch files left behind: {leftovers:?}"
        );
    }
}
