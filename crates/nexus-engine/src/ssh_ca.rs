//! Edge-side adoption of the cloud's SSH certificate authority, plus the
//! per-session principal lifecycle that makes admin-granted remote support
//! access possible without any inbound port or standing credential.
//!
//! # Why this module is so small
//!
//! The engine runs unprivileged as the `nexus` system user and cannot write
//! anywhere under `/etc/ssh`. Everything privileged happens in the pinned,
//! root-owned applier `/usr/local/sbin/nexus-apply-release`, reached through
//! the single frozen sudoers grant described in
//! [`deploy/sudoers.d/nexus-update`]. This module's entire job is to stage
//! *data* into the engine-writable tree and then ask the applier to adopt it:
//!
//! | staged by the engine            | adopted by the applier                  |
//! |---------------------------------|-----------------------------------------|
//! | `$state/ssh/ca.pub`             | `ssh-ca-install` → `/etc/ssh/nexus_ca.pub` + sshd drop-in |
//! | `$state/ssh/principals`         | `ssh-principals-sync` → `/etc/ssh/nexus_principals/nexus-remote` |
//! | (nothing)                       | `sshd-restart` → operator recovery      |
//!
//! No mode takes a path argument, because sudo's `*` does not match `/`. The
//! staging paths are therefore fixed on both sides.
//!
//! # The trust story
//!
//! Adopting a CA public key means "certificates signed by this key may log in
//! as `nexus-remote`". That is a real grant, so it is fenced three ways:
//!
//! 1. **The CA key arrives in the enrollment bundle**, over the same mTLS
//!    channel that already carries the core's client certificate and the
//!    entitlement signing key. A core that was never enrolled has no CA.
//! 2. **`TrustedUserCAKeys` alone grants nothing.** sshd additionally requires
//!    the certificate's principal to appear in `AuthorizedPrincipalsFile`. The
//!    principal is the *session UUID*, so a valid certificate for a session
//!    that has ended — or that this core never heard about — is refused by
//!    sshd itself.
//! 3. **The account is unprivileged.** `nexus-remote` is not in `sudo`.
//!
//! Any one of the three failing closes the door, and the middle one is
//! revocable in milliseconds without touching sshd config.

use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// The pinned, root-owned applier. Identical to the constant in
/// [`crate::cloud_update`] — kept separate rather than shared so a future
/// refactor of the OTA path cannot silently repoint the SSH-CA path.
#[cfg(target_os = "linux")]
const APPLY_RELEASE_WRAPPER: &str = "/usr/local/sbin/nexus-apply-release";

/// Errors surfaced to the caller. Deliberately coarse: these strings cross the
/// tunnel into the cloud console's session list, so they must never carry a
/// path, a key, or a hostname.
///
/// Most variants are only ever constructed on Linux, where the privileged
/// applier exists; the dev-workstation build sees them as unconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum SshCaError {
    /// The staged file could not be written (disk full, permissions).
    Stage,
    /// The applier is absent, not executable, or the sudoers grant is missing.
    ApplierUnavailable,
    /// The applier ran but exited non-zero.
    ApplierFailed,
    /// The supplied CA public key is not a plausible single-line SSH key.
    MalformedKey,
    /// Compiled for a platform with no privileged applier.
    UnsupportedPlatform,
}

impl SshCaError {
    /// Stable machine-readable code for the wire / audit log.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Stage => "stage_failed",
            Self::ApplierUnavailable => "applier_unavailable",
            Self::ApplierFailed => "applier_failed",
            Self::MalformedKey => "malformed_ca_key",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

impl std::fmt::Display for SshCaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for SshCaError {}

/// Root of the engine-writable state tree. Mirrors the engine's own
/// `NEXUS_STATE_DIR` handling so tests can redirect it.
#[cfg(target_os = "linux")]
fn state_dir() -> PathBuf {
    std::env::var_os("NEXUS_STATE_DIR")
        .map_or_else(|| PathBuf::from("/var/lib/nexus"), PathBuf::from)
}

/// Directory the applier reads its SSH handoff files from.
#[cfg(target_os = "linux")]
fn ssh_stage_dir() -> PathBuf {
    state_dir().join("ssh")
}

/// Reject anything that is not a single, plausible OpenSSH public key line
/// before it ever reaches the privileged applier. The applier re-validates
/// with `ssh-keygen -l`; this is the cheap first gate so a malformed bundle
/// never costs a `sudo` round trip.
fn validate_ca_key(pubkey: &str) -> Result<&str, SshCaError> {
    let line = pubkey.trim();
    if line.is_empty() || line.lines().count() != 1 {
        return Err(SshCaError::MalformedKey);
    }
    // `<type> <base64>[ <comment>]`
    let mut parts = line.split_whitespace();
    let key_type = parts.next().ok_or(SshCaError::MalformedKey)?;
    let blob = parts.next().ok_or(SshCaError::MalformedKey)?;
    if !key_type.starts_with("ssh-") && !key_type.starts_with("ecdsa-") {
        return Err(SshCaError::MalformedKey);
    }
    if blob.len() < 16
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return Err(SshCaError::MalformedKey);
    }
    Ok(line)
}

/// Serialise the live-session set into the applier's expected format: one
/// session UUID per line, sorted for a stable file (so an unchanged set does
/// not churn the file's mtime).
fn render_principals(sessions: &BTreeSet<String>) -> String {
    let mut out = String::new();
    for s in sessions {
        out.push_str(s);
        out.push('\n');
    }
    out
}

#[cfg(target_os = "linux")]
fn stage(name: &str, contents: &str) -> Result<(), SshCaError> {
    use std::io::Write as _;
    let dir = ssh_stage_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        tracing::warn!(error = %e, "ssh-ca: staging dir create failed");
        SshCaError::Stage
    })?;
    // Write-then-rename so the applier can never observe a half-written file.
    let tmp = dir.join(format!("{name}.tmp"));
    let final_path = dir.join(name);
    let mut f = std::fs::File::create(&tmp).map_err(|e| {
        tracing::warn!(error = %e, "ssh-ca: staging file create failed");
        SshCaError::Stage
    })?;
    f.write_all(contents.as_bytes()).map_err(|e| {
        tracing::warn!(error = %e, "ssh-ca: staging file write failed");
        SshCaError::Stage
    })?;
    f.sync_all().map_err(|e| {
        tracing::warn!(error = %e, "ssh-ca: staging file sync failed");
        SshCaError::Stage
    })?;
    drop(f);
    std::fs::rename(&tmp, &final_path).map_err(|e| {
        tracing::warn!(error = %e, "ssh-ca: staging file rename failed");
        SshCaError::Stage
    })
}

/// Run one argument-less applier mode under `sudo -n`.
#[cfg(target_os = "linux")]
fn run_applier(mode: &'static str) -> Result<(), SshCaError> {
    if !std::path::Path::new(APPLY_RELEASE_WRAPPER).is_file() {
        tracing::warn!(mode, "ssh-ca: applier not installed on this box");
        return Err(SshCaError::ApplierUnavailable);
    }
    let out = std::process::Command::new("sudo")
        .arg("-n")
        .arg(APPLY_RELEASE_WRAPPER)
        .arg(mode)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            tracing::info!(mode, "ssh-ca: applier ok");
            Ok(())
        }
        Ok(o) => {
            // stderr is the applier's own `ERROR:`/`WARN:` lines — safe to log,
            // it never echoes key material.
            let err = String::from_utf8_lossy(&o.stderr);
            tracing::warn!(mode, code = ?o.status.code(), stderr = %err.trim(), "ssh-ca: applier failed");
            Err(SshCaError::ApplierFailed)
        }
        Err(e) => {
            tracing::warn!(mode, error = %e, "ssh-ca: could not spawn applier");
            Err(SshCaError::ApplierUnavailable)
        }
    }
}

/// Adopt the cloud's SSH CA. Idempotent — called on every successful
/// enrollment refresh, so a re-issued CA rolls forward without operator action.
///
/// # Errors
/// Returns [`SshCaError`] if the key is malformed, staging fails, or the
/// privileged applier is unavailable / rejects the config.
#[cfg(target_os = "linux")]
pub async fn install_ca(pubkey: &str) -> Result<(), SshCaError> {
    let line = validate_ca_key(pubkey)?;
    let body = format!("{line}\n");
    tokio::task::spawn_blocking(move || {
        stage("ca.pub", &body)?;
        run_applier("ssh-ca-install")
    })
    .await
    .map_err(|_| SshCaError::Stage)?
}

/// Replace the live-session principal allowlist. Called on every session open
/// and close; an empty set revokes all certificate logins immediately.
///
/// # Errors
/// Returns [`SshCaError`] if staging fails or the applier rejects the file.
#[cfg(target_os = "linux")]
pub async fn sync_principals(sessions: BTreeSet<String>) -> Result<(), SshCaError> {
    let body = render_principals(&sessions);
    let count = sessions.len();
    tokio::task::spawn_blocking(move || {
        stage("principals", &body)?;
        run_applier("ssh-principals-sync")
    })
    .await
    .map_err(|_| SshCaError::Stage)?
    .inspect(|()| tracing::info!(count, "ssh-ca: principal allowlist synced"))
}

/// Operator recovery for a wedged sshd, reached from the cloud console. The
/// applier refuses to restart when `sshd -t` fails, so this can never lock the
/// operator out.
///
/// # Errors
/// Returns [`SshCaError`] if the applier is unavailable or the restart fails.
#[cfg(target_os = "linux")]
pub async fn restart_sshd() -> Result<(), SshCaError> {
    tokio::task::spawn_blocking(|| run_applier("sshd-restart"))
        .await
        .map_err(|_| SshCaError::ApplierUnavailable)?
}

/// Non-Linux stub — the dev workstation has no privileged applier.
#[cfg(not(target_os = "linux"))]
pub async fn install_ca(pubkey: &str) -> Result<(), SshCaError> {
    validate_ca_key(pubkey)?;
    Err(SshCaError::UnsupportedPlatform)
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub async fn sync_principals(sessions: BTreeSet<String>) -> Result<(), SshCaError> {
    let _ = render_principals(&sessions);
    Err(SshCaError::UnsupportedPlatform)
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub async fn restart_sshd() -> Result<(), SshCaError> {
    Err(SshCaError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_ed25519_ca_key_is_accepted() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGf3xQ9k1s2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345 nexus-ca";
        assert!(validate_ca_key(key).is_ok());
    }

    #[test]
    fn a_key_without_a_comment_is_accepted() {
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGf3xQ9k1s2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345";
        assert!(validate_ca_key(key).is_ok());
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let key = "\n  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGf3xQ9k1s2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345  \n";
        assert_eq!(
            validate_ca_key(key).unwrap(),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGf3xQ9k1s2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345"
        );
    }

    #[test]
    fn a_multi_key_blob_is_rejected() {
        // Smuggling a second CA past the validator would silently widen the
        // trust set, so more than one key line is a hard refusal.
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGf3xQ9k1s2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGevil2Ab4cDeFgHiJkLmNoPqRsTuVwXyZ012345";
        assert_eq!(validate_ca_key(key), Err(SshCaError::MalformedKey));
    }

    #[test]
    fn a_non_key_line_is_rejected() {
        assert_eq!(
            validate_ca_key("hello world"),
            Err(SshCaError::MalformedKey)
        );
        assert_eq!(validate_ca_key(""), Err(SshCaError::MalformedKey));
        assert_eq!(
            validate_ca_key("ssh-ed25519"),
            Err(SshCaError::MalformedKey)
        );
    }

    #[test]
    fn a_key_with_a_shell_metacharacter_payload_is_rejected() {
        // Defence in depth: the blob is never shell-interpolated, but a base64
        // field containing `;` or `$(` should still never reach the applier.
        let key = "ssh-ed25519 AAAA;rm_-rf_/AAAAAAAAAAAA";
        assert_eq!(validate_ca_key(key), Err(SshCaError::MalformedKey));
    }

    #[test]
    fn principals_render_one_uuid_per_line_sorted() {
        let mut set = BTreeSet::new();
        set.insert("bbbbbbbb-0000-0000-0000-000000000002".to_string());
        set.insert("aaaaaaaa-0000-0000-0000-000000000001".to_string());
        assert_eq!(
            render_principals(&set),
            "aaaaaaaa-0000-0000-0000-000000000001\nbbbbbbbb-0000-0000-0000-000000000002\n"
        );
    }

    #[test]
    fn an_empty_principal_set_renders_an_empty_file() {
        // This is the revoke-everything path — it must produce a truly empty
        // file, not a blank line that the applier would have to tolerate.
        assert_eq!(render_principals(&BTreeSet::new()), "");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(SshCaError::MalformedKey.code(), "malformed_ca_key");
        assert_eq!(SshCaError::ApplierFailed.code(), "applier_failed");
    }
}
