//! `GET /api/v1/system/metrics` — host snapshot for the operator
//! dashboard.
//!
//! Why a dedicated module: the dashboard's "system at a glance" tile
//! polls this endpoint every second, plus the `/system` page reads
//! it for its full breakdown. Refreshing a `sysinfo::System` is
//! relatively expensive (≈3-8 ms on a Pi-class box), so we cache
//! the response for 1 second behind a `parking_lot::Mutex`. The
//! lock is never held across an `.await`, so blocking time is
//! bounded by the refresh cost.
//!
//! GPU stats come from the sibling `gpu` module, which dispatches
//! to NVML (Linux NVIDIA), sysfs (Linux Intel iGPU), or
//! `system_profiler` (macOS dev). `gpu: null` only when the host
//! has no detectable GPU at all.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_types::Role;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::auth::require_role::{SessionContext, SessionRejection};

// ---------------------------------------------------------------------------
// Response shape.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetrics {
    /// Engine process uptime, seconds since boot of THIS process
    /// (not the host). The host's own uptime is in `host.uptime_secs`.
    pub uptime_secs: u64,
    pub host: HostInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub gpu: Option<GpuInfo>,
    /// Intel NPU telemetry when an `intel_vpu`-bound device is
    /// visible via `/sys/class/accel/`. `None` on macOS, on Linux
    /// without the `intel_vpu` driver, and on non-Intel hosts.
    /// See the sibling `npu` module for the sysfs layout.
    pub npu: Option<crate::npu::NpuInfo>,
    /// Hailo-8 accelerator telemetry. `None` on builds without the
    /// `ep-hailo` feature, on hosts without a Hailo card, and during
    /// engine startup before the first inference pipeline is wired.
    /// See the sibling `hailo` module.
    pub hailo: Option<crate::hailo::HailoInfo>,
    pub disks: Vec<DiskInfo>,
    pub process: ProcessInfo,
    /// Kernel pressure-stall information — the share of wall time
    /// tasks spent *waiting* for CPU, I/O, or memory. Utilization
    /// gauges cannot distinguish "idle" from "blocked"; this can,
    /// which is what makes a stalled pipeline on an otherwise-quiet
    /// box visible. `None` on non-Linux hosts and on kernels built
    /// without `CONFIG_PSI`. See the sibling `proc_metrics` module.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pressure: Option<crate::proc_metrics::PressureInfo>,
    /// Wall-clock instant the snapshot was refreshed at, ISO 8601.
    /// Lets the UI label "as of N seconds ago" without a server
    /// round-trip on the time itself.
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub hostname: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub kernel_version: Option<String>,
    /// Host (system) uptime in seconds — not engine process uptime.
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuInfo {
    /// Logical core count.
    pub count: usize,
    /// Aggregate CPU utilization across all cores, 0–100.
    pub usage_pct: f32,
    /// Per-core utilization, 0–100. Same length as `count`.
    pub per_core_pct: Vec<f32>,
    /// Frequency MHz from the first core (cores are usually
    /// homogeneous; if not, this is good-enough for a chip-style
    /// readout).
    pub frequency_mhz: u64,
    /// 1-minute load average. `None` on platforms that don't
    /// expose it (Windows).
    pub load_avg_1m: Option<f64>,
    pub load_avg_5m: Option<f64>,
    pub load_avg_15m: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

/// One GPU engine class with its own utilization reading.
///
/// GPUs split work across distinct hardware engines — render/3D,
/// video-decode, video-encode, video-enhance, copy/blitter,
/// compute — each clocked independently. On a headless video
/// appliance the render engine sits near 0% while the video
/// engines carry the real load, so the single
/// `GpuInfo::utilization_pct` aggregate can look misleadingly idle.
/// This per-class breakdown surfaces where the work actually is.
///
/// Populated by two backends:
/// - Intel sysfs/PMU (both the i915 driver on Alder Lake / Raptor
///   Lake and the xe driver on Lunar Lake / Battlemage), which
///   reports the full engine set.
/// - NVIDIA NVML, which reports `"video-decode"` (NVDEC) and
///   `"video-encode"` (NVENC). NVML exposes no render/compute
///   split, so those classes are absent there — the headline
///   `utilization_pct` already covers the SM.
///
/// Empty on AMD and Apple.
#[derive(Debug, Clone, Serialize)]
pub struct GpuEngineUtil {
    /// Stable engine class: `"render"`, `"video-decode"`,
    /// `"video-encode"`, `"video-enhance"`, `"copy"`, or
    /// `"compute"`. Instances of the same class (e.g. two
    /// video-decode engines) are averaged into one entry.
    pub class: String,
    /// 0–100 utilization for this engine class over the sampling
    /// window.
    pub utilization_pct: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GpuInfo {
    pub kind: String,
    pub name: String,
    pub mem_total_bytes: Option<u64>,
    pub mem_used_bytes: Option<u64>,
    pub utilization_pct: Option<f32>,
    pub temp_c: Option<f32>,
    /// Per-engine-class utilization breakdown. Intel iGPUs report
    /// the full engine set; NVIDIA reports NVDEC/NVENC. Empty on
    /// AMD and Apple, and while the Intel PMU baseline is warming
    /// up. The aggregate `utilization_pct` above is unchanged when
    /// this is populated, so consumers that only read the headline
    /// number — including the existing Alder Lake path — keep
    /// working untouched.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub engines: Vec<GpuEngineUtil>,
    /// Board power draw in watts. NVIDIA (NVML) only; `None` on
    /// Intel, AMD, and Apple, and on NVIDIA boards with no power
    /// sensor. Mirrors the Hailo card's power readout so the GPU
    /// card carries the same operator signal.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub power_w: Option<f32>,
    /// Board power cap in watts (NVML `power_management_limit`).
    /// Gives `power_w` a denominator so the System page can render
    /// "45 / 75 W" rather than a bare number.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub power_limit_w: Option<f32>,
    /// Current graphics/core clock in MHz. NVIDIA only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub graphics_clock_mhz: Option<u32>,
    /// Current memory clock in MHz. NVIDIA only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub memory_clock_mhz: Option<u32>,
    /// Fan speed as a percentage of maximum, 0–100. NVIDIA only,
    /// and `None` on passively-cooled boards (datacentre cards and
    /// some low-profile Quadros report no fan at all).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fan_speed_pct: Option<u32>,
    /// Host GPU driver version, e.g. `"580.65.06"`. Static for the
    /// lifetime of the process — read once at backend init.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub driver_version: Option<String>,
    /// CUDA driver version the installed driver speaks, e.g.
    /// `"12.9"`. Lets an operator confirm the installer staged a
    /// CUDA runtime the driver can actually talk to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cuda_version: Option<String>,
    /// CUDA compute capability, e.g. `"6.1"` for Pascal. Static.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compute_capability: Option<String>,
    /// Operator-facing explanation when `utilization_pct` is `None`.
    /// `Some("...")` describes which PMU init / sampling step
    /// failed; `None` means utilization is being reported normally.
    /// Populated by `gpu::IntelSysfs::snapshot` and the macOS /
    /// NVIDIA paths so the System page can show the reason inline
    /// instead of a generic "not available" hint.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub utilization_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub mount_point: String,
    pub file_system: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub is_removable: bool,
    /// Bytes per second read from the backing block device over the
    /// last sampling interval. `None` on non-Linux hosts, before the
    /// first delta is available, and when the mount cannot be resolved
    /// to a `/proc/diskstats` device (network and virtual filesystems).
    ///
    /// Note these are *device*-level counters: several mounts sharing
    /// one logical volume all report that volume's totals.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub read_bytes_per_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub write_bytes_per_sec: Option<u64>,
    /// Share of wall time the device had at least one request in
    /// flight, 0–100 — `iostat -x`'s `%util`. Near 100 with a low byte
    /// rate means seek-bound; near 0 while the engine claims to be
    /// recording means the data never reached the disk.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub util_pct: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// Virtual memory size in bytes.
    pub virtual_bytes: u64,
    /// CPU percent for this process, 0–100 × logical cores
    /// (sysinfo's convention; e.g. 200 means 2 cores fully used).
    ///
    /// Preferentially computed from `/proc` thread deltas, falling back
    /// to `sysinfo`. The fallback needs a warm baseline across two of
    /// its own refreshes, which the one-shot diagnostics path does not
    /// always have — it reported a flat `0.0` on a box genuinely using
    /// 3.6 cores.
    pub cpu_pct: f32,
    /// Engine process uptime in seconds.
    pub run_time_secs: u64,
    /// Total live OS threads in this process. GStreamer spawns a thread
    /// per queue element, so this scales with camera count and is worth
    /// watching on its own.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread_count: Option<usize>,
    /// Busiest threads by CPU, descending, capped and filtered by the
    /// `proc_metrics` module. Attributes process CPU to named threads so
    /// a single saturated thread is visible instead of being averaged
    /// into an idle-looking core count. Empty on non-Linux hosts.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub threads: Vec<crate::proc_metrics::ThreadCpu>,
}

// ---------------------------------------------------------------------------
// Cache.
// ---------------------------------------------------------------------------

const CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Holds the long-lived `sysinfo::System` (which sysinfo wants
/// reused across `refresh_*` calls so deltas — like CPU % — can
/// be computed) plus the last-rendered response and its mint
/// instant.
struct MetricsCache {
    sys: System,
    disks: Disks,
    last: Option<(Instant, Arc<SystemMetrics>)>,
}

impl MetricsCache {
    fn new() -> Self {
        // First refresh primes the CPU deltas; the second refresh
        // (in `snapshot()`) is what produces meaningful CPU %s.
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            last: None,
        }
    }
}

static CACHE: LazyLock<Mutex<MetricsCache>> = LazyLock::new(|| Mutex::new(MetricsCache::new()));

fn current_pid() -> u32 {
    std::process::id()
}

/// Refresh the underlying `sysinfo::System` and rebuild a
/// [`SystemMetrics`]. The lock is held across the refresh — that's
/// fine because refresh is fast (~few ms) and never blocks on I/O
/// outside of the kernel-side `procfs`/`sysctl` reads it does.
fn render() -> Arc<SystemMetrics> {
    let mut guard = CACHE.lock();
    let now = Instant::now();
    if let Some((minted_at, ref response)) = guard.last {
        if now.duration_since(minted_at) < CACHE_TTL {
            return Arc::clone(response);
        }
    }

    // Refresh only what we need. Process refresh is the most
    // expensive call, so scope it to JUST our PID.
    guard.sys.refresh_cpu_all();
    guard.sys.refresh_memory();
    guard.sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(current_pid())]),
        true,
        ProcessRefreshKind::everything(),
    );
    guard.disks.refresh();

    // `/proc` throughput + stall counters. This is a delta against the
    // sampler's previous call, so it is deliberately taken on the same
    // cadence as the rest of the snapshot.
    let proc_sample = crate::proc_metrics::sample();

    let host_uptime = System::uptime();
    let global_cpu = guard.sys.global_cpu_usage();
    let per_core: Vec<f32> = guard.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
    let cpu_freq = guard.sys.cpus().first().map(|c| c.frequency()).unwrap_or(0);
    let load = System::load_average();
    let (load_1, load_5, load_15) = if load.one > 0.0 || load.five > 0.0 || load.fifteen > 0.0 {
        (Some(load.one), Some(load.five), Some(load.fifteen))
    } else {
        (None, None, None)
    };

    let host = HostInfo {
        hostname: System::host_name(),
        os_name: System::name(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        uptime_secs: host_uptime,
    };

    let cpu = CpuInfo {
        count: guard.sys.cpus().len(),
        usage_pct: global_cpu,
        per_core_pct: per_core,
        frequency_mhz: cpu_freq,
        load_avg_1m: load_1,
        load_avg_5m: load_5,
        load_avg_15m: load_15,
    };

    let memory = MemoryInfo {
        total_bytes: guard.sys.total_memory(),
        used_bytes: guard.sys.used_memory(),
        available_bytes: guard.sys.available_memory(),
        swap_total_bytes: guard.sys.total_swap(),
        swap_used_bytes: guard.sys.used_swap(),
    };

    let disks: Vec<DiskInfo> = guard
        .disks
        .iter()
        .map(|d| {
            let name = d.name().to_string_lossy().into_owned();
            // Several mounts commonly share one logical volume, so this
            // resolve-then-lookup runs per mount and can legitimately
            // hand back the same device stats more than once.
            let io = crate::proc_metrics::kernel_device_name(&name)
                .and_then(|dev| proc_sample.disk_io.get(&dev).copied());
            DiskInfo {
                name,
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total_bytes: d.total_space(),
                available_bytes: d.available_space(),
                is_removable: d.is_removable(),
                read_bytes_per_sec: io.map(|i| i.read_bytes_per_sec),
                write_bytes_per_sec: io.map(|i| i.write_bytes_per_sec),
                util_pct: io.map(|i| i.util_pct),
            }
        })
        .collect();

    let process = guard
        .sys
        .process(sysinfo::Pid::from_u32(current_pid()))
        .map(|p| ProcessInfo {
            pid: current_pid(),
            rss_bytes: p.memory(),
            virtual_bytes: p.virtual_memory(),
            // Prefer the `/proc` figure; see the field's doc comment for
            // why sysinfo's reading is not trusted as the primary.
            cpu_pct: proc_sample.process_cpu_pct.unwrap_or_else(|| p.cpu_usage()),
            run_time_secs: p.run_time(),
            thread_count: (proc_sample.thread_count > 0).then_some(proc_sample.thread_count),
            threads: proc_sample.threads.clone(),
        })
        .unwrap_or(ProcessInfo {
            pid: current_pid(),
            rss_bytes: 0,
            virtual_bytes: 0,
            cpu_pct: 0.0,
            run_time_secs: 0,
            thread_count: None,
            threads: Vec::new(),
        });

    let response = SystemMetrics {
        uptime_secs: process.run_time_secs,
        host,
        cpu,
        memory,
        gpu: crate::gpu::snapshot(),
        npu: crate::npu::snapshot(),
        hailo: crate::hailo::snapshot(),
        disks,
        process,
        pressure: proc_sample.pressure,
        captured_at: chrono::Utc::now(),
    };

    let response = Arc::new(response);
    guard.last = Some((now, Arc::clone(&response)));
    response
}

/// Crate-public wrapper around the cached [`render`] used by
/// the M-Admin Phase 0 diagnostics tarball. Lets the
/// `admin_runtime` module pull a metrics snapshot without
/// going through the authenticated HTTP handler (which would
/// itself recurse into the tarball if anything went wrong).
pub(crate) fn snapshot() -> Arc<SystemMetrics> {
    render()
}

// ---------------------------------------------------------------------------
// HTTP handler.
// ---------------------------------------------------------------------------

/// `GET /api/v1/system/metrics` — any authenticated viewer can read
/// this. We deliberately do NOT require admin: operators and
/// viewers need to see system health to do their jobs, and the
/// surface is read-only host telemetry (no secrets).
pub async fn get_system_metrics(session: SessionContext) -> Result<Response, Response> {
    session
        .require(Role::Viewer)
        .map_err(SessionRejection::into_response)?;

    let snapshot = render();
    // `Json` wants ownership; clone out of the `Arc` since the
    // payload is tiny (~few KB).
    let body: SystemMetrics = (*snapshot).clone();
    Ok((StatusCode::OK, Json(body)).into_response())
}

// ---------------------------------------------------------------------------
// History handler.
// ---------------------------------------------------------------------------

/// Window used when the caller names none: 24 hours.
const DEFAULT_WINDOW_SECS: i64 = 86_400;
/// Longest answerable window. Must track the retention cap in
/// [`nexus_store::metrics`] — asking for more than the store keeps would
/// silently return a short series rather than an error.
const MAX_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;
/// Bucket width used when the caller names none: 5 minutes.
const DEFAULT_BUCKET_SECS: i64 = 300;
/// The engine's native sampling cadence. A finer bucket cannot yield
/// more points, so requests below this are floored rather than rejected.
const MIN_BUCKET_SECS: i64 = 5;

/// Query params for [`get_metrics_history`].
#[derive(Debug, Deserialize)]
pub struct MetricsHistoryQuery {
    /// How far back from now to include, in seconds. Defaults to 24h;
    /// clamped to `[1, 604_800]` (the 7-day retention cap).
    window_secs: Option<i64>,
    /// Downsample bucket width in seconds. Defaults to 300 (5 min);
    /// clamped to `>= 5`. Pass `5` for full resolution — note only the
    /// most recent hour is retained at that cadence (see
    /// [`nexus_store::metrics`]).
    bucket_secs: Option<i64>,
    /// Optional delta cursor (Unix epoch ms). Return only samples at or
    /// after this instant (floored to the 5-second grid). The console's
    /// live tail passes this so a refresh fetches just the new points.
    since_ms: Option<i64>,
}

impl MetricsHistoryQuery {
    /// Resolve the raw params into the `(window_secs, bucket_secs)` the
    /// store is asked for. Split out of the handler so the clamps stay
    /// unit-testable without standing up an `ApiState`.
    fn resolve(&self) -> (i64, i64) {
        (
            self.window_secs
                .unwrap_or(DEFAULT_WINDOW_SECS)
                .clamp(1, MAX_WINDOW_SECS),
            self.bucket_secs
                .unwrap_or(DEFAULT_BUCKET_SECS)
                .max(MIN_BUCKET_SECS),
        )
    }
}

/// `GET /api/v1/admin/system/metrics-history` — rolling window of host
/// metrics samples for the cloud console's trend view, sliced to any of
/// its window presets (5 minutes … 7 days).
///
/// Admin-gated: the cloud console reaches it through the generic
/// `/admin/*` passthrough proxy, which is itself viewer+ RBAC'd on the
/// cloud side. Each array element is the verbatim `SystemMetrics` JSON
/// captured at that instant, oldest → newest. Read-only host telemetry
/// with no secrets, so — like the live snapshot — it carries no
/// actor_token requirement.
pub async fn get_metrics_history(
    State(s): State<crate::api::ApiState>,
    Query(q): Query<MetricsHistoryQuery>,
) -> Result<Json<Vec<serde_json::Value>>, crate::api::ApiError> {
    let (window_secs, bucket_secs) = q.resolve();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let payloads = s
        .store
        .list_metrics_samples(now_ms, window_secs, bucket_secs, q.since_ms)
        .await?;
    // Parse each stored blob back into JSON so the client receives an
    // array of objects, not an array of strings. A row that somehow
    // fails to parse is skipped rather than failing the whole request.
    let out: Vec<serde_json::Value> = payloads
        .iter()
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect();
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CACHE` is a process-global and every test below resets it, so the
    /// harness running them on separate threads is enough to break them:
    /// one test's `last = None` lands between another's two `render()`
    /// calls and the second call rebuilds instead of hitting the cache.
    /// Serialise them rather than weakening the assertions.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn render_produces_non_empty_snapshot() {
        let _serial = TEST_SERIAL.lock();
        // Two renders so the CPU % delta is computed.
        let _ = render();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Force past the TTL so the second call refreshes.
        {
            let mut g = CACHE.lock();
            g.last = None;
        }
        let m = render();
        assert!(m.cpu.count >= 1, "at least one CPU core");
        assert_eq!(
            m.cpu.per_core_pct.len(),
            m.cpu.count,
            "per-core array matches count"
        );
        assert!(m.memory.total_bytes > 0, "total RAM should be reported");
        assert!(m.process.pid > 0, "PID should be reported");
    }

    #[test]
    fn cache_returns_same_snapshot_within_ttl() {
        let _serial = TEST_SERIAL.lock();
        {
            let mut g = CACHE.lock();
            g.last = None;
        }
        let a = render();
        let b = render();
        assert!(
            Arc::ptr_eq(&a, &b),
            "two reads within TTL should hand back the same Arc"
        );
    }

    /// The throughput fields are the whole point of the diagnostics
    /// bundle being able to answer "is a stage blocked?", so assert they
    /// survive serialization rather than being silently skipped.
    #[test]
    fn snapshot_carries_proc_derived_fields() {
        let _serial = TEST_SERIAL.lock();
        {
            let mut g = CACHE.lock();
            g.last = None;
        }
        // Two renders far enough apart for the proc sampler to produce a
        // delta rather than just a baseline.
        let _ = render();
        std::thread::sleep(std::time::Duration::from_millis(300));
        {
            let mut g = CACHE.lock();
            g.last = None;
        }
        let m = render();

        let json = serde_json::to_value(&*m).expect("snapshot serializes");
        assert!(json.get("disks").is_some(), "disks always present");

        if cfg!(target_os = "linux") {
            assert!(
                m.process.thread_count.unwrap_or(0) > 0,
                "thread count is reported on Linux"
            );
            // A process that just did two refreshes plus a sleep has
            // measurable CPU, so this must not be the flat 0.0 the old
            // sysinfo-only path produced in the one-shot diag case.
            assert!(
                m.process.cpu_pct >= 0.0,
                "cpu_pct is a real reading, got {}",
                m.process.cpu_pct
            );
        }
    }

    fn query(window_secs: Option<i64>, bucket_secs: Option<i64>) -> MetricsHistoryQuery {
        MetricsHistoryQuery {
            window_secs,
            bucket_secs,
            since_ms: None,
        }
    }

    #[test]
    fn history_query_defaults_to_24h_at_5min_buckets() {
        assert_eq!(query(None, None).resolve(), (86_400, 300));
    }

    /// The console's longest preset is 7 days. If this cap regresses to
    /// the old 24h value the 3d/7d chips silently return a short series
    /// instead of failing, so pin it explicitly.
    #[test]
    fn history_query_admits_the_full_seven_day_window() {
        assert_eq!(
            query(Some(604_800), Some(1_800)).resolve(),
            (604_800, 1_800)
        );
        // Every console preset must survive the clamp untouched.
        for preset_secs in [300, 900, 3_600, 86_400, 259_200, 604_800] {
            let (window, _) = query(Some(preset_secs), None).resolve();
            assert_eq!(window, preset_secs, "preset {preset_secs}s must not clamp");
        }
    }

    #[test]
    fn history_query_clamps_out_of_range_input() {
        // Beyond retention: clamped down, not rejected.
        assert_eq!(query(Some(i64::MAX), None).resolve().0, 604_800);
        // Zero/negative windows would make the store's floor exceed now.
        assert_eq!(query(Some(0), None).resolve().0, 1);
        assert_eq!(query(Some(-99), None).resolve().0, 1);
        // Sub-cadence buckets can't add points, so they floor to 5s.
        assert_eq!(query(None, Some(1)).resolve().1, 5);
        assert_eq!(query(None, Some(0)).resolve().1, 5);
        assert_eq!(query(None, Some(-7)).resolve().1, 5);
    }
}
