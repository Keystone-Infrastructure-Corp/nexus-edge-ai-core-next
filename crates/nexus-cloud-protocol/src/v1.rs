// @generated — DO NOT EDIT BY HAND
// Regenerate with `cargo xtask gen-proto` from proto/v1.json.
//
// Source schema: Nexus edge↔cloud wire protocol
// Canonical schema for v1 of the wire envelope. Message kinds: heartbeat, heartbeat_ack, alert, alert_ack, clip_replicated, clip_replicated_ack, entitlement_update, rpc_call, rpc_response, close_session, camera_roster, camera_roster_ack, entity_sighting, entity_sighting_batch, diag_collect, diag_ready, core_state_hashes, model_catalog, update_assignment, update_cancel, update_rollback, update_progress. HUMAN-EDITED source of truth. Rust types live in proto/generated/rust/v1.rs; TypeScript zod schemas in proto/generated/ts/v1.ts. `cargo xtask gen-proto` regenerates both; CI fails if they're stale.

use serde::{Deserialize, Serialize};

/// Structural shape of the JWT body inside ActorTokenJwt. Not transmitted standalone — included in this schema so codegen produces a typed verifier struct on the edge side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorTokenClaims {
    /// Phase 7.5 (additive). Present only when the human triggering the mutation belongs to a different org than the targeted core (a CMS operator acting on a monitored org via the fleet-settings hierarchy). Carries the acting org's UUID; org_id keeps the target (monitored) org's UUID. The token is still signed with the single global Ed25519 key, so engine verification is unchanged — it treats this as an opaque audit-only string. See WIRE_PROTOCOL.md §11.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_org_id: Option<Uuid>,
    pub aud: String,
    pub core_id: Uuid,
    /// MUST be iat + 30 s.
    pub exp: UnixSeconds,
    pub http_method: String,
    pub iat: UnixSeconds,
    /// https://entitlement.nexus.example
    pub iss: String,
    pub jti: Uuid,
    pub org_id: Uuid,
    /// Exact match against rpc_call.payload.path.
    pub path: String,
    pub role: String,
    /// User UUID, or `system:<svc-name>`.
    pub sub: String,
}

/// Compact JWS (Ed25519). Header `{alg:"EdDSA", kid:<keyvault-key-id>}`. Claims per WIRE_PROTOCOL.md §11.2; engine verifies before applying any state-mutating rpc_call. See ActorTokenClaims for the structural shape of the inner JWT body.
pub type ActorTokenJwt = String;

/// Cloud → Edge. permanent_failure tells the edge outbox to mark the row suppressed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertAckPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub status: String,
}

/// Edge → Cloud. AlertEvent — shape mirrors nexus-types/src/lib.rs on the edge side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertPayload {
    /// Phase 21.2 — clip pre-attached on edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_history: Option<bool>,
    /// [x, y, w, h] normalised 0..1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Vec<f64>>,
    /// Per-core integer id (matches cameras.edge_camera_id).
    pub camera_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_blob_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Core-local id. Dedup key on cloud INSERT (cores.id × edge_event_id).
    pub edge_event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_rule_id: Option<String>,
    /// Phase 8.2. Edge-populated pointer to the urgent-upload evidence clip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_clip_ref: Option<EvidenceClipRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_label: Option<String>,
    pub severity: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_blob_url: Option<String>,
    pub ts: Timestamp,
    /// Phase 8.2. Verifier confidence 0..1. Cloud-written; edge always omits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_confidence: Option<f64>,
    /// Phase 8.2. Human-readable VLM verdict headline. Cloud-written; edge always omits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_description: Option<String>,
    /// Phase 8.2. Structured-evidence map from the verifier. Cloud-written; edge always omits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_evidence: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// Phase 8.2 (additive). Edge fires `candidate`; cloud verifier overwrites. Omitted by N-1 edges → treated as `verified`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_state: Option<VerificationState>,
}

/// Cloud → Edge. Reply to a camera_roster. `permanent_failure` tells the edge to stop retrying this revision (e.g. malformed metadata). `accepted_revision` is echoed back so the edge can drop the outbox entry and advance its high-water-mark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraRosterAckPayload {
    pub accepted_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub status: String,
}

/// One camera in a CameraRosterPayload. Per AGENTS.md Rule 6 this struct MUST NOT carry any per-camera credential (RTSP URL with embedded creds, ONVIF password, etc.) — those stay edge-resident. Metadata only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraRosterEntry {
    /// Source video codec. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Per-core integer id. Cloud uses this as the dedup key together with (core_id).
    pub edge_camera_id: u64,
    /// Operator-controlled on the edge.
    pub enabled: bool,
    /// Source backend on the edge. Identifies how the edge ingests this camera but reveals no credential material.
    pub kind: String,
    /// Active detector kind on this camera (e.g. "yolo", "yolo_world", "yoloe", "mock"). Optional metadata; the wire shape doesn't constrain the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_kind: Option<String>,
    pub name: String,
    /// Edge-observed liveness in the last frame-source pass. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    /// Source resolution as negotiated. Optional — unknown for virtual/file kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<CameraRosterEntryResolution>,
    /// Monotonic counter incremented on every local mutation. Used by the cloud to ignore out-of-order rosters and (Phase D) for optimistic-concurrency on cloud-pushed config changes.
    pub revision: u64,
    /// Opaque key/value labels. Free-form; the cloud doesn't interpret them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
    /// Edge wall-clock at the latest mutation that produced this revision.
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraRosterEntryResolution {
    pub height: u64,
    pub width: u64,
}

/// Edge → Cloud. Full snapshot of the camera roster on this core. Sent (a) on tunnel-up after enrollment, (b) immediately after any local camera CRUD (POST/PATCH/DELETE on /api/v1/cameras), and (c) opportunistically on the heartbeat cadence if the roster has changed since the last ack. The cloud treats this as authoritative — cameras present here are upserted into `cameras`; cameras previously known for this core but absent here are soft-deleted (`cameras.deleted_at = now()`). No credential material crosses the tunnel (AGENTS.md Rule 6). Phase A of the cloud-managed CRUD wedge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraRosterPayload {
    /// Full list. Empty array is meaningful (= all cameras removed).
    pub cameras: Vec<CameraRosterEntry>,
    /// Monotonic per-core counter. Bumped on every local CRUD. Cloud drops envelopes whose revision is <= the last successfully-ingested one (out-of-order delivery defense).
    pub roster_revision: u64,
    /// Edge wall-clock at snapshot. Diagnostics only.
    pub snapshot_at: Timestamp,
}

/// Cloud → Edge. Reply to clip_replicated. Mirrors AlertAckPayload semantics — permanent_failure tells the edge outbox to mark the row suppressed (e.g. unknown camera_id, signature_invalid).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipReplicatedAckPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub status: String,
}

/// Edge → Cloud. Sent after the edge successfully PUTs a closed motion clip to the SAS URL issued by POST /v1/cores/me/blob-sas. Cloud INSERTs a row into the `clips` table; the `(core_id, edge_clip_id)` UNIQUE index makes the handler idempotent under outbox replay (ARCHITECTURE.md §3.6, §8.5). Phase 2.3 / Phase 2.8.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipReplicatedPayload {
    /// Phase 2.9 / ARCHITECTURE.md §21.2 — clip was already on the edge before this core enrolled. Cloud renders an `imported` badge and suppresses notify-svc fan-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_history: Option<bool>,
    /// Blob URL of the freshly-uploaded clip MP4. Always the SAS-issuing host; cloud strips the SAS query before storing.
    pub blob_url: String,
    /// Per-core integer id (matches cameras.edge_camera_id).
    pub camera_id: u64,
    /// Video codec inside the MP4. Optional; defaults to h264.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Container format. Optional; defaults to mp4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub duration_ms: u64,
    /// Core-local clip id. Dedup key on cloud INSERT (cores.id × edge_clip_id).
    pub edge_clip_id: String,
    /// Hex-encoded streaming SHA-256 of the clip bytes computed during MP4 write on the edge. Cloud stores in clips.sha256 and the Phase 6.17 integrity sweep verifies against Blob on read. Pairs with x-ms-blob-content-md5 set during PUT (ARCHITECTURE.md §8.5).
    pub sha256_hex: String,
    /// Final on-disk byte count after MP4 close. Used for tariff accounting + cold-storage cost projection.
    pub size_bytes: u64,
    pub started_at: Timestamp,
    /// Optional JPEG thumbnail blob URL. Same SAS-issuing host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_blob_url: Option<String>,
}

/// Cloud → Edge. Server-initiated clean disconnect. Edge reconnects immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseSessionPayload {
    pub reason: String,
}

/// Edge → Cloud. Phase 7.5.5 (additive on v=1). Periodic + change-triggered digest of the edge's current canonical configuration state, one SHA-256 per fleet-settings category (rules, text_prompts, visual_prompts, detector_config, delivery_settings). Each hash is the lower-hex SHA-256 of the canonical JSON (recursively sorted object keys, arrays in document order, no insignificant whitespace) the edge derives from its local config for that category — byte-identical canonicalisation to the cloud's `fleet/effective` projection, so the cloud compares reported vs projected to surface configuration drift without round-tripping the full config. The edge emits this on startup and (debounced) whenever local config mutates (config.changed / delivery.settings.changed bus signals). A category hash is omitted when that category has no state on the edge. The cloud upserts these into `core_runtime_hashes` keyed on core_id; the edge-gateway resolves org_id from the cores FK. Additive: pre-7.5.5 cloud peers ignore this kind, pre-7.5.5 edges never emit it. No ack — fire-and-forget; the cloud reconciler is the recovery path when the tunnel is down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreStateHashesPayload {
    /// Edge wall-clock when the hashes were computed. Diagnostics only — the cloud stores this as core_runtime_hashes.computed_at; reported_at is set server-side on receipt.
    pub computed_at: Timestamp,
    /// Lower-hex SHA-256 of the canonical JSON of the delivery settings singleton. Omitted when delivery settings are unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_settings_sha256: Option<String>,
    /// Lower-hex SHA-256 of the canonical JSON of the per-camera detector model overrides. Omitted when no camera overrides the detector model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_config_sha256: Option<String>,
    /// Lower-hex SHA-256 of the canonical JSON of the edge's full rule set. Omitted when the edge has no rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_sha256: Option<String>,
    /// Lower-hex SHA-256 of the canonical JSON of the per-camera detector text prompts. Omitted when no camera has text prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_prompts_sha256: Option<String>,
    /// Lower-hex SHA-256 of the canonical JSON of the per-camera attached visual prompts. Omitted when no visual prompts are attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_prompts_sha256: Option<String>,
}

/// One detector kind the engine knows how to build, with the prompt vocabulary it actually resolves at boot. The console renders prompt suggestions from these entries instead of a hand-maintained mirror, so closed-vocab kinds advertise the exact label set the detector emits and open-vocab kinds advertise the baked prompt vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorVocabEntry {
    /// Canonical detector kind name (e.g. "yolo", "yolo_world", "yoloe", "yoloe_promptfree", "yoloe_visual", "classifier_ensemble", "mock"). Matches CameraRosterEntry.model_kind values.
    pub kind: String,
    /// True iff the engine's router already built a layer for this kind at boot. The console may surface a restart-engine-to-activate hint for unloaded kinds chosen as a camera override.
    pub loaded: bool,
    /// Optional human-readable note describing the detector, shown beneath the suggestion strip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// True for open-vocab detectors (yolo_world / yoloe) that accept arbitrary user prompt strings but only emit labels from the baked vocabulary; false for closed-vocab detectors that emit a fixed label set. The console renders a free-text+suggestion box when true and a fixed chip strip when false.
    pub open_vocab: bool,
    /// Every label/prompt this detector kind is known to emit (closed-vocab) or accept (open-vocab baked vocabulary). Empty for detectors whose vocabulary is unknown (e.g. mock / visual-prompt).
    pub prompts: Vec<String>,
}

/// Cloud → Edge. Phase 7.0a. Operator-initiated diagnostic-tarball collection request. The cloud mints a single-use Azure Blob SAS PUT URL scoped to a unique `diag_id` path under the `diagnostics` container, then pushes this envelope down the existing edge-gateway tunnel. The edge tarballs (logs ∪ scrubbed-config ∪ telemetry snapshot ∪ optional sqlite dump), `reqwest::put`s the bytes to `sas_put_url`, then emits `diag_ready` (status=uploaded|failed) with the same `diag_id`. Operator-facing endpoints in the console resolve diag_id → fresh download SAS URL via Blob listBlobs. State-mutating on the edge (creates a tarball, makes outbound network calls), so `actor_token` is REQUIRED and the edge verifies signature + path-binding before any local work. The 30-second actor_token TTL is honoured for receipt-validation only; the tarball+upload work continues past the token expiry (the SAS URL has its own independent expiry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagCollectPayload {
    /// REQUIRED. JWT proves cloud-side operator provenance. Edge verifies signature, `aud=nexus-edge-rpc`, `path=diag_collect` (logical RPC path), and `exp > now`. State-mutating; same posture as `RpcCallPayload`.
    pub actor_token: ActorTokenJwt,
    /// Cloud-minted UUIDv7. Echoed back in the matching `diag_ready` envelope. Also the canonical key the cloud DB uses to track this collection request.
    pub diag_id: Uuid,
    /// SAS URL hard expiry. Edge gives up + emits `diag_ready{status=failed, error_code=sas_expired}` if it can't complete the upload by this time. Typical value is `now + 15 min`.
    pub expires_at: Timestamp,
    /// Optional. If true, include the live edge sqlite state DB in the tarball (operator opt-in — the DB may contain PII like camera names and clip metadata). Default false. If true but the edge can't snapshot the DB (file lock, disk full), the tarball is uploaded WITHOUT the sqlite file and `diag_ready` carries `error_code=include_sqlite_unavailable` with `status=uploaded` (partial success — operator can still inspect logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_sqlite: Option<bool>,
    /// Optional cap on the uncompressed tarball size, defense against runaway disk pressure. Default 52428800 (50 MiB). If the assembled tarball would exceed this, the edge truncates the noisiest input (typically logs) and adds a `_TRUNCATED.txt` marker file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Single-use HTTPS PUT URL. Must include `sp=cw` (Create+Write) and `sr=b` (blob scope) per the cloud SAS-minting contract. Edge MUST NOT log this URL (it carries the SAS token).
    pub sas_put_url: String,
}

/// Edge → Cloud. Phase 7.0a. Completion notification for a `diag_collect`. Routed by the cloud orchestrator to the diag-tracking row keyed on `diag_id` (NOT the envelope `in_reply_to` — diag_collect is a fire-and-confirm pattern, not a sync RPC, because tarball assembly + upload can take 30+ seconds well past the actor_token's TTL). Cloud uses (status, error_code) to set the diag row's final state; the SPA polls that row to surface progress + the download button.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagReadyPayload {
    /// Echoes back the `diag_collect.diag_id`. The cloud orchestrator uses this — not envelope.in_reply_to — to bind to the open diag row.
    pub diag_id: Uuid,
    /// Failure mode. Required when status=failed; ALSO present with status=uploaded for partial successes (currently only `include_sqlite_unavailable` — the tarball uploaded but the optional sqlite snapshot couldn't be included). `actor_token_invalid` arrives via this kind rather than as an envelope-level error so the cloud orchestrator can mark the diag row instead of just dropping the envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Operator-facing detail. Optional; safe to display verbatim in the SPA (the edge MUST scrub paths/secrets before populating).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Uncompressed tarball size (informational). Required when status=uploaded; omitted when status=failed (no tarball was assembled or the upload itself failed). Compressed size is unavailable — the edge streams `tar | gzip` straight into reqwest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// `uploaded` = the bytes are in Blob and the operator can download them. `failed` = no usable bytes are in Blob; the operator may retry by calling the collect endpoint again.
    pub status: String,
}

/// Cloud → Edge. Push triggered by Stripe webhook or initial enrollment. Edge persists + applies immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitlementUpdatePayload {
    /// Compact JWS. Verified with the entitlement public key bundled at enrollment.
    pub jwt: String,
}

/// Edge → Cloud. Phase M_PERF_CROWD A3 (additive on v=1). Batched form of `entity_sighting`: an array of up to 32 `EntitySightingPayload` items, intended to amortise WSS frame overhead and JSON dictionary warmup over a 100ms drain window. Cloud routing is identical to `entity_sighting` — each item is validated and inserted independently; a rejection of one item does NOT discard the batch. Edges MUST only emit this kind when the cloud advertised `entity_sighting_batch` in `HeartbeatAckPayload.cloud_capabilities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySightingBatchPayload {
    /// 1..32 sightings. Order is preserved on insert.
    pub items: Vec<EntitySightingPayload>,
}

/// Edge → Cloud. Phase 5 (additive on v=1). Appearance-embedding sighting for the identity-graph linker. Sent at first-detection per stable track + every 5 s while alive. `embedding_b64` is base64 of little-endian float32[embedding_dim]; allowed `embedding_model` values are `dinov2-s-v1` (384 dims) and `osnet-x1.0-v1` (512 dims). `entity_local_id` is the engine's per-track UUIDv7 — the cloud assigns the cross-camera `entity_global_id` via the pgvector linker. `bbox` is in the supervisor frame; `frame_w/frame_h` carry those dims so the cloud can scale to native MP4 resolution when overlaying. **Hard invariant per REPO_BOUNDARY R9:** appearance embeddings only; the gateway rejects envelopes whose `embedding_model` matches a face-recognition model name (`AdaFace`, `ArcFace`, `InsightFace`, `Buffalo`, `FaceNet`, `SphereFace`, `CosFace`, `MagFace`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySightingPayload {
    /// [x, y, w, h] absolute pixel coords in the supervisor frame (see `frame_w` / `frame_h`).
    pub bbox: Vec<u64>,
    /// Per-core integer id (matches cameras.edge_camera_id).
    pub camera_id: u64,
    /// Phase 6.8 (additive on v=1). Optional detector class name (raw detector vocabulary, e.g. `person`, `car`, `truck`, `bicycle`). Cloud normalises into `entities.kind` via the gateway-side allowlist: `person` → `person`; `car|truck|motorcycle|bus|bicycle|vehicle` → `vehicle`; anything else (or omitted) → `unknown`. Pre-6.8 edges omit this field and continue to produce `unknown`-kind entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_label: Option<String>,
    /// Detector confidence for the track at this sighting.
    pub confidence: f64,
    /// Base64 of `float32[embedding_dim]` in little-endian by default. When `embedding_dtype` is `"f16"`, length is `2 * embedding_dim` bytes (IEEE-754 binary16 little-endian) before base64 padding.
    pub embedding_b64: String,
    /// Must agree with `embedding_model`: 384 for `dinov2-s-v1`, 512 for `osnet-x1.0-v1`. Cloud rejects with `embedding_dim_mismatch` otherwise.
    pub embedding_dim: i64,
    /// Phase M_PERF_CROWD A1: numeric type of `embedding_b64`. Omitted/null/`"f32"` mean little-endian float32 (legacy / pre-Phase-A engines). `"f16"` means little-endian IEEE-754 binary16 — the cloud expands to FP32 on ingest. Edges MUST only emit `"f16"` when the cloud advertised `embedding_dtype_f16` in `HeartbeatAckPayload.cloud_capabilities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dtype: Option<String>,
    /// Free-form on the wire but constrained to the cloud's allowlist; unknown values are rejected with `embedding_model_unknown`. Face-recognition model names are rejected with `embedding_face_model_rejected` (REPO_BOUNDARY R9).
    pub embedding_model: String,
    /// Stable per-track id (engine UUIDv7). Two sightings with the same `(core_id, entity_local_id)` are the same track on the edge; the cloud uses it as the dedup key and to follow a track across re-sends.
    pub entity_local_id: String,
    /// Supervisor frame height (typically 540 for RTSP sources).
    pub frame_h: u64,
    /// Supervisor frame width (typically 960 for RTSP sources — see `RTSP_SOURCE_FRAME_WIDTH` in the engine).
    pub frame_w: u64,
    /// True for the first envelope emitted for this (core_id, entity_local_id); false for periodic re-sends.
    pub is_first_sighting: bool,
    /// Edge wall-clock of the FIRST frame the track was observed on.
    pub started_ts: Timestamp,
    /// Edge wall-clock of THIS sighting (== started_ts for the first envelope, > started_ts for periodic re-sends).
    pub ts: Timestamp,
}

/// Phase 8.2 — pointer to the urgent-upload clip the cloud verifier samples keyframes from. Edge populates this when an alert fires; sas_url is dereferenceable by the time the alert reaches the verifier (8.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceClipRef {
    /// Clip-relative end of the evidence window, ms.
    pub frame_end_ms: u64,
    /// Clip-relative start of the evidence window, ms.
    pub frame_start_ms: u64,
    /// Signed Blob SAS URL for the evidence clip MP4.
    pub sas_url: String,
}

/// Cloud → Edge. Reply to a heartbeat. May hint at cert rotation after day 75. Phase M_PERF_CROWD A: optionally carries `cloud_capabilities` so the edge can gate batched / FP16 sighting envelopes on cloud-side support.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatAckPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_rotate: Option<HeartbeatAckPayloadCertRotate>,
    /// Phase M_PERF_CROWD A: advertised gateway capabilities. Edge enables a feature only if the corresponding tag is present. Defined tags so far: `entity_sighting_batch` (gateway routes batched envelopes), `embedding_dtype_f16` (gateway decodes FP16 embeddings). Unknown tags are ignored; missing field is treated identically to an empty array (pre-Phase-A gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_capabilities: Option<Vec<String>>,
    pub server_ts: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatAckPayloadCertRotate {
    pub reason: String,
}

/// Edge → Cloud. Sent every 30 s. Minimum health snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatPayload {
    /// Phase 1.15: edge wall-clock for skew tracking (gateway writes EMA to cores.last_skew_ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_ts_unix_ms: Option<u64>,
    /// Phase C: operator-set engine display name (admin/server/identity). Gateway upserts into cores.name as a cache; UI renders this everywhere a core is listed. Omitted by pre-Phase-C edges; empty string is treated identically to omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub online_cameras: u64,
    pub queued_alerts: u64,
    /// Phase 7: OTA-update status block. Omitted by pre-Phase-7 edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseStatus>,
    /// Hardware tier from config/tiers/.
    pub tier: String,
    pub uptime_s: u64,
    /// Engine semver, e.g. "0.5.0".
    pub version: String,
}

/// Edge → Cloud. Additive on v=1. The detector-prompt vocabulary the engine actually resolved at boot — one DetectorVocabEntry per detector kind the engine knows how to build. Sent on tunnel-up and whenever the loaded model pack changes (rare — effectively per OTA / restart). The console renders prompt suggestions from this live data instead of a hand-maintained mirror of the engine's label map. Fire-and-forget — no ack in the v1 wire schema; the cloud upserts these into core_model_catalog keyed on core_id (org_id resolved from the cores FK), and the next report on reconnect is the recovery path. Pre-feature cloud peers ignore this kind; pre-feature edges never emit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogPayload {
    /// Edge wall-clock when the catalog was built (engine boot). Diagnostics only — the cloud stores this as core_model_catalog.computed_at; reported_at is set server-side on receipt.
    pub computed_at: Timestamp,
    /// inference.model.kind from the loaded config — the kind every camera that does NOT set a per-camera override runs against.
    pub default_kind: String,
    /// One entry per detector kind the engine knows how to build, regardless of whether the router currently has a layer for it (loaded distinguishes).
    pub kinds: Vec<DetectorVocabEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseStatus {
    pub channel: String,
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_attempt_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_result: Option<String>,
    pub recording_active: bool,
}

/// Cloud → Edge. Proxies an HTTP call to the edge's loopback admin API. State-mutating methods MUST carry actor_token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcCallPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_token: Option<ActorTokenJwt>,
    /// Optional request body. Base64 of the raw bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub method: String,
    /// Absolute path on the edge loopback admin API.
    pub path: String,
    /// Phase 1.16: propagated from HTTP Idempotency-Key for end-to-end dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
}

/// Shape of body on 4xx/5xx responses. See WIRE_PROTOCOL.md §4.3 for the full code catalogue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcErrorBody {
    pub error: String,
    pub message: String,
}

/// Edge → Cloud. Reply to an rpc_call. Uses envelope.in_reply_to to bind to the original.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcResponsePayload {
    pub body: serde_json::Value,
    pub status: u64,
}

pub type Timestamp = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceContext {
    /// 8-byte W3C span-id, hex-encoded.
    pub parent_span_id: String,
    /// 16-byte W3C trace-id, hex-encoded.
    pub trace_id: String,
}

pub type UnixSeconds = u64;

/// Cloud → Edge. Phase 7 (additive on v=1). The update-orchestrator-svc assigns a signed release to a core when its deterministic cohort bucket is reached for the active rollout (or immediately on operator Force). The engine's in-process update handler re-verifies the cloud's Ed25519 manifest `signature` against the public key embedded in the engine binary (NEXUS_RELEASE_SIGNING_PUBKEY_V1) BEFORE downloading the tarball, then re-verifies `artifact_sha256` over the downloaded bytes. Dispatch is fire-and-forget — progress flows back asynchronously via `update_progress`, never as a sync RPC reply. Routine cohort assignments rely on the manifest signature as the trust boundary; operator `force` and orchestrator-issued downgrades additionally carry a system/human actor_token at the envelope level (see WIRE_PROTOCOL §11.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAssignmentPayload {
    /// Optional. Present ONLY when `force` or `allow_downgrade` is set (operator force / orchestrator-issued downgrade); routine cohort assignments omit it and rely on the manifest signature as the trust boundary. When present the edge verifies signature, `aud=nexus-edge-rpc`, `path=update_assignment` (logical RPC path), `core_id`, and `exp > now`, and enforces the system-sub method whitelist for `system:<svc>` subjects (WIRE_PROTOCOL §11.4). Additive on v=1 — pre-feature edges ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_token: Option<ActorTokenJwt>,
    /// Optional. Permit installing a `target_version` older than the running version. Set true only by orchestrator-issued rollbacks/downgrades; refused otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_downgrade: Option<bool>,
    /// Lower-hex SHA-256 of the tarball bytes. The edge re-computes this over the downloaded artifact and aborts with `digest_mismatch` on any difference. Also bound into the signed manifest.
    pub artifact_sha256: String,
    /// HTTPS URL of the signed release tarball (GitHub Releases in v1; the trust model is host-agnostic so a Blob/SAS mirror is a future additive change). The edge downloads with reqwest only — bytes never transit the gateway.
    pub artifact_url: String,
    /// Cloud-minted UUID for this assignment. Echoed back in every `update_progress` envelope so the orchestrator can bind progress to the open update_history row.
    pub assignment_id: Uuid,
    /// Release channel this assignment is drawn from. The edge persists it as its assigned channel and reports it back in heartbeat.release.channel.
    pub channel: String,
    /// Optional. The edge holds the update until this wall-clock time (operator maintenance window). Omitted = apply now. Ignored when `force` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferral_until: Option<Timestamp>,
    /// Optional. Operator/orchestrator override that bypasses the maintenance window and any soak deferral. Force assignments are accompanied by an envelope-level actor_token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    /// Optional. URL of the full signed manifest JSON when it is not inlined. Omitted in v1 — the signature covers the canonical fields carried in this payload directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
    /// Base64-encoded Ed25519 signature over the canonical release manifest (version ∪ channel ∪ artifact_url ∪ artifact_sha256 ∪ min/max wire_v ∪ min_engine_version_to_apply). The engine re-verifies against NEXUS_RELEASE_SIGNING_PUBKEY_V1 before any download; failure → `update_progress.error = "failed:signature_invalid"`.
    pub signature: String,
    /// Engine semver to install, e.g. "0.6.0". The edge no-ops the assignment if it is already running this version (idempotent re-delivery).
    pub target_version: String,
}

/// Cloud → Edge. Phase 7 (additive on v=1). Idempotent abort of an in-progress assignment. The edge cancels if it has not yet passed the `restarting` phase; once the symlink flip + restart is committed the cancel is ignored (the new version will simply heartbeat and the orchestrator reconciles). No-op if no assignment is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCancelPayload {
    /// REQUIRED for the edge to honour the cancel. Operator-initiated cancels carry a human actor_token forwarded by the api-gateway. The edge verifies signature, `aud=nexus-edge-rpc`, `path=update_cancel` (logical RPC path), `core_id`, and `exp > now`, and enforces the system-sub method whitelist for `system:<svc>` subjects (WIRE_PROTOCOL §11.4). Kept out of `required` so the field is additive on v=1; the edge drops a cancel that arrives without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_token: Option<ActorTokenJwt>,
    /// The `update_assignment.assignment_id` to abort. Ignored if it does not match the edge's currently-active assignment.
    pub assignment_id: Uuid,
}

/// Edge → Cloud. Phase 7 (additive on v=1). Emitted by the engine's in-process update handler at each phase transition. Fire-and-forget — routed by the cloud on `assignment_id` (NOT envelope.in_reply_to; an update outlives any actor_token TTL and spans an engine restart). The orchestrator uses (phase, error) to advance the per-core update_history row and feed the rollout state machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProgressPayload {
    /// Echoes the `update_assignment.assignment_id`. Binds this progress event to the open update_history row.
    pub assignment_id: Uuid,
    /// Required when phase=failed. Format `failed:<code>` where code ∈ signature_invalid | digest_mismatch | recording_in_progress | artifact_unavailable | health_check_failed | rollback_also_failed. Surfaced verbatim in the console update history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional coarse progress percentage, primarily meaningful during `fetching_artifact`. Omitted for instantaneous phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<u64>,
    /// Current update phase. `restarting` is the last event the OLD binary emits; `verifying_health`/`success` are emitted by the NEW binary after it comes up (recovered from the persisted last_phase). `failed` is terminal for this attempt.
    pub phase: String,
}

/// Cloud → Edge. Phase 7 (additive on v=1). Forces re-install of the edge's locally-cached `previous_good` release. The previous-good directory is already on disk under /opt/nexus/releases/<version>/ and was Ed25519-verified when it was first installed, so a rollback carries NO artifact_url, NO sha256, and NO signature — the edge just flips the `current` symlink back to previous_good and restarts. Bypasses maintenance windows and implies allow_downgrade. Issued by the orchestrator on auto-halt or by an operator via the console; both paths carry an envelope-level system/human actor_token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRollbackPayload {
    /// REQUIRED for the edge to honour the rollback. Auto-halt rollbacks carry a `system:update-orchestrator` actor_token minted by entitlement-svc; operator rollbacks carry a human token forwarded by the api-gateway. The edge verifies signature, `aud=nexus-edge-rpc`, `path=update_rollback` (logical RPC path), `core_id`, and `exp > now`, and enforces the system-sub method whitelist for `system:<svc>` subjects (WIRE_PROTOCOL §11.4). Kept out of `required` so the field is additive on v=1; the edge drops a rollback that arrives without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_token: Option<ActorTokenJwt>,
    /// Operator-facing rollback reason (e.g. "auto-halt: alert volume collapsed", "operator rollback"). Recorded by the edge and surfaced in update_history.
    pub reason: String,
}

pub type Uuid = String;

/// Phase 8.2 — lifecycle of a behavior-verified alert. Edge fires `candidate`; cloud verifier advances it. A N-1 edge that omits the field is treated as `verified` (legacy rule-engine alerts predate the verifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Candidate,
    Pending,
    Verified,
    Dismissed,
    Review,
}

/// Envelope metadata — every field of [`Envelope`] except the
/// `kind`/`payload` discriminator, which is encoded by [`EnvelopeBody`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeMeta {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceContext>,
    pub ts: Timestamp,
    pub v: i64,
}

/// Tagged-union body of every [`Envelope`]. Serde writes `kind` +
/// `payload` as siblings of the envelope-meta fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EnvelopeBody {
    Heartbeat(HeartbeatPayload),
    HeartbeatAck(HeartbeatAckPayload),
    Alert(AlertPayload),
    AlertAck(AlertAckPayload),
    ClipReplicated(ClipReplicatedPayload),
    ClipReplicatedAck(ClipReplicatedAckPayload),
    EntitlementUpdate(EntitlementUpdatePayload),
    RpcCall(RpcCallPayload),
    RpcResponse(RpcResponsePayload),
    CloseSession(CloseSessionPayload),
    CameraRoster(CameraRosterPayload),
    CameraRosterAck(CameraRosterAckPayload),
    EntitySighting(EntitySightingPayload),
    EntitySightingBatch(EntitySightingBatchPayload),
    DiagCollect(DiagCollectPayload),
    DiagReady(DiagReadyPayload),
    CoreStateHashes(CoreStateHashesPayload),
    ModelCatalog(ModelCatalogPayload),
    UpdateAssignment(UpdateAssignmentPayload),
    UpdateCancel(UpdateCancelPayload),
    UpdateRollback(UpdateRollbackPayload),
    UpdateProgress(UpdateProgressPayload),
}

/// One WebSocket text frame on the wire. See the schema header for invariants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub meta: EnvelopeMeta,
    #[serde(flatten)]
    pub body: EnvelopeBody,
}
