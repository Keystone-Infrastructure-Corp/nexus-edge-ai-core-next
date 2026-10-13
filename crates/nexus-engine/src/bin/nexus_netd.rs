//! `nexus-netd` — privileged helper invoked by the engine to
//! mutate `/etc/netplan/90-nexus.yaml` and run `netplan
//! generate` + `netplan apply` (with rollback safety).
//!
//! The engine binary itself runs as `nexus_admin` without
//! CAP_NET_ADMIN. This helper is the *only* code path that
//! touches OS-level network state. Sudoers entry (installed by
//! `deploy/sudoers.d/nexus-netd`):
//!
//! ```text
//! nexus_admin ALL=(root) NOPASSWD: /usr/local/lib/nexus/nexus-netd
//! ```
//!
//! Subcommands:
//!
//!   apply <staged.yaml>
//!     Validates `<staged.yaml>` is valid YAML, copies the
//!     existing `/etc/netplan/90-nexus.yaml` to
//!     `/etc/netplan/.90-nexus.yaml.bak` (so `rollback` has
//!     something to restore), atomically renames `<staged>` into
//!     place, then runs `netplan generate && netplan apply`.
//!
//!   confirm
//!     Removes the `.bak`. Idempotent.
//!
//!   rollback
//!     If `.bak` exists, restore it + re-apply. If not, no-op
//!     and exit 0 (so the engine's rollback-on-timeout never
//!     errors when nothing was applied).
//!
//! Exit codes:
//!   0 = success
//!   1 = usage / invocation error
//!   2 = OS-side failure (file I/O, netplan command failed)
//!   3 = platform unsupported (non-Linux)
// `allow`, not `expect`: which prints fire depends on the target OS (the
// non-Linux stub and the Linux path print different streams).
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "privileged one-shot helper invoked via sudo; its status lines on stdout/stderr are read by the calling engine, no tracing subscriber is installed"
)]

use std::env;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(target_os = "linux")]
const TARGET: &str = "/etc/netplan/90-nexus.yaml";
#[cfg(target_os = "linux")]
const BACKUP: &str = "/etc/netplan/.90-nexus.yaml.bak";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let (sub, rest) = match args.split_first() {
        Some(x) => x,
        None => {
            usage_to_stderr();
            return ExitCode::from(1);
        }
    };

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("platform_unsupported: nexus-netd only runs on Linux");
        let _ = sub;
        let _ = rest;
        ExitCode::from(3)
    }

    #[cfg(target_os = "linux")]
    {
        let rc = match sub.as_str() {
            "apply" => cmd_apply(rest),
            "confirm" => cmd_confirm(),
            "rollback" => cmd_rollback(),
            _ => {
                usage_to_stderr();
                2
            }
        };
        ExitCode::from(rc)
    }
}

fn usage_to_stderr() {
    eprintln!(
        "usage: nexus-netd <apply <staged.yaml> | confirm | rollback>\n\
         must be invoked via the sudoers entry that pins this absolute path."
    );
}

#[cfg(target_os = "linux")]
fn cmd_apply(rest: &[String]) -> u8 {
    let staged = match rest.first() {
        Some(s) => PathBuf::from(s),
        None => {
            eprintln!("apply: missing <staged.yaml> argument");
            return 1;
        }
    };
    if !staged.is_file() {
        eprintln!("apply: staged path `{}` is not a file", staged.display());
        return 1;
    }
    // Validate as YAML before we touch /etc/netplan. Catches
    // truncated upload + the obvious "operator typed garbage"
    // case before disturbing the live config.
    let body = match std::fs::read_to_string(&staged) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("apply: read `{}` failed: {e}", staged.display());
            return 2;
        }
    };
    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&body) {
        eprintln!("apply: staged YAML is not valid: {e}");
        return 1;
    }

    // Back up the existing managed file (if any) so `rollback`
    // has something to restore. We do NOT touch other
    // /etc/netplan/*.yaml files — operator-managed configs are
    // left alone.
    if Path::new(TARGET).exists() {
        if let Err(e) = std::fs::copy(TARGET, BACKUP) {
            eprintln!("apply: backup `{TARGET}` -> `{BACKUP}` failed: {e}");
            return 2;
        }
    } else if Path::new(BACKUP).exists() {
        // No live config but a stale backup — clear it so a
        // rollback after this apply doesn't restore the wrong
        // bytes.
        let _ = std::fs::remove_file(BACKUP);
    }

    // Atomic swap via rename. Cross-fs rename is fine because
    // both paths are under /etc on every supported tier.
    if let Err(e) = std::fs::copy(&staged, TARGET) {
        eprintln!("apply: write `{TARGET}` failed: {e}");
        return 2;
    }
    // Tight perms — netplan refuses to apply files with
    // world-readable mode (warnings appear in journalctl). Match
    // the canonical 0600 root:root.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(TARGET) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = std::fs::set_permissions(TARGET, perm);
        }
    }

    if let Err(e) = run("netplan", &["generate"]) {
        eprintln!("apply: netplan generate failed: {e}");
        let _ = restore_backup();
        let _ = run("netplan", &["apply"]);
        return 2;
    }
    if let Err(e) = run("netplan", &["apply"]) {
        eprintln!("apply: netplan apply failed: {e}");
        let _ = restore_backup();
        let _ = run("netplan", &["apply"]);
        return 2;
    }
    0
}

#[cfg(target_os = "linux")]
fn cmd_confirm() -> u8 {
    if Path::new(BACKUP).exists() {
        if let Err(e) = std::fs::remove_file(BACKUP) {
            eprintln!("confirm: remove `{BACKUP}` failed: {e}");
            return 2;
        }
    }
    0
}

#[cfg(target_os = "linux")]
fn cmd_rollback() -> u8 {
    if !Path::new(BACKUP).exists() {
        // Nothing to restore. The engine's rollback-on-timeout
        // calls this defensively; we mustn't surface an error
        // for the no-op case.
        return 0;
    }
    if let Err(e) = restore_backup() {
        eprintln!("rollback: restore failed: {e}");
        return 2;
    }
    if let Err(e) = run("netplan", &["generate"]) {
        eprintln!("rollback: netplan generate failed: {e}");
        return 2;
    }
    if let Err(e) = run("netplan", &["apply"]) {
        eprintln!("rollback: netplan apply failed: {e}");
        return 2;
    }
    0
}

#[cfg(target_os = "linux")]
fn restore_backup() -> std::io::Result<()> {
    std::fs::copy(BACKUP, TARGET)?;
    std::fs::remove_file(BACKUP)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run(prog: &str, args: &[&str]) -> std::io::Result<()> {
    let status = std::process::Command::new(prog).args(args).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{prog} {args:?} exited {status:?}"
        )));
    }
    Ok(())
}
