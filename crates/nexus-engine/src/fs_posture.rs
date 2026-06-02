//! Phase 6.14b — filesystem posture probe for the directories that
//! hold edge-side secrets at rest.
//!
//! Two locations matter for the edge appliance:
//!
//! * `/etc/nexus/tls/` — mTLS client cert + key used by
//!   `nexus_cloud_client::tunnel` to authenticate to the cloud
//!   `edge-gateway`. Mode `2750 root:nexus` is enforced by the
//!   installer (`scripts/lib/install-common.sh`).
//! * `/var/lib/nexus/` — SQLite database (`nexus.db`) which holds
//!   the local-auth admin password hash and the cloud-enrollment
//!   record. Mode `0750 nexus:nexus`.
//!
//! Posix file modes are a first line of defence (`other` cannot
//! read either path). The mount options on the underlying
//! filesystem are the second line:
//!
//! * `nodev`  — block any character/block device nodes from being
//!   honoured under this mount. Defence-in-depth against a local
//!   attacker who chrooted in and tried to mknod a backdoor over a
//!   privileged path.
//! * `nosuid` — block setuid/setgid bits from elevating privileges
//!   on any binary on this mount. Same rationale.
//!
//! Both flags are safe to set on any non-root mount and incur no
//! runtime cost.  They are *not* defaults — Ubuntu Server's stock
//! `/var` and `/etc` live on the root filesystem, so unless an
//! operator carved a dedicated `/var` mount AND opted in via
//! `/etc/fstab`, the secret paths inherit whatever `/` has (almost
//! always neither flag).
//!
//! ## Severity
//!
//! Missing `nodev` / `nosuid` is a **warn**, never a **fail**.  A
//! single-root-mount appliance is the common case and is not
//! actually broken — the file modes still keep the secrets private.
//! The probe surfaces the missing flags so an operator who *can*
//! re-mount with the hardening flags knows to do so.
//!
//! ## Consumers
//!
//! * `nexus-doctor` step `9.10` (`filesystem_posture`) calls
//!   [`probe_paths`] over the canonical TLS + state dirs.
//! * `scripts/install.sh` post-install banner reuses the same
//!   probe via `nexus-doctor` after `systemctl start nexus-engine`.
//!
//! ## Platform
//!
//! Linux-only at runtime.  `/proc/self/mountinfo` is the canonical
//! interface (man `proc(5)`).  On macOS / Windows the probe returns
//! `PathPosture::unsupported` so the doctor row shows a `Skip`.
//! The parser itself is cross-platform so the unit tests run on
//! every developer workstation, not just CI.

// Doctor's `check_filesystem_posture` only consumes this module on
// Linux; on macOS the doctor short-circuits to `Outcome::skip`
// without touching `probe_paths`. Suppress the resulting
// "function never used" warning on non-Linux platforms so macOS
// dev builds stay clean.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

use std::path::{Path, PathBuf};

/// Posture of a single inspected path. `mount_point` is the longest
/// mount-point prefix that covers `path`; `options` is the raw
/// mount-option list from `/proc/self/mountinfo` (super-block
/// options are appended after the per-mount options so that callers
/// see the full effective set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPosture {
    pub path: PathBuf,
    /// `None` when the path was not found in mountinfo (does not
    /// exist on disk, or the probe could not read mountinfo).
    pub mount_point: Option<PathBuf>,
    /// Combined per-mount + super-block options. Empty when
    /// `mount_point` is `None`.
    pub options: Vec<String>,
    pub has_nodev: bool,
    pub has_nosuid: bool,
    /// Human-readable detail surfaced as the `actual` doctor field.
    pub detail: String,
}

impl PathPosture {
    /// Convenience constructor for non-Linux callers (and the test
    /// fixture that needs to fabricate a "skip" row).
    pub fn unsupported(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            mount_point: None,
            options: Vec::new(),
            has_nodev: false,
            has_nosuid: false,
            detail: "probe unsupported on this platform".into(),
        }
    }

    /// True when both hardening flags are present.
    pub fn is_hardened(&self) -> bool {
        self.has_nodev && self.has_nosuid
    }

    /// Names of the flags that are missing, in canonical order.
    /// Empty when [`is_hardened`] is true.
    pub fn missing_flags(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.has_nodev {
            out.push("nodev");
        }
        if !self.has_nosuid {
            out.push("nosuid");
        }
        out
    }
}

/// Probe several paths in one call. Reads `/proc/self/mountinfo`
/// exactly once.
#[cfg(target_os = "linux")]
pub fn probe_paths(paths: &[&Path]) -> Vec<PathPosture> {
    let body = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(s) => s,
        Err(e) => {
            return paths
                .iter()
                .map(|p| PathPosture {
                    path: p.to_path_buf(),
                    mount_point: None,
                    options: Vec::new(),
                    has_nodev: false,
                    has_nosuid: false,
                    detail: format!("cannot read /proc/self/mountinfo: {e}"),
                })
                .collect();
        }
    };
    paths
        .iter()
        .map(|p| parse_mountinfo_for_path(&body, p))
        .collect()
}

/// Test-friendly stub for non-Linux builds. Real callers on macOS
/// should use [`PathPosture::unsupported`] directly.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)] // doctor short-circuits on macOS before calling probe_paths
pub fn probe_paths(paths: &[&Path]) -> Vec<PathPosture> {
    paths.iter().map(|p| PathPosture::unsupported(p)).collect()
}

/// Parse `/proc/self/mountinfo` and return the posture for `path`.
/// Picks the entry whose `mount_point` is the longest prefix of
/// `path`.  Pub(crate) for unit testing — the doctor and the
/// `probe_paths` entry point read the real file.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_mountinfo_for_path(body: &str, path: &Path) -> PathPosture {
    let mut best: Option<MountEntry> = None;
    for line in body.lines() {
        let Some(entry) = parse_mountinfo_line(line) else {
            continue;
        };
        if !path_starts_with(path, &entry.mount_point) {
            continue;
        }
        let better = match &best {
            None => true,
            Some(current) => {
                entry.mount_point.as_os_str().len() > current.mount_point.as_os_str().len()
            }
        };
        if better {
            best = Some(entry);
        }
    }
    match best {
        Some(entry) => {
            let mut combined = entry.mount_options.clone();
            // `mountinfo` reports per-mount options before the `-`
            // separator and super-block options after. Either one
            // can set `nodev` / `nosuid`; merge them so the probe
            // reflects the effective view.
            for o in &entry.super_options {
                if !combined.contains(o) {
                    combined.push(o.clone());
                }
            }
            let has_nodev = combined.iter().any(|o| o == "nodev");
            let has_nosuid = combined.iter().any(|o| o == "nosuid");
            let detail = format!(
                "mount_point={} fs_type={} options={}",
                entry.mount_point.display(),
                entry.fs_type,
                combined.join(","),
            );
            PathPosture {
                path: path.to_path_buf(),
                mount_point: Some(entry.mount_point),
                options: combined,
                has_nodev,
                has_nosuid,
                detail,
            }
        }
        None => PathPosture {
            path: path.to_path_buf(),
            mount_point: None,
            options: Vec::new(),
            has_nodev: false,
            has_nosuid: false,
            detail: format!(
                "no mount entry covers {} (path may not exist or mountinfo was empty)",
                path.display()
            ),
        },
    }
}

/// True when `path` equals `prefix` or has `prefix` as a directory
/// ancestor. Operates on `Path` components so `/varlib` does not
/// match a `/var` prefix.
#[cfg(any(target_os = "linux", test))]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    let mut p_iter = path.components();
    for pref_comp in prefix.components() {
        match p_iter.next() {
            Some(c) if c == pref_comp => {}
            _ => return false,
        }
    }
    true
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
struct MountEntry {
    mount_point: PathBuf,
    mount_options: Vec<String>,
    fs_type: String,
    super_options: Vec<String>,
}

/// Parse a single `/proc/self/mountinfo` line. Format per `proc(5)`:
///
/// ```text
/// 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
/// (1)(2)(3)  (4)   (5)   (6)        (7)      (8) (9)   (10)        (11)
/// ```
///
/// Fields 1–6 are fixed-position; the optional fields (7) end at a
/// literal `-` separator; fields after the dash are
/// `fs_type source_dev super_options`.
#[cfg(any(target_os = "linux", test))]
fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let mut parts = line.split(' ');
    // Skip mount_id, parent_id, major:minor, root.
    parts.next()?; // 1 mount_id
    parts.next()?; // 2 parent_id
    parts.next()?; // 3 major:minor
    parts.next()?; // 4 root
    let mount_point_raw = parts.next()?; // 5 mount_point
    let mount_options_raw = parts.next()?; // 6 mount_options

    // Optional fields up to `-`.
    for tok in parts.by_ref() {
        if tok == "-" {
            break;
        }
    }
    let fs_type = parts.next()?.to_string();
    parts.next()?; // mount source
    let super_options_raw = parts.next().unwrap_or("");

    Some(MountEntry {
        mount_point: PathBuf::from(decode_mountinfo_octal(mount_point_raw)),
        mount_options: split_options(mount_options_raw),
        fs_type,
        super_options: split_options(super_options_raw),
    })
}

#[cfg(any(target_os = "linux", test))]
fn split_options(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// `/proc/self/mountinfo` encodes spaces, tabs, newlines, and
/// backslashes in path fields as octal escapes (`\040`, `\011`,
/// `\012`, `\134`). Decode them so a path like
/// `/mnt/foo bar` round-trips correctly.
#[cfg(any(target_os = "linux", test))]
fn decode_mountinfo_octal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let triple = &bytes[i + 1..i + 4];
            if triple.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let val =
                    ((triple[0] - b'0') << 6) | ((triple[1] - b'0') << 3) | (triple[2] - b'0');
                out.push(val as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic /proc/self/mountinfo with a typical Ubuntu Server
    // layout: root + a separate /var on ext4 with the hardening
    // flags, plus a tmpfs for /run.
    const SAMPLE_HARDENED: &str = "\
22 28 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
23 28 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:14 - proc proc rw
28 1 259:1 / / rw,relatime shared:1 - ext4 /dev/root rw,errors=remount-ro
35 28 8:17 / /var rw,relatime,nosuid,nodev shared:42 - ext4 /dev/sdb1 rw
40 35 0:42 / /var/lib/nexus rw,relatime shared:99 - ext4 /dev/sdc1 rw,nodev,nosuid
";

    // Root-only layout (the common appliance default) — `/var` and
    // `/etc` inherit `/`'s lack of hardening flags.
    const SAMPLE_ROOT_ONLY: &str = "\
22 28 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
28 1 259:1 / / rw,relatime shared:1 - ext4 /dev/root rw,errors=remount-ro
";

    #[test]
    fn longest_prefix_wins() {
        let posture =
            parse_mountinfo_for_path(SAMPLE_HARDENED, Path::new("/var/lib/nexus/nexus.db"));
        assert_eq!(
            posture.mount_point.as_deref(),
            Some(Path::new("/var/lib/nexus"))
        );
        assert!(posture.has_nodev, "expected nodev on /var/lib/nexus mount");
        assert!(
            posture.has_nosuid,
            "expected nosuid on /var/lib/nexus mount"
        );
        assert!(posture.is_hardened());
        assert!(posture.missing_flags().is_empty());
    }

    #[test]
    fn parent_mount_inherits_when_no_submount() {
        // `/etc` lives under `/` in the sample → falls back to `/`.
        let posture = parse_mountinfo_for_path(SAMPLE_HARDENED, Path::new("/etc/nexus/tls"));
        assert_eq!(posture.mount_point.as_deref(), Some(Path::new("/")));
        assert!(!posture.has_nodev);
        assert!(!posture.has_nosuid);
        assert_eq!(posture.missing_flags(), vec!["nodev", "nosuid"]);
    }

    #[test]
    fn root_only_layout_reports_missing_flags() {
        let posture = parse_mountinfo_for_path(SAMPLE_ROOT_ONLY, Path::new("/var/lib/nexus"));
        assert_eq!(posture.mount_point.as_deref(), Some(Path::new("/")));
        assert!(!posture.is_hardened());
        assert_eq!(posture.missing_flags(), vec!["nodev", "nosuid"]);
    }

    #[test]
    fn empty_mountinfo_returns_no_match() {
        let posture = parse_mountinfo_for_path("", Path::new("/var/lib/nexus"));
        assert!(posture.mount_point.is_none());
        assert!(posture.options.is_empty());
        assert!(posture.detail.contains("no mount entry"));
    }

    #[test]
    fn path_starts_with_respects_components() {
        // `/varlib` must NOT match prefix `/var` — boundary check.
        assert!(!path_starts_with(Path::new("/varlib/x"), Path::new("/var"),));
        assert!(path_starts_with(
            Path::new("/var/lib/nexus"),
            Path::new("/var"),
        ));
        assert!(path_starts_with(Path::new("/"), Path::new("/")));
    }

    #[test]
    fn super_options_merge_with_per_mount_options() {
        // `nosuid` appears only in the super-block options field;
        // probe must still see it.
        let body = "\
50 1 0:99 / /opt rw,relatime shared:1 - ext4 /dev/sde1 rw,nosuid
";
        let posture = parse_mountinfo_for_path(body, Path::new("/opt/something"));
        assert!(posture.has_nosuid);
        assert!(!posture.has_nodev);
    }

    #[test]
    fn octal_escapes_in_mount_point_decode() {
        // mountinfo encodes space as `\040`.
        let body = "\
60 1 0:88 / /mnt/with\\040space rw,nodev,nosuid shared:1 - ext4 /dev/sdf1 rw
";
        let posture = parse_mountinfo_for_path(body, Path::new("/mnt/with space/sub"));
        assert_eq!(
            posture.mount_point.as_deref(),
            Some(Path::new("/mnt/with space"))
        );
        assert!(posture.is_hardened());
    }

    #[test]
    fn unsupported_constructor_yields_skip_shape() {
        let posture = PathPosture::unsupported(Path::new("/etc/nexus/tls"));
        assert!(posture.mount_point.is_none());
        assert!(!posture.is_hardened());
        assert!(posture.detail.contains("unsupported"));
    }
}
