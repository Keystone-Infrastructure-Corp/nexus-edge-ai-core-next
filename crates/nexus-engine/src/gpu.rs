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
// Linux Intel iGPU backend.
//
// Two sources, layered:
//
//   * **sysfs** (always available, unprivileged) — friendly
//     device name from PCI ID + live clock from
//     `gt/gt0/rps_cur_freq_mhz`.
//
//   * **`i915` PMU via `perf_event_open(2)`** (requires
//     `CAP_PERFMON` — granted by the shipped systemd unit) —
//     per-engine `*-busy` counters in nanoseconds. We open one
//     fd per engine at init, sample on each snapshot, and
//     compute % utilization as `(busy_ns_delta / (n_engines *
//     elapsed_ns)) * 100`. The denominator divides by engine
//     count so a fully-saturated render engine on a chip whose
//     blitter is idle still reads ~25% (1/4 engines), matching
//     `intel_gpu_top -L`'s reporting convention.
//
// If perf_event_open fails (`EACCES` on a kernel that requires
// the cap but the binary doesn't have it, or `ENOSYS` on
// ancient kernels), we log once at INFO and fall through to the
// sysfs-only path so the operator still sees the device name.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod intel {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Instant;

    use super::{read_sysfs_string, GpuInfo};

    /// Cached probe of the first Intel render node we find.
    pub(super) struct IntelSysfs {
        name: String,
        // Path to `gt/gt0/rps_cur_freq_mhz` if present; we read
        // it per-snapshot so the operator sees current clock.
        freq_path: Option<PathBuf>,
        // i915 PMU state. `None` when perf_event_open returned
        // `EACCES` / `ENOSYS` at init, or when the kernel
        // doesn't expose the `i915` event source at all.
        pmu: Option<Mutex<IntelPmu>>,
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

            let pmu = IntelPmu::try_open().map(Mutex::new);

            tracing::info!(
                name = %name,
                pmu_open = pmu.is_some(),
                "GPU backend: Intel iGPU (sysfs + PMU)",
            );
            return Some(IntelSysfs {
                name,
                freq_path,
                pmu,
            });
        }
        None
    }

    impl IntelSysfs {
        pub(super) fn snapshot(&self) -> GpuInfo {
            // Stitch current frequency into the name when we
            // have it so the operator dashboard isn't completely
            // static. Memory/temp truly aren't readable without
            // elevated caps so we honestly return None.
            let mut display = self.name.clone();
            if let Some(p) = &self.freq_path {
                if let Ok(s) = read_sysfs_string(p) {
                    if let Ok(mhz) = s.trim().parse::<u32>() {
                        display = format!("{} @ {mhz} MHz", self.name);
                    }
                }
            }
            let utilization_pct = self.pmu.as_ref().and_then(|m| {
                let mut guard = m.lock().ok()?;
                guard.snapshot()
            });
            GpuInfo {
                kind: "intel".to_string(),
                name: display,
                mem_total_bytes: None,
                mem_used_bytes: None,
                utilization_pct,
                temp_c: None,
            }
        }
    }

    /// Open and sample the `i915` PMU. Holds one fd per engine
    /// busy event plus the previous sample so we can compute
    /// deltas.
    pub(super) struct IntelPmu {
        // Each fd is an open `perf_event_open(2)` handle for an
        // `i915:<engine>-busy` event. Counter value is total
        // engine busy nanoseconds since fd creation; deltas give
        // us per-second utilization.
        engine_fds: Vec<OwnedFd>,
        // (Sample wall time, busy-ns per engine from the
        // previous read). `None` until the first snapshot warms
        // the baseline.
        last_sample: Option<(Instant, Vec<u64>)>,
    }

    impl IntelPmu {
        fn try_open() -> Option<Self> {
            let base = Path::new("/sys/bus/event_source/devices/i915");
            if !base.exists() {
                tracing::debug!(
                    "i915 PMU not exposed at /sys/bus/event_source/devices/i915 \
                     (no kernel module or pre-4.14 kernel); skipping",
                );
                return None;
            }
            let type_id: u32 = read_sysfs_u32(&base.join("type"))?;

            // Enumerate engine-busy events. Each event file
            // (e.g. `rcs0-busy`, `bcs0-busy`, `vcs0-busy`,
            // `vecs0-busy`) contains `event=0x...` with the raw
            // PMU config value. Variants depend on the chip:
            // a UHD 770 has 1 render + 1 blit + 2 video + 1
            // VEnh = 5 engines; Lunar Lake has different counts.
            let events_dir = base.join("events");
            let mut event_files: Vec<PathBuf> = std::fs::read_dir(&events_dir)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Only the plain `<engine>-busy` events,
                    // not the `*.unit` / `*.scale` metadata
                    // sidecar files the kernel writes next to
                    // each one.
                    name.ends_with("-busy") && !name.contains('.')
                })
                .collect();
            // Deterministic order so per-engine reads pair up
            // across samples even if readdir order shifts.
            event_files.sort();

            let mut engine_fds = Vec::with_capacity(event_files.len());
            for path in &event_files {
                let Some(config) = read_event_config(path) else {
                    continue;
                };
                match open_i915_event(type_id, config) {
                    Ok(fd) => engine_fds.push(fd),
                    Err(e) => {
                        if e == nix_eaccess() {
                            tracing::info!(
                                "i915 PMU open returned EACCES; the engine \
                                 process lacks CAP_PERFMON. GPU utilization will \
                                 be unavailable. Grant the cap via the systemd \
                                 unit (AmbientCapabilities=CAP_PERFMON) or run \
                                 with `--cap-add=PERFMON` under Docker.",
                            );
                            return None;
                        }
                        tracing::debug!(
                            error = e,
                            event = %path.display(),
                            "i915 PMU event open failed; skipping engine",
                        );
                    }
                }
            }
            if engine_fds.is_empty() {
                return None;
            }
            tracing::info!(
                engines = engine_fds.len(),
                "i915 PMU opened; sampling utilization on each /system/metrics call",
            );
            Some(IntelPmu {
                engine_fds,
                last_sample: None,
            })
        }

        fn snapshot(&mut self) -> Option<f32> {
            let now = Instant::now();
            let mut values = Vec::with_capacity(self.engine_fds.len());
            for fd in &self.engine_fds {
                let mut buf = [0u8; 8];
                // SAFETY: `read(2)` on a perf_event_open fd
                // always returns 8 bytes (single u64 counter)
                // when `read_format == 0` (our default). EINTR
                // isn't possible on a non-blocking sample read.
                let n = unsafe {
                    libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                };
                if n != buf.len() as isize {
                    return None;
                }
                values.push(u64::from_ne_bytes(buf));
            }
            let result = match &self.last_sample {
                Some((prev_t, prev_v)) if prev_v.len() == values.len() => {
                    let elapsed_ns = now.duration_since(*prev_t).as_nanos() as u64;
                    // <100 ms apart is too noisy (and the 1 s
                    // cache TTL above us should normally space
                    // them ~1 s).
                    if elapsed_ns < 100_000_000 {
                        None
                    } else {
                        let busy_ns: u64 = values
                            .iter()
                            .zip(prev_v.iter())
                            .map(|(c, p)| c.saturating_sub(*p))
                            .sum();
                        let n_engines = values.len() as u64;
                        let pct = (busy_ns as f64
                            / (n_engines as f64 * elapsed_ns as f64))
                            * 100.0;
                        Some((pct.clamp(0.0, 100.0)) as f32)
                    }
                }
                _ => None,
            };
            self.last_sample = Some((now, values));
            result
        }
    }

    /// Read a sysfs file and parse a single `u32`.
    fn read_sysfs_u32(p: &Path) -> Option<u32> {
        read_sysfs_string(p).ok()?.trim().parse().ok()
    }

    /// Parse the `event=0xNN` (or `event=0xNN,...`) line that
    /// lives in each `events/<name>` file under the PMU's sysfs
    /// directory. Only the `event=` term contributes to the
    /// PMU's `config` u64; other terms (when present) like
    /// `umask=` are folded into bits we don't use today.
    fn read_event_config(p: &Path) -> Option<u64> {
        let raw = read_sysfs_string(p).ok()?;
        for token in raw.trim().split(',') {
            if let Some(rest) = token.trim().strip_prefix("event=") {
                let rest = rest.trim();
                if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
                    return u64::from_str_radix(hex, 16).ok();
                }
                return rest.parse().ok();
            }
        }
        None
    }

    /// Returns the `EACCES` errno value so the caller can
    /// compare without leaking the `nix` crate's `Errno` enum
    /// through the public surface.
    fn nix_eaccess() -> i32 {
        libc::EACCES
    }

    /// Open one `i915` PMU event. Modelled after `intel_gpu_top`:
    ///   `pid = -1` (system-wide)
    ///   `cpu = 0`  (uncore PMU is per-device, attach to CPU 0)
    ///   `disabled = 0` (start counting immediately)
    fn open_i915_event(type_id: u32, config: u64) -> Result<OwnedFd, i32> {
        use perf_event_open_sys as pes;
        // SAFETY: zero-init is the documented baseline for
        // `perf_event_attr`. All bitfields (`disabled`,
        // `exclude_*`, ...) default to 0 which matches the
        // "start enabled, count kernel + hv" mode we want for
        // an uncore PMU counter.
        let mut attr: pes::bindings::perf_event_attr =
            unsafe { std::mem::zeroed() };
        attr.size = std::mem::size_of::<pes::bindings::perf_event_attr>() as u32;
        attr.type_ = type_id;
        attr.config = config;

        // SAFETY: `attr` has been zero-initialised and only
        // populated with the fields we care about. `pid=-1`,
        // `cpu=0`, `group_fd=-1`, `flags=0` is the standard
        // single-event uncore PMU invocation (same parameters
        // `intel_gpu_top` uses).
        let raw = unsafe { pes::perf_event_open(&mut attr, -1, 0, -1, 0) };
        if raw < 0 {
            // SAFETY: glibc / musl both expose `__errno_location`
            // as a thread-local pointer; the deref is always
            // valid on a live thread.
            let errno = unsafe { *libc::__errno_location() };
            return Err(errno);
        }
        // SAFETY: `raw` is a fresh, valid file descriptor we
        // just obtained from the syscall. Wrapping in `OwnedFd`
        // transfers close-on-drop ownership.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) })
    }

    use std::os::fd::FromRawFd;

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
