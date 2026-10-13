//! `nexus-engine set-admin-password` — operator-facing one-shot
//! that sets (or creates) the local `admin` user's password from
//! a stdin-supplied plaintext. Replaces the historic "engine
//! generates an OTP, installer greps a sentinel file, operator
//! changes it on first login" dance with a flow where the
//! operator picks the password during install.
//!
//! Behaviour:
//! * The plaintext is read from stdin (one line, trailing
//!   newline trimmed) so it never appears in `ps aux` or shell
//!   history. `--password-file <PATH>` is offered as an
//!   alternative for unattended installs (Ansible / packer)
//!   that pre-drop a 0600 file.
//! * Runs the existing argon2id + password-policy pipeline
//!   (`MIN_PASSWORD_LEN = 12`); any policy failure surfaces a
//!   clear `Err` that install.sh re-prompts on.
//! * If the user exists → password is replaced,
//!   `force_password_reset = false` (the operator just chose
//!   this password; there is no reason to force them to change
//!   it again on next login). Any active lockout is cleared so
//!   a stale brute-force lockout can't block recovery.
//! * If the user does NOT exist → created as `Role::Admin` with
//!   `force_password_reset = false`.
//! * Either way, the bootstrap-password sentinel
//!   (`<state_dir>/bootstrap-password.txt`) is best-effort
//!   removed so a stale OTP doesn't linger on disk.
//!
//! The subcommand opens its OWN connection to the SQLite store
//! and does NOT require `nexus-engine` to be running; install.sh
//! invokes it BEFORE `systemctl start nexus-engine` on first
//! install. The engine's first boot then observes
//! `count_users() > 0` and skips the OTP-generation path
//! entirely.

use std::io::{self, BufRead};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use nexus_config::Config;
use nexus_store::{NewUser, Store};
use nexus_types::Role;

use crate::auth::bootstrap::{clear_bootstrap_sentinel, BOOTSTRAP_USERNAME};
use crate::auth::passwords::hash_password;

/// CLI args for `nexus-engine set-admin-password`.
#[derive(Debug, Clone, Args)]
pub struct SetAdminPasswordArgs {
    /// Local username whose password to set. Defaults to the
    /// bootstrap admin (`admin`). Useful for the rare case of
    /// resetting a non-admin user's password from the CLI.
    #[arg(long, default_value = BOOTSTRAP_USERNAME)]
    pub username: String,

    /// Read the new password from a file on disk instead of
    /// stdin. The file must be exactly one line; trailing
    /// `\r` / `\n` are trimmed. Mode `0600` strongly recommended.
    ///
    /// When omitted, the password is read from stdin (one line).
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
}

/// Run the subcommand against the store described by `cfg`.
#[expect(
    clippy::print_stderr,
    reason = "`set-admin-password` is a one-shot operator subcommand that runs before telemetry is initialised; stderr is its output"
)]
pub async fn run(cfg: &Config, args: &SetAdminPasswordArgs) -> Result<()> {
    if !cfg.auth.mode.allows_local() {
        return Err(anyhow!(
            "auth.mode = {:?} does not allow local users; set-admin-password requires `local` or `hybrid`",
            cfg.auth.mode
        ));
    }

    let username = args.username.trim();
    if username.is_empty() {
        return Err(anyhow!("--username must not be empty"));
    }

    let password = read_password(args.password_file.as_deref())?;
    if password.is_empty() {
        return Err(anyhow!(
            "empty password supplied; refusing to write (pipe a non-empty line on stdin)"
        ));
    }

    // `hash_password` runs `check_password_policy` first so a
    // <12-char or denylisted password surfaces as a clear `Err`
    // BEFORE we touch the database. install.sh catches the
    // non-zero exit and re-prompts.
    let hash = hash_password(&password).map_err(|e| anyhow!("password rejected by policy: {e}"))?;

    let store = Store::open(&cfg.store)
        .await
        .with_context(|| format!("opening store at {}", cfg.store.url))?;

    match store.get_user_by_username(username).await? {
        Some(existing) => {
            store
                .update_user_password(existing.id, &hash, false)
                .await
                .with_context(|| format!("updating password for user '{username}'"))?;
            // Defensive: a prior brute-force lockout would
            // otherwise block the operator from logging in
            // with the password they just chose.
            let _ = store.clear_lockout(existing.id).await;
            eprintln!("nexus-engine: password updated for user '{username}'");
        }
        None => {
            store
                .create_user(&NewUser {
                    username,
                    role: Role::Admin,
                    password_hash: Some(&hash),
                    oidc_subject_hash: None,
                    force_password_reset: false,
                })
                .await
                .with_context(|| format!("creating user '{username}'"))?;
            eprintln!("nexus-engine: created admin user '{username}' with the supplied password");
        }
    }

    // Whether we created or updated, any pre-existing OTP
    // sentinel is now obsolete. Best-effort: logs but never
    // fails the caller.
    clear_bootstrap_sentinel(&cfg.runtime.state_dir);
    Ok(())
}

fn read_password(from_file: Option<&std::path::Path>) -> Result<String> {
    let raw = match from_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading password from {}", path.display()))?,
        None => {
            let mut buf = String::new();
            io::stdin()
                .lock()
                .read_line(&mut buf)
                .context("reading password from stdin")?;
            buf
        }
    };
    // Trim trailing CR/LF only; preserve any other whitespace
    // operators may legitimately use inside the password.
    Ok(raw.trim_end_matches(['\n', '\r']).to_string())
}
