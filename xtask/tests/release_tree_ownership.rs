//! Release trees must end root-owned and free of macOS metadata (BUG-337).
//!
//! GNU tar run as root restores the archive's owners, and CI packs as the
//! runner's uid 1001, which on an appliance is `nexus-remote` — the support
//! login that is deliberately not in sudo (ADR-066). It then owned
//! `bin/nexus-engine` and the scripts `refresh_appliers` copies into
//! /usr/local/sbin as root. Nothing on a box reports ownership, so this asserts
//! over the shipped shell instead.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn read(rel: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ always has a parent")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Code lines (comments dropped) that run `tar` with an extract flag.
fn tar_extracts(src: &str) -> Vec<&str> {
    src.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter(|l| {
            l.split_whitespace()
                .skip_while(|w| *w != "tar")
                .any(|f| f.starts_with('-') && !f.starts_with("--") && f.contains('x'))
        })
        .collect()
}

#[test]
fn applier_extracts_without_archive_owners_and_chowns_every_release() {
    let applier = read("deploy/nexus-apply-release");
    let extracts = tar_extracts(&applier);
    assert!(
        !extracts.is_empty(),
        "expected the applier to extract the staged tarball"
    );
    for line in extracts {
        assert!(
            line.contains("--no-same-owner"),
            "root-run extract keeps the archive's owners: {line}"
        );
    }
    // Releases already on disk (rollback targets) were extracted by older
    // appliers with the CI uid; the next apply must take them back too.
    assert!(
        applier.contains(r#"chown -hR root:root "$releases_dir""#),
        "do_apply must chown the whole releases dir to root"
    );
}

#[test]
fn release_tarball_is_packed_as_root_and_verified() {
    let wf = read(".github/workflows/release.yml");
    let pack = wf
        .lines()
        .find(|l| l.contains(r#"tar "#) && l.contains(r#""${name}.tar.gz" "$topdir""#))
        .expect("release.yml must pack ${name}.tar.gz from $topdir");
    for flag in ["--owner=0", "--group=0", "--numeric-owner"] {
        assert!(
            pack.contains(flag),
            "release tarball packed without {flag}: {pack}"
        );
    }
    assert!(
        wf.contains("- name: verify tarball owners and metadata"),
        "release.yml must fail the release on a non-root entry or macOS metadata"
    );
}

#[test]
fn tts_payload_drops_macos_metadata() {
    let wf = read(".github/workflows/release.yml");
    let line = wf
        .lines()
        .find(|l| l.contains("models/tts.tar") && l.contains("-xf"))
        .expect("release.yml must extract models/tts.tar");
    for pat in [
        "--exclude='._*'",
        "--exclude='.DS_Store'",
        "--exclude='__MACOSX'",
    ] {
        assert!(
            line.contains(pat),
            "tts.tar extract keeps macOS metadata ({pat}): {line}"
        );
    }
}
