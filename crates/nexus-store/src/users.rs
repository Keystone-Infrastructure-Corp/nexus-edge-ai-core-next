//! M6 Phase 2 Step 2.1: local users CRUD.
//!
//! Wraps the `users` table (migration `0010_local_users.sql`)
//! with a strongly-typed API the engine handlers consume.
//! Two design choices the handlers depend on:
//!
//! 1. **Last-admin protection is in the store layer.** Every
//!    function that can reduce the active-admin count (role
//!    change, disable, soft-delete) checks
//!    `count_active_admins()` inside the same transaction as
//!    the mutation and returns [`UsersError::LastAdmin`]
//!    before doing damage. Handlers translate that into a 409.
//!    Doing it in the store guarantees that any caller
//!    (admin HTTP, future scripted maintenance, anything) gets
//!    the same safety net.
//!
//! 2. **Soft-delete renames the username.** A deleted user's
//!    row sticks around forever (so the engine can still hand
//!    out the original numeric id when an old audit row
//!    references it) but the canonical username slot is
//!    freed by renaming to `<id>:deleted-<ts>`. This matches
//!    the M6 spec ("audit_log foreign keys survive"). The
//!    rename is unconditional — there is no recovery path
//!    for a deleted user; admin recreates them.
//!
//! Lockout state lives here too because it's user-row data
//! (failed_login_count, locked_until) — the FSM that turns
//! "this attempt failed" into "user is locked for 15min" lives
//! in `nexus-engine::auth::lockout` (Phase 2 Step 2.3) and
//! calls into this module via [`Store::record_login_failure`]
//! / [`Store::clear_lockout`].

use chrono::{DateTime, Utc};
use nexus_types::Role;
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};

use crate::{Store, StoreError};

/// Stable numeric identifier — exposed to the UI as a string
/// for safety across JS-number precision, but stored as
/// SQLite INTEGER so existing `id INTEGER PRIMARY KEY`
/// conventions hold.
pub type UserId = i64;

/// One row of `users` projected for the handler / UI.
/// `password_hash` is NOT included — the login handler queries
/// it via [`Store::get_password_hash_for_login`] which doesn't
/// surface it outside the verification path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    /// `true` iff the password was set by an admin (reset flow
    /// or first-boot bootstrap). Cleared by `/change-password`.
    pub force_password_reset: bool,
    /// `true` iff an admin explicitly disabled the account.
    /// Login is rejected for disabled users with the same
    /// `invalid_credentials` shape as a wrong password.
    pub disabled: bool,
    pub failed_login_count: i64,
    pub locked_until: Option<DateTime<Utc>>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_failed_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `Some` iff the user has been soft-deleted. UI hides
    /// these by default; admin views surface them under a
    /// "show deleted" toggle.
    pub deleted_at: Option<DateTime<Utc>>,
    /// `true` iff the user has an OIDC subject mapped (i.e. can
    /// log in via OIDC). Local-only users have this false.
    pub has_oidc: bool,
    /// `true` iff the user has a password hash on file (i.e.
    /// can log in via the local password flow). OIDC-only
    /// users have this false.
    pub has_password: bool,
}

/// What the caller hands to [`Store::create_user`]. At least one
/// of `password_hash` / `oidc_subject_hash` must be `Some` — the
/// CHECK constraint enforces this and the function returns
/// [`UsersError::NoAuthMethod`] before reaching the DB.
#[derive(Debug, Clone, Default)]
pub struct NewUser<'a> {
    pub username: &'a str,
    pub role: Role,
    /// Pre-computed argon2id PHC string (see
    /// `nexus-engine::auth::passwords`). The store never
    /// hashes — handlers do.
    pub password_hash: Option<&'a str>,
    /// Pre-computed SHA-256 hex of the OIDC `sub`. Handlers
    /// hash before calling so plaintext subs never live in
    /// SQL params or query logs.
    pub oidc_subject_hash: Option<&'a str>,
    /// Force a password change on first login. Set to true for
    /// admin-created users + first-boot bootstrap.
    pub force_password_reset: bool,
}

/// Errors surfaced from `users::*` operations beyond raw
/// SQL errors. These map 1:1 to handler-friendly outcomes.
#[derive(Debug, thiserror::Error)]
pub enum UsersError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("username already exists")]
    UsernameTaken,
    #[error("oidc subject already linked")]
    OidcSubjectTaken,
    #[error("user not found")]
    NotFound,
    #[error("user has no authentication method (need password_hash or oidc_subject)")]
    NoAuthMethod,
    #[error("would leave the system without any active admin")]
    LastAdmin,
}

impl Store {
    /// Insert a new local or OIDC user. Returns the newly-allocated `id`.
    ///
    /// Caller is responsible for: normalising the username
    /// (typically lowercase + trim) and pre-hashing the password
    /// (argon2id) and OIDC subject (SHA-256 hex). The store
    /// only enforces the at-least-one-auth-method invariant and
    /// the UNIQUE constraints.
    pub async fn create_user(&self, new: &NewUser<'_>) -> Result<UserId, UsersError> {
        if new.password_hash.is_none() && new.oidc_subject_hash.is_none() {
            return Err(UsersError::NoAuthMethod);
        }
        let force_pw_reset = if new.force_password_reset { 1i64 } else { 0i64 };
        let res = sqlx::query_scalar::<_, i64>(
            "INSERT INTO users (
                username, password_hash, oidc_subject, role, force_password_reset
            ) VALUES (?, ?, ?, ?, ?)
            RETURNING id",
        )
        .bind(new.username)
        .bind(new.password_hash)
        .bind(new.oidc_subject_hash)
        .bind(new.role.as_str())
        .bind(force_pw_reset)
        .fetch_one(&self.pool)
        .await;
        match res {
            Ok(id) => Ok(id),
            Err(sqlx::Error::Database(db)) if is_unique_violation(&*db) => {
                // Distinguish "which UNIQUE was it" by re-checking
                // — SQLite error messages aren't structurally stable
                // enough to parse, so the second hop costs us one
                // extra round-trip on the unhappy path only.
                if new.oidc_subject_hash.is_some()
                    && self
                        .get_user_by_oidc_subject(new.oidc_subject_hash.unwrap())
                        .await
                        .map_err(UsersError::Store)?
                        .is_some()
                {
                    Err(UsersError::OidcSubjectTaken)
                } else {
                    Err(UsersError::UsernameTaken)
                }
            }
            Err(e) => Err(UsersError::Store(StoreError::Sqlx(e))),
        }
    }

    /// Login lookup by username (case-insensitive, matches the
    /// NOCASE index). Returns `None` for both
    /// "no such user" and "soft-deleted user" — the login
    /// handler treats them identically to avoid enumeration.
    pub async fn get_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<User>, StoreError> {
        let row = sqlx::query(USER_SELECT_PREFIX_SQL)
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        row.map(decode_user_row).transpose()
    }

    /// Admin lookup by id. Includes soft-deleted users so the
    /// admin UI can render them under "show deleted".
    pub async fn get_user_by_id(&self, id: UserId) -> Result<Option<User>, StoreError> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, oidc_subject, role,
                    force_password_reset, disabled,
                    failed_login_count, locked_until,
                    last_login_at, last_failed_login_at,
                    created_at, updated_at, deleted_at
             FROM users WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(decode_user_row).transpose()
    }

    /// OIDC callback lookup by hashed `sub`. Returns `None`
    /// for both no-such-user and soft-deleted, matching
    /// [`get_user_by_username`].
    pub async fn get_user_by_oidc_subject(
        &self,
        oidc_subject_hash: &str,
    ) -> Result<Option<User>, StoreError> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, oidc_subject, role,
                    force_password_reset, disabled,
                    failed_login_count, locked_until,
                    last_login_at, last_failed_login_at,
                    created_at, updated_at, deleted_at
             FROM users WHERE oidc_subject = ? AND deleted_at IS NULL",
        )
        .bind(oidc_subject_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(decode_user_row).transpose()
    }

    /// /admin/users listing. Pass `include_deleted = true` to
    /// surface tombstones; default UI hides them.
    pub async fn list_users(
        &self,
        include_deleted: bool,
    ) -> Result<Vec<User>, StoreError> {
        let sql = if include_deleted {
            "SELECT id, username, password_hash, oidc_subject, role,
                    force_password_reset, disabled,
                    failed_login_count, locked_until,
                    last_login_at, last_failed_login_at,
                    created_at, updated_at, deleted_at
             FROM users
             ORDER BY deleted_at IS NOT NULL, username COLLATE NOCASE"
        } else {
            "SELECT id, username, password_hash, oidc_subject, role,
                    force_password_reset, disabled,
                    failed_login_count, locked_until,
                    last_login_at, last_failed_login_at,
                    created_at, updated_at, deleted_at
             FROM users WHERE deleted_at IS NULL
             ORDER BY username COLLATE NOCASE"
        };
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        rows.into_iter().map(decode_user_row).collect()
    }

    /// Returns the argon2id PHC string for a user (or None if
    /// the user has no local password OR has been deleted). The
    /// only legitimate caller is the login handler; never
    /// project this onto the API surface.
    pub async fn get_password_hash_for_login(
        &self,
        username: &str,
    ) -> Result<Option<(UserId, String)>, StoreError> {
        let row = sqlx::query(
            "SELECT id, password_hash FROM users
             WHERE username = ? COLLATE NOCASE
               AND deleted_at IS NULL
               AND password_hash IS NOT NULL",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get::<i64, _>(0), r.get::<String, _>(1))))
    }

    /// Active admins = role = 'admin' AND not soft-deleted AND
    /// not disabled. Used by last-admin protection on every
    /// downgrade path.
    pub async fn count_active_admins(&self) -> Result<i64, StoreError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users
             WHERE role = 'admin'
               AND deleted_at IS NULL
               AND disabled = 0",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Atomically: read current row, apply last-admin check (if
    /// downgrading from admin), then `UPDATE`. Returns `NotFound`
    /// if the user does not exist or is already soft-deleted.
    pub async fn update_user_role(
        &self,
        id: UserId,
        new_role: Role,
    ) -> Result<(), UsersError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        let current = read_user_for_update(&mut tx, id).await?;
        if current.role == Role::Admin
            && new_role != Role::Admin
            && active_admins_in_tx(&mut tx).await? <= 1
        {
            return Err(UsersError::LastAdmin);
        }
        sqlx::query(
            "UPDATE users
                SET role = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(new_role.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        tx.commit()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        Ok(())
    }

    /// Disable / re-enable a user. Disabling an admin trips
    /// last-admin protection. Re-enabling never does.
    pub async fn set_user_disabled(
        &self,
        id: UserId,
        disabled: bool,
    ) -> Result<(), UsersError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        let current = read_user_for_update(&mut tx, id).await?;
        if disabled
            && !current.disabled
            && current.role == Role::Admin
            && active_admins_in_tx(&mut tx).await? <= 1
        {
            return Err(UsersError::LastAdmin);
        }
        let val = if disabled { 1i64 } else { 0i64 };
        sqlx::query(
            "UPDATE users
                SET disabled = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(val)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        tx.commit()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        Ok(())
    }

    /// Replace the password hash. Caller supplies the new
    /// PHC-format argon2id string; `force_password_reset`
    /// becomes true when an admin reset it (so the target user
    /// must change it on next login) and false when the user
    /// changed it themselves via `/change-password`.
    pub async fn update_user_password(
        &self,
        id: UserId,
        new_password_hash: &str,
        force_password_reset: bool,
    ) -> Result<(), UsersError> {
        let force = if force_password_reset { 1i64 } else { 0i64 };
        let res = sqlx::query(
            "UPDATE users
                SET password_hash = ?,
                    force_password_reset = ?,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(new_password_hash)
        .bind(force)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        if res.rows_affected() == 0 {
            return Err(UsersError::NotFound);
        }
        Ok(())
    }

    /// Link an OIDC subject hash to an existing user. Used by
    /// the OIDC callback's "first-time login" path for a user
    /// that already exists locally (hybrid mode). Errors with
    /// `OidcSubjectTaken` if the hash is already linked to a
    /// different user.
    pub async fn set_user_oidc_subject(
        &self,
        id: UserId,
        oidc_subject_hash: &str,
    ) -> Result<(), UsersError> {
        let res = sqlx::query(
            "UPDATE users
                SET oidc_subject = ?,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(oidc_subject_hash)
        .bind(id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() == 0 => Err(UsersError::NotFound),
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if is_unique_violation(&*db) => {
                Err(UsersError::OidcSubjectTaken)
            }
            Err(e) => Err(UsersError::Store(StoreError::Sqlx(e))),
        }
    }

    /// Record a successful login: bumps `last_login_at`, zeroes
    /// the lockout counters. The login handler calls this AFTER
    /// password verification succeeds and BEFORE handing out
    /// the JWT/refresh pair.
    pub async fn record_login_success(&self, id: UserId) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE users
                SET last_login_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    failed_login_count = 0,
                    locked_until = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a failed password attempt. The caller (the
    /// lockout FSM in `nexus-engine::auth::lockout`) computes
    /// `new_count` (which may reset to 1 when the failure
    /// window has expired) and `lock_until` (which is `Some`
    /// only on the attempt that trips the lockout). The store
    /// stays policy-free: it just writes what it's told.
    ///
    /// `new_count` overwrites `failed_login_count` rather than
    /// incrementing — the FSM cannot express "reset and start
    /// over at 1" through an increment-only API, and folding
    /// reset semantics into the SQL would couple the store to
    /// the policy.
    pub async fn record_login_failure(
        &self,
        id: UserId,
        new_count: i64,
        lock_until: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE users
                SET failed_login_count = ?,
                    last_failed_login_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    locked_until = ?,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(new_count)
        .bind(lock_until.map(|t| t.to_rfc3339()))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Admin-driven unlock. Zeroes failed_login_count + clears
    /// locked_until; does NOT touch the password hash. The
    /// unlock endpoint is admin-only.
    pub async fn clear_lockout(&self, id: UserId) -> Result<(), UsersError> {
        let res = sqlx::query(
            "UPDATE users
                SET failed_login_count = 0,
                    locked_until = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        if res.rows_affected() == 0 {
            return Err(UsersError::NotFound);
        }
        Ok(())
    }

    /// Soft-delete: sets `deleted_at` AND renames username to
    /// `<id>:deleted-<iso8601-ts>` so the slot is reusable. Trips
    /// last-admin protection. Idempotent in the sense that a
    /// second call on an already-deleted user errors with
    /// `NotFound` — the row is still there, but as far as login
    /// + admin listings are concerned it doesn't exist.
    pub async fn soft_delete_user(&self, id: UserId) -> Result<(), UsersError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        let current = read_user_for_update(&mut tx, id).await?;
        if current.role == Role::Admin && active_admins_in_tx(&mut tx).await? <= 1 {
            return Err(UsersError::LastAdmin);
        }
        let now = Utc::now();
        let renamed = format!(
            "{}:deleted-{}",
            id,
            now.format("%Y%m%dT%H%M%S%.3fZ")
        );
        sqlx::query(
            "UPDATE users
                SET username = ?,
                    disabled = 1,
                    deleted_at = ?,
                    updated_at = ?
             WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&renamed)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        tx.commit()
            .await
            .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

const USER_SELECT_PREFIX_SQL: &str =
    "SELECT id, username, password_hash, oidc_subject, role,
            force_password_reset, disabled,
            failed_login_count, locked_until,
            last_login_at, last_failed_login_at,
            created_at, updated_at, deleted_at
     FROM users
     WHERE username = ? COLLATE NOCASE AND deleted_at IS NULL";

/// Inside-tx counterpart to [`Store::count_active_admins`]. Used
/// by the mutation paths so the count + the UPDATE are part of
/// one read-modify-write under SQLite's default
/// `BEGIN DEFERRED` (which on write upgrades to RESERVED). For
/// SQLite + a single-process engine this is sufficient — no
/// other writer can sneak between the COUNT and the UPDATE.
async fn active_admins_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
) -> Result<i64, UsersError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users
         WHERE role = 'admin' AND deleted_at IS NULL AND disabled = 0",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
    Ok(row.0)
}

async fn read_user_for_update(
    tx: &mut Transaction<'_, Sqlite>,
    id: UserId,
) -> Result<User, UsersError> {
    let row = sqlx::query(
        "SELECT id, username, password_hash, oidc_subject, role,
                force_password_reset, disabled,
                failed_login_count, locked_until,
                last_login_at, last_failed_login_at,
                created_at, updated_at, deleted_at
         FROM users WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| UsersError::Store(StoreError::Sqlx(e)))?;
    let row = row.ok_or(UsersError::NotFound)?;
    decode_user_row(row).map_err(UsersError::Store)
}

fn is_unique_violation(db: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLite uses extended error codes for UNIQUE: SQLITE_CONSTRAINT_UNIQUE = 2067.
    // sqlx returns the code as a string. Both the primary (19) and the
    // extended (2067) shapes show up depending on driver settings.
    db.code()
        .map(|c| c == "2067" || c == "19" || c == "1555")
        .unwrap_or(false)
}

fn decode_user_row(row: sqlx::sqlite::SqliteRow) -> Result<User, StoreError> {
    let role_str: String = row.try_get("role")?;
    let role = role_str
        .parse::<Role>()
        .map_err(|e| StoreError::Decode(format!("users.role: {e}")))?;
    let password_hash: Option<String> = row.try_get("password_hash")?;
    let oidc_subject: Option<String> = row.try_get("oidc_subject")?;
    Ok(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        role,
        force_password_reset: row.try_get::<i64, _>("force_password_reset")? != 0,
        disabled: row.try_get::<i64, _>("disabled")? != 0,
        failed_login_count: row.try_get("failed_login_count")?,
        locked_until: parse_optional_ts(row.try_get("locked_until")?)?,
        last_login_at: parse_optional_ts(row.try_get("last_login_at")?)?,
        last_failed_login_at: parse_optional_ts(row.try_get("last_failed_login_at")?)?,
        created_at: parse_ts(row.try_get::<String, _>("created_at")?.as_str())?,
        updated_at: parse_ts(row.try_get::<String, _>("updated_at")?.as_str())?,
        deleted_at: parse_optional_ts(row.try_get("deleted_at")?)?,
        has_oidc: oidc_subject.is_some(),
        has_password: password_hash.is_some(),
    })
}

fn parse_optional_ts(s: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    match s {
        None => Ok(None),
        Some(s) => parse_ts(&s).map(Some),
    }
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|n| n.and_utc())
        .map_err(|e| StoreError::Decode(format!("users timestamp {s:?}: {e}")))
}
