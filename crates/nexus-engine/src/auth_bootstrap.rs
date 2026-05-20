//! Boot-time auth posture enforcement.
//!
//! Responsibilities (added in M-Install Checkpoint 2):
//!
//! 1. **Secure-by-default token provisioning.** When `auth.mode =
//!    "dev_token"` and no `dev_token` is configured in TOML, the
//!    engine reads the persisted token from `<state_dir>/dev-token`.
//!    If that file is missing it generates a fresh 32-byte URL-safe
//!    random token, persists it with mode 0600, and prints it to
//!    the WARN log so operators can copy it into their browser
//!    exactly once.
//!
//! 2. **Auto-provisioned admin secret for `Local` / `Hybrid`.** When
//!    `auth.mode in {"local", "hybrid"}` and `auth.admin_secret_path`
//!    is unset, the engine writes a fresh 32-byte URL-safe random
//!    secret to `<state_dir>/admin-secret` (mode 0600) and patches
//!    `cfg.auth.admin_secret_path` so the session-JWT signer can
//!    find it. This is the customer-facing default since M6 closure
//!    (see [docs/ARCHITECTURE.md §11][arch11]); operators who
//!    already manage a secret (k8s Secret, Docker secret, systemd
//!    LoadCredential) keep their existing `admin_secret_path = ...`
//!    pin and this branch is a no-op.
//!
//!    [arch11]: ../../../docs/ARCHITECTURE.md#11-identity--authentication
//!
//! 3. **Non-loopback `mode = none` rejection.** Operators can
//!    still opt into "no auth" with `auth.mode = "none"`, but
//!    only when the API binds to `127.0.0.1` (or `::1`). Any
//!    other bind value with `mode = none` aborts boot — the
//!    engine refuses to leak unauthenticated writes onto a LAN.
//!
//! 4. **Grandfather WARN.** When `nexus-config`'s
//!    `load_with_compat` reports that the on-disk `nexus.toml`
//!    had no `[auth]` section, this module logs a one-time
//!    deprecation warning that names the upgrade deadline
//!    (7 days from boot). The grandfather window itself is
//!    enforced by the config crate; this module just surfaces it
//!    to the operator.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::{general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{Duration, Utc};
use nexus_config::{AuthMode, CompatNotice, Config};

/// File on disk that holds the auto-generated dev token. Lives
/// alongside the engine state directory so a wipe of `/var/lib/nexus`
/// rotates the token along with the rest of the box's identity.
#[cfg(not(feature = "prod-auth"))]
const DEV_TOKEN_FILE: &str = "dev-token";

/// File on disk that holds the auto-generated admin secret used to
/// sign session JWTs in `auth.mode in {local, hybrid}`. Same
/// rotate-with-state-dir semantics as the dev token.
const ADMIN_SECRET_FILE: &str = "admin-secret";

/// Length, in raw bytes, of the generated dev token. URL-safe-no-pad
/// base64 expands this to 43 characters — long enough for a 256-bit
/// secret and short enough to copy/paste into a browser tab without
/// truncation.
#[cfg(not(feature = "prod-auth"))]
const DEV_TOKEN_BYTES: usize = 32;

/// Length, in raw bytes, of the auto-generated admin secret. 32 bytes
/// of CSPRNG output — the HS256 session signer treats this as opaque
/// key material (any non-empty UTF-8 string works; URL-safe-no-pad
/// base64 keeps it inspectable + copy-pasteable).
const ADMIN_SECRET_BYTES: usize = 32;

/// Apply Checkpoint-2 auth-posture rules. Mutates `cfg.auth.dev_token`
/// and `cfg.auth.admin_secret_path` in place when their respective
/// secrets are auto-generated, so the rest of the engine sees the
/// resolved values.
///
/// `state_dir` is `cfg.runtime.state_dir` resolved by the caller —
/// passed in (instead of re-derived here) so the test path can
/// point at a tempdir.
///
/// Returns `Err` for boot-fatal posture violations (currently only
/// the non-loopback `mode = none` case).
pub fn apply(cfg: &mut Config, state_dir: &Path, notice: CompatNotice) -> Result<()> {
    if notice.auth_grandfathered {
        let deadline = Utc::now() + Duration::days(7);
        eprintln!(
            "nexus-engine: WARN nexus.toml has no [auth] section; \
             pinning auth.mode = \"none\" for backward compatibility. \
             This grandfather will be removed on or after {}. Add an \
             explicit [auth] block to silence this warning — see \
             config/nexus.example.toml.",
            deadline.format("%Y-%m-%d")
        );
    }

    match cfg.auth.mode {
        AuthMode::None => enforce_loopback_only(&cfg.server.api_bind)?,
        #[cfg(not(feature = "prod-auth"))]
        AuthMode::DevToken => {
            if cfg.auth.dev_token.is_none() {
                let token = ensure_dev_token(state_dir)?;
                cfg.auth.dev_token = Some(token);
            } else {
                eprintln!(
                    "nexus-engine: auth: dev_token sourced from nexus.toml \
                     (auto-provisioning skipped)"
                );
            }
        }
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

/// True iff `bind` resolves (textually OR via std::net parsing) to
/// a loopback address. We accept three shapes:
///
/// * `"127.0.0.1:8089"` — the canonical LAN-only bind.
/// * `"[::1]:8089"`     — IPv6 loopback.
/// * `"localhost:8089"` — string-only check; we don't DNS-resolve
///   here because boot must not block on resolver state.
fn enforce_loopback_only(bind: &str) -> Result<()> {
    // Fast path: textual match on the exact strings INSTALL.md
    // recommends. Avoids parsing pitfalls on hosts where
    // SocketAddr's lexer is stricter than the operator expected.
    let lower = bind.to_ascii_lowercase();
    if lower.starts_with("127.0.0.1:")
        || lower.starts_with("[::1]:")
        || lower.starts_with("localhost:")
    {
        return Ok(());
    }

    // Slow path: try to parse and check the IP. Anything that
    // doesn't parse + isn't textually loopback is rejected.
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        if addr.ip().is_loopback() {
            return Ok(());
        }
    }

    Err(anyhow!(
        "auth.mode = \"none\" is only allowed when server.api_bind is on \
         loopback (127.0.0.1, [::1], or localhost). Got `{bind}`. Either \
         change the bind to loopback or set auth.mode = \"dev_token\" / \
         \"oidc\". See INSTALL.md §11 for details."
    ))
}

/// Read `<state_dir>/dev-token` if present; otherwise generate a
/// fresh 32-byte URL-safe random token, write it with mode 0600,
/// and log the value at WARN so operators can copy it once.
#[cfg(not(feature = "prod-auth"))]
fn ensure_dev_token(state_dir: &Path) -> Result<String> {
    let path = state_dir.join(DEV_TOKEN_FILE);

    if path.exists() {
        let s = fs::read_to_string(&path)
            .with_context(|| format!("reading dev token from {}", path.display()))?
            .trim()
            .to_string();
        if s.is_empty() {
            return Err(anyhow!(
                "dev token file {} exists but is empty; delete it to regenerate",
                path.display()
            ));
        }
        eprintln!(
            "nexus-engine: auth: dev_token loaded from disk path={}",
            path.display()
        );
        return Ok(s);
    }

    fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    let token = generate_token(DEV_TOKEN_BYTES);
    write_secret(&path, token.as_bytes())
        .with_context(|| format!("writing dev token to {}", path.display()))?;

    // The whole point of WARN here is operator visibility — INFO
    // gets filtered out on noisy boxes. We deliberately print the
    // token in plaintext: it lives in a 0600 file the operator
    // can read anyway, and surfacing it once at boot beats forcing
    // a `cat /var/lib/nexus/dev-token` round-trip on first use.
    eprintln!(
        "nexus-engine: WARN auth: generated new dev token. \
         Send `Authorization: Bearer <dev_token>` on every API call. \
         path={} dev_token={} \
         (file mode 0600; delete to rotate)",
        path.display(),
        token
    );
    Ok(token)
}

/// Read `<state_dir>/admin-secret` if present; otherwise generate a
/// fresh 32-byte URL-safe random secret, write it with mode 0600,
/// and return the path. Unlike the dev-token branch we DON'T log
/// the secret value — it's the HS256 signing key for session JWTs,
/// not an operator-visible bearer token.
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
    fn loopback_bind_is_accepted() {
        for ok in [
            "127.0.0.1:8089",
            "127.0.0.1:1",
            "[::1]:8089",
            "localhost:8089",
            "LOCALHOST:9000",
        ] {
            enforce_loopback_only(ok).unwrap_or_else(|e| panic!("`{ok}` rejected: {e}"));
        }
    }

    #[test]
    fn non_loopback_bind_is_rejected_for_mode_none() {
        for bad in ["0.0.0.0:8089", "192.168.1.10:8089", "[::]:8089"] {
            let err = enforce_loopback_only(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("loopback") && msg.contains(bad),
                "expected loopback-rejection for `{bad}`, got: {msg}"
            );
        }
    }

    #[test]
    #[cfg(not(feature = "prod-auth"))]
    fn ensure_dev_token_generates_then_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_dev_token(dir.path()).unwrap();
        assert!(!first.is_empty(), "generated token must be non-empty");
        // 32 bytes -> 43 chars URL-safe-no-pad.
        assert_eq!(first.len(), 43, "token len = {}", first.len());

        // Second call must return the same value (no rotation on
        // boot).
        let second = ensure_dev_token(dir.path()).unwrap();
        assert_eq!(first, second);

        // File must exist with the token bytes (no trailing
        // newline once trimmed).
        let on_disk = std::fs::read_to_string(dir.path().join(DEV_TOKEN_FILE)).unwrap();
        assert_eq!(on_disk.trim(), first);

        // Mode-0600 check on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(dir.path().join(DEV_TOKEN_FILE)).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "dev-token must be 0600, got 0o{mode:o}");
        }
    }

    #[test]
    #[cfg(not(feature = "prod-auth"))]
    fn apply_dev_token_branch_populates_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::DevToken,
                dev_token: None,
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
        assert!(cfg.auth.dev_token.as_deref().map(str::len) == Some(43));
    }

    #[test]
    fn apply_none_branch_blocks_non_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::None,
                ..AuthConfig::default()
            },
            server: nexus_config::ServerConfig {
                api_bind: "0.0.0.0:8089".into(),
                ..nexus_config::ServerConfig::default()
            },
            ..Config::default()
        };
        let err = apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap_err();
        assert!(err.to_string().contains("loopback"));
    }

    #[test]
    fn apply_none_branch_accepts_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            auth: AuthConfig {
                mode: AuthMode::None,
                ..AuthConfig::default()
            },
            server: nexus_config::ServerConfig {
                api_bind: "127.0.0.1:8089".into(),
                ..nexus_config::ServerConfig::default()
            },
            ..Config::default()
        };
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
        assert!(
            cfg.auth.dev_token.is_none(),
            "mode=none must NOT auto-provision"
        );
    }

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
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
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
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
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
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
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
        apply(&mut cfg, dir.path(), CompatNotice::default()).unwrap();
        assert!(
            cfg.auth.admin_secret_path.is_none(),
            "pure-OIDC mode must NOT auto-provision an admin secret"
        );
        assert!(!dir.path().join(ADMIN_SECRET_FILE).exists());
    }
}
