// @generated — DO NOT EDIT BY HAND
// Regenerate with `cargo xtask gen-proto` from proto/v1.json.
//
// Source schema: Nexus edge↔cloud wire protocol
// Canonical schema for v1 of the wire envelope. Message kinds: heartbeat, heartbeat_ack, alert, alert_ack, clip_replicated, clip_replicated_ack, entitlement_update, rpc_call, rpc_response, close_session, camera_roster, camera_roster_ack, entity_sighting. HUMAN-EDITED source of truth. Rust types live in proto/generated/rust/v1.rs; TypeScript zod schemas in proto/generated/ts/v1.ts. `cargo xtask gen-proto` regenerates both; CI fails if they're stale.

use serde::{Deserialize, Serialize};

/// Structural shape of the JWT body inside ActorTokenJwt. Not transmitted standalone — included in this schema so codegen produces a typed verifier struct on the edge side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorTokenClaims {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_label: Option<String>,
    pub severity: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_blob_url: Option<String>,
    pub ts: Timestamp,
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

/// Cloud → Edge. Push triggered by Stripe webhook or initial enrollment. Edge persists + applies immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitlementUpdatePayload {
    /// Compact JWS. Verified with the entitlement public key bundled at enrollment.
    pub jwt: String,
}

/// Edge → Cloud. Phase 5 (additive on v=1). Appearance-embedding sighting for the identity-graph linker. Sent at first-detection per stable track + every 5 s while alive. `embedding_b64` is base64 of little-endian float32[embedding_dim]; allowed `embedding_model` values are `dinov2-s-v1` (384 dims) and `osnet-x1.0-v1` (512 dims). `entity_local_id` is the engine's per-track UUIDv7 — the cloud assigns the cross-camera `entity_global_id` via the pgvector linker. `bbox` is in the supervisor frame; `frame_w/frame_h` carry those dims so the cloud can scale to native MP4 resolution when overlaying. **Hard invariant per REPO_BOUNDARY R9:** appearance embeddings only; the gateway rejects envelopes whose `embedding_model` matches a face-recognition model name (`AdaFace`, `ArcFace`, `InsightFace`, `Buffalo`, `FaceNet`, `SphereFace`, `CosFace`, `MagFace`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySightingPayload {
    /// [x, y, w, h] absolute pixel coords in the supervisor frame (see `frame_w` / `frame_h`).
    pub bbox: Vec<u64>,
    /// Per-core integer id (matches cameras.edge_camera_id).
    pub camera_id: u64,
    /// Detector confidence for the track at this sighting.
    pub confidence: f64,
    /// Base64 of `float32[embedding_dim]` in little-endian. Length must equal `ceil(4 * embedding_dim / 3) * 4` after padding.
    pub embedding_b64: String,
    /// Must agree with `embedding_model`: 384 for `dinov2-s-v1`, 512 for `osnet-x1.0-v1`. Cloud rejects with `embedding_dim_mismatch` otherwise.
    pub embedding_dim: i64,
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

/// Cloud → Edge. Reply to a heartbeat. May hint at cert rotation after day 75.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatAckPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_rotate: Option<HeartbeatAckPayloadCertRotate>,
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

pub type Uuid = String;

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
}

/// One WebSocket text frame on the wire. See the schema header for invariants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub meta: EnvelopeMeta,
    #[serde(flatten)]
    pub body: EnvelopeBody,
}
