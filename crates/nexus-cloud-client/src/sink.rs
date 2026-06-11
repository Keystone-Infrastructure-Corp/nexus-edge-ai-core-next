//! `CloudConsoleSink` — engine-facing sink that projects local
//! `AlertEvent`s into wire `Envelope::Alert` frames and hands them to
//! the [`crate::tunnel::TunnelHandle`].
//!
//! Phase 1.7 ships the projection function and the sink shell; the
//! engine wires the sink into its bus in Phase 1.11 alongside the
//! tunnel body.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nexus_cloud_protocol::v1::{
    AlertPayload, ClipReplicatedPayload, EntitySightingBatchPayload, EntitySightingPayload,
    Envelope, EnvelopeBody, EnvelopeMeta,
};
use uuid::Uuid;

use crate::tunnel::{TunnelError, TunnelHandle};

/// Edge-side projection of an alert as the sink expects it. The engine
/// already has a richer `AlertEvent` in [`nexus_types`]; this struct
/// names just the fields that travel over the wire.
#[derive(Debug, Clone)]
pub struct AlertProjection {
    /// Core-local id. Dedup key on cloud INSERT (`cores.id × edge_event_id`).
    pub edge_event_id: String,
    /// Wall-clock timestamp at which the event was detected.
    pub ts: DateTime<Utc>,
    /// Per-core integer id (matches `cameras.edge_camera_id`).
    pub camera_id: u64,
    /// 0..255 severity. The cloud-console rule mapper bins these into
    /// the SPA's visual severities.
    pub severity: u64,
    /// Edge rule id that fired, when known.
    pub edge_rule_id: Option<String>,
    /// Matched label (`"person"`, `"vehicle"`, …) when the rule was
    /// label-bound.
    pub matched_label: Option<String>,
    /// Detector confidence, when applicable.
    pub confidence: Option<f64>,
    /// Bounding box as `[x, y, w, h]` normalised 0..1.
    pub bbox: Option<Vec<f64>>,
    /// SAS URL of the snapshot blob, pre-uploaded by the storage layer.
    pub snapshot_blob_url: Option<String>,
    /// SAS URL of the clip blob, when the rule had clip-on.
    pub clip_blob_url: Option<String>,
    /// Phase 21.2 — set when the clip was pre-attached on edge.
    pub attached_history: Option<bool>,
}

/// Sink shell. Wraps any [`TunnelHandle`] impl so the engine can
/// substitute a mock tunnel in tests.
pub struct CloudConsoleSink {
    tunnel: Arc<dyn TunnelHandle>,
}

impl CloudConsoleSink {
    /// Build a sink around the given tunnel handle.
    #[must_use]
    pub fn new(tunnel: Arc<dyn TunnelHandle>) -> Self {
        Self { tunnel }
    }

    /// Publish one alert. The sink builds the envelope and hands it to
    /// the tunnel; persistence + retry on send-failure is the tunnel
    /// implementation's job (the engine outbox sits in front of this).
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] from the underlying handle. Phase 1.7
    /// always surfaces [`TunnelError::NotImplemented`] from the default
    /// [`TunnelClient`](crate::tunnel::TunnelClient).
    pub async fn publish_alert(&self, alert: AlertProjection) -> Result<(), TunnelError> {
        let envelope = build_alert_envelope(alert);
        self.tunnel.send(envelope).await
    }

    /// Publish one cold-replicated clip. The sink builds the envelope
    /// from the edge-side projection and hands it to the tunnel.
    ///
    /// Phase 2 · Step 2.8 — paired with the cold replicator: after
    /// `Store::mark_cold_replicated` stamps the row, the replicator
    /// builds a [`ClipReplicatedProjection`] from the [`ClipRow`]
    /// plus the `PutReceipt::cold_url` returned by the backend, then
    /// fires this method via the shared [`crate::TunnelOutbox`].
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] from the underlying handle (e.g.
    /// `Disconnected` when the WSS tunnel is currently down — the
    /// replicator logs and continues; the polling backstop +
    /// future Phase 6.17 sweep reconcile any lost envelopes).
    pub async fn publish_clip_replicated(
        &self,
        clip: ClipReplicatedProjection,
    ) -> Result<(), TunnelError> {
        let envelope = build_clip_replicated_envelope(clip);
        self.tunnel.send(envelope).await
    }

    /// Publish one appearance-embedding sighting from the per-camera
    /// tracker. Phase 5.6 4c-i: pairs with the cloud-side
    /// `services/edge-gateway/src/entity_sighting.rs` ingester and
    /// migration `db/migrations/0035_entity_sightings.sql`.
    ///
    /// Per AGENTS.md rule 7 and `WIRE_PROTOCOL.md` §4 / `REPO_BOUNDARY.md`
    /// R9 the cloud gateway rejects payloads whose `embedding_model`
    /// matches a face-recognition brand or whose `embedding_dim`
    /// disagrees with the model. Build the projection from
    /// [`nexus_reid::Extractor::model_id`] + [`nexus_reid::Embedding::dim`]
    /// to guarantee both fields stay in lockstep — see
    /// [`build_entity_sighting_envelope`] for the validation that
    /// happens inside the projection.
    ///
    /// # Errors
    ///
    /// Returns [`TunnelError`] from the underlying handle (most
    /// commonly `Disconnected` when the WSS tunnel is currently down
    /// — the pipeline supervisor logs and continues; the next live
    /// sighting will retry on its own cadence).
    pub async fn publish_entity_sighting(
        &self,
        sighting: EntitySightingProjection,
    ) -> Result<(), TunnelError> {
        let envelope = build_entity_sighting_envelope(sighting);
        self.tunnel.send(envelope).await
    }
}

/// Pure-function projection. Public so engine tests can construct
/// reference envelopes without instantiating a sink.
#[must_use]
pub fn build_alert_envelope(alert: AlertProjection) -> Envelope {
    let payload = AlertPayload {
        edge_event_id: alert.edge_event_id,
        ts: alert.ts.to_rfc3339(),
        camera_id: alert.camera_id,
        severity: alert.severity,
        edge_rule_id: alert.edge_rule_id,
        matched_label: alert.matched_label,
        confidence: alert.confidence,
        bbox: alert.bbox,
        snapshot_blob_url: alert.snapshot_blob_url,
        clip_blob_url: alert.clip_blob_url,
        attached_history: alert.attached_history,
    };
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::Alert(payload),
    }
}

#[async_trait]
trait _SinkContract: Send + Sync {
    async fn publish_alert(&self, alert: AlertProjection) -> Result<(), TunnelError>;
    async fn publish_clip_replicated(
        &self,
        clip: ClipReplicatedProjection,
    ) -> Result<(), TunnelError>;
    async fn publish_entity_sighting(
        &self,
        sighting: EntitySightingProjection,
    ) -> Result<(), TunnelError>;
}

/// Edge-side projection of a cold-replicated clip as the sink expects it.
/// Mirrors [`AlertProjection`] in shape (engine has a richer [`nexus_store::motion::ClipRow`];
/// this struct names just the fields that travel over the wire as a
/// [`ClipReplicatedPayload`]).
///
/// Phase 2 · Step 2.8.
#[derive(Debug, Clone)]
pub struct ClipReplicatedProjection {
    /// Core-local clip id. Dedup key on cloud INSERT (`cores.id × edge_clip_id`).
    pub edge_clip_id: String,
    /// Per-core integer id (matches `cameras.edge_camera_id`).
    pub camera_id: u64,
    /// Bare blob URL (no SAS query) of the freshly-uploaded clip MP4.
    /// Sourced from `PutReceipt::cold_url`, which the Azure backend
    /// stamps as the bare `blob_url_unsigned` minted by the SAS issuer.
    pub blob_url: String,
    /// Wall-clock at which the clip recording started.
    pub started_at: DateTime<Utc>,
    /// Final clip duration in milliseconds.
    pub duration_ms: u64,
    /// Final on-disk byte count after MP4 close.
    pub size_bytes: u64,
    /// 64-character lowercase hex of the streaming SHA-256 computed
    /// over the MP4 bytes during write on the edge. Cloud stores in
    /// `clips.sha256` and the Phase 6.17 integrity sweep verifies
    /// against Blob on read.
    pub sha256_hex: String,
    /// Video codec inside the MP4. `None` → cloud defaults to `h264`.
    pub codec: Option<String>,
    /// Container format. `None` → cloud defaults to `mp4`.
    pub container: Option<String>,
    /// SAS-stripped URL of the per-clip thumbnail, when generated.
    pub thumbnail_blob_url: Option<String>,
    /// Phase 21.2 — set when the clip was pre-attached on edge
    /// (i.e. existed on disk before this core enrolled).
    pub attached_history: Option<bool>,
}

/// Pure-function projection. Public so engine tests can construct
/// reference envelopes without instantiating a sink.
///
/// Phase 2 · Step 2.8.
#[must_use]
pub fn build_clip_replicated_envelope(clip: ClipReplicatedProjection) -> Envelope {
    let payload = ClipReplicatedPayload {
        attached_history: clip.attached_history,
        blob_url: clip.blob_url,
        camera_id: clip.camera_id,
        codec: clip.codec,
        container: clip.container,
        duration_ms: clip.duration_ms,
        edge_clip_id: clip.edge_clip_id,
        sha256_hex: clip.sha256_hex,
        size_bytes: clip.size_bytes,
        started_at: clip.started_at.to_rfc3339(),
        thumbnail_blob_url: clip.thumbnail_blob_url,
    };
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::ClipReplicated(payload),
    }
}

/// Edge-side projection of an appearance-embedding sighting as the
/// sink expects it. Mirrors [`AlertProjection`] / [`ClipReplicatedProjection`]
/// in shape; the engine pipeline supervisor builds one of these per
/// stable track at first-detection + every 5 s while alive, then hands
/// it to [`CloudConsoleSink::publish_entity_sighting`].
///
/// Phase 5.6 · slice 4c-i.
///
/// # Wire-shape invariants
///
/// * `embedding_model` MUST match the cloud's allowlist
///   (`dinov2-s-v1`, `osnet-x1.0-v1` as of v1). Anything else is
///   rejected by the gateway with `embedding_model_unknown`; mock
///   model ids (e.g. `mock_dinov2_s_224` produced by
///   [`nexus_reid::MockExtractor::default`]) should NOT be published
///   — the engine pipeline supervisor is responsible for skipping
///   emission when the extractor is a mock.
/// * `embedding_dim` MUST equal the actual length of the f32 slice
///   that was base64'd into `embedding_b64`; the cloud cross-checks
///   the dimension against the model.
/// * `bbox` is `[x, y, w, h]` in **supervisor frame coords**
///   (typically 960×540 on RTSP sources — see
///   `RTSP_SOURCE_FRAME_WIDTH` in `nexus-pipeline::source`). The
///   accompanying `frame_w` / `frame_h` carry the actual supervisor
///   dimensions so the cloud can scale to native MP4 resolution when
///   overlaying.
/// * Coordinates and dimensions are clamped to `u64` at projection
///   time — negative bbox values from upstream are saturated to 0,
///   matching the JSON Schema `minimum: 0` constraint.
#[derive(Debug, Clone)]
pub struct EntitySightingProjection {
    /// Per-core integer id (matches `cameras.edge_camera_id`).
    pub camera_id: u64,
    /// Stable per-track id (UUIDv7 minted on the edge). Two sightings
    /// sharing `(core_id, entity_local_id)` are the same track; the
    /// cloud uses this as the dedup key and to follow a track across
    /// re-sends. Capped at 64 bytes per the JSON Schema.
    pub entity_local_id: String,
    /// Raw L2-normalised float32 embedding in little-endian wire
    /// order. Length MUST equal `embedding_model`'s declared
    /// dimension (384 for `dinov2-s-v1`, 512 for `osnet-x1.0-v1`).
    /// `build_entity_sighting_envelope` base64-encodes this verbatim.
    pub embedding: Vec<f32>,
    /// Model id string from [`nexus_reid::Extractor::model_id`]. MUST
    /// match the cloud's allowlist — see struct-level docs.
    pub embedding_model: String,
    /// Bbox `[x, y, w, h]` in supervisor-frame pixel coords. Signed
    /// inputs are saturated to `0..=u64::MAX` at projection time.
    pub bbox: [i64; 4],
    /// Detector confidence at this sighting, 0..=1.
    pub confidence: f64,
    /// Supervisor frame width (typically 960 for RTSP sources).
    pub frame_w: u64,
    /// Supervisor frame height (typically 540 for RTSP sources).
    pub frame_h: u64,
    /// Wall-clock of the FIRST frame this track was observed on.
    pub started_ts: DateTime<Utc>,
    /// Wall-clock of THIS sighting. Equals `started_ts` for the
    /// first envelope, `>started_ts` for periodic re-sends.
    pub ts: DateTime<Utc>,
    /// `true` for the first envelope emitted for this track,
    /// `false` for every periodic re-send.
    pub is_first_sighting: bool,
    /// Phase 6.8: optional raw detector class label for this track
    /// (e.g. `"person"`, `"car"`). `None` means "no class signal";
    /// the cloud-side `class_to_kind` falls back to `'unknown'`.
    /// Wire shape: `EntitySightingPayload.class_label` (optional).
    pub class_label: Option<String>,
}

/// Pure-function projection. Public so engine tests can construct
/// reference envelopes without instantiating a sink.
///
/// Phase 5.6 · slice 4c-i.
#[must_use]
pub fn build_entity_sighting_envelope(sighting: EntitySightingProjection) -> Envelope {
    build_entity_sighting_envelope_with_dtype(sighting, false)
}

/// Phase M_PERF_CROWD A1: same as [`build_entity_sighting_envelope`]
/// but selects FP16 wire encoding when `use_f16` is true.
/// Callers MUST gate `use_f16` on the cloud advertising
/// [`crate::cloud_capabilities::EMBEDDING_DTYPE_F16`] in
/// `HeartbeatAckPayload.cloud_capabilities`.
#[must_use]
pub fn build_entity_sighting_envelope_with_dtype(
    sighting: EntitySightingProjection,
    use_f16: bool,
) -> Envelope {
    let payload = build_entity_sighting_payload(sighting, use_f16);
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::EntitySighting(payload),
    }
}

/// Phase M_PERF_CROWD A3: bundle 1..=32 sightings into a single
/// `entity_sighting_batch` envelope. Callers MUST gate emission on
/// the cloud advertising
/// [`crate::cloud_capabilities::ENTITY_SIGHTING_BATCH`].
///
/// # Panics
///
/// Debug-only assert that `1..=32` items are passed — the wire
/// schema rejects empty and >32. In release builds an oversized
/// vector silently produces an envelope the cloud will refuse, so
/// the worker pre-chunks to 32.
#[must_use]
pub fn build_entity_sighting_batch_envelope(
    sightings: Vec<EntitySightingProjection>,
    use_f16: bool,
) -> Envelope {
    debug_assert!(
        !sightings.is_empty() && sightings.len() <= 32,
        "entity_sighting_batch requires 1..=32 items, got {}",
        sightings.len()
    );
    let items = sightings
        .into_iter()
        .map(|s| build_entity_sighting_payload(s, use_f16))
        .collect();
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::EntitySightingBatch(EntitySightingBatchPayload { items }),
    }
}

fn build_entity_sighting_payload(
    sighting: EntitySightingProjection,
    use_f16: bool,
) -> EntitySightingPayload {
    let embedding_dim = sighting.embedding.len() as i64;
    let bytes = if use_f16 {
        let mut out = Vec::with_capacity(sighting.embedding.len() * 2);
        for v in &sighting.embedding {
            out.extend_from_slice(&f32_to_f16_bits(*v).to_le_bytes());
        }
        out
    } else {
        let mut out = Vec::with_capacity(sighting.embedding.len() * 4);
        for v in &sighting.embedding {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    };
    use base64::Engine as _;
    let embedding_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let bbox: Vec<u64> = sighting
        .bbox
        .iter()
        .map(|v| u64::try_from(*v).unwrap_or(0))
        .collect();
    EntitySightingPayload {
        bbox,
        camera_id: sighting.camera_id,
        confidence: sighting.confidence,
        embedding_b64,
        embedding_dim,
        embedding_dtype: if use_f16 {
            Some("f16".to_string())
        } else {
            None
        },
        embedding_model: sighting.embedding_model,
        entity_local_id: sighting.entity_local_id,
        frame_h: sighting.frame_h,
        frame_w: sighting.frame_w,
        is_first_sighting: sighting.is_first_sighting,
        started_ts: sighting.started_ts.to_rfc3339(),
        ts: sighting.ts.to_rfc3339(),
        class_label: sighting.class_label,
    }
}

/// Mirror of `nexus_reid::f32_to_f16_bits`. Duplicated here to keep
/// `nexus-cloud-client` free of the `nexus-reid` (ORT-heavy) dep.
/// Both paths are checked in `crates/nexus-reid/tests/fp16_parity.rs`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 31) & 0x1) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        let mantissa16 = if mantissa != 0 { 0x0200 } else { 0 };
        return (sign << 15) | 0x7c00 | mantissa16;
    }
    let new_exp = exponent - 127 + 15;
    if new_exp >= 0x1f {
        return (sign << 15) | 0x7c00;
    }
    if new_exp <= 0 {
        return sign << 15;
    }
    let mantissa16 = (mantissa >> 13) as u16;
    (sign << 15) | ((new_exp as u16) << 10) | mantissa16
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapturingTunnel {
        last: parking_lot::Mutex<Option<Envelope>>,
    }

    #[async_trait]
    impl TunnelHandle for CapturingTunnel {
        async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
            *self.last.lock() = Some(envelope);
            Ok(())
        }
    }

    #[tokio::test]
    async fn publish_alert_builds_v1_envelope() {
        let tunnel = Arc::new(CapturingTunnel {
            last: parking_lot::Mutex::new(None),
        });
        let sink = CloudConsoleSink::new(tunnel.clone());
        sink.publish_alert(AlertProjection {
            edge_event_id: "evt-1".into(),
            ts: Utc::now(),
            camera_id: 7,
            severity: 4,
            edge_rule_id: Some("rule_person".into()),
            matched_label: Some("person".into()),
            confidence: Some(0.91),
            bbox: Some(vec![0.0, 0.0, 1.0, 1.0]),
            snapshot_blob_url: None,
            clip_blob_url: None,
            attached_history: None,
        })
        .await
        .expect("send");
        let captured = tunnel.last.lock().clone().expect("captured envelope");
        assert_eq!(captured.meta.v, 1);
        match captured.body {
            EnvelopeBody::Alert(p) => {
                assert_eq!(p.edge_event_id, "evt-1");
                assert_eq!(p.camera_id, 7);
            }
            other => panic!("expected Alert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_clip_replicated_builds_v1_envelope() {
        let tunnel = Arc::new(CapturingTunnel {
            last: parking_lot::Mutex::new(None),
        });
        let sink = CloudConsoleSink::new(tunnel.clone());
        let started_at = Utc::now();
        sink.publish_clip_replicated(ClipReplicatedProjection {
            edge_clip_id: "clip-42".into(),
            camera_id: 3,
            blob_url: "https://core.blob.core.windows.net/clips/clip-42.mp4".into(),
            started_at,
            duration_ms: 30_000,
            size_bytes: 1_234_567,
            sha256_hex: "a".repeat(64),
            codec: Some("h264".into()),
            container: Some("mp4".into()),
            thumbnail_blob_url: None,
            attached_history: None,
        })
        .await
        .expect("send");
        let captured = tunnel.last.lock().clone().expect("captured envelope");
        assert_eq!(captured.meta.v, 1);
        match captured.body {
            EnvelopeBody::ClipReplicated(p) => {
                assert_eq!(p.edge_clip_id, "clip-42");
                assert_eq!(p.camera_id, 3);
                assert_eq!(p.duration_ms, 30_000);
                assert_eq!(p.size_bytes, 1_234_567);
                assert_eq!(p.sha256_hex.len(), 64);
                assert_eq!(p.started_at, started_at.to_rfc3339());
            }
            other => panic!("expected ClipReplicated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_entity_sighting_builds_v1_envelope() {
        let tunnel = Arc::new(CapturingTunnel {
            last: parking_lot::Mutex::new(None),
        });
        let sink = CloudConsoleSink::new(tunnel.clone());
        let started_ts = Utc::now();
        let embedding = vec![0.1f32, 0.2, -0.3, 0.5];
        sink.publish_entity_sighting(EntitySightingProjection {
            camera_id: 3,
            entity_local_id: "0192d3c2-7c4f-7000-8000-000000000001".into(),
            embedding: embedding.clone(),
            embedding_model: "dinov2-s-v1".into(),
            bbox: [10, 20, 100, 200],
            confidence: 0.87,
            frame_w: 960,
            frame_h: 540,
            started_ts,
            ts: started_ts,
            is_first_sighting: true,
            class_label: Some("person".into()),
        })
        .await
        .expect("send");
        let captured = tunnel.last.lock().clone().expect("captured envelope");
        assert_eq!(captured.meta.v, 1);
        match captured.body {
            EnvelopeBody::EntitySighting(p) => {
                assert_eq!(p.camera_id, 3);
                assert_eq!(p.embedding_model, "dinov2-s-v1");
                assert_eq!(p.embedding_dim, 4);
                assert_eq!(p.bbox, vec![10u64, 20, 100, 200]);
                assert!(p.is_first_sighting);
                assert_eq!(p.frame_w, 960);
                assert_eq!(p.frame_h, 540);
                assert!((p.confidence - 0.87).abs() < 1e-9);
                assert_eq!(p.class_label.as_deref(), Some("person"));
                // round-trip the embedding through base64 and confirm
                // we got the same float32-little-endian bytes back
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(p.embedding_b64.as_bytes())
                    .expect("base64 decodes");
                assert_eq!(bytes.len(), embedding.len() * 4);
                for (i, v) in embedding.iter().enumerate() {
                    let mut le = [0u8; 4];
                    le.copy_from_slice(&bytes[i * 4..i * 4 + 4]);
                    assert_eq!(f32::from_le_bytes(le).to_bits(), v.to_bits());
                }
            }
            other => panic!("expected EntitySighting, got {other:?}"),
        }
    }

    #[test]
    fn build_entity_sighting_envelope_clamps_negative_bbox_to_zero() {
        let env = build_entity_sighting_envelope(EntitySightingProjection {
            camera_id: 1,
            entity_local_id: "t1".into(),
            embedding: vec![0.0f32; 384],
            embedding_model: "dinov2-s-v1".into(),
            // Negative inputs are saturated to 0 to keep the JSON
            // Schema `minimum: 0` invariant on the bbox array
            // honoured at the wire boundary.
            bbox: [-5, -1, 10, 20],
            confidence: 0.5,
            frame_w: 960,
            frame_h: 540,
            started_ts: Utc::now(),
            ts: Utc::now(),
            is_first_sighting: false,
            class_label: None,
        });
        match env.body {
            EnvelopeBody::EntitySighting(p) => {
                assert_eq!(p.bbox, vec![0u64, 0, 10, 20]);
                assert_eq!(p.embedding_dim, 384);
                assert!(!p.is_first_sighting);
                assert!(p.class_label.is_none());
            }
            other => panic!("expected EntitySighting, got {other:?}"),
        }
    }
}
