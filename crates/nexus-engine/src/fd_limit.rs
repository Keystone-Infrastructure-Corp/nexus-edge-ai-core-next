//! Raise the per-process file-descriptor cap at startup.
//!
//! Why this exists
//! ---------------
//! The LAN discovery sweep races ~3 TCP probes per host (RTSP +
//! ONVIF/80 + ONVIF/8080) with a default 64-host concurrency,
//! which means up to ~192 sockets are open simultaneously at
//! peak. Combined with the engine's own footprint (axum HTTP
//! listener + open client connections, sqlite connection pool,
//! gstreamer pipelines for live cameras, OS bookkeeping FDs),
//! a /24 sweep can easily exceed the macOS default `ulimit -n`
//! of 256 — at which point `socket()` starts returning EMFILE
//! ("Too many open files") and, worse, GLib (used transitively
//! by gstreamer for pipeline-internal pipes) calls `g_error()`
//! and aborts the entire process via SIGTRAP.
//!
//! Strategy
//! --------
//! On startup we read the current RLIMIT_NOFILE soft/hard pair
//! and raise the soft limit toward the hard limit (capped at a
//! sane 65_536). macOS in particular sets a low default soft
//! limit (256–4096) but allows raising it to `OPEN_MAX` /
//! `kern.maxfilesperproc` (typically 24_576) without root. If
//! the bump fails we log a warning and continue with the old
//! limit — the scan code clamps its concurrency to whatever
//! limit is in effect.

#![cfg(unix)]

use tracing::{info, warn};

/// Hard cap we never exceed even if the kernel allows more.
/// 65_536 is enough headroom for the largest realistic sweep
/// (/22 = 1024 hosts × 3 ports = 3072 in-flight sockets at
/// max concurrency) plus the engine baseline.
const TARGET_SOFT: u64 = 65_536;

/// Raise the soft `RLIMIT_NOFILE` toward the hard limit and
/// return the resulting soft limit. Always returns a usable
/// number — falls back to the original soft limit on any error
/// so the caller can still clamp scan concurrency sensibly.
pub fn raise_fd_soft_limit() -> u64 {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};

    let (soft, hard) = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok(pair) => pair,
        Err(e) => {
            warn!(error = %e, "getrlimit(RLIMIT_NOFILE) failed; cannot raise FD cap");
            return 1024; // conservative guess
        }
    };

    // Target = min(hard, TARGET_SOFT). On macOS the hard limit
    // may report as a huge value (effectively unbounded), but
    // `setrlimit` will refuse anything above `kern.maxfilesperproc`
    // (~24_576). We try the target first, then fall back through
    // a couple of common ceilings before giving up.
    let target = hard.min(TARGET_SOFT);
    if target <= soft {
        // Already at or above where we'd ask. Nothing to do.
        info!(soft, hard, "FD limit already adequate");
        return soft;
    }

    for candidate in [target, 24_576, 10_240, 4_096, 2_048] {
        if candidate <= soft {
            continue;
        }
        match setrlimit(Resource::RLIMIT_NOFILE, candidate, hard) {
            Ok(()) => {
                info!(
                    soft_before = soft,
                    soft_after = candidate,
                    hard,
                    "raised FD soft limit"
                );
                return candidate;
            }
            Err(e) => {
                warn!(
                    candidate,
                    error = %e,
                    "setrlimit attempt failed; trying lower"
                );
            }
        }
    }

    warn!(
        soft,
        hard, "every setrlimit attempt failed; keeping original FD limit"
    );
    soft
}

/// Read the CURRENT soft `RLIMIT_NOFILE`. Used by the discovery
/// scanner to clamp its concurrency at runtime — even after a
/// successful `raise_fd_soft_limit()` the operator may have
/// loaded a config that asks for more sockets than the kernel
/// will give us, in which case we'd rather scan slowly than
/// abort the process.
pub fn current_fd_soft_limit() -> u64 {
    use nix::sys::resource::{getrlimit, Resource};
    getrlimit(Resource::RLIMIT_NOFILE)
        .map(|(soft, _hard)| soft)
        .unwrap_or(1024)
}
