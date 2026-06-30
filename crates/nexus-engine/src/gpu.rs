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
    Amd(amd::AmdSysfs),
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
            // AMD before Intel: a board may carry an AMD APU/dGPU
            // (vendor 0x1002) and an Intel iGPU is mutually
            // exclusive on the AMD platforms we ship, but probing
            // AMD first keeps the discrete-Radeon path ahead of
            // any vestigial Intel display node.
            if let Some(state) = amd::try_init() {
                return GpuBackend::Amd(state);
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
            GpuBackend::Amd(state) => Some(state.snapshot()),
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
            let utilization_status = if util.is_none() {
                Some("NVML utilization_rates() returned an error".to_string())
            } else {
                None
            };
            Some(GpuInfo {
                kind: "nvidia".to_string(),
                name: self.name.clone(),
                mem_total_bytes: mem.as_ref().map(|m| m.total).or(self.mem_total),
                mem_used_bytes: mem.map(|m| m.used),
                utilization_pct: util,
                temp_c: temp,
                engines: Vec::new(),
                utilization_status,
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
    use std::collections::HashSet;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Instant;

    use super::{read_sysfs_string, GpuInfo};
    use crate::system_metrics::GpuEngineUtil;

    /// Cached probe of the first Intel render node we find.
    pub(super) struct IntelSysfs {
        name: String,
        // Path to `gt/gt0/rps_cur_freq_mhz` if present; we read
        // it per-snapshot so the operator sees current clock.
        freq_path: Option<PathBuf>,
        // GPU PMU state. `None` when neither the legacy `i915`
        // PMU nor the newer per-device `xe_<bdf>` PMU could be
        // opened (kernel too old, missing CAP_PERFMON, iGPU
        // unbound, etc). The failure reason is carried in
        // `pmu_init_error` so the System page can show it
        // inline.
        pmu: Option<Mutex<IntelPmuBackend>>,
        // Operator-facing reason when `pmu` is `None`. Mirrored
        // into `GpuInfo::utilization_status` on every snapshot
        // so the UI can render a specific hint ("missing
        // CAP_PERFMON", "i915 PMU not exposed by this kernel",
        // …) instead of a generic "not available" line.
        pmu_init_error: Option<String>,
    }

    /// Either of the two Intel GPU PMU surfaces. `i915` is the
    /// historical one (one event per engine, returning busy-ns
    /// directly); `xe` is the Lunar-Lake / Battlemage successor
    /// that exposes a per-device PMU (e.g.
    /// `xe_0000_00_02.0`) with two events (`engine-active-ticks`,
    /// `engine-total-ticks`) and an engine-class/instance encoded
    /// into `config`. We sample either with the same
    /// `snapshot()` signature so callers don't care which one
    /// is alive.
    pub(super) enum IntelPmuBackend {
        I915(IntelPmu),
        Xe(XePmu),
        /// Unprivileged per-engine utilisation read from the engine's
        /// own DRM client fdinfo (`/proc/self/fdinfo/*`). Preferred
        /// over the xe perf PMU on the `xe` driver: it surfaces every
        /// engine class (including the video-decode / video-enhance
        /// engines the xe PMU omits on Lunar Lake) and needs no
        /// CAP_PERFMON.
        Fdinfo(XeFdinfo),
    }

    /// Result of sampling either Intel GPU PMU: the overall
    /// aggregate utilization (unchanged from the historical
    /// single-number behaviour) plus a per-engine-class breakdown.
    #[derive(Default)]
    struct PmuSample {
        /// Overall 0–100 utilization, or `None` while the baseline
        /// warms up. Computed exactly as before the per-engine
        /// breakdown was added, so the headline number — and the
        /// Alder Lake path that depends on it — is untouched.
        overall: Option<f32>,
        /// `(class, pct)` per engine class, instances averaged.
        /// Empty whenever `overall` is `None`.
        engines: Vec<(&'static str, f32)>,
    }

    impl IntelPmuBackend {
        fn snapshot(&mut self) -> PmuSample {
            match self {
                IntelPmuBackend::I915(p) => p.snapshot(),
                IntelPmuBackend::Xe(p) => p.snapshot(),
                IntelPmuBackend::Fdinfo(p) => p.snapshot(),
            }
        }
    }

    /// Accumulate per-engine `(class, numerator, denominator)`
    /// readings into one entry per class (instances summed) and
    /// emit them in a stable display order. `numerator/denominator`
    /// is the class busy fraction: `busy_ns / elapsed_ns` for i915,
    /// `active_ticks / total_ticks` for xe.
    fn group_engine_classes(
        per_engine: impl IntoIterator<Item = (&'static str, f64, f64)>,
    ) -> Vec<(&'static str, f32)> {
        const ORDER: [&str; 6] = [
            "render",
            "video-decode",
            "video-enhance",
            "copy",
            "compute",
            "other",
        ];
        // At most six classes, so a linear find-or-insert is fine.
        let mut acc: Vec<(&'static str, f64, f64)> = Vec::new();
        for (class, num, den) in per_engine {
            if let Some(slot) = acc.iter_mut().find(|(c, _, _)| *c == class) {
                slot.1 += num;
                slot.2 += den;
            } else {
                acc.push((class, num, den));
            }
        }
        let mut out: Vec<(&'static str, f32)> = Vec::new();
        for class in ORDER {
            if let Some((_, num, den)) = acc.iter().find(|(c, _, _)| *c == class) {
                if *den > 0.0 {
                    out.push((class, ((num / den) * 100.0).clamp(0.0, 100.0) as f32));
                }
            }
        }
        out
    }

    /// Map an i915 `<engine>-busy` event basename (e.g. `vcs0-busy`,
    /// `vecs0-busy`) to a stable engine class. i915 abbreviates
    /// engines `rcs`/`bcs`/`vcs`/`vecs`/`ccs` with a trailing
    /// instance index.
    fn i915_engine_class(event_basename: &str) -> &'static str {
        let stem = event_basename
            .strip_suffix("-busy")
            .unwrap_or(event_basename);
        drm_engine_class(stem)
    }

    /// Map a DRM engine keystring (`rcs`, `vcs0`, `vecs`, `bcs`,
    /// `ccs`, …) to a stable display class. Shared by the i915
    /// `<engine>-busy` PMU event names and the xe
    /// `drm-(total-)cycles-<engine>` fdinfo keys, which use the same
    /// abbreviations (optionally with a trailing instance index).
    fn drm_engine_class(keystr: &str) -> &'static str {
        let prefix: String = keystr
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        match prefix.as_str() {
            "rcs" => "render",
            "bcs" => "copy",
            "vcs" => "video-decode",
            "vecs" => "video-enhance",
            "ccs" => "compute",
            _ => "other",
        }
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

            // PCI BDF (`0000:00:02.0`) from the symlink target;
            // the xe PMU is namespaced per-device as
            // `xe_<bdf-with-underscores>`.
            let pci_bdf = std::fs::read_link(&base).ok().and_then(|target| {
                target
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(str::to_string)
            });

            // Try the legacy i915 PMU first (covers Alder Lake-N,
            // Raptor Lake, Tiger Lake, Arc A-series); fall back
            // to the per-device xe PMU for Lunar Lake /
            // Battlemage / anything booted with the xe driver.
            let (pmu, pmu_init_error) = match IntelPmu::try_open() {
                Ok(p) => (Some(Mutex::new(IntelPmuBackend::I915(p))), None),
                // Not the legacy i915 driver. On the newer `xe` driver
                // (Lunar Lake / Battlemage) prefer the unprivileged
                // drm-fdinfo reader: it surfaces every engine class —
                // including the video-decode / video-enhance engines
                // that carry a camera decode workload and that the xe
                // perf PMU's engine-tick events omit on Lunar Lake —
                // and it needs no CAP_PERFMON. Fall back to the xe perf
                // PMU only if fdinfo can't be opened.
                Err(i915_reason) => match XeFdinfo::try_open(pci_bdf.as_deref()) {
                    Ok(p) => (Some(Mutex::new(IntelPmuBackend::Fdinfo(p))), None),
                    Err(fdinfo_reason) => match XePmu::try_open(pci_bdf.as_deref()) {
                        Ok(p) => (Some(Mutex::new(IntelPmuBackend::Xe(p))), None),
                        Err(xe_reason) => {
                            let combined = format!(
                                "i915 PMU: {i915_reason}; xe fdinfo: {fdinfo_reason}; \
                                 xe PMU: {xe_reason}"
                            );
                            tracing::warn!(
                                reason = %combined,
                                "no Intel GPU utilization source could be opened; \
                                 GPU utilization will be unavailable",
                            );
                            (None, Some(combined))
                        }
                    },
                },
            };

            tracing::info!(
                name = %name,
                pmu_open = pmu.is_some(),
                "GPU backend: Intel iGPU (sysfs + PMU)",
            );
            return Some(IntelSysfs {
                name,
                freq_path,
                pmu,
                pmu_init_error,
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
            let (utilization_pct, engines, utilization_status) = match &self.pmu {
                None => (None, Vec::new(), self.pmu_init_error.clone()),
                Some(m) => match m.lock() {
                    Err(_) => (None, Vec::new(), Some("PMU mutex poisoned".to_string())),
                    Ok(mut guard) => {
                        let sample = guard.snapshot();
                        match sample.overall {
                            Some(pct) => (
                                Some(pct),
                                sample
                                    .engines
                                    .into_iter()
                                    .map(|(class, util)| GpuEngineUtil {
                                        class: class.to_string(),
                                        utilization_pct: util,
                                    })
                                    .collect(),
                                None,
                            ),
                            None => (
                                None,
                                Vec::new(),
                                Some(
                                    "GPU PMU baseline warming up \u{2014} \
                                     a reading will appear after the next snapshot"
                                        .to_string(),
                                ),
                            ),
                        }
                    }
                },
            };
            GpuInfo {
                kind: "intel".to_string(),
                name: display,
                mem_total_bytes: None,
                mem_used_bytes: None,
                utilization_pct,
                temp_c: None,
                engines,
                utilization_status,
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
        // Engine class for each fd, in lockstep with `engine_fds`
        // (e.g. `vcs0-busy` → `"video-decode"`). Used to group the
        // per-engine deltas into the operator-facing breakdown.
        engine_classes: Vec<&'static str>,
        // (Sample wall time, busy-ns per engine from the
        // previous read). `None` until the first snapshot warms
        // the baseline.
        last_sample: Option<(Instant, Vec<u64>)>,
    }

    impl IntelPmu {
        fn try_open() -> Result<Self, String> {
            let base = Path::new("/sys/bus/event_source/devices/i915");
            if !base.exists() {
                return Err("/sys/bus/event_source/devices/i915 not present \u{2014} \
                     the kernel may use the newer `xe` driver (kernel \
                     6.8+ on Battlemage/Lunar Lake) or no DRM driver is \
                     bound to the iGPU"
                    .to_string());
            }
            let type_id: u32 = read_sysfs_u32(&base.join("type")).ok_or_else(|| {
                "could not read /sys/bus/event_source/devices/i915/type".to_string()
            })?;

            // Enumerate engine-busy events. Each event file
            // (e.g. `rcs0-busy`, `bcs0-busy`, `vcs0-busy`,
            // `vecs0-busy`) contains either `event=0x...` or
            // `config=0x...` with the raw PMU config value
            // (i915 on kernel 6.x emits the `config=` form;
            // standard CPU/uncore PMUs use `event=`). Variants
            // depend on the chip: a UHD 770 has 1 render + 1
            // blit + 2 video + 1 VEnh = 5 engines; Lunar Lake
            // has different counts.
            let events_dir = base.join("events");
            let mut event_files: Vec<PathBuf> = std::fs::read_dir(&events_dir)
                .map_err(|e| format!("could not enumerate {}: {e}", events_dir.display()))?
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

            if event_files.is_empty() {
                return Err(format!(
                    "no per-engine busy events under {} (kernel exposes the \
                     i915 PMU but with no `<engine>-busy` counters \u{2014} \
                     try a newer kernel)",
                    events_dir.display()
                ));
            }

            let total_events = event_files.len();
            let mut engine_fds = Vec::with_capacity(total_events);
            let mut engine_classes: Vec<&'static str> = Vec::with_capacity(total_events);
            let mut skipped: Vec<(String, i32)> = Vec::new();
            for path in &event_files {
                let Some(config) = read_event_config(path) else {
                    skipped.push((short_name(path), -1));
                    continue;
                };
                match open_i915_event(type_id, config) {
                    Ok(fd) => {
                        engine_fds.push(fd);
                        engine_classes.push(i915_engine_class(&short_name(path)));
                    }
                    Err(e) => {
                        if e == nix_eaccess() || e == libc::EPERM {
                            // First EACCES/EPERM is decisive — the
                            // kernel rejected the open because the
                            // process lacks CAP_PERFMON (or
                            // perf_event_paranoid ≥ 3 on a stock
                            // Ubuntu kernel). Bail with a specific
                            // reason; no further events will succeed.
                            return Err(format!(
                                "perf_event_open returned {} on {} \u{2014} the \
                                 engine process is missing CAP_PERFMON. \
                                 Grant it via the systemd unit \
                                 (AmbientCapabilities=CAP_PERFMON; this is \
                                 the default in v0.1.14+) or run with \
                                 `--cap-add=PERFMON` under Docker. Check \
                                 `grep CapEff /proc/$(pgrep nexus-engine)/status` \
                                 \u{2014} CAP_PERFMON is bit 38 (0x4000000000).",
                                errno_name(e),
                                short_name(path),
                            ));
                        }
                        tracing::warn!(
                            errno = e,
                            event = %path.display(),
                            "i915 PMU event open failed; skipping engine",
                        );
                        skipped.push((short_name(path), e));
                    }
                }
            }
            if engine_fds.is_empty() {
                return Err(format!(
                    "all {total_events} i915 PMU event opens failed (errnos: {skipped:?})",
                ));
            }
            tracing::info!(
                engines = engine_fds.len(),
                total = total_events,
                "i915 PMU opened; sampling utilization on each /system/metrics call",
            );
            Ok(IntelPmu {
                engine_fds,
                engine_classes,
                last_sample: None,
            })
        }

        fn snapshot(&mut self) -> PmuSample {
            let now = Instant::now();
            let mut values = Vec::with_capacity(self.engine_fds.len());
            for fd in &self.engine_fds {
                let mut buf = [0u8; 8];
                // SAFETY: `read(2)` on a perf_event_open fd
                // always returns 8 bytes (single u64 counter)
                // when `read_format == 0` (our default). EINTR
                // isn't possible on a non-blocking sample read.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n != buf.len() as isize {
                    return PmuSample::default();
                }
                values.push(u64::from_ne_bytes(buf));
            }
            let mut sample = PmuSample::default();
            if let Some((prev_t, prev_v)) = &self.last_sample {
                if prev_v.len() == values.len() {
                    let elapsed_ns = now.duration_since(*prev_t).as_nanos() as u64;
                    // <100 ms apart is too noisy (and the 1 s
                    // cache TTL above us should normally space
                    // them ~1 s).
                    if elapsed_ns >= 100_000_000 {
                        let busy_ns: u64 = values
                            .iter()
                            .zip(prev_v.iter())
                            .map(|(c, p)| c.saturating_sub(*p))
                            .sum();
                        let n_engines = values.len() as u64;
                        let pct = (busy_ns as f64 / (n_engines as f64 * elapsed_ns as f64)) * 100.0;
                        sample.overall = Some(pct.clamp(0.0, 100.0) as f32);
                        // Per-engine: each engine's own busy-ns over
                        // the same elapsed window; instances of a
                        // class are averaged by group_engine_classes.
                        sample.engines = group_engine_classes(
                            self.engine_classes
                                .iter()
                                .zip(values.iter().zip(prev_v.iter()))
                                .map(|(class, (c, p))| {
                                    (*class, c.saturating_sub(*p) as f64, elapsed_ns as f64)
                                }),
                        );
                    }
                }
            }
            self.last_sample = Some((now, values));
            sample
        }
    }

    // -----------------------------------------------------------------
    // xe PMU backend (Lunar Lake / Battlemage and any future Intel
    // GPU booted under the new `xe` driver instead of `i915`). The
    // shape is intentionally different from `IntelPmu`:
    //
    //   * the PMU is per-device (e.g. `xe_0000_00_02.0`), not
    //     shared across all Intel cards;
    //   * the kernel exposes ONE event for active ticks and ONE
    //     for total ticks (`engine-active-ticks`, `engine-total-ticks`),
    //     with the engine identity packed into `config`:
    //       bits  0..11  event id (0x02 = active, 0x03 = total)
    //       bits 12..19  engine instance
    //       bits 20..27  engine class (0=render, 1=copy,
    //                    2=video-decode, 3=video-enhance, 4=compute)
    //       bits 60..63  gt (always 0 on consumer iGPUs)
    //
    // We probe (class, instance) pairs against the kernel via
    // `perf_event_open`: kernels reject unknown engines with
    // `ENOENT`, so we open one (active, total) pair per engine
    // the device actually exposes. Utilization per snapshot is
    // `sum(\u0394active) / sum(\u0394total) * 100`.
    // -----------------------------------------------------------------

    /// Each open xe engine — one (active, total) fd pair plus a
    /// short label kept for tracing.
    struct XeEngine {
        active_fd: OwnedFd,
        total_fd: OwnedFd,
        label: String,
        // Engine class (`"render"`, `"video-decode"`, …) — the
        // second element of the `class_labels` table. Used to
        // group per-engine readings into the operator-facing
        // breakdown.
        class: &'static str,
    }

    pub(super) struct XePmu {
        engines: Vec<XeEngine>,
        // Wall clock + per-engine (active, total) reading from
        // the previous snapshot. `None` until the first sample
        // warms the baseline (same contract as `IntelPmu`).
        last_sample: Option<(Instant, Vec<(u64, u64)>)>,
    }

    impl XePmu {
        fn try_open(pci_bdf: Option<&str>) -> Result<Self, String> {
            let devices_root = Path::new("/sys/bus/event_source/devices");

            // Pick the `xe_<bdf>` PMU entry. If the caller knew
            // which PCI device we care about, prefer the matching
            // one; otherwise the first xe entry wins (single-iGPU
            // boxes are the common case).
            let wanted = pci_bdf.map(|bdf| format!("xe_{}", bdf.replace(':', "_")));
            let mut chosen: Option<PathBuf> = None;
            let mut all_xe: Vec<String> = Vec::new();
            match std::fs::read_dir(devices_root) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if !name.starts_with("xe_") {
                            continue;
                        }
                        all_xe.push(name.clone());
                        if let Some(w) = wanted.as_deref() {
                            if name == w {
                                chosen = Some(e.path());
                                break;
                            }
                        } else if chosen.is_none() {
                            chosen = Some(e.path());
                        }
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "could not enumerate {}: {e}",
                        devices_root.display()
                    ));
                }
            }
            let base = chosen.ok_or_else(|| {
                if all_xe.is_empty() {
                    "no xe_<bdf> PMU entries under \
                     /sys/bus/event_source/devices/ (kernel may use \
                     the legacy i915 driver, which the i915 path \
                     above handles)"
                        .to_string()
                } else {
                    format!("no xe_<bdf> entry matches this card; saw {all_xe:?}")
                }
            })?;

            let type_id: u32 = read_sysfs_u32(&base.join("type"))
                .ok_or_else(|| format!("could not read {}/type", base.display()))?;

            // Confirm the two events we rely on are actually
            // present. We don't trust hard-coded 0x02/0x03 values
            // \u2014 read them from the sysfs `events/` files so the
            // backend keeps working if a future xe revision
            // shuffles event IDs.
            let events_dir = base.join("events");
            let active_event = read_event_config(&events_dir.join("engine-active-ticks"))
                .ok_or_else(|| {
                    format!(
                        "{} missing the `engine-active-ticks` event \
                         (kernel xe PMU layout changed?)",
                        events_dir.display()
                    )
                })?;
            let total_event = read_event_config(&events_dir.join("engine-total-ticks"))
                .ok_or_else(|| {
                    format!(
                        "{} missing the `engine-total-ticks` event \
                         (kernel xe PMU layout changed?)",
                        events_dir.display()
                    )
                })?;

            // The xe PMU is rooted on a single CPU (cpumask = "0"
            // on Lunar Lake, may differ on multi-socket gear). We
            // honour whatever the kernel says so we don't get
            // EINVAL when opening on the wrong CPU.
            let cpu = read_sysfs_string(&base.join("cpumask"))
                .ok()
                .and_then(|s| {
                    s.trim()
                        .split(&[',', '-'][..])
                        .next()
                        .and_then(|t| t.parse::<i32>().ok())
                })
                .unwrap_or(0);

            // Probe (class, instance) pairs against the kernel.
            // Engine classes from `include/uapi/drm/xe_drm.h`:
            //   0 = DRM_XE_ENGINE_CLASS_RENDER
            //   1 = DRM_XE_ENGINE_CLASS_COPY
            //   2 = DRM_XE_ENGINE_CLASS_VIDEO_DECODE
            //   3 = DRM_XE_ENGINE_CLASS_VIDEO_ENHANCE
            //   4 = DRM_XE_ENGINE_CLASS_COMPUTE
            // gt=0 covers every single-tile consumer iGPU. We
            // walk instances 0..16 per class \u2014 the kernel returns
            // ENOENT/EINVAL for absent instances which we silently
            // skip.
            let class_labels = [
                (0u64, "render"),
                (1, "copy"),
                (2, "video-decode"),
                (3, "video-enhance"),
                (4, "compute"),
            ];
            let mut engines: Vec<XeEngine> = Vec::new();
            let mut first_unexpected_errno: Option<i32> = None;
            for &(class, label_class) in &class_labels {
                for instance in 0u64..16 {
                    let cfg_base = (class << 20) | (instance << 12);
                    let cfg_active = cfg_base | active_event;
                    let cfg_total = cfg_base | total_event;
                    let active_fd = match open_pmu_event(type_id, cfg_active, cpu) {
                        Ok(fd) => fd,
                        Err(e) => {
                            if e == libc::EACCES || e == libc::EPERM {
                                return Err(format!(
                                    "perf_event_open returned {} on xe PMU \u{2014} the \
                                     engine process is missing CAP_PERFMON. \
                                     Grant it via the systemd unit \
                                     (AmbientCapabilities=CAP_PERFMON; this is \
                                     the default in v0.1.14+). Check \
                                     `grep CapEff /proc/$(pgrep nexus-engine)/status` \
                                     \u{2014} CAP_PERFMON is bit 38 (0x4000000000).",
                                    errno_name(e),
                                ));
                            }
                            // ENOENT / EINVAL just mean this
                            // (class, instance) pair isn't an
                            // engine on this chip \u2014 keep walking.
                            if e != libc::ENOENT && e != libc::EINVAL {
                                first_unexpected_errno.get_or_insert(e);
                            }
                            continue;
                        }
                    };
                    let total_fd = match open_pmu_event(type_id, cfg_total, cpu) {
                        Ok(fd) => fd,
                        Err(e) => {
                            tracing::warn!(
                                errno = e,
                                class = label_class,
                                instance,
                                "xe PMU active event opened but total event did not; skipping engine",
                            );
                            // Drop active_fd by letting it go
                            // out of scope (OwnedFd closes on
                            // Drop).
                            drop(active_fd);
                            continue;
                        }
                    };
                    engines.push(XeEngine {
                        active_fd,
                        total_fd,
                        label: format!("{label_class}{instance}"),
                        class: label_class,
                    });
                }
            }
            if engines.is_empty() {
                return Err(format!(
                    "no xe engines could be opened under {} (last unexpected errno: {:?})",
                    base.display(),
                    first_unexpected_errno.map(errno_name),
                ));
            }
            let labels: Vec<&str> = engines.iter().map(|e| e.label.as_str()).collect();
            tracing::info!(
                pmu = %base.display(),
                engines = engines.len(),
                engine_list = ?labels,
                "xe PMU opened; sampling utilization on each /system/metrics call",
            );
            Ok(XePmu {
                engines,
                last_sample: None,
            })
        }

        fn snapshot(&mut self) -> PmuSample {
            let now = Instant::now();
            let mut values = Vec::with_capacity(self.engines.len());
            for eng in &self.engines {
                let mut a = [0u8; 8];
                let mut t = [0u8; 8];
                // SAFETY: perf event fds always return exactly
                // 8 bytes for the default read_format (a single
                // u64 counter value).
                let na = unsafe { libc::read(eng.active_fd.as_raw_fd(), a.as_mut_ptr().cast(), 8) };
                let nt = unsafe { libc::read(eng.total_fd.as_raw_fd(), t.as_mut_ptr().cast(), 8) };
                if na != 8 || nt != 8 {
                    return PmuSample::default();
                }
                values.push((u64::from_ne_bytes(a), u64::from_ne_bytes(t)));
            }
            let mut sample = PmuSample::default();
            if let Some((prev_t, prev_v)) = &self.last_sample {
                if prev_v.len() == values.len() {
                    let elapsed_ns = now.duration_since(*prev_t).as_nanos() as u64;
                    if elapsed_ns >= 100_000_000 {
                        let mut active_delta: u64 = 0;
                        let mut total_delta: u64 = 0;
                        for ((a, t), (pa, pt)) in values.iter().zip(prev_v.iter()) {
                            active_delta = active_delta.saturating_add(a.saturating_sub(*pa));
                            total_delta = total_delta.saturating_add(t.saturating_sub(*pt));
                        }
                        if total_delta != 0 {
                            let pct = (active_delta as f64 / total_delta as f64) * 100.0;
                            sample.overall = Some(pct.clamp(0.0, 100.0) as f32);
                            // Per-engine: each engine's own active /
                            // total ticks; instances of a class are
                            // averaged by group_engine_classes.
                            sample.engines = group_engine_classes(
                                self.engines
                                    .iter()
                                    .zip(values.iter().zip(prev_v.iter()))
                                    .map(|(eng, ((a, t), (pa, pt)))| {
                                        (
                                            eng.class,
                                            a.saturating_sub(*pa) as f64,
                                            t.saturating_sub(*pt) as f64,
                                        )
                                    }),
                            );
                        }
                    }
                }
            }
            self.last_sample = Some((now, values));
            sample
        }
    }

    // -----------------------------------------------------------------
    // xe drm-fdinfo backend (PREFERRED for the `xe` driver).
    //
    // Reads the engine's OWN DRM clients from `/proc/self/fdinfo/*`
    // (the in-process GStreamer VA decode + VA postproc contexts) and
    // sums the standard DRM usage-stats cycle counters per engine
    // class:
    //
    //   drm-cycles-<eng>        busy cycles for this client on <eng>
    //   drm-total-cycles-<eng>  free-running <eng> cycle timeline
    //
    //   util(class) = Δ(Σ_clients drm-cycles) / Δ(drm-total-cycles) ·100
    //
    // Why this instead of the xe perf PMU: on Lunar Lake the xe PMU's
    // `engine-active-ticks` / `engine-total-ticks` events do not
    // surface the video-decode / video-enhance engines (precisely the
    // busy ones under a camera decode workload), and opening them
    // needs CAP_PERFMON. drm-fdinfo is per-engine, exposes the video
    // engines, and the unprivileged `nexus` process can read its own
    // fds with no extra capability — so the metrics populate out of
    // the box.
    // -----------------------------------------------------------------

    /// One fdinfo sweep: per-class `(class, busy_cycles_summed,
    /// total_cycles_repr)` plus the wall time it was taken so the next
    /// sweep can delta against it. `total_cycles_repr` is the max
    /// across clients — every client samples the same shared engine
    /// timeline, just at slightly different instants.
    struct XeFdinfoSample {
        at: Instant,
        per_class: Vec<(&'static str, u64, u64)>,
    }

    /// One parsed DRM client from a `/proc/<pid>/fdinfo/<fd>` file:
    /// its `drm-client-id` plus `(class, busy_cycles, total_cycles)`
    /// merged per engine class (multiple instances of one class within
    /// a single client — e.g. `vcs0` + `vcs1` — sum their busy cycles
    /// and share the engine timeline).
    struct FdinfoClient {
        id: u64,
        engines: Vec<(&'static str, u64, u64)>,
    }

    /// Parse one fdinfo file body. Returns `None` for any fd that is
    /// not a DRM client (no `drm-client-id` key) so the caller can
    /// skip sockets/pipes/regular files cheaply. Pure (no I/O) so the
    /// `vcs`/`vecs` class disambiguation and the busy/total merge are
    /// unit-testable without a live `/proc`.
    fn parse_fdinfo_client(content: &str) -> Option<FdinfoClient> {
        // Cheap pre-filter: only DRM client fds carry this key.
        if !content.contains("drm-client-id") {
            return None;
        }
        let mut id: Option<u64> = None;
        let mut engines: Vec<(&'static str, u64, u64)> = Vec::new();
        // Find-or-insert the `(class, busy, total)` slot for a class.
        fn slot<'a>(
            acc: &'a mut Vec<(&'static str, u64, u64)>,
            class: &'static str,
        ) -> &'a mut (&'static str, u64, u64) {
            if let Some(i) = acc.iter().position(|(c, _, _)| *c == class) {
                &mut acc[i]
            } else {
                acc.push((class, 0, 0));
                acc.last_mut().expect("just pushed")
            }
        }
        for line in content.lines() {
            let Some((key, val)) = line.split_once(':') else {
                continue;
            };
            let (key, val) = (key.trim(), val.trim());
            if key == "drm-client-id" {
                id = val.parse().ok();
            } else if let Some(eng) = key.strip_prefix("drm-total-cycles-") {
                // Must be tested before the `drm-cycles-` prefix below;
                // it is a longer prefix on the same lines' siblings.
                if let Ok(v) = val.parse::<u64>() {
                    let s = slot(&mut engines, drm_engine_class(eng));
                    s.2 = s.2.max(v);
                }
            } else if let Some(eng) = key.strip_prefix("drm-cycles-") {
                if let Ok(v) = val.parse::<u64>() {
                    slot(&mut engines, drm_engine_class(eng)).1 += v;
                }
            }
        }
        Some(FdinfoClient { id: id?, engines })
    }

    pub(super) struct XeFdinfo {
        fdinfo_dir: PathBuf,
        last: Option<XeFdinfoSample>,
    }

    impl XeFdinfo {
        fn try_open(pci_bdf: Option<&str>) -> Result<Self, String> {
            // Only take this path for a real `xe`-driver GPU. The xe
            // driver always registers a per-device PMU node at
            // /sys/bus/event_source/devices/xe_<bdf> even though we
            // sample utilisation from fdinfo, so its presence is a
            // reliable "this iGPU runs on xe" signal that costs no
            // capability to check.
            let devices_root = Path::new("/sys/bus/event_source/devices");
            let wanted = pci_bdf.map(|bdf| format!("xe_{}", bdf.replace(':', "_")));
            let saw_xe = std::fs::read_dir(devices_root).is_ok_and(|rd| {
                rd.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    match wanted.as_deref() {
                        Some(w) => name == w,
                        None => name.starts_with("xe_"),
                    }
                })
            });
            if !saw_xe {
                return Err("no xe_<bdf> device under /sys/bus/event_source/devices \
                     (iGPU is not bound to the xe driver)"
                    .to_string());
            }
            let fdinfo_dir = PathBuf::from("/proc/self/fdinfo");
            if !fdinfo_dir.is_dir() {
                return Err("/proc/self/fdinfo is not a readable directory".to_string());
            }
            tracing::info!(
                fdinfo = %fdinfo_dir.display(),
                "xe drm-fdinfo opened; sampling per-engine GPU utilization on each /system/metrics call",
            );
            Ok(XeFdinfo {
                fdinfo_dir,
                last: None,
            })
        }

        /// Sweep `/proc/self/fdinfo`, dedupe DRM clients by
        /// `drm-client-id`, and accumulate the per-engine-class cycle
        /// counters: busy cycles summed across distinct clients, total
        /// (engine timeline) cycles taken as the per-class max.
        fn read_per_class(&self) -> Vec<(&'static str, u64, u64)> {
            let mut seen: HashSet<u64> = HashSet::new();
            let mut acc: Vec<(&'static str, u64, u64)> = Vec::new();
            let Ok(rd) = std::fs::read_dir(&self.fdinfo_dir) else {
                return acc;
            };
            for ent in rd.flatten() {
                let Ok(content) = std::fs::read_to_string(ent.path()) else {
                    // procfs fdinfo for sockets/pipes/etc. — or a fd
                    // that closed mid-sweep. Skip; never fail the whole
                    // snapshot for one unreadable entry.
                    continue;
                };
                let Some(client) = parse_fdinfo_client(&content) else {
                    continue;
                };
                // Several fds can alias one DRM client (dup'd handles);
                // the cycle counters are identical, so count each
                // client exactly once.
                if !seen.insert(client.id) {
                    continue;
                }
                for (class, busy, total) in client.engines {
                    if let Some(s) = acc.iter_mut().find(|(c, _, _)| *c == class) {
                        s.1 += busy;
                        s.2 = s.2.max(total);
                    } else {
                        acc.push((class, busy, total));
                    }
                }
            }
            acc
        }

        fn snapshot(&mut self) -> PmuSample {
            let now = Instant::now();
            let per_class = self.read_per_class();
            let mut sample = PmuSample::default();
            if let Some(prev) = &self.last {
                // <100 ms apart is too noisy; the 1 s cache TTL above
                // us normally spaces snapshots ~1 s.
                if now.duration_since(prev.at).as_millis() >= 100 {
                    let mut deltas: Vec<(&'static str, f64, f64)> =
                        Vec::with_capacity(per_class.len());
                    for &(class, busy_now, total_now) in &per_class {
                        let (busy_prev, total_prev) = prev
                            .per_class
                            .iter()
                            .find(|&&(c, _, _)| c == class)
                            .map(|&(_, b, t)| (b, t))
                            .unwrap_or((0, 0));
                        deltas.push((
                            class,
                            busy_now.saturating_sub(busy_prev) as f64,
                            total_now.saturating_sub(total_prev) as f64,
                        ));
                    }
                    let engines = group_engine_classes(deltas);
                    // Headline = busiest engine so the "GPU %" honestly
                    // reflects that the GPU is working (render alone is
                    // a misleading 0 under a pure decode workload).
                    let overall = engines.iter().map(|(_, p)| *p).fold(0.0_f32, f32::max);
                    sample.overall = Some(overall);
                    sample.engines = engines;
                }
            }
            self.last = Some(XeFdinfoSample { at: now, per_class });
            sample
        }
    }

    /// Variant of `open_i915_event` that takes an explicit CPU
    /// argument. The xe PMU's `cpumask` may be non-zero on
    /// multi-socket systems, so the caller resolves it from
    /// sysfs and passes it through here.
    fn open_pmu_event(type_id: u32, config: u64, cpu: i32) -> Result<OwnedFd, i32> {
        use perf_event_open_sys as pes;
        // SAFETY: zero-init is the documented baseline for
        // `perf_event_attr`. All bitfields default to 0 which
        // matches "start enabled, count kernel + hv".
        let mut attr: pes::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
        attr.size = std::mem::size_of::<pes::bindings::perf_event_attr>() as u32;
        attr.type_ = type_id;
        attr.config = config;
        // SAFETY: `attr` populated above; pid=-1 (system-wide),
        // group_fd=-1, flags=0 \u2014 standard uncore PMU call.
        let raw = unsafe { pes::perf_event_open(&mut attr, -1, cpu, -1, 0) };
        if raw < 0 {
            // SAFETY: glibc/musl TLS pointer, valid on a live
            // thread.
            let errno = unsafe { *libc::__errno_location() };
            return Err(errno);
        }
        // SAFETY: fresh fd from syscall, OwnedFd transfers
        // close-on-drop ownership.
        Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
    }

    /// Read a sysfs file and parse a single `u32`.
    fn read_sysfs_u32(p: &Path) -> Option<u32> {
        read_sysfs_string(p).ok()?.trim().parse().ok()
    }

    /// Parse the value line that lives in each `events/<name>`
    /// file under the PMU's sysfs directory and return what
    /// goes into `perf_event_attr.config`. Two prefixes are
    /// recognised:
    ///   * `event=0xNN` — CPU / uncore PMUs (Intel cstate,
    ///     Intel uncore_imc, AMD core, etc.).
    ///   * `config=0xNN` — Intel i915 PMU (its only format
    ///     field is `i915_eventid` mapped to `config:0-20`,
    ///     and the kernel emits per-engine `<engine>-busy`
    ///     files using the raw `config=` form on 6.x).
    ///
    /// Other terms (`umask=`, ...) when present are ignored.
    fn read_event_config(p: &Path) -> Option<u64> {
        let raw = read_sysfs_string(p).ok()?;
        for token in raw.trim().split(',') {
            let token = token.trim();
            let rest = token
                .strip_prefix("event=")
                .or_else(|| token.strip_prefix("config="));
            if let Some(rest) = rest {
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
        let mut attr: pes::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
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

    /// Best-effort short label for an event sysfs path
    /// (e.g. `/sys/bus/event_source/devices/i915/events/rcs0-busy`
    /// → `rcs0-busy`). Falls back to the full display path when
    /// the basename is missing for some reason.
    fn short_name(p: &Path) -> String {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| p.display().to_string())
    }

    /// Render a small set of operator-visible errno values
    /// (everything we expect to see from `perf_event_open` on
    /// the iGPU PMU) into a short string. Anything else falls
    /// back to the numeric value.
    fn errno_name(e: i32) -> String {
        match e {
            libc::EACCES => "EACCES".to_string(),
            libc::EPERM => "EPERM".to_string(),
            libc::ENOENT => "ENOENT".to_string(),
            libc::ENODEV => "ENODEV".to_string(),
            libc::ENOSYS => "ENOSYS".to_string(),
            libc::EINVAL => "EINVAL".to_string(),
            other => format!("errno {other}"),
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
            // (0x46a6 is Alder Lake-P GT2 — GMKTec NucBox M3
            // and similar 12th-gen Intel mini-PCs.)
            "0x9a40" | "0x9a49" | "0x9a78" | "0x9ac0" | "0x9ac9" | "0x46a6" => {
                "Intel Iris Xe Graphics".to_string()
            }
            // Arc A-series (T36)
            "0x56a0" | "0x56a1" | "0x56a5" | "0x56a6" => "Intel Arc A380 / A580".to_string(),
            // Lunar Lake (T36-S)
            "0x6420" | "0x64a0" | "0x64b0" => "Intel Arc Graphics (Lunar Lake)".to_string(),
            other => format!("Intel integrated graphics ({other})"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::read_event_config;
        use super::{drm_engine_class, group_engine_classes, parse_fdinfo_client};
        use std::io::Write;

        fn write_sysfs(content: &str) -> tempfile::NamedTempFile {
            let mut f = tempfile::NamedTempFile::new().expect("tempfile");
            f.write_all(content.as_bytes()).expect("write");
            f
        }

        #[test]
        fn parses_event_prefix_used_by_cpu_and_uncore_pmus() {
            let f = write_sysfs("event=0x2a\n");
            assert_eq!(read_event_config(f.path()), Some(0x2a));
        }

        #[test]
        fn parses_config_prefix_used_by_i915_pmu() {
            // Real samples captured on an Alder Lake-P Iris Xe
            // running kernel 6.17; this is the form that broke
            // utilization sampling before the parser was widened.
            for (raw, want) in [
                ("config=0x0\n", 0x0),
                ("config=0x1000\n", 0x1000),
                ("config=0x2010\n", 0x2010),
                ("config=0x3000\n", 0x3000),
            ] {
                let f = write_sysfs(raw);
                assert_eq!(
                    read_event_config(f.path()),
                    Some(want),
                    "failed to parse {raw:?}",
                );
            }
        }

        #[test]
        fn returns_none_when_no_recognised_prefix_present() {
            let f = write_sysfs("umask=0xff,inv=1\n");
            assert_eq!(read_event_config(f.path()), None);
        }

        #[test]
        fn drm_engine_keys_map_to_stable_classes() {
            // The xe driver emits bare engine abbreviations (no
            // instance suffix) in fdinfo; i915 appends an index. Both
            // must resolve to the same class — and `vcs` (decode) must
            // not be confused with `vecs` (enhance), the exact bug that
            // hid the busy video engines.
            assert_eq!(drm_engine_class("rcs"), "render");
            assert_eq!(drm_engine_class("bcs"), "copy");
            assert_eq!(drm_engine_class("vcs"), "video-decode");
            assert_eq!(drm_engine_class("vcs0"), "video-decode");
            assert_eq!(drm_engine_class("vecs"), "video-enhance");
            assert_eq!(drm_engine_class("vecs0"), "video-enhance");
            assert_eq!(drm_engine_class("ccs"), "compute");
            assert_eq!(drm_engine_class("xyz"), "other");
        }

        #[test]
        fn parse_fdinfo_skips_non_drm_fds() {
            // A pipe/socket fdinfo has no `drm-client-id`.
            assert!(parse_fdinfo_client("pos:\t0\nflags:\t0100002\n").is_none());
        }

        #[test]
        fn parse_fdinfo_reads_xe_engine_cycles() {
            // Verbatim shape captured from the production Lunar Lake
            // box (`/proc/<engine-pid>/fdinfo/<fd>`, xe driver).
            let body = "\
pos:\t0
drm-driver:\txe
drm-client-id:\t339
drm-pdev:\t0000:00:02.0
drm-cycles-rcs:\t0
drm-total-cycles-rcs:\t8077280786690
drm-cycles-vcs:\t1202534501
drm-total-cycles-vcs:\t8077280786690
drm-cycles-vecs:\t1098801757
drm-total-cycles-vecs:\t8077280786690
drm-cycles-bcs:\t0
drm-total-cycles-bcs:\t8077280786690
drm-cycles-ccs:\t0
drm-total-cycles-ccs:\t8077280786690
";
            let client = parse_fdinfo_client(body).expect("is a drm client");
            assert_eq!(client.id, 339);
            let get = |class: &str| {
                client
                    .engines
                    .iter()
                    .find(|(c, _, _)| *c == class)
                    .map(|&(_, b, t)| (b, t))
            };
            assert_eq!(get("video-decode"), Some((1202534501, 8077280786690)));
            assert_eq!(get("video-enhance"), Some((1098801757, 8077280786690)));
            assert_eq!(get("render"), Some((0, 8077280786690)));
            assert_eq!(get("copy"), Some((0, 8077280786690)));
            assert_eq!(get("compute"), Some((0, 8077280786690)));
        }

        #[test]
        fn group_engine_classes_computes_busy_fraction_in_display_order() {
            // Two decode deltas accumulate; render is idle. Output is
            // in canonical display order and skips zero-denominator
            // classes.
            let out = group_engine_classes([
                ("render", 0.0, 1000.0),
                ("video-decode", 177.0, 1000.0),
                ("video-enhance", 176.0, 1000.0),
                ("copy", 0.0, 0.0), // den == 0 → dropped
            ]);
            let classes: Vec<&str> = out.iter().map(|(c, _)| *c).collect();
            assert_eq!(classes, vec!["render", "video-decode", "video-enhance"]);
            let pct = |class: &str| out.iter().find(|(c, _)| *c == class).map(|(_, p)| *p);
            assert!((pct("render").unwrap() - 0.0).abs() < 0.01);
            assert!((pct("video-decode").unwrap() - 17.7).abs() < 0.01);
            assert!((pct("video-enhance").unwrap() - 17.6).abs() < 0.01);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux AMD (amdgpu) backend.
//
// Unlike the Intel iGPU — whose utilization needs CAP_PERFMON via the
// i915/xe PMU — `amdgpu` publishes everything we need as plain,
// world-readable sysfs files under `/sys/class/drm/card*/device/`:
//
//   * `vendor`               — must be `0x1002` (AMD/ATI)
//   * `device`               — PCI device ID → friendly name
//   * `gpu_busy_percent`     — integer 0..100 GPU activity (the same
//                              counter `rocm-smi --showuse` reads)
//   * `mem_info_vram_total`  — VRAM (or APU carve-out) total bytes
//   * `mem_info_vram_used`   — VRAM bytes currently allocated
//   * `hwmon/hwmon*/temp1_input` — edge temperature in millidegrees C
//
// All of these are readable by the unprivileged `nexus` user, so no
// CAP_PERFMON / sudo is required and there is no PMU baseline to warm
// up — every snapshot reports a live value. This covers the AMD APU
// tiers (e.g. Beelink EQR7 / Ryzen Radeon 680M, gfx1035) running the
// ROCm execution provider. NOTE: `gpu_busy_percent` on these APUs is a
// coarse 0/100 instantaneous gauge that spikes to 100 for ~1 ms per
// inference burst and reads 0 otherwise, so a single read almost always
// catches an idle gap. A background sampler thread polls it at 50 ms and
// publishes a rolling multi-second mean (the GPU-busy duty cycle), which
// `snapshot()` returns as `utilization_pct` — a stable value that tracks
// real load instead of flickering between 0 and 100.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod amd {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{read_sysfs_string, GpuInfo};

    /// Background sampler cadence. `gpu_busy_percent` on AMD APUs is a
    /// coarse 0/100 instantaneous gauge, so we poll it far faster than
    /// the UI metrics interval and report a rolling time-average.
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
    /// Rolling window length (`SAMPLE_INTERVAL * WINDOW` ≈ 4 s @ 50 ms).
    const SAMPLE_WINDOW: usize = 80;

    /// Cached probe of the first AMD render node we find. All
    /// dynamic values are re-read per snapshot from the cached
    /// paths (sysfs reads are a few microseconds each).
    pub(super) struct AmdSysfs {
        name: String,
        busy_path: Option<PathBuf>,
        vram_total_path: Option<PathBuf>,
        vram_used_path: Option<PathBuf>,
        // First `hwmon*/temp1_input` under the device (millidegrees C).
        temp_path: Option<PathBuf>,
        // Rolling, time-averaged GPU-busy duty cycle in centi-percent
        // (0..=10000), updated by a background sampler thread. `Some`
        // only when the sampler thread was spawned successfully; reads
        // are lock-free so `snapshot()` never blocks. When `None` we
        // fall back to a single direct read of `busy_path`.
        busy_avg_centi: Option<Arc<AtomicU32>>,
    }

    /// Continuously sample `gpu_busy_percent` and publish a rolling
    /// mean (the GPU-busy duty cycle) into `out`, in centi-percent.
    ///
    /// The raw counter is a binary 0/100 gauge that spikes for ~1 ms
    /// per inference burst and reads 0 otherwise, so a single read is
    /// almost always 0. Averaging over a multi-second window yields a
    /// stable, representative utilization that tracks real load and is
    /// non-zero whenever the GPU is doing periodic work.
    fn sample_busy_loop(path: PathBuf, out: Arc<AtomicU32>) {
        let mut ring = [0u32; SAMPLE_WINDOW];
        let mut idx = 0usize;
        let mut filled = 0usize;
        loop {
            let v = read_sysfs_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
                .min(100);
            ring[idx] = v;
            idx = (idx + 1) % SAMPLE_WINDOW;
            if filled < SAMPLE_WINDOW {
                filled += 1;
            }
            // Mean of 0..=100 samples → centi-percent (two decimals).
            let sum: u32 = ring[..filled].iter().sum();
            out.store(sum * 100 / filled as u32, Ordering::Relaxed);
            std::thread::sleep(SAMPLE_INTERVAL);
        }
    }

    pub(super) fn try_init() -> Option<AmdSysfs> {
        // Walk /sys/class/drm/card{0..9} (typically only card0/card1).
        for n in 0..10u32 {
            let base = PathBuf::from(format!("/sys/class/drm/card{n}/device"));
            if !base.exists() {
                continue;
            }
            let vendor = read_sysfs_string(&base.join("vendor")).unwrap_or_default();
            if vendor.trim() != "0x1002" {
                continue;
            }
            let device_id = read_sysfs_string(&base.join("device"))
                .ok()
                .unwrap_or_default();
            let name = amd_pci_name(device_id.trim());

            let exists = |rel: &str| -> Option<PathBuf> {
                let p = base.join(rel);
                p.exists().then_some(p)
            };
            let busy_path = exists("gpu_busy_percent");
            let vram_total_path = exists("mem_info_vram_total");
            let vram_used_path = exists("mem_info_vram_used");
            let temp_path = find_hwmon_temp(&base);

            // Spawn a background sampler so `utilization_pct` reports a
            // time-averaged duty cycle instead of a single instantaneous
            // read (which almost always catches an idle gap → 0%). If
            // the thread can't be spawned we leave this `None` and
            // `snapshot()` falls back to a direct read.
            let busy_avg_centi = busy_path.as_ref().and_then(|p| {
                let cell = Arc::new(AtomicU32::new(0));
                let writer = Arc::clone(&cell);
                let path = p.clone();
                std::thread::Builder::new()
                    .name("amd-gpu-busy-sampler".to_string())
                    .spawn(move || sample_busy_loop(path, writer))
                    .ok()
                    .map(|_| cell)
            });

            tracing::info!(
                name = %name,
                busy = busy_path.is_some(),
                vram = vram_total_path.is_some(),
                temp = temp_path.is_some(),
                averaged = busy_avg_centi.is_some(),
                "GPU backend: AMD amdgpu (sysfs)",
            );
            return Some(AmdSysfs {
                name,
                busy_path,
                vram_total_path,
                vram_used_path,
                temp_path,
                busy_avg_centi,
            });
        }
        None
    }

    /// Locate the first `temp1_input` under the device's hwmon
    /// directory (`/sys/class/drm/cardN/device/hwmon/hwmonM/`).
    fn find_hwmon_temp(base: &std::path::Path) -> Option<PathBuf> {
        let hwmon_dir = base.join("hwmon");
        let entries = std::fs::read_dir(&hwmon_dir).ok()?;
        for entry in entries.flatten() {
            let candidate = entry.path().join("temp1_input");
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }

    impl AmdSysfs {
        pub(super) fn snapshot(&self) -> GpuInfo {
            let read_u64 = |p: &Option<PathBuf>| -> Option<u64> {
                let p = p.as_ref()?;
                read_sysfs_string(p).ok()?.trim().parse::<u64>().ok()
            };

            let utilization_pct = match &self.busy_avg_centi {
                // Time-averaged duty cycle published by the sampler
                // thread (centi-percent → percent).
                Some(cell) => Some(cell.load(Ordering::Relaxed) as f32 / 100.0),
                // Sampler unavailable: fall back to a single read.
                None => self
                    .busy_path
                    .as_ref()
                    .and_then(|p| read_sysfs_string(p).ok())
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .map(|v| v.clamp(0.0, 100.0)),
            };

            // amdgpu reports temp in millidegrees C.
            let temp_c = self
                .temp_path
                .as_ref()
                .and_then(|p| read_sysfs_string(p).ok())
                .and_then(|s| s.trim().parse::<f32>().ok())
                .map(|milli| milli / 1000.0);

            // `gpu_busy_percent` is always present on amdgpu, so a
            // `None` here means the file genuinely couldn't be read
            // (permissions / unbound device) rather than the
            // CAP_PERFMON limitation the Intel path hits.
            let utilization_status = if utilization_pct.is_none() {
                Some(
                    "amdgpu gpu_busy_percent not readable \u{2014} the \
                     device may be unbound or the kernel too old"
                        .to_string(),
                )
            } else {
                None
            };

            GpuInfo {
                kind: "amd".to_string(),
                name: self.name.clone(),
                mem_total_bytes: read_u64(&self.vram_total_path),
                mem_used_bytes: read_u64(&self.vram_used_path),
                utilization_pct,
                temp_c,
                engines: Vec::new(),
                utilization_status,
            }
        }
    }

    /// Map a handful of common AMD APU / Radeon PCI device IDs to
    /// friendly names. Anything unknown falls back to "AMD Radeon
    /// Graphics" + the raw ID for support tickets.
    fn amd_pci_name(device_id: &str) -> String {
        match device_id.to_ascii_lowercase().as_str() {
            // Rembrandt / Ryzen 6000 (RDNA2) — Radeon 660M/680M.
            // Beelink EQR7 (Ryzen 7 6800H) lands here (gfx1035).
            "0x1681" => "AMD Radeon 680M (Rembrandt)".to_string(),
            // Phoenix / Ryzen 7040 (RDNA3) — Radeon 740M/760M/780M.
            "0x15bf" => "AMD Radeon 780M (Phoenix)".to_string(),
            "0x15c8" => "AMD Radeon 740M (Phoenix2)".to_string(),
            // Raphael desktop iGPU (RDNA2, 2 CU).
            "0x164e" => "AMD Radeon Graphics (Raphael)".to_string(),
            // Cezanne / Ryzen 5000 (Vega).
            "0x1638" => "AMD Radeon Graphics (Cezanne)".to_string(),
            // Strix Point (RDNA3.5) — Radeon 880M/890M.
            "0x150e" => "AMD Radeon 890M (Strix)".to_string(),
            other => format!("AMD Radeon Graphics ({other})"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::amd_pci_name;

        #[test]
        fn maps_known_apu_and_falls_back() {
            assert_eq!(amd_pci_name("0x1681"), "AMD Radeon 680M (Rembrandt)");
            // Case-insensitive on the hex digits.
            assert_eq!(amd_pci_name("0x1681"), amd_pci_name("0X1681"));
            // Unknown ID is preserved verbatim for support.
            assert_eq!(
                amd_pci_name("0xabcd"),
                "AMD Radeon Graphics (0xabcd)".to_string(),
            );
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
                engines: Vec::new(),
                utilization_status: Some(
                    "live utilization requires Apple's private IOReport \
                     framework (not implemented in this build); device \
                     name is detected via system_profiler"
                        .to_string(),
                ),
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
