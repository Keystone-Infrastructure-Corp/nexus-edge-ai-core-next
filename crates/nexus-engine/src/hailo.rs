//! Hailo accelerator telemetry — `hailo: HailoInfo | null` field on
//! `GET /api/v1/system/metrics`.
//!
//! Sourced from the active inference session (the single
//! `HailoYoloDetector` in `nexus-inference`'s process-wide cache,
//! itself the one consumer of the one-per-process Hailo vdevice).
//! `nexus_inference::hailo_telemetry_snapshot()` returns `None`
//! whenever no Hailo session exists (cross-platform dev builds, hosts
//! without the card, engine startup before the first pipeline is
//! wired). The cost on the cached path is 3 FFI calls per chip; safe
//! to call at the dashboard's 1–2 s poll cadence.
//!
//! Hailo-8 has no `/sys/class/hwmon` entry on the in-tree `hailo_pci`
//! kernel module shipped with HailoRT 4.23, so going through libhailort
//! is the only path. See repo memory
//! `nexus-hailo-vdevice-single-per-process.md` for the constraint that
//! drove this design (a second `hailo_create_vdevice` returns
//! `HAILO_OUT_OF_PHYSICAL_DEVICES`, so telemetry must piggyback on the
//! inference session's existing vdevice).

use serde::Serialize;

/// Snapshot of every Hailo accelerator backing the active inference
/// session. `None` for the whole field is the "no Hailo on this host"
/// signal — the UI hides the entire card. `Some` with an empty `devices`
/// list and a populated `status` means we have a session but the FFI
/// call failed, which the UI surfaces as the reason text.
#[derive(Debug, Clone, Serialize)]
pub struct HailoInfo {
    pub devices: Vec<HailoDeviceInfo>,
    /// Operator-facing reason when [`devices`] is empty. `None` when
    /// devices are populated normally.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<String>,
    /// Session-wide inferences/sec, measured as a delta between
    /// consecutive `/system/metrics` polls (the session keeps the
    /// last-snapshot watermark internally). 0.0 on the first poll and
    /// when `status` is set.
    pub inferences_per_sec: f32,
    /// Lifetime inference count for the active session.
    pub frames_total: u64,
    /// Fraction of wall-clock time spent inside `infer_blocking`'s
    /// write+read FFI pair between consecutive `/system/metrics` polls,
    /// expressed as 0–100. HailoRT 4.x exposes no per-chip
    /// utilization counter; this busy% is the operator-facing signal
    /// the System tab renders as the prominent "utilization" tile so
    /// the Hailo card matches the NPU and GPU cards. 0.0 on the first
    /// poll after open and when `status` is set.
    pub utilization_pct: f32,
}

/// Per-chip telemetry. Identity fields are always populated when the
/// device handle resolves; live readings may be `null` on FFI failure.
#[derive(Debug, Clone, Serialize)]
pub struct HailoDeviceInfo {
    pub board_name: String,
    pub serial: String,
    /// "major.minor.revision" (e.g. "4.23.0").
    pub fw_version: String,
    pub part_number: String,
    pub product_name: String,
    pub temperature_c: Option<f32>,
    pub power_w: Option<f32>,
}

/// Crate-public entry point used by `system_metrics::render()`.
pub(crate) fn snapshot() -> Option<HailoInfo> {
    let raw = nexus_inference::hailo_telemetry_snapshot()?;
    Some(HailoInfo {
        devices: raw
            .devices
            .into_iter()
            .map(|d| HailoDeviceInfo {
                board_name: d.board_name,
                serial: d.serial,
                fw_version: d.fw_version,
                part_number: d.part_number,
                product_name: d.product_name,
                temperature_c: d.temperature_c,
                power_w: d.power_w,
            })
            .collect(),
        status: raw.status,
        inferences_per_sec: raw.inferences_per_sec,
        frames_total: raw.frames_total,
        utilization_pct: raw.utilization_pct,
    })
}
