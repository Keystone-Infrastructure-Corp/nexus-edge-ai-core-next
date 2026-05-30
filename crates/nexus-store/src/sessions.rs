//! M6 Phase 2 Step 2.4: refresh-token store.
//!
//! Wraps the `auth_refresh_tokens` table (migration
//! `0011_auth_refresh_tokens.sql`) with the primitives the
//! login / refresh / logout handlers (Step 2.7) compose:
//!
//! * [`Store::insert_refresh_token`] — issue a new token row.
//!   Called by both `POST /auth/login` (creates the chain
//!   root) and `POST /auth/refresh` (creates the next link
//!   with `parent_id` set).
//!
//! * [`Store::get_refresh_token_by_hash`] — look up a token
//!   by its SHA-256 hash. The plaintext secret never enters
//!   the store layer; the handler hashes first.
//!
//! * [`Store::mark_refresh_token_rotated`] — flips
//!   `rotated_at` non-null, rendering the token single-use. A
//!   second `mark_*` call on the same id is the replay
//!   indicator.
//!
//! * [`Store::revoke_chain`] — sets `revoked_at` on EVERY row
//!   that shares a `chain_id`. Called on replay detection and
//!   on `POST /auth/logout`.
//!
//! * [`Store::revoke_refresh_token`] — single-row revoke used
//!   by `POST /admin/sessions/:id/revoke` (Step 2.8).
//!
//! * [`Store::list_active_refresh_tokens_for_user`] —
//!   `/admin/sessions` list: one row per *live* chain (head =
//!   not rotated, not revoked, not expired).
//!
//! * [`Store::delete_expired_refresh_tokens`] — retention
//!   sweeper hook. Drops rows where `expires_at <= now` AND
//!   the row is rotated or revoked (so the audit-trail of
//!   live sessions is never deleted).
//!
//! ## What this module does NOT do
//!
//! * **Generate refresh secrets.** Random-bytes generation +
//!   URL-safe base64 + SHA-256 hashing all live in
//!   `nexus-engine::auth::sessions`. The store sees only the
//!   already-hashed value.
//!
//! * **Issue or verify access JWTs.** Same — that's the
//!   engine's `auth::sessions` module. The store only persists
//!   the refresh half.
//!
//! * **Enforce single-use semantics.** The handler must check
//!   `rotated_at` and `revoked_at` and react accordingly.
//!   Doing it here would require returning a side-channel
//!   `was_already_rotated` flag and is easier to get wrong
//!   than a plain handler-side `if`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Store, StoreError};

pub type RefreshTokenId = i64;

/// One row of `auth_refresh_tokens`. The plaintext secret is
/// NEVER part of this struct — the store sees only the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshToken {
    pub id: RefreshTokenId,
    /// SHA-256 hex of the secret half. 64 lowercase ASCII chars.
    pub token_hash: String,
    pub user_id: i64,
    /// UUIDv7 shared by every generation of one login.
    pub chain_id: String,
    /// `Some` for every rotation, `None` for the chain root.
    pub parent_id: Option<RefreshTokenId>,
    pub expires_at: DateTime<Utc>,
    /// `Some` iff the token has been rotated (i.e. used once).
    /// A second attempt to rotate this id is the replay
    /// indicator the handler watches for.
    pub rotated_at: Option<DateTime<Utc>>,
    /// `Some` iff the chain was revoked. Once set, no refresh
    /// in the chain is valid even if `rotated_at` is null.
    pub revoked_at: Option<DateTime<Utc>>,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    pub created_at: DateTime<Utc>,
    /// v0.1.36 — sliding-window inactivity timestamp. Bumped
    /// by the idle-bump task on every authenticated request
    /// for any token in this chain. `None` only on rows that
    /// pre-date migration 0019 (the migration backfills
    /// `last_active_at = created_at` for existing rows, so
    /// `None` in practice means "the row was inserted by an
    /// engine build older than v0.1.36"; treat it as
    /// `created_at` via [`Self::effective_last_active_at`]).
    pub last_active_at: Option<DateTime<Utc>>,
    /// v0.1.36 — `Some` iff the chain was killed by idle
    /// expiry (as opposed to logout / replay / admin revoke,
    /// which use `revoked_at`). Lets the audit log surface
    /// expired sessions distinctly without a follow-up join.
    pub idle_revoked_at: Option<DateTime<Utc>>,
}

impl RefreshToken {
    /// True iff this row is usable as a refresh right now —
    /// not yet rotated, not yet revoked, not yet expired. The
    /// handler still calls into `revoke_chain` on a replay,
    /// but `is_live_at` is the cheap pre-check.
    pub fn is_live_at(&self, now: DateTime<Utc>) -> bool {
        self.rotated_at.is_none()
            && self.revoked_at.is_none()
            && self.idle_revoked_at.is_none()
            && self.expires_at > now
    }

    /// `last_active_at` with the pre-v0.1.36 fallback applied:
    /// rows inserted by an older engine never get a non-NULL
    /// `last_active_at` from the write path, so the idle check
    /// uses `created_at` for those (i.e. they expire 20 min
    /// after creation if nothing bumps them). Practically all
    /// such rows are also already past the 30-day TTL, so this
    /// fallback only matters for the immediate post-upgrade
    /// window.
    pub fn effective_last_active_at(&self) -> DateTime<Utc> {
        self.last_active_at.unwrap_or(self.created_at)
    }
}

/// Insertion payload. Mirrors the user-facing CRUD shape from
/// `nexus-store::users::NewUser` — borrows where it can to
/// avoid allocations in the login hot path.
#[derive(Debug, Clone)]
pub struct NewRefreshToken<'a> {
    pub token_hash: &'a str,
    pub user_id: i64,
    pub chain_id: &'a str,
    pub parent_id: Option<RefreshTokenId>,
    pub expires_at: DateTime<Utc>,
    pub user_agent: Option<&'a str>,
    pub ip: Option<&'a str>,
}

/// Error variants the handler translates to HTTP responses.
#[derive(Debug, thiserror::Error)]
pub enum SessionsError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// `insert_refresh_token` raced and lost — the token_hash
    /// already exists. In practice this is "an attacker
    /// generated a 256-bit collision" so it's a 500. Returned
    /// instead of bubbling sqlx so the handler doesn't have
    /// to know about sqlx::error::ErrorKind.
    #[error("refresh token hash collision (treat as 500)")]
    TokenHashCollision,
    /// Lookup target was deleted between `get_*` and a
    /// subsequent mutation. Should never happen in the
    /// single-request refresh flow but is exposed so the
    /// handler can return 401 instead of 500.
    #[error("refresh token id {0} no longer exists")]
    NotFound(RefreshTokenId),
}

const REFRESH_SELECT_PREFIX_SQL: &str = "SELECT id, token_hash, user_id, chain_id, parent_id, \
    expires_at, rotated_at, revoked_at, user_agent, ip, created_at, last_active_at, idle_revoked_at \
    FROM auth_refresh_tokens";

impl Store {
    /// Insert a brand-new refresh-token row and return the
    /// fully-populated struct (id + created_at filled in by
    /// SQLite). Returns [`SessionsError::TokenHashCollision`]
    /// on `UNIQUE(token_hash)` violation.
    pub async fn insert_refresh_token(
        &self,
        token: NewRefreshToken<'_>,
    ) -> Result<RefreshToken, SessionsError> {
        let res = sqlx::query(
            "INSERT INTO auth_refresh_tokens \
                (token_hash, user_id, chain_id, parent_id, expires_at, user_agent, ip, last_active_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(token.token_hash)
        .bind(token.user_id)
        .bind(token.chain_id)
        .bind(token.parent_id)
        .bind(token.expires_at.to_rfc3339())
        .bind(token.user_agent)
        .bind(token.ip)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await;

        let id = match res {
            Ok(r) => r.last_insert_rowid(),
            Err(sqlx::Error::Database(db)) if is_unique_violation(&*db) => {
                return Err(SessionsError::TokenHashCollision);
            }
            Err(e) => return Err(SessionsError::Store(e.into())),
        };

        let row = sqlx::query(&format!("{REFRESH_SELECT_PREFIX_SQL} WHERE id = ?"))
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SessionsError::Store(e.into()))?;
        Ok(decode_refresh_token_row(row)?)
    }

    /// Look up a refresh token by its SHA-256 hash. Returns
    /// `None` if no row matches — the handler treats that
    /// identically to "matches but rotated_at is non-null" (a
    /// 401 with no detail).
    pub async fn get_refresh_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<RefreshToken>, StoreError> {
        let row = sqlx::query(&format!("{REFRESH_SELECT_PREFIX_SQL} WHERE token_hash = ?"))
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(decode_refresh_token_row(r)?)),
        }
    }

    /// Flip `rotated_at` non-null. Idempotent at the SQL level
    /// (a second call simply overwrites), but the handler must
    /// detect "already rotated" via the prior `get_*` —
    /// re-rotating without the replay-revoke is a bug.
    pub async fn mark_refresh_token_rotated(
        &self,
        id: RefreshTokenId,
        rotated_at: DateTime<Utc>,
    ) -> Result<(), SessionsError> {
        let res = sqlx::query(
            "UPDATE auth_refresh_tokens \
                SET rotated_at = ? \
              WHERE id = ?",
        )
        .bind(rotated_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| SessionsError::Store(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(SessionsError::NotFound(id));
        }
        Ok(())
    }

    /// Set `revoked_at` on every row that shares this
    /// `chain_id`. Called on replay-detect and on logout. Safe
    /// to call repeatedly (later timestamps overwrite, no
    /// other state to corrupt).
    pub async fn revoke_chain(
        &self,
        chain_id: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE auth_refresh_tokens \
                SET revoked_at = ? \
              WHERE chain_id = ? AND revoked_at IS NULL",
        )
        .bind(revoked_at.to_rfc3339())
        .bind(chain_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Revoke exactly one row — used by the admin
    /// `POST /admin/sessions/:id/revoke` endpoint. Returns
    /// [`SessionsError::NotFound`] if the id no longer exists.
    pub async fn revoke_refresh_token(
        &self,
        id: RefreshTokenId,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), SessionsError> {
        let res = sqlx::query(
            "UPDATE auth_refresh_tokens \
                SET revoked_at = ? \
              WHERE id = ?",
        )
        .bind(revoked_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| SessionsError::Store(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(SessionsError::NotFound(id));
        }
        Ok(())
    }

    /// List the "live" sessions for a user — exactly one row
    /// per active chain (the head, i.e. not rotated, not
    /// revoked, not expired). Ordered newest-first.
    pub async fn list_active_refresh_tokens_for_user(
        &self,
        user_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Vec<RefreshToken>, StoreError> {
        let rows = sqlx::query(&format!(
            "{REFRESH_SELECT_PREFIX_SQL} \
             WHERE user_id = ? \
               AND rotated_at IS NULL \
               AND revoked_at IS NULL \
               AND expires_at > ? \
             ORDER BY created_at DESC",
        ))
        .bind(user_id)
        .bind(now.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(decode_refresh_token_row).collect()
    }

    /// Retention sweeper: hard-delete refresh tokens that are
    /// both expired AND either rotated or revoked. Live
    /// sessions (rotated_at IS NULL AND revoked_at IS NULL)
    /// are preserved even when past `expires_at`, so the
    /// /admin/sessions UI can still show them as "expired
    /// session". Pair with a separate sweep for the audit-log
    /// retention window (Step 4.x).
    pub async fn delete_expired_refresh_tokens(
        &self,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "DELETE FROM auth_refresh_tokens \
              WHERE expires_at <= ? \
                AND (rotated_at IS NOT NULL OR revoked_at IS NOT NULL)",
        )
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// v0.1.36 — sliding-window activity bump. Updates
    /// `last_active_at` on every row in the chain that's still
    /// live (not rotated, not revoked, not idle-revoked) and
    /// where the new timestamp is actually newer. The "newer"
    /// guard makes the call idempotent for the batched bump
    /// pipeline: the idle-bump task coalesces multiple
    /// requests against the same chain into one UPDATE every
    /// 250 ms, but a stale message arriving out-of-order
    /// doesn't roll the timestamp backwards.
    ///
    /// Returns the number of rows touched — 0 means the chain
    /// is already dead and the caller can drop the bump on the
    /// floor instead of retrying.
    pub async fn bump_refresh_chain_active_at(
        &self,
        chain_id: &str,
        active_at: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE auth_refresh_tokens \
                SET last_active_at = ? \
              WHERE chain_id = ? \
                AND revoked_at IS NULL \
                AND idle_revoked_at IS NULL \
                AND (last_active_at IS NULL OR last_active_at < ?)",
        )
        .bind(active_at.to_rfc3339())
        .bind(chain_id)
        .bind(active_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// v0.1.36 — mark a chain dead due to idle expiry. Sets
    /// `idle_revoked_at` on every row that shares this
    /// `chain_id` and hasn't already been killed for some
    /// other reason. Distinct from [`Self::revoke_chain`] so
    /// the audit log can differentiate "user walked away" from
    /// "stolen token replay".
    pub async fn idle_revoke_chain(
        &self,
        chain_id: &str,
        idle_revoked_at: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE auth_refresh_tokens \
                SET idle_revoked_at = ? \
              WHERE chain_id = ? \
                AND idle_revoked_at IS NULL \
                AND revoked_at IS NULL",
        )
        .bind(idle_revoked_at.to_rfc3339())
        .bind(chain_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// v0.1.36 — newest `last_active_at` across the chain.
    /// Used by the refresh handler to decide whether the chain
    /// is still inside the idle window without needing to load
    /// every row.
    pub async fn max_chain_last_active_at(
        &self,
        chain_id: &str,
    ) -> Result<Option<DateTime<Utc>>, StoreError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT MAX(COALESCE(last_active_at, created_at)) \
               FROM auth_refresh_tokens \
              WHERE chain_id = ? \
                AND revoked_at IS NULL \
                AND idle_revoked_at IS NULL",
        )
        .bind(chain_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((Some(s),)) = row else {
            return Ok(None);
        };
        parse_ts(&s).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

fn is_unique_violation(db: &dyn sqlx::error::DatabaseError) -> bool {
    db.code()
        .map(|c| c == "2067" || c == "19" || c == "1555")
        .unwrap_or(false)
}

fn decode_refresh_token_row(row: sqlx::sqlite::SqliteRow) -> Result<RefreshToken, StoreError> {
    Ok(RefreshToken {
        id: row.try_get("id")?,
        token_hash: row.try_get("token_hash")?,
        user_id: row.try_get("user_id")?,
        chain_id: row.try_get("chain_id")?,
        parent_id: row.try_get("parent_id")?,
        expires_at: parse_ts(row.try_get::<String, _>("expires_at")?.as_str())?,
        rotated_at: parse_optional_ts(row.try_get("rotated_at")?)?,
        revoked_at: parse_optional_ts(row.try_get("revoked_at")?)?,
        user_agent: row.try_get("user_agent")?,
        ip: row.try_get("ip")?,
        created_at: parse_ts(row.try_get::<String, _>("created_at")?.as_str())?,
        last_active_at: parse_optional_ts(row.try_get("last_active_at")?)?,
        idle_revoked_at: parse_optional_ts(row.try_get("idle_revoked_at")?)?,
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
        .map_err(|e| StoreError::Decode(format!("auth_refresh_tokens timestamp {s:?}: {e}")))
}
