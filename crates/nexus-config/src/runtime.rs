//! Runtime

use crate::clips::ClipsConfig;
use crate::hw_decode::RuntimeDecodeConfig;
use crate::runtime_auth::{RuntimeAuditConfig, RuntimeAuthConfig, RuntimeDeliveryConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// 0 = num_cpus.
    #[serde(default)]
    pub worker_threads: usize,
    #[serde(default = "default_blocking_threads")]
    pub blocking_threads: usize,
    /// Writable directory for per-camera persisted state
    /// (static-object registries, etc.). Created on demand.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// M3.1 — directory holding stored visual-prompt reference crops
    /// (one file per `VisualPromptId`, original PNG/JPEG). The detector
    /// encodes them once into per-prompt embedding vectors persisted in
    /// the SQLite `visual_prompts` table; this directory is the source
    /// of truth for the original pixels (re-encoding on model change,
    /// thumbnail rendering in the admin UI). Created on demand.
    #[serde(default = "default_visual_prompts_dir")]
    pub visual_prompts_dir: PathBuf,
    /// M2.1 motion-clip recording + safety-floor configuration.
    #[serde(default)]
    pub clips: ClipsConfig,
    /// Hardware-decode strategy for the RTSP ingest path. Defaults to
    /// `Auto` (probe for a VA-capable GPU, fall back to software), so
    /// configs that predate this knob auto-enable hardware decode on a
    /// capable box with zero migration.
    #[serde(default)]
    pub decode: RuntimeDecodeConfig,
    /// M6 auth-side runtime knobs (lockout FSM thresholds, audit
    /// retention). All have safe defaults so existing configs that
    /// predate M6 boot unchanged.
    #[serde(default)]
    pub auth: RuntimeAuthConfig,
    /// M6 audit-log retention. Daily sweeper deletes rows older
    /// than `retention_days`. Defaults to 365 days.
    #[serde(default)]
    pub audit: RuntimeAuditConfig,
    /// M7 alert-delivery retention. Daily sweeper trims terminal
    /// `alert_sink_outbox` rows. Defaults are safe for existing
    /// configs that predate the knob.
    #[serde(default)]
    pub delivery: RuntimeDeliveryConfig,
    /// Phase 10 HD live-view transport knobs. Defaults preserve the
    /// hardware-driven behaviour, so configs predating this section
    /// boot unchanged.
    #[serde(default)]
    pub live_view: RuntimeLiveViewConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            blocking_threads: default_blocking_threads(),
            state_dir: default_state_dir(),
            visual_prompts_dir: default_visual_prompts_dir(),
            clips: ClipsConfig::default(),
            decode: RuntimeDecodeConfig::default(),
            auth: RuntimeAuthConfig::default(),
            audit: RuntimeAuditConfig::default(),
            delivery: RuntimeDeliveryConfig::default(),
            live_view: RuntimeLiveViewConfig::default(),
        }
    }
}

/// `[runtime.live_view]` section. Kept as a struct so further HD transport
/// tuning lands as an additive field rather than a breaking reshape.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLiveViewConfig {
    /// Force every HD session onto the passthrough pipeline, even where a
    /// hardware H.264 encoder is available.
    ///
    /// Passthrough forwards the camera's own stream, so it never builds an
    /// encoder and never pays the reconfiguration cost that a transcode
    /// session does. This is the operator escape hatch for a box whose
    /// hardware encoder misbehaves, and it is what Gen9 LP (`i965`) already
    /// gets automatically — see `nexus_pipeline::webrtc::transcode_allowed`.
    #[serde(default)]
    pub force_passthrough: bool,
}

fn default_blocking_threads() -> usize {
    // 64 leaves enough headroom for camera-scaled spawn_blocking
    // callers (clip-recorder appsrc pushes, drain tasks, detector
    // reply recvs, sqlx sqlite workers) on a 32-camera box.
    //
    // The historical default of 8 wedged 10-camera deployments
    // when the GStreamer bus pumps were on this pool (see
    // preroll_ingester::run_session). Those have since moved to
    // dedicated std::threads, but the pool still gates many other
    // short-lived blocking ops — keeping it generous is cheap
    // (each tokio blocking thread is on-demand, ~8 KB stack).
    64
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/state")
}

fn default_visual_prompts_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/visual_prompts")
}
