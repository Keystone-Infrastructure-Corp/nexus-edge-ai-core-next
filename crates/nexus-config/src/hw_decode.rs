//! Hardware decode

use serde::{Deserialize, Serialize};

/// Decoder-selection strategy for the RTSP ingest path (the RGB tap that
/// feeds the detector). Serialised as `[runtime.decode] mode = "..."`
/// and optionally overridden per camera via [`CameraIngest::decode`].
///
/// The actual element selection (which `vah26Xdec` / `avdec_h26X` /
/// `msdkh26Xdec` chain to launch, with fail-open fallback) lives in
/// `nexus_pipeline::decode`. This enum is only the operator-facing knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeMode {
    /// Probe for the best available hardware backend (libva `va`, then
    /// NVIDIA `nvdec`) and fall back to software when neither is
    /// registered. Default.
    #[default]
    Auto,
    /// Force the libva `va` backend (`vah26Xdec` + `vapostproc`). Falls
    /// back to software with a warning if those elements are missing.
    Va,
    /// Force the Intel Media-SDK backend (`msdkh26Xdec` + `msdkvpp`).
    /// Falls back to VA, then software.
    Msdk,
    /// Force the NVIDIA NVDEC backend (`nvh26Xdec`, from the `nvcodec`
    /// plugin). Falls back to software with a warning if the plugin is
    /// missing or the driver's NVDEC userspace is unreachable.
    Nvdec,
    /// Force the software backend (`avdec_h26X`). Always available.
    Software,
}

/// `[runtime.decode]` section. Currently a single `mode` knob; kept as a
/// struct so future decode tuning (e.g. an explicit libva driver name or
/// a VRAM budget) is an additive field rather than a breaking reshape.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDecodeConfig {
    /// Global decode strategy. Per-camera [`CameraIngest::decode`]
    /// overrides this when set.
    #[serde(default)]
    pub mode: DecodeMode,
}
