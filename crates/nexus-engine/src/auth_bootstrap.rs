//! Boot-time auth posture enforcement.
//!
//! M-Admin Phase 0 closeout retired the legacy `AuthMode::None`
//! and `AuthMode::DevToken` variants — there are no more
//! shared-secret or no-auth code paths to wire up here. This
//! module now does exactly one thing:
//!
//! **Auto-provisioned admin secret for `Local` / `Hybrid`.** When
//! `auth.mode in {"local", "hybrid"}` and `auth.admin_secret_path`
//! is unset, the engine writes a fresh 32-byte URL-safe random
//! secret to `<state_dir>/admin-secret` (mode 0600) and patches
//! `cfg.auth.admin_secret_path` so the session-JWT signer can
//! find it. Operators who already manage a secret (k8s Secret,
//! Docker secret, systemd LoadCredential) keep their existing
//! `admin_secret_path = ...` pin and this branch is a no-op.
//!
//! `Oidc` mode is a no-op — pure-OIDC deployments don't need a
//! local admin secret (the IdP signs everything). The OIDC
//! verifier is built later from `cfg.auth.oidc`; if that block
//! is missing, the verifier itself surfaces a clear error.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::{general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nexus_config::{AuthMode, Config};

/// File on disk that holds the auto-generated admin secret used to
/// sign session JWTs in `auth.mode in {local, hybrid}`. Lives
/// alongside the engine state directory so a wipe of
/// `/var/lib/nexus` rotates the secret along with the rest of the
/// box's identity.
const ADMIN_SECRET_FILE: &str = "admin-secret";

/// Length, in raw bytes, of the auto-generated admin secret. 32 bytes
/// of CSPRNG output — the HS256 session signer treats this as opaque
/// key material (any non-empty UTF-8 string works; URL-safe-no-pad
/// base64 keeps it inspectable + copy-pasteable).
const ADMIN_SECRET_BYTES: usize = 32;

/// Apply Phase-0 auth-posture rules. Mutates
/// `cfg.auth.admin_secret_path` in place when the secret is
/// auto-generated, so the rest of the engine sees the resolved
/// value.
///
/// `state_dir` is `cfg.runtime.state_dir` resolved by the caller —
/// passed in (instead of re-derived here) so the test path can
/// point at a tempdir.
#[expect(
    clippy::print_stderr,
    reason = "runs during boot before telemetry is initialised (main.rs calls it ahead of nexus_telemetry::init)"
)]
pub fn apply(cfg: &mut Config, state_dir: &Path) -> Result<()> {
    match cfg.auth.mode {
        AuthMode::Local | AuthMode::Hybrid => {
            if cfg.auth.admin_secret_path.is_none() {
                let path = ensure_admin_secret(state_dir)?;
                cfg.auth.admin_secret_path = Some(path);
            } else {
                eprintln!(
                    "nexus-engine: auth: admin_secret_path sourced from nexus.toml \
                     (auto-provisioning skipped)"
                );
            }
        }
        AuthMode::Oidc => {
            // Pure-OIDC deployments don't need a local admin
            // secret — the IdP signs everything. The OIDC verifier
            // is built later from cfg.auth.oidc; if that block is
            // missing, the verifier itself surfaces a clear error.
        }
    }
    Ok(())
}

/// Read `<state_dir>/admin-secret` if present; otherwise generate a
/// fresh 32-byte URL-safe random secret, write it with mode 0600,
/// and return the path. We DON'T log the secret value — it's the
/// HS256 signing key for session JWTs, not an operator-visible
/// bearer token.
#[expect(
    clippy::print_stderr,
    reason = "runs during boot before telemetry is initialised (main.rs calls it ahead of nexus_telemetry::init)"
)]
fn ensure_admin_secret(state_dir: &Path) -> Result<PathBuf> {
    let path = state_dir.join(ADMIN_SECRET_FILE);

    if path.exists() {
        // Validate that the file is non-empty so an empty leftover
        // file from a botched provision doesn't silently boot the
        // engine into a state where every login returns 503.
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading admin secret from {}", path.display()))?;
        if raw.trim().is_empty() {
            return Err(anyhow!(
                "admin secret file {} exists but is empty; delete it to regenerate",
                path.display()
            ));
        }
        eprintln!(
            "nexus-engine: auth: admin_secret loaded from disk path={}",
            path.display()
        );
        return Ok(path);
    }

    fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    let secret = generate_token(ADMIN_SECRET_BYTES);
    write_secret(&path, secret.as_bytes())
        .with_context(|| format!("writing admin secret to {}", path.display()))?;

    eprintln!(
        "nexus-engine: auth: generated new admin secret for session JWT signing. \
         path={} (file mode 0600; delete to rotate — invalidates all active sessions)",
        path.display()
    );
    Ok(path)
}

/// `n_bytes` from the OS RNG, encoded as URL-safe-no-pad base64.
/// `getrandom` is the same crate `rand` ultimately calls; using it
/// directly keeps nexus-engine's dep graph one fewer crate wide.
fn generate_token(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    // OsRng.fill_bytes never fails on the platforms the engine
    // ships to (Linux, macOS); a fallback path here would be dead
    // code. Panicking is acceptable: a system without a working
    // CSPRNG cannot host a security-bearing engine in any case.
    #[expect(
        clippy::expect_used,
        reason = "the OS RNG does not fail on the platforms the engine ships to; a host without one cannot hold a signing key"
    )]
    getrandom::fill(&mut buf).expect("OS RNG must succeed for token generation");
    URL_SAFE_NO_PAD.encode(&buf)
}

/// Write `bytes` to `path` with mode 0600. On non-unix systems
/// (the engine doesn't ship there, but tests can run on macOS)
/// the mode bits are skipped — `OpenOptions::mode` is unix-only.
fn write_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Resolve the engine state directory the same way the rest of
/// `main.rs` does. Centralised here so [`apply`] can be unit-tested
/// against a tempdir without re-implementing the lookup.
pub fn state_dir(cfg: &Config) -> PathBuf {
    cfg.runtime.state_dir.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::AuthConfig;

    #[test]
    fn ensure_admin_secret_generates_then_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_admin_secret(dir.path()).unwrap();
        assert_eq!(first, dir.path().join(ADMIN_SECRET_FILE));
        assert!(first.exists(), "admin secret file must be created");

        // Second call must return the same path and NOT rotate.
        let on_disk_before = std::fs::read_to_string(&first).unwrap();
        let second = ensure_admin_secret(dir.path()).unwrap();
        assert_eq!(first, second);
        let on_disk_after = std::fs::read_to_string(&second).unwrap();
        assert_eq!(
            on_disk_before, on_disk_after,
            "admin secret must NOT rotate on subsequent boots"
        );

        // 32 bytes -> 43 URL-safe-no-pad chars.
        assert_eq!(on_disk_after.trim().len(), 43);

        // Mode-0600 check on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "admin-secret must be 0600, got 0o{mode:o}");
        }
    }

    #[test]
    fn ensure_admin_secret_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ADMIN_SECRET_FILE);
        std::fs::write(&path, "").unwrap();
        let err = ensure_admin_secret(dir.path()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn apply_local_branch_auto_provisions_admin_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::Local,
                admin_secret_path: None,
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path()).unwrap();
        let p = cfg
            .auth
            .admin_secret_path
            .as_ref()
            .expect("local mode must auto-provision admin_secret_path");
        assert_eq!(p, &dir.path().join(ADMIN_SECRET_FILE));
        assert!(p.exists());
    }

    #[test]
    fn apply_hybrid_branch_auto_provisions_admin_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::Hybrid,
                admin_secret_path: None,
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path()).unwrap();
        assert!(cfg.auth.admin_secret_path.is_some());
    }

    #[test]
    fn apply_local_branch_preserves_operator_pinned_path() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = dir.path().join("operator-managed-secret");
        std::fs::write(&pinned, "operator-supplied-value").unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::Local,
                admin_secret_path: Some(pinned.clone()),
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path()).unwrap();
        assert_eq!(cfg.auth.admin_secret_path.as_ref(), Some(&pinned));
        // The auto-provision file MUST NOT have been created.
        assert!(!dir.path().join(ADMIN_SECRET_FILE).exists());
    }

    #[test]
    fn apply_oidc_branch_does_not_provision_admin_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::Oidc,
                admin_secret_path: None,
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path()).unwrap();
        assert!(
            cfg.auth.admin_secret_path.is_none(),
            "pure-OIDC mode must NOT auto-provision an admin secret"
        );
        assert!(!dir.path().join(ADMIN_SECRET_FILE).exists());
    }
}
