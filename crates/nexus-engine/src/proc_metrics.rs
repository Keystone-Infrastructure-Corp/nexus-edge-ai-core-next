//! `/proc`-derived throughput metrics that `sysinfo` does not expose.
//!
//! Why this module exists: the operator dashboard and the diagnostics
//! tarball both carried *capacity* numbers (CPU %, RAM used, disk free)
//! but no *throughput* or *stall* numbers. That combination is unable to
//! distinguish "the box is busy" from "the box is idle but a stage inside
//! the engine is blocked" — and the second case is what a silently
//! degrading recording pipeline actually looks like: every utilization
//! gauge reads healthy while frames are being dropped.
//!
//! Three additions close that gap, all read straight from `procfs` with
//! no new dependencies and no shelling out:
//!
//! - **Disk I/O rates** from `/proc/diskstats` — bytes/sec read and
//!   written plus `util_pct` (the fraction of wall time the device had a
//!   request in flight). A device at 0.4% util while the engine claims to
//!   be recording is decisive evidence the data never reached the disk.
//! - **Pressure stall information** from `/proc/pressure/{cpu,io,memory}`
//!   — the share of wall time tasks spent *stalled* waiting for each
//!   resource. Unlike a utilization gauge, PSI is non-zero precisely when
//!   work is being delayed, which is the question an operator is actually
//!   asking. `None` when the kernel is built without `CONFIG_PSI`.
//! - **Per-thread CPU** from `/proc/self/task/*/stat` — attributes the
//!   process's CPU to named threads, so a single saturated thread inside
//!   an otherwise-idle 16-core box is visible instead of being averaged
//!   away.
//!
//! All three are rate/delta measurements, so this module keeps the
//! previous counter set behind a mutex and divides by elapsed wall time.
//! The first call after startup therefore reports no rates — it only
//! establishes the baseline. [`system_metrics`](crate::system_metrics)
//! refreshes on a 1 s cache TTL and the history sampler runs every 5 s,
//! so the baseline is warm long before anyone reads a bundle.
//!
//! Everything here is Linux-only. On macOS dev boxes the readers return
//! empty and the serialized fields are omitted, so the response shape
//! stays valid on every platform.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

/// Sectors are always 512 bytes in `/proc/diskstats`, independent of the
/// device's real logical block size — this is a kernel ABI constant, not
/// a property of the hardware.
const DISKSTATS_SECTOR_BYTES: u64 = 512;

/// Ignore deltas shorter than this. Two reads inside the same
/// scheduler tick produce meaningless rates (and risk dividing by
/// something very close to zero).
const MIN_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Cap on how many threads are reported. The engine runs ~26 threads per
/// camera (GStreamer spawns one per queue element), which reaches four
/// figures on a 50-camera box. The whole list would dwarf the rest of the
/// payload and this snapshot is persisted to the metrics history every
/// 5 s, so only the busiest threads are kept.
const MAX_THREADS_REPORTED: usize = 16;

/// Threads quieter than this are dropped from the list entirely. Keeps
/// the payload focused on whatever is actually consuming time.
const MIN_THREAD_PCT: f32 = 1.0;

// ---------------------------------------------------------------------------
// Response shapes.
// ---------------------------------------------------------------------------

/// One resource's pressure-stall readings, as percentages of wall time.
///
/// `some` is the share of time *at least one* task was stalled on the
/// resource; `full` is the share of time *every* runnable task was
/// stalled. `full` on the CPU line is always zero by definition (a
/// running task is not stalled), so a non-zero `full` on `io` or
/// `memory` is the strong signal.
#[derive(Debug, Clone, Serialize)]
pub struct PressureStat {
    pub some_avg10: f32,
    pub some_avg60: f32,
    pub some_avg300: f32,
    pub full_avg10: f32,
    pub full_avg60: f32,
    pub full_avg300: f32,
}

/// Pressure stall information for the three resources the kernel tracks.
/// Each field is `None` when that specific file is unreadable.
#[derive(Debug, Clone, Serialize)]
pub struct PressureInfo {
    pub cpu: Option<PressureStat>,
    pub io: Option<PressureStat>,
    pub memory: Option<PressureStat>,
}

/// Throughput for one block device over the sampling interval.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DiskIo {
    pub read_bytes_per_sec: u64,
    pub write_bytes_per_sec: u64,
    /// Share of wall time the device had at least one request in flight,
    /// 0–100. This is the same number `iostat -x` prints as `%util`.
    pub util_pct: f32,
}

/// CPU attributed to one thread over the sampling interval.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadCpu {
    pub tid: u32,
    /// Kernel thread name (`comm`), truncated by the kernel to 15 bytes.
    pub name: String,
    /// Percent of a *single* core. 100 means one core fully consumed;
    /// this deliberately does not normalise by core count so a saturated
    /// single thread reads as 100 rather than 6.25 on a 16-core box.
    pub cpu_pct: f32,
}

/// One complete `/proc` sample. Empty on non-Linux hosts.
#[derive(Debug, Clone, Default)]
pub struct ProcSample {
    pub pressure: Option<PressureInfo>,
    /// Keyed by kernel device name as it appears in `/proc/diskstats`
    /// (`dm-0`, `nvme0n1p3`), not by mount point or `/dev` path.
    pub disk_io: HashMap<String, DiskIo>,
    /// Busiest threads, descending by CPU. See [`MAX_THREADS_REPORTED`].
    pub threads: Vec<ThreadCpu>,
    /// Total live thread count, before the reporting cap is applied.
    pub thread_count: usize,
    /// Whole-process CPU as a percent of a single core, summed across
    /// every thread. Computed from `/proc` deltas rather than taken from
    /// `sysinfo`, whose per-process reading needs a warm baseline that
    /// the one-shot diagnostics path does not always have.
    pub process_cpu_pct: Option<f32>,
}

// ---------------------------------------------------------------------------
// Sampler state.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Sampler {
    /// device name -> (sectors read, sectors written, ms doing I/O)
    prev_disks: HashMap<String, (u64, u64, u64)>,
    /// tid -> cumulative utime + stime, in clock ticks
    prev_threads: HashMap<u32, u64>,
    prev_at: Option<Instant>,
}

static SAMPLER: LazyLock<Mutex<Sampler>> = LazyLock::new(|| Mutex::new(Sampler::default()));

/// Take a `/proc` sample, differencing against the previous call.
///
/// Returns an empty [`ProcSample`] on the first call (baseline only), on
/// calls made less than [`MIN_SAMPLE_INTERVAL`] apart, and on every
/// non-Linux platform.
pub(crate) fn sample() -> ProcSample {
    let mut guard = SAMPLER.lock();
    let now = Instant::now();

    let disks_now = read_diskstats();
    let (threads_now, thread_count) = read_threads();
    let pressure = read_pressure();

    let elapsed = guard.prev_at.map(|p| now.duration_since(p));
    let fresh_enough = elapsed.is_some_and(|e| e >= MIN_SAMPLE_INTERVAL);

    let mut out = ProcSample {
        pressure,
        thread_count,
        ..Default::default()
    };

    if let (Some(elapsed), true) = (elapsed, fresh_enough) {
        let secs = elapsed.as_secs_f64();
        out.disk_io = diff_disks(&guard.prev_disks, &disks_now, secs);
        let (threads, total_pct) = diff_threads(&guard.prev_threads, &threads_now, secs);
        out.threads = threads;
        out.process_cpu_pct = Some(total_pct);
    }

    // Only advance the baseline once the delta has been consumed,
    // otherwise a burst of sub-interval calls would keep resetting it and
    // rates would never be produced at all.
    if !fresh_enough && guard.prev_at.is_some() {
        return out;
    }
    guard.prev_disks = disks_now;
    // Names are re-read each sample, so only the tick counters carry
    // forward — a thread that exits simply drops out of the map.
    guard.prev_threads = threads_now
        .into_iter()
        .map(|(tid, (_, ticks))| (tid, ticks))
        .collect();
    guard.prev_at = Some(now);
    out
}

fn diff_disks(
    prev: &HashMap<String, (u64, u64, u64)>,
    now: &HashMap<String, (u64, u64, u64)>,
    secs: f64,
) -> HashMap<String, DiskIo> {
    let mut out = HashMap::new();
    for (dev, &(r1, w1, io1)) in now {
        let Some(&(r0, w0, io0)) = prev.get(dev) else {
            continue;
        };
        // Counters are monotonic but can reset if a device is removed and
        // re-added under the same name; saturating_sub keeps that from
        // wrapping into an absurd rate.
        let read = r1.saturating_sub(r0) * DISKSTATS_SECTOR_BYTES;
        let write = w1.saturating_sub(w0) * DISKSTATS_SECTOR_BYTES;
        let busy_ms = io1.saturating_sub(io0) as f64;
        out.insert(
            dev.clone(),
            DiskIo {
                read_bytes_per_sec: (read as f64 / secs) as u64,
                write_bytes_per_sec: (write as f64 / secs) as u64,
                util_pct: ((busy_ms / (secs * 1000.0)) * 100.0).clamp(0.0, 100.0) as f32,
            },
        );
    }
    out
}

fn diff_threads(
    prev: &HashMap<u32, u64>,
    now: &HashMap<u32, (String, u64)>,
    secs: f64,
) -> (Vec<ThreadCpu>, f32) {
    let hz = clock_ticks_per_sec();
    let mut rows: Vec<ThreadCpu> = Vec::new();
    let mut total = 0.0_f32;
    for (tid, (name, ticks_now)) in now {
        let Some(&ticks_prev) = prev.get(tid) else {
            continue;
        };
        let delta = ticks_now.saturating_sub(ticks_prev) as f64;
        let pct = ((delta / hz) / secs * 100.0) as f32;
        total += pct;
        if pct >= MIN_THREAD_PCT {
            rows.push(ThreadCpu {
                tid: *tid,
                name: name.clone(),
                cpu_pct: pct,
            });
        }
    }
    rows.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(MAX_THREADS_REPORTED);
    (rows, total)
}

// ---------------------------------------------------------------------------
// Linux readers.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> f64 {
    // SAFETY: `sysconf` with a valid name is a pure lookup with no
    // preconditions and no memory access.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 {
        hz as f64
    } else {
        100.0
    }
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_sec() -> f64 {
    100.0
}

/// Parse `/proc/diskstats` into `name -> (sectors read, sectors written,
/// ms doing I/O)`.
///
/// Field order is the documented stable layout (see
/// `Documentation/admin-guide/iostats.rst`): index 2 is the device name,
/// 5 is sectors read, 9 is sectors written, 12 is milliseconds spent
/// doing I/O. Kernels ≥ 4.18 append more fields; trailing additions do
/// not shift the ones read here.
#[cfg(target_os = "linux")]
fn read_diskstats() -> HashMap<String, (u64, u64, u64)> {
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() <= 12 {
            continue;
        }
        let (Ok(read), Ok(written), Ok(io_ms)) = (
            f[5].parse::<u64>(),
            f[9].parse::<u64>(),
            f[12].parse::<u64>(),
        ) else {
            continue;
        };
        // Skip devices that have never done anything — loop0..loop63 and
        // empty card readers would otherwise dominate the map.
        if read == 0 && written == 0 {
            continue;
        }
        out.insert(f[2].to_string(), (read, written, io_ms));
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn read_diskstats() -> HashMap<String, (u64, u64, u64)> {
    HashMap::new()
}

/// Read every live thread's cumulative CPU ticks from
/// `/proc/self/task/<tid>/stat`.
///
/// `comm` is wrapped in parentheses and may itself contain spaces and
/// parentheses, so the fields after it are located by scanning to the
/// *last* `)` rather than by splitting the whole line. Past that point
/// the first field is `state` (field 3), which puts `utime` (field 14)
/// at offset 11 and `stime` (field 15) at offset 12.
#[cfg(target_os = "linux")]
fn read_threads() -> (HashMap<u32, (String, u64)>, usize) {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return (HashMap::new(), 0);
    };
    let mut out = HashMap::new();
    for entry in dir.flatten() {
        let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // A thread can exit between the readdir and the open; that is
        // routine, not an error worth logging.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        if close <= open || close + 2 >= stat.len() {
            continue;
        }
        let name = stat[open + 1..close].to_string();
        let rest: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        if rest.len() <= 12 {
            continue;
        }
        let (Ok(utime), Ok(stime)) = (rest[11].parse::<u64>(), rest[12].parse::<u64>()) else {
            continue;
        };
        out.insert(tid, (name, utime + stime));
    }
    let count = out.len();
    (out, count)
}

#[cfg(not(target_os = "linux"))]
fn read_threads() -> (HashMap<u32, (String, u64)>, usize) {
    (HashMap::new(), 0)
}

/// Read `/proc/pressure/{cpu,io,memory}`. `None` when the kernel lacks
/// `CONFIG_PSI` (the directory simply does not exist).
#[cfg(target_os = "linux")]
fn read_pressure() -> Option<PressureInfo> {
    let cpu = read_pressure_file("cpu");
    let io = read_pressure_file("io");
    let memory = read_pressure_file("memory");
    if cpu.is_none() && io.is_none() && memory.is_none() {
        return None;
    }
    Some(PressureInfo { cpu, io, memory })
}

#[cfg(not(target_os = "linux"))]
fn read_pressure() -> Option<PressureInfo> {
    None
}

/// Parse one PSI file, whose two lines look like:
///
/// ```text
/// some avg10=3.33 avg60=3.38 avg300=3.31 total=56999426175
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=0
/// ```
#[cfg(target_os = "linux")]
fn read_pressure_file(which: &str) -> Option<PressureStat> {
    let text = std::fs::read_to_string(format!("/proc/pressure/{which}")).ok()?;
    let mut stat = PressureStat {
        some_avg10: 0.0,
        some_avg60: 0.0,
        some_avg300: 0.0,
        full_avg10: 0.0,
        full_avg60: 0.0,
        full_avg300: 0.0,
    };
    let mut saw_line = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next()?;
        let is_full = match kind {
            "some" => false,
            "full" => true,
            _ => continue,
        };
        saw_line = true;
        for kv in parts {
            let Some((key, value)) = kv.split_once('=') else {
                continue;
            };
            let Ok(v) = value.parse::<f32>() else {
                continue;
            };
            match (key, is_full) {
                ("avg10", false) => stat.some_avg10 = v,
                ("avg60", false) => stat.some_avg60 = v,
                ("avg300", false) => stat.some_avg300 = v,
                ("avg10", true) => stat.full_avg10 = v,
                ("avg60", true) => stat.full_avg60 = v,
                ("avg300", true) => stat.full_avg300 = v,
                _ => {}
            }
        }
    }
    saw_line.then_some(stat)
}

// ---------------------------------------------------------------------------
// Device-name resolution.
// ---------------------------------------------------------------------------

/// Map a `sysinfo` disk name onto the kernel device name used by
/// `/proc/diskstats`.
///
/// `sysinfo` reports the `/dev` path a filesystem was mounted from, which
/// for LVM and crypt volumes is a symlink (`/dev/mapper/vg-lv` →
/// `../dm-0`) while `/proc/diskstats` only ever knows `dm-0`. Resolving
/// the symlink is what lets the two be joined. Plain partitions
/// (`/dev/nvme0n1p2`) need only the basename.
pub(crate) fn kernel_device_name(disk_name: &str) -> Option<String> {
    if !disk_name.starts_with("/dev/") {
        return None;
    }
    let resolved = std::fs::canonicalize(disk_name).unwrap_or_else(|_| disk_name.into());
    resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_infallible_and_baselines_first() {
        // First call establishes the baseline and reports no rates.
        let first = sample();
        assert!(
            first.disk_io.is_empty(),
            "first sample cannot produce a rate"
        );

        std::thread::sleep(MIN_SAMPLE_INTERVAL + std::time::Duration::from_millis(50));
        let second = sample();
        // Rates are only asserted on Linux; macOS dev boxes return empty.
        if cfg!(target_os = "linux") {
            assert!(second.thread_count > 0, "process has at least one thread");
            assert!(
                second.process_cpu_pct.is_some(),
                "second sample computes process CPU"
            );
        }
    }

    #[test]
    fn disk_diff_computes_rates() {
        let mut prev = HashMap::new();
        prev.insert("dm-0".to_string(), (0_u64, 0_u64, 0_u64));
        let mut now = HashMap::new();
        // 2048 sectors written = 1 MiB, device busy 500 ms of 2 s = 25%.
        now.insert("dm-0".to_string(), (0_u64, 2048_u64, 500_u64));

        let out = diff_disks(&prev, &now, 2.0);
        let io = out.get("dm-0").expect("dm-0 present");
        assert_eq!(io.write_bytes_per_sec, 512 * 1024);
        assert_eq!(io.read_bytes_per_sec, 0);
        assert!(
            (io.util_pct - 25.0).abs() < 0.01,
            "util was {}",
            io.util_pct
        );
    }

    #[test]
    fn disk_diff_skips_devices_without_a_baseline() {
        let prev = HashMap::new();
        let mut now = HashMap::new();
        now.insert("nvme0n1".to_string(), (10_u64, 10_u64, 10_u64));
        assert!(
            diff_disks(&prev, &now, 1.0).is_empty(),
            "a device seen for the first time has no rate yet"
        );
    }

    #[test]
    fn disk_diff_survives_counter_reset() {
        let mut prev = HashMap::new();
        prev.insert("dm-0".to_string(), (900_u64, 900_u64, 900_u64));
        let mut now = HashMap::new();
        // Device re-added under the same name: counters went backwards.
        now.insert("dm-0".to_string(), (5_u64, 5_u64, 5_u64));
        let out = diff_disks(&prev, &now, 1.0);
        let io = out.get("dm-0").expect("dm-0 present");
        assert_eq!(io.read_bytes_per_sec, 0, "reset must not wrap");
        assert_eq!(io.write_bytes_per_sec, 0, "reset must not wrap");
    }

    #[test]
    fn thread_diff_ranks_and_caps() {
        let hz = clock_ticks_per_sec() as u64;
        let mut prev = HashMap::new();
        let mut now = HashMap::new();
        // 40 threads, thread N burning N% of a core over 1 second.
        for tid in 1_u32..=40 {
            prev.insert(tid, 0_u64);
            now.insert(tid, (format!("worker-{tid}"), hz * u64::from(tid) / 100));
        }
        let (rows, total) = diff_threads(&prev, &now, 1.0);
        assert_eq!(rows.len(), MAX_THREADS_REPORTED, "list is capped");
        assert_eq!(rows[0].name, "worker-40", "hottest thread sorts first");
        assert!(
            rows.windows(2).all(|w| w[0].cpu_pct >= w[1].cpu_pct),
            "descending by cpu"
        );
        // Total spans every thread, not just the reported ones:
        // sum(1..=40) = 820 percent.
        assert!((total - 820.0).abs() < 1.0, "total was {total}");
    }

    #[test]
    fn thread_diff_drops_idle_threads() {
        let mut prev = HashMap::new();
        let mut now = HashMap::new();
        prev.insert(7_u32, 0_u64);
        now.insert(7_u32, ("idle-thread".to_string(), 0_u64));
        let (rows, total) = diff_threads(&prev, &now, 1.0);
        assert!(rows.is_empty(), "a thread using no CPU is not reported");
        assert_eq!(total, 0.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pressure_parses_or_is_absent() {
        // Kernels without CONFIG_PSI legitimately have no such file, so
        // this asserts on shape rather than presence.
        if let Some(p) = read_pressure() {
            if let Some(cpu) = p.cpu {
                assert!(cpu.some_avg10 >= 0.0, "percentages are non-negative");
                assert!(
                    cpu.full_avg10 == 0.0,
                    "cpu `full` is zero by definition, got {}",
                    cpu.full_avg10
                );
            }
        }
    }

    #[test]
    fn kernel_device_name_takes_basename() {
        assert_eq!(
            kernel_device_name("/dev/nvme0n1p2").as_deref(),
            Some("nvme0n1p2")
        );
        assert_eq!(kernel_device_name("tmpfs"), None, "non-/dev is skipped");
    }
}
