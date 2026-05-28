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

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_types::Role;
use parking_lot::Mutex;
use serde::Serialize;
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

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub kind: String,
    pub name: String,
    pub mem_total_bytes: Option<u64>,
    pub mem_used_bytes: Option<u64>,
    pub utilization_pct: Option<f32>,
    pub temp_c: Option<f32>,
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
