//! NPU telemetry — `npu: NpuInfo | null` field on
//! `GET /api/v1/system/metrics`.
//!
//! Intel NPU (Meteor Lake NPU 3.7, Arrow Lake NPU 3.7, Lunar Lake
//! NPU 4) is bound by the upstream `intel_vpu` driver and exposed
//! at `/sys/class/accel/accelN/device/`. Files we sample:
//!
//!   * `vendor` / `device`             — PCI ID for the friendly name
//!   * `npu_busy_time_us`              — monotonic busy counter (µs)
//!   * `npu_current_frequency_mhz`     — current operating freq
//!   * `npu_max_frequency_mhz`         — max freq (read once at init)
//!   * `npu_memory_utilization`        — allocated NPU memory (bytes)
//!
//! All four files are world-readable (`r--r--r--`), so unlike the
//! GPU PMU we do **not** need `CAP_PERFMON` and never call
//! `perf_event_open(2)`. Utilization is `Δbusy_us / Δwall_us × 100`,
//! gated on ≥100 ms elapsed so the first call after boot doesn't
//! divide by ~0.
//!
//! Operator-facing signal: on Lunar Lake the OpenVINO EP advertises
//! NPU support, then silently falls back to CPU when the model uses
//! dynamic shapes or unsupported ops. In that case the NPU sits at
//! `busy_time_us == 0`, `current_frequency_mhz == 0` while inference
//! is happening — surfacing `utilization_pct == 0` in the dashboard
//! is exactly the signal needed to catch this misconfiguration.
//!
//! Cross-platform: macOS / Windows return `None`. Apple Silicon has
//! a Neural Engine but its utilization is only available through
//! the private `IOReport` framework, which we don't link.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Public response shape.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct NpuInfo {
    /// Stable identifier for the backend kind. Currently always
    /// `"intel-npu"` when present.
    pub kind: String,
    /// Friendly device name (PCI-ID → marketing name where known).
    pub name: String,
    /// 0–100; `None` while the baseline sample is warming up or
    /// when the sysfs read failed. Reason is in
    /// `utilization_status` when `None`.
    pub utilization_pct: Option<f32>,
    /// Current NPU operating frequency (MHz). `0` when the NPU is
    /// power-gated (idle) — typical when nothing is using it.
    pub current_freq_mhz: Option<u32>,
    /// Maximum NPU frequency exposed by the driver (MHz). Read
    /// once at init; treated as constant.
    pub max_freq_mhz: Option<u32>,
    /// Currently-allocated NPU memory in bytes (sum of buffer
    /// objects). The kernel surfaces only "used", not "total" —
    /// match the field name to that.
    pub memory_bytes: Option<u64>,
    /// Operator-facing reason when `utilization_pct` is `None`.
    /// Populated by the sysfs path on a sample-read failure or
    /// when the baseline hasn't warmed up yet. `None` when the
    /// utilization number is being reported normally.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub utilization_status: Option<String>,
}

// ---------------------------------------------------------------------------
// Backend dispatch.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
static BACKEND: std::sync::LazyLock<std::sync::Mutex<Option<linux::NpuSysfs>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(linux::try_init()));

/// Crate-public entry point used by `system_metrics::render()`.
pub(crate) fn snapshot() -> Option<NpuInfo> {
    #[cfg(target_os = "linux")]
    {
        let mut guard = BACKEND.lock().ok()?;
        let backend = guard.as_mut()?;
        Some(backend.snapshot())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Linux sysfs backend.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::NpuInfo;

    pub(super) struct NpuSysfs {
        /// `/sys/class/accel/accelN/device`
        device_dir: PathBuf,
        name: String,
        max_freq_mhz: Option<u32>,
        /// (wall-clock at read, busy_time_us at read). Initial
        /// `None`; first snapshot primes it and returns no
        /// utilization yet.
        last_sample: Option<(Instant, u64)>,
    }

    pub(super) fn try_init() -> Option<NpuSysfs> {
        // Walk /sys/class/accel/accel{0..3}. Only one Intel NPU
        // per box on the supported tiers; loop bounds match the
        // gpu module's card{0..9} convention.
        for n in 0..4u32 {
            let class_link = PathBuf::from(format!("/sys/class/accel/accel{n}"));
            if !class_link.exists() {
                continue;
            }
            let device_dir = class_link.join("device");
            if !device_dir.exists() {
                continue;
            }
            let vendor = read_string(&device_dir.join("vendor")).unwrap_or_default();
            if vendor != "0x8086" {
                continue;
            }
            // `npu_busy_time_us` is the file we depend on — if
            // it's not there (older intel_vpu kernel module), the
            // device doesn't qualify.
            if !device_dir.join("npu_busy_time_us").exists() {
                tracing::debug!(
                    accel = ?class_link,
                    "Intel accel device found but npu_busy_time_us missing — kernel intel_vpu too old"
                );
                continue;
            }
            let device_id = read_string(&device_dir.join("device")).unwrap_or_default();
            let name = npu_name_from_pci(&device_id);
            let max_freq_mhz = read_u32(&device_dir.join("npu_max_frequency_mhz"));
            tracing::info!(
                name = %name,
                pci_id = %device_id,
                accel = ?class_link,
                max_freq_mhz = ?max_freq_mhz,
                "NPU backend: Intel NPU via sysfs"
            );
            return Some(NpuSysfs {
                device_dir,
                name,
                max_freq_mhz,
                last_sample: None,
            });
        }
        tracing::debug!(
            "no Intel NPU detected at /sys/class/accel/accel{{0..3}} — \
             intel_vpu driver not loaded or unsupported hardware"
        );
        None
    }

    impl NpuSysfs {
        pub(super) fn snapshot(&mut self) -> NpuInfo {
            let busy_path = self.device_dir.join("npu_busy_time_us");
            let busy = read_u64(&busy_path);
            let now = Instant::now();
            let (utilization_pct, utilization_status) = match (busy, self.last_sample) {
                (Some(b), Some((t0, b0))) => {
                    let elapsed = now.duration_since(t0);
                    if elapsed.as_millis() < 100 {
                        // Two snapshots inside the same 100 ms
                        // window — the metric isn't meaningful
                        // yet; hold the prior baseline so the
                        // next call (which the dashboard makes
                        // every second) computes against ≥1 s.
                        (None, Some("NPU baseline warming up".to_string()))
                    } else {
                        let elapsed_us = elapsed.as_micros() as u64;
                        let delta = b.saturating_sub(b0);
                        // Defensive divide guard: elapsed_us is
                        // ≥100_000 from the gate above, but keep
                        // the f64 path so the clamp does its job.
                        let pct = (delta as f64 * 100.0 / elapsed_us as f64).clamp(0.0, 100.0);
                        self.last_sample = Some((now, b));
                        (Some(pct as f32), None)
                    }
                }
                (Some(b), None) => {
                    // First call after init — prime the baseline,
                    // surface a "warming up" status so the UI can
                    // render a hint rather than a missing-data
                    // banner.
                    self.last_sample = Some((now, b));
                    (None, Some("NPU baseline warming up".to_string()))
                }
                (None, _) => (
                    None,
                    Some(format!("failed to read {}", busy_path.display())),
                ),
            };

            let current_freq_mhz = read_u32(&self.device_dir.join("npu_current_frequency_mhz"));
            let memory_bytes = read_u64(&self.device_dir.join("npu_memory_utilization"));

            NpuInfo {
                kind: "intel-npu".to_string(),
                name: self.name.clone(),
                utilization_pct,
                current_freq_mhz,
                max_freq_mhz: self.max_freq_mhz,
                memory_bytes,
                utilization_status,
            }
        }
    }

    fn read_string(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn read_u32(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn read_u64(path: &Path) -> Option<u64> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Map known Intel NPU PCI IDs to friendly names. Falls back
    /// to `"Intel NPU (0x...)"` for unknown IDs so an unmapped
    /// future part still surfaces something operator-readable.
    fn npu_name_from_pci(device_id: &str) -> String {
        match device_id.to_ascii_lowercase().as_str() {
            // Meteor Lake (NPU 3.7) — Core Ultra (Series 1).
            "0x7d1d" => "Intel AI Boost NPU 3.7 (Meteor Lake)".to_string(),
            // Arrow Lake (NPU 3.7) — Core Ultra (Series 2, desktop).
            "0xad1d" => "Intel AI Boost NPU 3.7 (Arrow Lake)".to_string(),
            // Lunar Lake (NPU 4) — Core Ultra (Series 2, mobile).
            "0x643e" => "Intel AI Boost NPU 4 (Lunar Lake)".to_string(),
            other if other.is_empty() => "Intel NPU".to_string(),
            other => format!("Intel NPU ({other})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_does_not_panic() {
        // Whatever backend resolves on the test host (None on
        // macOS dev / CI without an NPU; Some on a Lunar Lake
        // box), calling snapshot must succeed without panicking.
        let _ = snapshot();
    }
}
