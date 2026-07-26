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
    pub cpu_pct: f32,
    /// Engine process uptime in seconds.
    pub run_time_secs: u64,
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
        .map(|d| DiskInfo {
            name: d.name().to_string_lossy().into_owned(),
            mount_point: d.mount_point().to_string_lossy().into_owned(),
            file_system: d.file_system().to_string_lossy().into_owned(),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
            is_removable: d.is_removable(),
        })
        .collect();

    let process = guard
        .sys
        .process(sysinfo::Pid::from_u32(current_pid()))
        .map(|p| ProcessInfo {
            pid: current_pid(),
            rss_bytes: p.memory(),
            virtual_bytes: p.virtual_memory(),
            cpu_pct: p.cpu_usage(),
            run_time_secs: p.run_time(),
        })
        .unwrap_or(ProcessInfo {
            pid: current_pid(),
            rss_bytes: 0,
            virtual_bytes: 0,
            cpu_pct: 0.0,
            run_time_secs: 0,
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

/// Query params for [`get_metrics_history`].
#[derive(Debug, Deserialize)]
pub struct MetricsHistoryQuery {
    /// How far back from now to include, in seconds. Defaults to 24h;
    /// clamped to `[1, 86_400]`.
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

/// `GET /api/v1/admin/system/metrics-history` — rolling window of host
/// metrics samples for the cloud console's "last 24 hours" trend view.
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
    let window_secs = q.window_secs.unwrap_or(86_400).clamp(1, 86_400);
    let bucket_secs = q.bucket_secs.unwrap_or(300).max(5);
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

    #[test]
    fn render_produces_non_empty_snapshot() {
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
}
