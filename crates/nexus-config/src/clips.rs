//! Clips (M2.1 motion timeline + clip recording + safety floor)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Pick which clip-recorder implementation the engine wires up at
/// boot. `Stub` writes 0-byte placeholder files; `Gstreamer` writes
/// real H.264-pass-through fragmented mp4 via
/// `nexus_pipeline::GstClipRecorder` (only available when the
/// `gstreamer` feature is on for `nexus-pipeline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecorderKind {
    #[default]
    Stub,
    Gstreamer,
}

/// Recording, retention, and disk-safety knobs for the motion timeline.
///
/// **Hand-written `impl Default`.** The codebase rule (see DEV_NOTES.md
/// "Cargo / Rust") is: never combine `#[derive(Default)]` with
/// `#[serde(default = "fn")]`. The serde defaults below fire for
/// missing keys during deserialise; this `impl Default` keeps
/// `T::default()` callers (tests, builders) producing the same values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipsConfig {
    /// Which recorder implementation to wire up at boot.
    #[serde(default)]
    pub recorder: RecorderKind,
    /// Where the recorder writes mp4 files. Created on demand.
    #[serde(default = "default_clips_dir")]
    pub clips_dir: PathBuf,
    /// How long an unevicted clip lives before the daily retention
    /// sweeper deletes it. The watermark sampler can evict sooner if
    /// disk is tight.
    #[serde(default = "default_motion_clips_retention_days")]
    pub motion_clips_retention_days: u32,
    /// Cap on `track.updated` motion-event row writes per active track
    /// per second. `track.born` and `track.died` are always emitted.
    /// Default 1.0 ≈ one row per track per second.
    #[serde(default = "default_motion_events_sample_hz")]
    pub motion_events_sample_hz: f32,
    /// Below this percentage of free space on `clips_dir`'s filesystem
    /// the watermark sampler starts evicting one round per check.
    #[serde(default = "default_low_watermark_pct")]
    pub low_watermark_pct: u8,
    /// Below this percentage the recorder refuses to open new clips
    /// and the eviction loop runs hard until free space recovers to
    /// `low_watermark_pct + 5`.
    #[serde(default = "default_panic_watermark_pct")]
    pub panic_watermark_pct: u8,
    /// How often the watermark sampler runs.
    #[serde(default = "default_watermark_sample_interval_secs")]
    pub watermark_sample_interval_secs: u32,
    /// How long the supervisor waits after the last live track
    /// disappears before closing the open clip. A new motion event
    /// arriving inside the grace window cancels the pending close,
    /// so a single clip spans the brief gap between two intermittent
    /// tracks. 0 disables post-roll entirely (the clip closes the
    /// moment `live_track_count` hits zero, matching pre-B3 behaviour).
    #[serde(default = "default_post_roll_secs")]
    pub post_roll_secs: u32,
    /// Pre-roll buffer length in seconds — how much encoded H.264
    /// the always-on ingester keeps in RAM ahead of motion. When a
    /// new clip opens, the ring buffer's snapshot is prepended to
    /// the file so the operator sees the moment leading up to
    /// motion onset, not just the moment after.
    ///
    /// 0 disables pre-roll entirely; the recorder behaves exactly
    /// as it did before B8 (clips start at the first sample taken
    /// AFTER the open call). Default 5s matches the M2.1 spec; the
    /// per-camera RAM cost is roughly `bitrate * pre_roll_secs`,
    /// e.g. ~2 MB for a 4 Mbps 1080p camera.
    #[serde(default = "default_pre_roll_secs")]
    pub pre_roll_secs: u32,
    /// M2.2 Phase 3: when set, the recorder routes new clips to the
    /// USB volume with this label (e.g. `"NEXUS_VAULT"`) if the
    /// `usb_watch` task currently sees it attached. When the label
    /// is unset, missing, or the volume is unmounted, the recorder
    /// falls back to writing under `clips_dir` (`hot_handle = "local"`).
    /// In-flight clips never migrate mid-recording — attach/detach
    /// only takes effect on the next `open()` call.
    #[serde(default)]
    pub preferred_usb_label: Option<String>,
    /// Hard ceiling on a single clip's on-disk size in bytes. A
    /// healthy camera never approaches this — a 30-60 s 1080p clip is
    /// a few MB. The cap exists to bound the damage when a camera
    /// emits a corrupt H.264 stream whose byte-rate explodes (seen in
    /// the field: 640x360 clips ballooning to ~2 GiB at 160-740 Mbps).
    /// The recorder stats the in-flight file periodically and rotates
    /// when it crosses this size (Phase 3b enforcement); the cold
    /// replicator derives its own pre-emptive quarantine ceiling from
    /// this value so a runaway clip never head-of-line-blocks the
    /// upload queue. Default 256 MiB.
    #[serde(default = "default_max_clip_bytes")]
    pub max_clip_bytes: u64,
    /// M-Alert-Clip: short, burned-in alert clips delivered to sinks
    /// promptly, independent of the 5-minute motion clip. All-defaulted;
    /// disabled by default. See docs/edge-core/M_ALERT_CLIP.md.
    #[serde(default)]
    pub alert_clips: AlertClipsConfig,
    /// Guarantee every alert/event has an underlying full-resolution
    /// motion clip. The cheap `MotionGate` and the motion tracker are
    /// tuned for sustained movement, so a small / distant / briefly
    /// moving object can trip a rule (fire an alert) on a keyframe pass
    /// without the tracker ever declaring `Born` — leaving the event
    /// with no surrounding native-resolution video. When this is set,
    /// an alert firing on a frame with no clip open force-opens a
    /// native-resolution motion clip (identical to the motion path,
    /// including pre-roll) and each subsequent alert frame keeps it
    /// open through the same `post_roll_secs` grace window. This is the
    /// full-res, longer-capture motion clip — NOT the reduced-resolution
    /// burned-in alert clip, which remains governed by `alert_clips`.
    /// Default `true`.
    #[serde(default = "default_record_motion_clip_on_alert")]
    pub record_motion_clip_on_alert: bool,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            recorder: RecorderKind::default(),
            clips_dir: default_clips_dir(),
            motion_clips_retention_days: default_motion_clips_retention_days(),
            motion_events_sample_hz: default_motion_events_sample_hz(),
            low_watermark_pct: default_low_watermark_pct(),
            panic_watermark_pct: default_panic_watermark_pct(),
            watermark_sample_interval_secs: default_watermark_sample_interval_secs(),
            post_roll_secs: default_post_roll_secs(),
            pre_roll_secs: default_pre_roll_secs(),
            preferred_usb_label: None,
            max_clip_bytes: default_max_clip_bytes(),
            alert_clips: AlertClipsConfig::default(),
            record_motion_clip_on_alert: default_record_motion_clip_on_alert(),
        }
    }
}

fn default_clips_dir() -> PathBuf {
    PathBuf::from("/var/lib/nexus/clips")
}

fn default_motion_clips_retention_days() -> u32 {
    30
}

fn default_motion_events_sample_hz() -> f32 {
    1.0
}

fn default_low_watermark_pct() -> u8 {
    15
}

fn default_panic_watermark_pct() -> u8 {
    5
}

fn default_watermark_sample_interval_secs() -> u32 {
    30
}

fn default_post_roll_secs() -> u32 {
    10
}

fn default_pre_roll_secs() -> u32 {
    5
}

fn default_max_clip_bytes() -> u64 {
    // 256 MiB. A healthy 30-60 s clip is a few MB; this only ever
    // trips on corrupt byte-exploding streams.
    256 * 1024 * 1024
}

fn default_record_motion_clip_on_alert() -> bool {
    true
}

/// M-Alert-Clip: configuration for the short, burned-in "alert clip"
/// that covers only the alert timeframe and is delivered to sinks
/// promptly, independent of the 5-minute motion clip. Enabled by
/// default; operators disable it per-org / per-core from the cloud
/// console's delivery settings (which flips the runtime
/// `DeliverySettings.attach_alert_clip` gate) without editing this
/// file. See docs/edge-core/M_ALERT_CLIP.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertClipsConfig {
    /// Capability switch. Default `true`: the edge builds alert clips
    /// and clip-attaching sinks resolve them (the operator-facing on/off
    /// lives in delivery settings, `DeliverySettings.attach_alert_clip`,
    /// AND-gated with this). Set `false` here to hard-disable the
    /// capability on a box regardless of the cloud toggle.
    #[serde(default = "default_alert_clips_enabled")]
    pub enabled: bool,
    /// Seconds of footage before the alert timestamp to include.
    /// Effectively bounded by the ingester's `pre_roll_secs` (the
    /// window can't include more than is buffered). Default 3.
    #[serde(default = "default_alert_clip_pre_secs")]
    pub pre_secs: u32,
    /// Seconds of footage after the alert timestamp to keep collecting
    /// before the clip is finalized. Sets the delivery-latency floor:
    /// the clip is ready ~`post_secs` after the alert. Default 5.
    #[serde(default = "default_alert_clip_post_secs")]
    pub post_secs: u32,
    /// Downscale cap (pixels of width) applied to the native frame
    /// before the bbox burn-in re-encode, to bound per-alert CPU.
    /// `0` disables the cap (encode at native width). Default 1280.
    #[serde(default = "default_alert_clip_max_encode_width")]
    pub max_encode_width: u32,
    /// How long the sink dispatcher waits for the alert clip to
    /// finalize before delivering the alarm clip-less. Should exceed
    /// `post_secs` plus worst-case encode time. Default 30.
    #[serde(default = "default_alert_clip_build_timeout_secs")]
    pub build_timeout_secs: u32,
}

impl Default for AlertClipsConfig {
    fn default() -> Self {
        Self {
            enabled: default_alert_clips_enabled(),
            pre_secs: default_alert_clip_pre_secs(),
            post_secs: default_alert_clip_post_secs(),
            max_encode_width: default_alert_clip_max_encode_width(),
            build_timeout_secs: default_alert_clip_build_timeout_secs(),
        }
    }
}

fn default_alert_clips_enabled() -> bool {
    true
}

fn default_alert_clip_pre_secs() -> u32 {
    3
}

fn default_alert_clip_post_secs() -> u32 {
    5
}

fn default_alert_clip_max_encode_width() -> u32 {
    1280
}

fn default_alert_clip_build_timeout_secs() -> u32 {
    30
}
