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
    }

    impl IntelPmuBackend {
        fn snapshot(&mut self) -> Option<f32> {
            match self {
                IntelPmuBackend::I915(p) => p.snapshot(),
                IntelPmuBackend::Xe(p) => p.snapshot(),
            }
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
                Err(i915_reason) => match XePmu::try_open(pci_bdf.as_deref()) {
                    Ok(p) => (Some(Mutex::new(IntelPmuBackend::Xe(p))), None),
                    Err(xe_reason) => {
                        let combined = format!("i915 PMU: {i915_reason}; xe PMU: {xe_reason}");
                        tracing::warn!(
                            reason = %combined,
                            "neither i915 nor xe PMU could be opened; GPU utilization will be unavailable",
                        );
                        (None, Some(combined))
                    }
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
            let (utilization_pct, utilization_status) = match &self.pmu {
                None => (None, self.pmu_init_error.clone()),
                Some(m) => match m.lock() {
                    Err(_) => (None, Some("PMU mutex poisoned".to_string())),
                    Ok(mut guard) => match guard.snapshot() {
                        Some(pct) => (Some(pct), None),
                        None => (
                            None,
                            Some(
                                "GPU PMU baseline warming up \u{2014} \
                                 a reading will appear after the next snapshot"
                                    .to_string(),
                            ),
                        ),
                    },
                },
            };
            GpuInfo {
                kind: "intel".to_string(),
                name: display,
                mem_total_bytes: None,
                mem_used_bytes: None,
                utilization_pct,
                temp_c: None,
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
            let mut skipped: Vec<(String, i32)> = Vec::new();
            for path in &event_files {
                let Some(config) = read_event_config(path) else {
                    skipped.push((short_name(path), -1));
                    continue;
                };
                match open_i915_event(type_id, config) {
                    Ok(fd) => engine_fds.push(fd),
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
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
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
                        let pct = (busy_ns as f64 / (n_engines as f64 * elapsed_ns as f64)) * 100.0;
                        Some((pct.clamp(0.0, 100.0)) as f32)
                    }
                }
                _ => None,
            };
            self.last_sample = Some((now, values));
            result
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

        fn snapshot(&mut self) -> Option<f32> {
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
                    return None;
                }
                values.push((u64::from_ne_bytes(a), u64::from_ne_bytes(t)));
            }
            let result = match &self.last_sample {
                Some((prev_t, prev_v)) if prev_v.len() == values.len() => {
                    let elapsed_ns = now.duration_since(*prev_t).as_nanos() as u64;
                    if elapsed_ns < 100_000_000 {
                        None
                    } else {
                        let mut active_delta: u64 = 0;
                        let mut total_delta: u64 = 0;
                        for ((a, t), (pa, pt)) in values.iter().zip(prev_v.iter()) {
                            active_delta = active_delta.saturating_add(a.saturating_sub(*pa));
                            total_delta = total_delta.saturating_add(t.saturating_sub(*pt));
                        }
                        if total_delta == 0 {
                            None
                        } else {
                            let pct = (active_delta as f64 / total_delta as f64) * 100.0;
                            Some(pct.clamp(0.0, 100.0) as f32)
                        }
                    }
                }
                _ => None,
            };
            self.last_sample = Some((now, values));
            result
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
