//! The unattended-upgrades hold must cover every VA-API driver the
//! installer can put on a box (issue #337, PR #366 review).
//!
//! The hold is a list of package-name regexes in a heredoc, and the driver
//! packages are named in three other places (the install arrays, the OTA
//! requirements file, and the OTA allowlist). A driver added to any of those
//! but not to the hold is upgraded overnight, untested, and nothing fails —
//! holding the `va-driver-all` metapackage does not hold its dependencies.

use std::collections::BTreeSet;
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

/// Every VA driver package named on a non-comment line of `text`.
fn va_driver_packages(text: &str, out: &mut BTreeSet<String>) {
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for token in line.split(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c))) {
            if token.contains("va-driver") {
                out.insert(token.to_string());
            }
        }
    }
}

#[test]
fn unattended_upgrades_hold_covers_every_installed_va_driver() {
    let install = read("scripts/lib/install-common.sh");
    let hold = install
        .split_once("Unattended-Upgrade::Package-Blacklist {")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(block, _)| block)
        .expect("install-common.sh lost the Unattended-Upgrade::Package-Blacklist block");

    let mut packages = BTreeSet::new();
    va_driver_packages(&install, &mut packages);
    va_driver_packages(&read("deploy/apt-requirements.txt"), &mut packages);
    va_driver_packages(&read("deploy/nexus-apply-deps"), &mut packages);
    // Not named by the installer: arrives as a dependency of `va-driver-all`.
    packages.insert("intel-media-va-driver".to_string());

    let unheld: Vec<_> = packages
        .iter()
        .filter(|p| !hold.contains(&format!("\"^{}$\";", p.replace('.', "\\."))))
        .collect();
    assert!(
        unheld.is_empty(),
        "VA drivers installed but not held from unattended-upgrades: {unheld:?}"
    );
}
