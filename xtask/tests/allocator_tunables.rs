//! Allocator-tuning invariants for the engine unit (BUG-036).
//!
//! These assert over the shipped systemd unit rather than over Rust, because
//! that is where the behaviour lives — `install_systemd_unit` copies the
//! template verbatim, so the unit file *is* the deployed configuration. Every
//! failure mode guarded here is silent: dropping either variable, misspelling
//! `MALLOC_MMAP_THRESHOLD_` without its trailing underscore, or "tidying" the
//! threshold upward all leave a perfectly healthy-looking engine that simply
//! stops returning memory. The symptom then takes a multi-hour soak on real
//! hardware to see, which is how the original bug survived as long as it did.

use std::path::{Path, PathBuf};

/// Widest supervisor frame at the smallest rung of the native-aspect ladder:
/// 512x288 RGB, the smallest per-sample buffer the RGB tap ever allocates.
const SMALLEST_RGB_FRAME_BYTES: u64 = 512 * 288 * 3;

fn unit() -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ always has a parent")
        .join("deploy/systemd/nexus-engine.service");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn env_value(unit: &str, key: &str) -> Option<String> {
    let prefix = format!("Environment={key}=");
    unit.lines()
        .map(str::trim)
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].to_string())
}

/// The mmap threshold must stay pinned, and pinned *below* a frame buffer.
///
/// The whole point is that the RGB tap's per-sample allocation keeps going
/// through `mmap`, so it is handed back to the OS on free instead of landing in
/// an arena that only trims from its top. A threshold at or above the frame size
/// silently reverts to the broken behaviour — the variable is still set, the
/// unit still looks tuned, and RSS climbs exactly as it did before.
#[test]
fn mmap_threshold_stays_below_a_frame_buffer() {
    let unit = unit();
    let raw = env_value(&unit, "MALLOC_MMAP_THRESHOLD_").expect(
        "nexus-engine.service must set MALLOC_MMAP_THRESHOLD_ (note the trailing \
         underscore — glibc ignores the name without it, so a rename is a silent \
         regression). Without it, retained RSS after a four-camera workload goes \
         from 320 MB back to ~4.5 GB (BUG-036).",
    );
    let threshold: u64 = raw
        .parse()
        .unwrap_or_else(|e| panic!("MALLOC_MMAP_THRESHOLD_={raw} is not a number: {e}"));

    assert!(
        threshold < SMALLEST_RGB_FRAME_BYTES,
        "MALLOC_MMAP_THRESHOLD_={threshold} is not below the smallest RGB frame \
         the tap allocates ({SMALLEST_RGB_FRAME_BYTES} bytes = 512x288x3). Above \
         that, frame buffers are served from the arenas again and never returned \
         to the OS, which is the entire fault BUG-036 fixed."
    );
}

/// The arena cap must stay, and stay small.
///
/// The engine runs 140+ threads and glibc will stand up an arena per contending
/// thread, each reserving 64 MiB it never hands back. On its own the cap only
/// recovers about a third of the retention, but the measured fix is the pair —
/// with the threshold pinned and the arenas uncapped, retention was still
/// 2072 MB against 320 MB for both together.
#[test]
fn arena_count_stays_capped() {
    let unit = unit();
    let raw = env_value(&unit, "MALLOC_ARENA_MAX")
        .expect("nexus-engine.service must set MALLOC_ARENA_MAX (BUG-036)");
    let arenas: u64 = raw
        .parse()
        .unwrap_or_else(|e| panic!("MALLOC_ARENA_MAX={raw} is not a number: {e}"));

    assert!(
        (1..=4).contains(&arenas),
        "MALLOC_ARENA_MAX={arenas} is outside the range this was measured over. \
         2 was measured at no throughput or CPU cost; raising it trades retained \
         memory back for allocator concurrency and needs a fresh run of \
         scripts/perf-bench/bug036-arena-ab.sh, not a guess."
    );
}
