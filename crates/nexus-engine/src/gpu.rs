//! GPU telemetry — `gpu: GpuInfo | null` field on
//! `GET /api/v1/system/metrics`.
//!
//! Cross-platform strategy:
//!
//!   * **Linux NVIDIA** — `nvml-wrapper` dynamically loads
//!     `libnvidia-ml.so` at first call. On a box without an NVIDIA
//!     driver the `Nvml::init()` returns Err; we fall through.
//!     Everything is queryable: name, memory totals, utilization,
//!     temperature.
//!
//!   * **Linux Intel iGPU** (T10 N100, T24 Iris Xe, T36 Arc A380,
//!     T36-S Lunar Lake) — read `/sys/class/drm/card*/device/`:
//!     `vendor` (must be `0x8086`), `device` PCI ID for the
//!     family name. Frequency is exposed at
//!     `gt/gt0/rps_cur_freq_mhz` but utilization requires
//!     CAP_PERFMON via `intel_gpu_top` (perf events), which we
//!     don't gate behind sudo for an unprivileged engine. So
//!     util/mem/temp are `None`; the operator still sees the
//!     device is detected and named.
//!
//!   * **macOS Apple Silicon (dev only)** — shell
//!     `system_profiler SPDisplaysDataType -json`, parse the
//!     first `sppci_model`. IOReport private framework gives
//!     real utilization but requires unsafe IOKit FFI; we
//!     report device name only.
//!
//! Static info (name, kind, total memory) is cached at process
//! start. Dynamic info (utilization, used memory, temperature)
//! is re-queried per snapshot when the backend supports it
//! (NVIDIA only today). Sysinfo's `MetricsCache` already wraps
//! us in a 1 second TTL so we don't hammer NVML.

use std::sync::{LazyLock, Mutex};

use crate::system_metrics::GpuInfo;

// ---------------------------------------------------------------------------
// Backend dispatch.
// ---------------------------------------------------------------------------

/// Resolves the GPU backend once and caches the choice. The backend
/// is queried for a fresh snapshot on every call (cheap for sysfs
/// and Apple's cached system_profiler output; ~1ms for NVML).
static BACKEND: LazyLock<Mutex<GpuBackend>> = LazyLock::new(|| Mutex::new(GpuBackend::resolve()));

/// Public entry point used by `system_metrics::render()`.
pub(crate) fn snapshot() -> Option<GpuInfo> {
    let mut guard = BACKEND.lock().ok()?;
    guard.snapshot()
}

// Variant sizes diverge by ~hundreds of bytes (NVML state holds a
// thread-safe handle + cached strings; the `None` variant is empty),
// but `GpuBackend` lives behind a single process-wide `Mutex<…>`
// in a `LazyLock` — exactly one instance ever exists, boxing the
// payloads would just add a heap-indirection per access for no
// memory win. Suppress the lint here, not workspace-wide.
#[allow(clippy::large_enum_variant)]
enum GpuBackend {
    None,
    #[cfg(target_os = "linux")]
    Nvidia(nvidia::NvidiaState),
    #[cfg(target_os = "linux")]
    IntelSysfs(intel::IntelSysfs),
    #[cfg(target_os = "macos")]
    Apple(apple::AppleStaticInfo),
}

impl GpuBackend {
    fn resolve() -> Self {
        #[cfg(target_os = "linux")]
        {
            if let Some(state) = nvidia::try_init() {
                return GpuBackend::Nvidia(state);
            }
            if let Some(state) = intel::try_init() {
                return GpuBackend::IntelSysfs(state);
            }
            GpuBackend::None
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(state) = apple::try_init() {
                return GpuBackend::Apple(state);
            }
            GpuBackend::None
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            GpuBackend::None
        }
    }

    fn snapshot(&mut self) -> Option<GpuInfo> {
        match self {
            GpuBackend::None => None,
            #[cfg(target_os = "linux")]
            GpuBackend::Nvidia(state) => state.snapshot(),
            #[cfg(target_os = "linux")]
            GpuBackend::IntelSysfs(state) => Some(state.snapshot()),
            #[cfg(target_os = "macos")]
            GpuBackend::Apple(state) => Some(state.snapshot()),
        }
    }
}

// ---------------------------------------------------------------------------
// Linux NVIDIA backend (nvml-wrapper).
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod nvidia {
    use nvml_wrapper::error::NvmlError;
    use nvml_wrapper::Nvml;

    use super::GpuInfo;

    pub(super) struct NvidiaState {
        nvml: Nvml,
        // Cached static info for device 0. We only surface the
        // first GPU — multi-GPU edge boxes are out of scope.
        name: String,
        mem_total: Option<u64>,
    }

    pub(super) fn try_init() -> Option<NvidiaState> {
        let nvml = match Nvml::init() {
            Ok(n) => n,
            Err(NvmlError::LibloadingError(_)) => {
                tracing::debug!("NVML library not present; skipping NVIDIA GPU probe");
                return None;
            }
            Err(e) => {
                tracing::debug!("NVML init failed: {e}");
                return None;
            }
        };
        let device_count = nvml.device_count().ok()?;
        if device_count == 0 {
            return None;
        }
        let device = nvml.device_by_index(0).ok()?;
        let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".to_string());
        let mem_total = device.memory_info().ok().map(|m| m.total);
        tracing::info!(name = %name, "GPU backend: NVIDIA via NVML");
        Some(NvidiaState {
            nvml,
            name,
            mem_total,
        })
    }

    impl NvidiaState {
        pub(super) fn snapshot(&mut self) -> Option<GpuInfo> {
            let device = self.nvml.device_by_index(0).ok()?;
            let mem = device.memory_info().ok();
            let util = device.utilization_rates().ok().map(|u| u.gpu as f32);
            // Temperature in Celsius for the GPU die sensor.
            let temp = device
                .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f32);
            Some(GpuInfo {
                kind: "nvidia".to_string(),
                name: self.name.clone(),
                mem_total_bytes: mem.as_ref().map(|m| m.total).or(self.mem_total),
                mem_used_bytes: mem.map(|m| m.used),
                utilization_pct: util,
                temp_c: temp,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Linux Intel iGPU backend (sysfs).
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod intel {
    use std::path::PathBuf;

    use super::{read_sysfs_string, GpuInfo};

    /// Cached probe of the first Intel render node we find.
    pub(super) struct IntelSysfs {
        name: String,
        // Path to `gt/gt0/rps_cur_freq_mhz` if present; we read
        // it per-snapshot so the operator sees current clock.
        freq_path: Option<PathBuf>,
    }

    pub(super) fn try_init() -> Option<IntelSysfs> {
        // Walk /sys/class/drm/card{0..9} (typically only card0/card1).
        for n in 0..10u32 {
            let base = PathBuf::from(format!("/sys/class/drm/card{n}/device"));
            if !base.exists() {
                continue;
            }
            let vendor = read_sysfs_string(&base.join("vendor")).unwrap_or_default();
            if vendor.trim() != "0x8086" {
                continue;
            }
            // Resolve a human-readable name. `device` is the PCI
            // device ID (e.g. 0xa780 for Raptor Lake-S UHD); we
            // fall back to a generic label if we can't map it.
            let device_id = read_sysfs_string(&base.join("device"))
                .ok()
                .unwrap_or_default();
            let name = intel_pci_name(device_id.trim()).to_string();

            let freq_path = ["gt/gt0/rps_cur_freq_mhz", "gt_cur_freq_mhz"]
                .iter()
                .map(|p| base.join(p))
                .find(|p| p.exists());

            tracing::info!(name = %name, "GPU backend: Intel iGPU (sysfs)");
            return Some(IntelSysfs { name, freq_path });
        }
        None
    }

    impl IntelSysfs {
        pub(super) fn snapshot(&self) -> GpuInfo {
            // Stitch current frequency into the name when we have
            // it so the operator dashboard isn't completely
            // static. Memory/util/temp truly aren't readable
            // without elevated caps so we honestly return None.
            let mut display = self.name.clone();
            if let Some(p) = &self.freq_path {
                if let Ok(s) = read_sysfs_string(p) {
                    if let Ok(mhz) = s.trim().parse::<u32>() {
                        display = format!("{} @ {mhz} MHz", self.name);
                    }
                }
            }
            GpuInfo {
                kind: "intel".to_string(),
                name: display,
                mem_total_bytes: None,
                mem_used_bytes: None,
                utilization_pct: None,
                temp_c: None,
            }
        }
    }

    /// Map a handful of common Intel iGPU PCI device IDs to
    /// friendly names. Anything unknown falls back to "Intel
    /// integrated graphics" + the raw ID for support tickets.
    fn intel_pci_name(device_id: &str) -> String {
        match device_id.to_ascii_lowercase().as_str() {
            // Alder Lake-N (T10 / N100)
            "0x46d0" | "0x46d1" | "0x46d2" | "0x46d3" | "0x46d4" => {
                "Intel UHD Graphics (Alder Lake-N)".to_string()
            }
            // Raptor Lake-S UHD (T24 family / N305 etc.)
            "0xa780" | "0xa781" | "0xa782" | "0xa783" => "Intel UHD Graphics 770".to_string(),
            // Iris Xe — Tiger Lake / Alder Lake-P
            "0x9a40" | "0x9a49" | "0x9a78" | "0x9ac0" | "0x9ac9" => {
                "Intel Iris Xe Graphics".to_string()
            }
            // Arc A-series (T36)
            "0x56a0" | "0x56a1" | "0x56a5" | "0x56a6" => "Intel Arc A380 / A580".to_string(),
            // Lunar Lake (T36-S)
            "0x6420" | "0x64a0" | "0x64b0" => "Intel Arc Graphics (Lunar Lake)".to_string(),
            other => format!("Intel integrated graphics ({other})"),
        }
    }
}

// ---------------------------------------------------------------------------
// macOS Apple Silicon backend (system_profiler).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod apple {
    use std::process::Command;

    use super::GpuInfo;

    pub(super) struct AppleStaticInfo {
        name: String,
    }

    pub(super) fn try_init() -> Option<AppleStaticInfo> {
        // `system_profiler SPDisplaysDataType -json` returns a
        // 200-ish KB JSON blob; first `sppci_model` is the
        // integrated GPU on M-series machines. Shell-out is fine
        // here — we only call it ONCE at process start. Timeout
        // is generous because system_profiler can take 1-3s.
        let out = Command::new("/usr/sbin/system_profiler")
            .arg("SPDisplaysDataType")
            .arg("-json")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let body = String::from_utf8_lossy(&out.stdout);
        // Cheap text scrape — pulling in serde_json for a
        // single field would be overkill. The JSON shape we
        // care about is:
        //   {"SPDisplaysDataType":[{"sppci_model":"Apple M2 Pro", ...}]}
        let key = "\"sppci_model\"";
        let idx = body.find(key)?;
        let after = &body[idx + key.len()..];
        let colon = after.find(':')?;
        let after = &after[colon + 1..];
        let start = after.find('"')?;
        let after = &after[start + 1..];
        let end = after.find('"')?;
        let name = after[..end].to_string();
        if name.is_empty() {
            return None;
        }
        tracing::info!(name = %name, "GPU backend: Apple Silicon (system_profiler)");
        Some(AppleStaticInfo { name })
    }

    impl AppleStaticInfo {
        pub(super) fn snapshot(&self) -> GpuInfo {
            // Apple Silicon GPUs share unified memory with the
            // CPU, so a discrete "VRAM" figure isn't meaningful;
            // utilization/temp require IOReport private API.
            // We honestly report just the model name.
            GpuInfo {
                kind: "apple".to_string(),
                name: self.name.clone(),
                mem_total_bytes: None,
                mem_used_bytes: None,
                utilization_pct: None,
                temp_c: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn read_sysfs_string(p: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(p)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_does_not_panic() {
        // Whatever backend resolves on the test host, calling
        // snapshot must succeed (returning Some or None) without
        // tripping the LazyLock mutex or panicking.
        let _ = snapshot();
    }
}
