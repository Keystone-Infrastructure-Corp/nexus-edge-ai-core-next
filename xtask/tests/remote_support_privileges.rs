//! Provisioning invariants for admin-granted remote support (ADR-066 / SPEC-067).
//!
//! These assert over the shipped shell artifacts rather than over Rust, because
//! that is where the behaviour lives: the support login's privileges are decided
//! by a group list in one script, an allowlist in another, and a `cp` line in a
//! release workflow. Every failure mode this guards is silent — a package that
//! is declared but not allowlisted is skipped with a warning nobody reads, and a
//! wrapper that is never copied into the tarball leaves a sudoers grant pointing
//! at a file that does not exist. Nothing else in CI would notice either one.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ always has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The support login must be able to see the accelerators.
///
/// On a box whose entire job is hardware decode and accelerated inference, a
/// support account without `video`/`render` is the only identity present that
/// cannot open `/dev/dri`, `/dev/accel` or `/dev/hailo` — which is exactly the
/// class of fault it gets called for.
///
/// Note `ensure_ssh_account` skips any group `getent` cannot resolve, so a
/// typo here fails silently rather than loudly. Both names were confirmed
/// present on a live appliance (`video:44`, `render:993`); this guards the
/// spelling.
#[test]
fn support_login_keeps_its_diagnostic_groups() {
    let applier = read("deploy/nexus-apply-release");
    let line = applier
        .lines()
        .find(|l| l.starts_with("ssh_account_groups="))
        .expect("nexus-apply-release must define ssh_account_groups");

    for group in ["nexus", "adm", "systemd-journal", "video", "render"] {
        assert!(
            line.contains(group),
            "ssh_account_groups lost `{group}`: {line}\n\
             The support login is provisioned from this one list (ADR-066)."
        );
    }
}

/// `sudo` for the support login must stay a single pinned path.
#[test]
fn support_login_gets_exactly_one_sudo_grant() {
    let sudoers = read("deploy/sudoers.d/nexus-diag");
    let grants: Vec<&str> = sudoers
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect();

    assert_eq!(
        grants,
        vec!["nexus-remote ALL=(root) NOPASSWD: /usr/local/sbin/nexus-diag *"],
        "the support grant must stay exactly one argv-independent wildcard on the \
         pinned wrapper path — per-command rules couple this file to a caller's \
         argv, which is the drift that bricked an OTA once already"
    );
    assert!(
        !sudoers.contains("NOPASSWD: ALL"),
        "general sudo for the support login is refused by ADR-066"
    );

    let applier = read("deploy/nexus-apply-release");
    assert!(
        applier.contains("visudo -cf"),
        "the grant must be validated with `visudo -c` before install — a malformed \
         drop-in breaks sudo for every user on the box, including the OTA path"
    );
}

/// The grant has to reach boxes that are already in the field.
///
/// It ships in the release under `etc-templates/`, exactly like the OTA grant,
/// and is applied by the installer on a fresh box and by the applier during
/// `ssh-ca-install` on a deployed one. Drop either leg and the grant arrives
/// only where somebody visited.
#[test]
fn grant_ships_in_the_release_and_installs_on_both_paths() {
    let release_yml = read(".github/workflows/release.yml");
    assert!(
        release_yml.contains(r#""$R/etc-templates/sudoers.d/nexus-diag""#),
        "release.yml no longer stages deploy/sudoers.d/nexus-diag into the tarball; \
         both install paths read it from there and would silently skip the grant"
    );

    let applier = read("deploy/nexus-apply-release");
    assert!(
        applier.contains("$current_link/etc-templates/sudoers.d/nexus-diag"),
        "nexus-apply-release must install the grant from the release payload"
    );
    assert!(
        applier.contains("    ensure_diag_sudoers\n}"),
        "ensure_diag_sudoers must still be called from ssh-ca-install, which is what \
         confines the grant to boxes with remote access enabled"
    );

    let install = read("scripts/lib/install-common.sh");
    assert!(
        install.contains("install_diag_sudoers()"),
        "install-common.sh lost the fresh-install path for the grant"
    );
    assert!(
        read("scripts/install.sh").contains("install_diag_sudoers \"$RELEASE_DIR\""),
        "install.sh no longer calls install_diag_sudoers"
    );
}

/// Every package the release declares must survive the OTA allowlist.
#[test]
fn every_declared_package_is_allowlisted() {
    let requirements = read("deploy/apt-requirements.txt");
    let deps = read("deploy/nexus-apply-deps");

    // The allowlist arm is a shell `case`; assert membership textually rather
    // than re-implementing the matcher, so this fails when the arm is edited.
    let allowlist = deps
        .split_once("is_allowed()")
        .expect("nexus-apply-deps must define is_allowed")
        .1;
    let allowlist = allowlist
        .split_once("\n}")
        .expect("is_allowed must close")
        .0;

    for pkg in requirements
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let covered = allowlist.contains(pkg)
            || (pkg.starts_with("gstreamer1.0-") && allowlist.contains("gstreamer1.0-*"));
        assert!(
            covered,
            "`{pkg}` is declared in deploy/apt-requirements.txt but is not in the \
             nexus-apply-deps allowlist — the OTA would skip it with a warning and \
             the tool would simply be missing on a minimized image"
        );
    }
}

/// The diagnostic tools support actually needs must stay declared.
#[test]
fn diagnostic_tooling_stays_declared() {
    let requirements = read("deploy/apt-requirements.txt");
    for pkg in ["tcpdump", "lsof", "strace", "smartmontools"] {
        assert!(
            requirements.lines().any(|l| l.trim() == pkg),
            "`{pkg}` is no longer declared; a support engineer's first move in a \
             time-boxed session would be discovering the tool is absent and that \
             they cannot run apt (ADR-066)"
        );
    }
}

/// A wrapper that never reaches the tarball is a grant pointing at nothing.
#[test]
fn diag_wrapper_is_shipped_installed_and_refreshed() {
    assert!(
        repo_root().join("deploy/nexus-diag").is_file(),
        "deploy/nexus-diag is missing"
    );

    let release_yml = read(".github/workflows/release.yml");
    assert!(
        release_yml.contains("cp deploy/nexus-diag"),
        ".github/workflows/release.yml no longer stages deploy/nexus-diag into the \
         release tarball — the installer and the OTA refresh both look for \
         scripts/nexus-diag and would silently skip it, leaving the sudoers grant \
         pointing at a file that does not exist"
    );
    assert!(
        release_yml.contains("\"$R/scripts/nexus-diag\""),
        "release.yml must also mark scripts/nexus-diag executable"
    );

    let applier = read("deploy/nexus-apply-release");
    assert!(
        applier.contains("for name in nexus-apply-release nexus-apply-deps nexus-diag; do"),
        "nexus-diag dropped out of refresh_appliers — a fix to it could then never \
         reach an already-installed box over the air"
    );

    let install = read("scripts/lib/install-common.sh");
    assert!(
        install.contains("install_diag_wrapper()"),
        "install-common.sh no longer installs the diagnostic wrapper"
    );
    assert!(
        read("scripts/install.sh").contains("install_diag_wrapper \"$RELEASE_DIR\""),
        "install.sh no longer calls install_diag_wrapper"
    );
}

/// The wrapper is the whole privilege boundary, so its shape is the invariant.
#[test]
fn diag_wrapper_validates_and_bounds_itself() {
    let diag = read("deploy/nexus-diag");

    assert!(
        diag.contains("set -euo pipefail"),
        "nexus-diag must fail closed"
    );
    for mode in ["restart-engine", "capture", "hw-report", "engine-proc"] {
        assert!(
            diag.contains(mode),
            "nexus-diag lost the `{mode}` subcommand"
        );
    }
    assert!(
        diag.contains("*) die \"unknown mode"),
        "nexus-diag must refuse unknown modes rather than falling through — a \
         permissive default in a root-owned wrapper is the whole attack surface"
    );
    assert!(
        diag.contains(r#"[[ "$arg1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,14}$ ]]"#),
        "the capture interface must stay charset-validated; it is the only \
         caller-supplied value that reaches a command's argv"
    );
    assert!(
        diag.contains("capture_max_secs=") && diag.contains("capture_max_packets="),
        "packet capture must stay bounded in both duration and size — a capture \
         that outlives the session is a packet dump nobody is watching filling a \
         customer's disk"
    );
    assert!(
        !diag.contains("$arg1\" 2>/dev/null | sh") && !diag.contains("eval "),
        "nexus-diag must never eval caller input"
    );
    assert!(
        diag.contains("audit()") && diag.contains("SUDO_USER"),
        "every privileged invocation must be logged: native sessions are not \
         recorded (ADR-005), so this is the only trail of what was done as root"
    );
}

/// Uninstall must take the grant with it.
#[test]
fn uninstall_removes_the_grant_and_the_wrapper() {
    let uninstall = read("scripts/uninstall.sh");
    assert!(
        uninstall.contains("/etc/sudoers.d/nexus-diag"),
        "uninstall.sh must remove the support sudoers grant; it is written by the \
         applier rather than the installer, so it is easy to forget here"
    );
    assert!(
        uninstall.contains("/usr/local/sbin/nexus-diag"),
        "uninstall.sh must remove the diagnostic wrapper"
    );
}
