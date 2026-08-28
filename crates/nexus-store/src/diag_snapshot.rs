//! Phase 7.0a — secret-scrubbed SQLite snapshot for the diagnostics
//! bundle (operator `include_sqlite` opt-in).
//!
//! The cloud diagnostics flow can ask a core to include its live state
//! DB in the support tarball. The raw `nexus.db` MUST NEVER cross the
//! tunnel: it holds the core's mTLS client private key, the Ed25519
//! actor-token signing key, RTSP/ONVIF camera credentials, alert-sink
//! API keys, storage-backend OAuth tokens, local-user password hashes,
//! and refresh-token verifiers (edge Hard Rule 6 / cloud R5b). This
//! module produces a copy with all of that removed.
//!
//! ## Policy — allowlist, fail-closed
//!
//! 1. [`VACUUM INTO`] a throwaway file beside the live DB (a consistent
//!    snapshot, safe under WAL, never touching the original).
//! 2. DROP every table that is not in [`KEEP_TABLES`]. A migration that
//!    later adds a secret-bearing table is therefore excluded by
//!    default — the failure mode is "missing data", never "leaked
//!    secret".
//! 3. In the surviving tables, redact every secret column. The redaction
//!    set is the union of an explicit [`SECRET_COLUMNS`] list (belt) and
//!    a [`SECRET_MARKERS`] name-pattern net (suspenders) that catches
//!    secret columns added to a kept table by a future migration.
//! 4. For generic `(key, value)` settings tables ([`KV_TABLES`]) redact
//!    the value of any row whose key looks secret-ish.
//! 5. `VACUUM` to reclaim the freed pages, read the bytes, delete the
//!    throwaway file.
//!
//! A unit test asserts that a DB seeded with known secrets produces a
//! snapshot in which (a) the dropped tables are gone, (b) the secret
//! columns are redacted, and (c) no seeded secret string survives
//! anywhere in the raw bytes.
//!
//! [`VACUUM INTO`]: https://sqlite.org/lang_vacuum.html#vacuuminto

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection, SqlitePool};

use crate::StoreError;

/// Value written into every redacted cell. A non-NULL sentinel (not
/// `NULL`) so the UPDATE can't trip a `NOT NULL` constraint on columns
/// like `cameras.url` or `cloud_enrollment.private_key_pem`.
const REDACTED: &str = "[redacted]";

/// Tables permitted (verbatim, modulo column redaction) in a scrubbed
/// snapshot. Anything NOT listed here is DROPped from the copy, so a
/// future migration that adds a secret-bearing table is excluded by
/// default — fail-closed. Keep this list in sync with `migrations/`.
///
/// Deliberately ABSENT (and therefore dropped):
/// - `engine_state` — internal `(key, value)` boot/runtime scratch.
/// - `auth_refresh_tokens` — `token_hash` is a credential verifier.
/// - `alert_sink_outbox` — rendered alert payloads may embed secrets.
const KEEP_TABLES: &[&str] = &[
    "alert_sinks", // config_json redacted
    "audit_log",   // already shipped as audit-log.json
    "camera_visual_prompts",
    "cameras",          // url + config_json redacted
    "cloud_enrollment", // cert/key/jwt redacted
    "delivery_settings",
    "engine_runtime_settings", // kv value redacted on secret-ish keys
    "entity_local_state",
    "events",
    "fleet_managed_markers",
    "motion_clips",
    "motion_events",
    "rules",
    "schema_migrations",
    "storage_backends", // config_json redacted
    "storage_cold_replica",
    "users", // password_hash redacted
    "visual_prompts",
];

/// Explicit `(table, column)` cells to redact in [`KEEP_TABLES`]. The
/// belt to [`SECRET_MARKERS`]' suspenders — these names (`url`,
/// `config_json`) don't match the pattern net but absolutely carry
/// secrets.
const SECRET_COLUMNS: &[(&str, &str)] = &[
    ("alert_sinks", "config_json"),
    // Audit payloads are wholesale serialisations of the mutated domain
    // object, so a camera row carries `url` / `analysis_url` with their
    // `user:pass@` intact. New rows are scrubbed at write time by
    // `api::camera_audit_json`, but rows already at rest in a deployed
    // appliance's DB are not — and this snapshot is what carries them off
    // the box. Neither column name matches SECRET_MARKERS.
    ("audit_log", "before_json"),
    ("audit_log", "after_json"),
    ("cameras", "url"),
    ("cameras", "config_json"),
    ("cloud_enrollment", "cert_pem"),
    ("cloud_enrollment", "private_key_pem"),
    ("cloud_enrollment", "ca_chain_pem"),
    ("cloud_enrollment", "entitlement_jwt"),
    ("cloud_enrollment", "signing_key_pem"),
    ("storage_backends", "config_json"),
    ("users", "password_hash"),
];

/// Generic `(table, key_col, value_col)` settings tables. The value of
/// any row whose key matches [`is_secret_name`] is redacted, since the
/// secret hides behind a generic `value` column the pattern net can't
/// see.
const KV_TABLES: &[(&str, &str, &str)] = &[("engine_runtime_settings", "key", "value")];

/// Lowercased substrings that mark a column name (or a kv key) as
/// carrying a secret. Note the deliberate absence of a bare `key`: it
/// would match the primary-key column of every `(key, value)` settings
/// table. `_key` / `key_` still catch `private_key_pem`,
/// `signing_key_pem`, etc.
const SECRET_MARKERS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "passphrase",
    "token",
    "_pem",
    "pem_",
    "_key",
    "key_",
    "apikey",
    "api_key",
    "hmac",
    "jwt",
    "credential",
    "private",
];

/// True if `name` (case-insensitive) contains any [`SECRET_MARKERS`]
/// substring.
fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|m| lower.contains(m))
}

/// Double-quote a SQL identifier, escaping embedded quotes. Identifiers
/// here come from `sqlite_master` / `pragma_table_info` (real table and
/// column names), never user input, but quoting keeps reserved words
/// like `key` / `value` valid in the generated DDL.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Best-effort owner-only permissions on the throwaway snapshot while it
/// still holds pre-scrub secrets.
fn restrict_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Deletes the throwaway snapshot (and any SQLite sidecar files) on
/// drop, on every exit path including early `?` returns.
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        for ext in ["-wal", "-shm", "-journal"] {
            let mut sidecar = self.0.clone().into_os_string();
            sidecar.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
    }
}

/// Build a secret-scrubbed copy of the live SQLite DB behind `pool` and
/// return its bytes. See the module docs for the policy.
pub(crate) async fn export_scrubbed_snapshot(pool: &SqlitePool) -> Result<Vec<u8>, StoreError> {
    // Locate the live DB file so the throwaway copy lands on the same
    // filesystem (fast VACUUM INTO, same perms regime). Empty for an
    // in-memory DB — fall back to the system temp dir.
    let main_path: Option<String> =
        sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .fetch_one(pool)
            .await?;
    let main_path = main_path.unwrap_or_default();

    let dir: PathBuf = Path::new(&main_path)
        .parent()
        .filter(|_| !main_path.is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".nexus-diag-snapshot-{}-{}.sqlite",
        std::process::id(),
        nanos
    ));
    let _guard = TmpGuard(tmp.clone());

    // 1. Consistent snapshot of the live DB. The path is engine-generated
    //    (pid + nanos in a known dir); we single-quote-escape it anyway
    //    out of habit. VACUUM INTO requires the target not already exist.
    let vacuum_into = format!(
        "VACUUM INTO '{}'",
        tmp.to_string_lossy().replace('\'', "''")
    );
    sqlx::query(&vacuum_into).execute(pool).await?;
    restrict_perms(&tmp);

    // 2. Scrub the copy on its own connection. FK off so DROP TABLE does
    //    not fire ON DELETE CASCADE into kept child tables; MEMORY
    //    journal so no -wal/-shm sidecars linger.
    let mut conn = SqliteConnectOptions::new()
        .filename(&tmp)
        .create_if_missing(false)
        .foreign_keys(false)
        .journal_mode(SqliteJournalMode::Memory)
        .connect()
        .await?;

    scrub(&mut conn).await?;

    // 3. Reclaim the freed pages so the on-wire snapshot is tight, then
    //    flush + close before reading the file.
    sqlx::query("VACUUM").execute(&mut conn).await?;
    conn.close().await?;

    let bytes = std::fs::read(&tmp)?;
    Ok(bytes)
}

/// Drop non-allowlisted tables and redact secret columns in the rest,
/// in place, on an already-open connection to the throwaway copy.
async fn scrub(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let tables: Vec<String> = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>("name"))
    .collect();

    for table in &tables {
        if !KEEP_TABLES.contains(&table.as_str()) {
            sqlx::query(&format!("DROP TABLE IF EXISTS {}", quote_ident(table)))
                .execute(&mut *conn)
                .await?;
            continue;
        }
        redact_columns(conn, table).await?;
        redact_kv(conn, table).await?;
    }
    Ok(())
}

/// Redact every secret column of `table`: the explicit
/// [`SECRET_COLUMNS`] entries plus any non-PK column whose name matches
/// [`is_secret_name`]. PK columns are skipped so we never null an
/// identity/unique key.
async fn redact_columns(conn: &mut SqliteConnection, table: &str) -> Result<(), StoreError> {
    let infos = sqlx::query(&format!("PRAGMA table_info({})", quote_ident(table)))
        .fetch_all(&mut *conn)
        .await?;

    for info in &infos {
        let col: String = info.get("name");
        let pk: i64 = info.get("pk");
        let explicit = SECRET_COLUMNS.iter().any(|(t, c)| *t == table && *c == col);
        let patterned = pk == 0 && is_secret_name(&col);
        if explicit || patterned {
            sqlx::query(&format!(
                "UPDATE {} SET {} = ?",
                quote_ident(table),
                quote_ident(&col)
            ))
            .bind(REDACTED)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// For a generic `(key, value)` settings table, redact the value of any
/// row whose key looks secret-ish (today a no-op — no such keys exist —
/// but fail-closed against a future secret setting).
async fn redact_kv(conn: &mut SqliteConnection, table: &str) -> Result<(), StoreError> {
    let Some((_, key_col, val_col)) = KV_TABLES.iter().find(|(t, _, _)| *t == table) else {
        return Ok(());
    };

    let keys: Vec<String> = sqlx::query(&format!(
        "SELECT {} AS k FROM {}",
        quote_ident(key_col),
        quote_ident(table)
    ))
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .filter_map(|row| row.get::<Option<String>, _>("k"))
    .collect();

    for key in keys.iter().filter(|k| is_secret_name(k)) {
        sqlx::query(&format!(
            "UPDATE {} SET {} = ? WHERE {} = ?",
            quote_ident(table),
            quote_ident(val_col),
            quote_ident(key_col)
        ))
        .bind(REDACTED)
        .bind(key)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Known secret strings seeded into the DB. None of them may appear
    /// anywhere in the scrubbed snapshot bytes.
    const SECRETS: &[&str] = &[
        "rtsp://admin:hunter2@cam.local/stream",
        "BEGIN-PRIVATE-KEY-DO-NOT-LEAK",
        "sink-api-key-9f3c",
        "gdrive-oauth-refresh-token-xyz",
        "argon2id$v=19$super$hash",
        "refresh-token-hash-abcdef",
        "engine-state-secret-blob",
        "runtime-secret-value-zzz",
    ];

    async fn seed(pool: &SqlitePool) {
        let ddl = [
            "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY)",
            "CREATE TABLE cameras (id INTEGER PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL, config_json TEXT NOT NULL)",
            "CREATE TABLE cloud_enrollment (id INTEGER PRIMARY KEY CHECK (id = 1), core_id TEXT NOT NULL, private_key_pem TEXT NOT NULL, signing_kid TEXT)",
            "CREATE TABLE alert_sinks (sink_id TEXT PRIMARY KEY, kind TEXT NOT NULL, config_json TEXT NOT NULL)",
            "CREATE TABLE storage_backends (handle TEXT PRIMARY KEY, kind TEXT NOT NULL, config_json TEXT NOT NULL)",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL, password_hash TEXT)",
            "CREATE TABLE engine_runtime_settings (key TEXT PRIMARY KEY, value TEXT)",
            "CREATE TABLE rules (id TEXT PRIMARY KEY, config_json TEXT NOT NULL)",
            // Tables that must be dropped wholesale.
            "CREATE TABLE engine_state (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE auth_refresh_tokens (id INTEGER PRIMARY KEY, token_hash TEXT NOT NULL)",
        ];
        for stmt in ddl {
            sqlx::query(stmt).execute(pool).await.unwrap();
        }
        sqlx::query("INSERT INTO schema_migrations (id) VALUES ('0001_initial')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO cameras (id, name, url, config_json) VALUES (1, 'Front Gate', ?, ?)",
        )
        .bind(SECRETS[0])
        .bind(format!("{{\"onvif_pass\":\"{}\"}}", SECRETS[0]))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO cloud_enrollment (id, core_id, private_key_pem, signing_kid) VALUES (1, 'core-123', ?, 'kid-7')")
            .bind(SECRETS[1])
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO alert_sinks (sink_id, kind, config_json) VALUES ('sureview:gate', 'sureview', ?)")
            .bind(format!("{{\"api_key\":\"{}\"}}", SECRETS[2]))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO storage_backends (handle, kind, config_json) VALUES ('gd', 'gdrive', ?)",
        )
        .bind(format!("{{\"refresh_token\":\"{}\"}}", SECRETS[3]))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (1, 'admin', ?)")
            .bind(SECRETS[4])
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO engine_runtime_settings (key, value) VALUES ('server.bind', '0.0.0.0:8089'), ('cloud.api_secret', ?)")
            .bind(SECRETS[7])
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO engine_state (key, value) VALUES ('blob', ?)")
            .bind(SECRETS[6])
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO auth_refresh_tokens (id, token_hash) VALUES (1, ?)")
            .bind(SECRETS[5])
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO rules (id, config_json) VALUES ('r1', '{\"label\":\"person\"}')")
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_drops_secret_tables_and_redacts_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nexus.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .unwrap();
        seed(&pool).await;

        let bytes = export_scrubbed_snapshot(&pool).await.unwrap();
        assert!(!bytes.is_empty(), "snapshot is empty");

        // Strongest assertion: no seeded secret survives anywhere in the
        // raw snapshot bytes.
        for secret in SECRETS {
            let needle = secret.as_bytes();
            let leaked = bytes.windows(needle.len()).any(|window| window == needle);
            assert!(!leaked, "secret leaked into snapshot: {secret}");
        }

        // Open the snapshot and verify structure.
        let out = dir.path().join("snapshot.sqlite");
        std::fs::write(&out, &bytes).unwrap();
        let snap = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&out))
            .await
            .unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(&snap)
        .await
        .unwrap();

        // Dropped wholesale.
        assert!(!tables.contains(&"engine_state".to_string()));
        assert!(!tables.contains(&"auth_refresh_tokens".to_string()));
        // Kept.
        assert!(tables.contains(&"cameras".to_string()));
        assert!(tables.contains(&"schema_migrations".to_string()));

        // Secret columns redacted; useful columns preserved.
        let (name, url): (String, String) =
            sqlx::query_as("SELECT name, url FROM cameras WHERE id = 1")
                .fetch_one(&snap)
                .await
                .unwrap();
        assert_eq!(name, "Front Gate", "non-secret column should survive");
        assert_eq!(url, REDACTED, "camera url must be redacted");

        let pk_pem: String =
            sqlx::query_scalar("SELECT private_key_pem FROM cloud_enrollment WHERE id = 1")
                .fetch_one(&snap)
                .await
                .unwrap();
        assert_eq!(pk_pem, REDACTED, "mTLS private key must be redacted");

        let kid: String =
            sqlx::query_scalar("SELECT signing_kid FROM cloud_enrollment WHERE id = 1")
                .fetch_one(&snap)
                .await
                .unwrap();
        assert_eq!(kid, "kid-7", "non-secret signing_kid should survive");

        let pw: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = 1")
            .fetch_one(&snap)
            .await
            .unwrap();
        assert_eq!(
            pw.as_deref(),
            Some(REDACTED),
            "password hash must be redacted"
        );

        // kv table: secret-ish key redacted, normal key preserved.
        let bind: Option<String> = sqlx::query_scalar(
            "SELECT value FROM engine_runtime_settings WHERE key = 'server.bind'",
        )
        .fetch_one(&snap)
        .await
        .unwrap();
        assert_eq!(bind.as_deref(), Some("0.0.0.0:8089"));
        let api_secret: Option<String> = sqlx::query_scalar(
            "SELECT value FROM engine_runtime_settings WHERE key = 'cloud.api_secret'",
        )
        .fetch_one(&snap)
        .await
        .unwrap();
        assert_eq!(api_secret.as_deref(), Some(REDACTED));
    }
}
