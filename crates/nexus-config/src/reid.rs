//! Phase 5.6 — cross-camera re-identification

use crate::inference::default_ep_priority;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[reid]` block. Disabled by default. When `enabled = true`, the
/// per-camera supervisor runs the configured [`nexus_reid::Extractor`]
/// on each stable track once on first-stable and again every
/// `emit_interval_s` of capture time, publishing `entity_sighting`
/// envelopes through the cloud tunnel. See
/// `crates/nexus-pipeline/src/entity_sighting.rs` for the per-track
/// FSM and `WEDGE_PLAN.md §4` for the wire contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReidConfig {
    /// Master switch. `false` keeps the supervisor's per-frame
    /// scheduler tick alive (it's cheap) but installs the
    /// [`nexus_pipeline::NoopSightingHook`] so nothing reaches the
    /// cloud.
    #[serde(default)]
    pub enabled: bool,
    /// Optional ONNX model path. When `Some(_)` AND the engine is
    /// built with `--features ort`, the engine loads
    /// `nexus_reid::DinoV2Extractor`. When `None` (or `ort` feature
    /// off), the engine falls back to
    /// `nexus_reid::MockExtractor::with_config(model_id, dim)` —
    /// useful for end-to-end wire tests against a real cloud without
    /// shipping ONNX weights to the dev box.
    #[serde(default)]
    pub model_path: Option<PathBuf>,
    /// Model id string. MUST match the cloud's wire allowlist —
    /// "dinov2-s-v1" (384-dim, default) or "osnet-x1.0-v1" (512-dim).
    /// Anything else is rejected by the edge-gateway at ingest time.
    #[serde(default = "default_reid_model_id")]
    pub model_id: String,
    /// Embedding dimension. Must match `model_id`'s declared dim
    /// (384 for dinov2-s-v1, 512 for osnet-x1.0-v1).
    #[serde(default = "default_reid_dim")]
    pub dim: usize,
    /// Periodic re-emit cadence in seconds. After the first
    /// stable-track emit, the scheduler waits this long before
    /// firing again. Default 5s — bandwidth-friendly at ~7-10
    /// concurrent tracks per camera.
    #[serde(default = "default_reid_emit_interval_s")]
    pub emit_interval_s: u64,
    /// Concurrent-track count above which the scheduler switches
    /// the periodic re-emit branch to [`crowded_emit_interval_s`].
    /// `0` disables the adaptive cadence (always use
    /// `emit_interval_s`). M_PERF_CROWD B2 — defaults to 15.
    /// First-emit unaffected: freshly-stable entities still emit
    /// promptly so the cloud linker can stitch them.
    #[serde(default = "default_reid_crowded_track_threshold")]
    pub crowded_track_threshold: u32,
    /// Periodic re-emit cadence in seconds used while the per-camera
    /// tracked-object count exceeds [`crowded_track_threshold`].
    /// M_PERF_CROWD B2 — defaults to 15s, giving ~3× bandwidth
    /// reduction at 30+ concurrent tracks (30 × every-5s →
    /// 30 × every-15s).
    #[serde(default = "default_reid_crowded_emit_interval_s")]
    pub crowded_emit_interval_s: u64,
    /// Minimum tracker `age_frames` before the first emit fires.
    /// Filters out single-frame false positives that the tracker
    /// would otherwise let through. Default 5 frames (~165 ms at
    /// 30 fps; ~1 s at 5 fps).
    #[serde(default = "default_reid_min_track_age_frames")]
    pub min_track_age_frames: u32,
    /// M_PERF_CROWD B4 — worker-side bbox-width floor (pixels in
    /// supervisor-frame coordinates, pre-crop-resize) for re-id
    /// extraction. Snapshots whose source bbox width is below this
    /// value are dropped before the batched ORT call so we don't
    /// burn compute on crops too small to embed reliably.
    ///
    /// Threshold tuning:
    /// * **Reduce (20-24)** if same-person sightings from different
    ///   cameras/poses aren't linking — relaxation captures more
    ///   pose variations so the cross-camera kNN has more chances
    ///   to find common neighbors (trades some noise for recall).
    /// * **Increase (48-64)** if the same person is appearing as
    ///   multiple entities due to embedding drift across angles
    ///   — stricter filtering reduces noisy small-crop embeddings.
    ///
    /// DINOv2-S patch_size=14 reaches stability around 30-40px per
    /// side; below 20px drift becomes significant. Default 40
    /// (paired with a 96px height floor) keeps only crops large
    /// enough to embed cleanly — daytime-crowd validation showed
    /// sub-40px crops were the dominant source of cross-camera
    /// embedding drift. `0` disables the filter.
    #[serde(default = "default_reid_min_crop_w_px")]
    pub min_crop_w_px: u32,
    /// M_PERF_CROWD B4 — worker-side bbox-height floor (pixels) for
    /// re-id extraction. See [`min_crop_w_px`] for tuning guidance.
    /// Default 96. `0` disables the filter.
    #[serde(default = "default_reid_min_crop_h_px")]
    pub min_crop_h_px: u32,
    /// EP priority list for the ORT session. Applies to the auto-discovered
    /// release-staged `dinov2_s_224.onnx` too, not just an explicit
    /// `model_path`. `nexus-probe emit-config` pins this per box; the
    /// default chain below matches no shipped hardware.
    ///
    /// **Note for accelerator boxes:** bare `"openvino"` resolves to
    /// OpenVINO `device_type = AUTO`, which can only place the graph on
    /// a device whose OpenVINO plugin is actually loaded. On a box that
    /// only exposes one accelerator, name it explicitly (`"npu"` /
    /// `"gpu"`) or the ViT silently lands on the CPU EP.
    #[serde(default = "default_ep_priority")]
    pub ep_priority: Vec<String>,
    /// ORT intra-op thread-pool size for the extractor session.
    /// `None` (default) auto-sizes — see
    /// [`InferenceConfig::ort_intra_threads`].
    #[serde(default)]
    pub ort_intra_threads: Option<usize>,
    /// Let the extractor's ORT thread pool busy-wait before blocking.
    /// Disabled by default — see [`InferenceConfig::ort_allow_spinning`].
    #[serde(default)]
    pub ort_allow_spinning: bool,
}

impl Default for ReidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: None,
            model_id: default_reid_model_id(),
            dim: default_reid_dim(),
            emit_interval_s: default_reid_emit_interval_s(),
            crowded_track_threshold: default_reid_crowded_track_threshold(),
            crowded_emit_interval_s: default_reid_crowded_emit_interval_s(),
            min_track_age_frames: default_reid_min_track_age_frames(),
            min_crop_w_px: default_reid_min_crop_w_px(),
            min_crop_h_px: default_reid_min_crop_h_px(),
            ep_priority: default_ep_priority(),
            ort_intra_threads: None,
            ort_allow_spinning: false,
        }
    }
}

fn default_reid_model_id() -> String {
    "dinov2-s-v1".into()
}
fn default_reid_dim() -> usize {
    384
}
fn default_reid_emit_interval_s() -> u64 {
    5
}
fn default_reid_crowded_track_threshold() -> u32 {
    15
}
fn default_reid_crowded_emit_interval_s() -> u64 {
    15
}
fn default_reid_min_track_age_frames() -> u32 {
    5
}
fn default_reid_min_crop_w_px() -> u32 {
    40
}
fn default_reid_min_crop_h_px() -> u32 {
    96
}
