//! M6 Phase 2 Step 2.4 — schema + CRUD coverage for the
//! `auth_refresh_tokens` table (migration
//! `0011_auth_refresh_tokens.sql`) and `nexus-store::sessions`.
//!
//! Exercises:
//!
//! * Migration 0011 applies cleanly and registers itself.
//! * `insert_refresh_token` round-trips by id + by hash;
//!   `is_live_at` flips with `rotated_at` / `revoked_at` /
//!   `expires_at`.
//! * `UNIQUE(token_hash)` is enforced and surfaces as
//!   `SessionsError::TokenHashCollision`.
//! * `mark_refresh_token_rotated` flips `rotated_at`;
//!   missing-id returns `SessionsError::NotFound`.
//! * `revoke_chain` revokes every row sharing a `chain_id`
//!   AND returns the number of rows touched.
//! * `revoke_refresh_token` revokes one row; missing-id is
//!   `NotFound`.
//! * `list_active_refresh_tokens_for_user` returns only the
//!   live head (no rotated / revoked / expired rows).
//! * `delete_expired_refresh_tokens` purges expired+rotated
//!   and expired+revoked rows but leaves expired-but-still-
//!   "head" rows (so the UI can render "expired session").
//! * `ON DELETE CASCADE` from `users` drops the refresh rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::StoreConfig;
use nexus_store::{NewRefreshToken, NewUser, SessionsError, Store};
use nexus_types::Role;
use tempfile::TempDir;

async fn fresh_store() -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db_path.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    let store = Store::open(&cfg).await.expect("Store::open");
    (store, dir)
}

async fn make_user(store: &Store, username: &str, role: Role) -> i64 {
    store
        .create_user(&NewUser {
            username,
            role,
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def"),
            oidc_subject_hash: None,
            force_password_reset: false,
        })
        .await
        .expect("create user")
}

#[tokio::test]
async fn refresh_tokens_migration_registers() {
    let (store, _tmp) = fresh_store().await;
    let row: (String,) =
        sqlx::query_as("SELECT id FROM schema_migrations WHERE id = '0011_auth_refresh_tokens'")
            .fetch_one(store.pool())
            .await
            .expect("migration registered");
    assert_eq!(row.0, "0011_auth_refresh_tokens");
}

#[tokio::test]
async fn insert_refresh_token_round_trips_by_id_and_hash() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    let inserted = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "abc123",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: None,
            expires_at: exp,
            user_agent: Some("test/1.0"),
            ip: Some("127.0.0.1"),
        })
        .await
        .expect("insert refresh token");

    assert!(inserted.id > 0);
    assert_eq!(inserted.token_hash, "abc123");
    assert_eq!(inserted.user_id, uid);
    assert_eq!(inserted.chain_id, "chain-1");
    assert_eq!(inserted.parent_id, None);
    assert_eq!(inserted.user_agent.as_deref(), Some("test/1.0"));
    assert_eq!(inserted.ip.as_deref(), Some("127.0.0.1"));
    assert!(inserted.rotated_at.is_none());
    assert!(inserted.revoked_at.is_none());
    assert!(inserted.is_live_at(Utc::now()));

    let by_hash = store
        .get_refresh_token_by_hash("abc123")
        .await
        .unwrap()
        .expect("found by hash");
    assert_eq!(by_hash.id, inserted.id);

    let missing = store.get_refresh_token_by_hash("nope").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn duplicate_hash_returns_collision_error() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "dup",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .expect("first insert");

    let err = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "dup",
            user_id: uid,
            chain_id: "chain-2",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .expect_err("dup hash must collide");
    assert!(matches!(err, SessionsError::TokenHashCollision), "{err:?}");
}

#[tokio::test]
async fn mark_rotated_flips_rotated_at_and_makes_not_live() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    let row = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "h1",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    let now = Utc::now();
    store.mark_refresh_token_rotated(row.id, now).await.unwrap();

    let reloaded = store
        .get_refresh_token_by_hash("h1")
        .await
        .unwrap()
        .unwrap();
    assert!(reloaded.rotated_at.is_some());
    assert!(!reloaded.is_live_at(Utc::now()), "rotated → not live");
}

#[tokio::test]
async fn mark_rotated_missing_id_returns_notfound() {
    let (store, _tmp) = fresh_store().await;
    let err = store
        .mark_refresh_token_rotated(99_999, Utc::now())
        .await
        .expect_err("missing id");
    assert!(matches!(err, SessionsError::NotFound(99_999)), "{err:?}");
}

#[tokio::test]
async fn revoke_chain_revokes_every_row_with_same_chain_id() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    // Three rows in the same chain (a refresh rotated twice).
    let r0 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "h0",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    let r1 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "h1",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: Some(r0.id),
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    let _r2 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "h2",
            user_id: uid,
            chain_id: "chain-1",
            parent_id: Some(r1.id),
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    // One row in an unrelated chain — must NOT be touched.
    let _other = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "other",
            user_id: uid,
            chain_id: "chain-2",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    let touched = store.revoke_chain("chain-1", Utc::now()).await.unwrap();
    assert_eq!(touched, 3, "all three in chain-1");

    for hash in ["h0", "h1", "h2"] {
        let row = store
            .get_refresh_token_by_hash(hash)
            .await
            .unwrap()
            .unwrap();
        assert!(row.revoked_at.is_some(), "{hash} should be revoked");
    }
    let other = store
        .get_refresh_token_by_hash("other")
        .await
        .unwrap()
        .unwrap();
    assert!(other.revoked_at.is_none(), "chain-2 untouched");

    // Second revoke is idempotent: WHERE revoked_at IS NULL
    // skips the already-revoked rows.
    let touched2 = store.revoke_chain("chain-1", Utc::now()).await.unwrap();
    assert_eq!(touched2, 0, "second revoke is a no-op");
}

#[tokio::test]
async fn revoke_single_token_returns_notfound_for_unknown_id() {
    let (store, _tmp) = fresh_store().await;
    let err = store
        .revoke_refresh_token(99_999, Utc::now())
        .await
        .expect_err("missing id");
    assert!(matches!(err, SessionsError::NotFound(99_999)), "{err:?}");
}

#[tokio::test]
async fn list_active_returns_only_live_head_rows() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    // Chain A: head live, parent rotated → list returns 1 row.
    let a0 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "a0",
            user_id: uid,
            chain_id: "A",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    store
        .mark_refresh_token_rotated(a0.id, Utc::now())
        .await
        .unwrap();
    let _a1 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "a1",
            user_id: uid,
            chain_id: "A",
            parent_id: Some(a0.id),
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    // Chain B: revoked entirely → 0 rows.
    let b0 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "b0",
            user_id: uid,
            chain_id: "B",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    store.revoke_refresh_token(b0.id, Utc::now()).await.unwrap();

    // Chain C: expired → 0 rows.
    let _c0 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "c0",
            user_id: uid,
            chain_id: "C",
            parent_id: None,
            expires_at: Utc::now() - Duration::minutes(1),
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    // Chain D: fresh, live → 1 row.
    let _d0 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "d0",
            user_id: uid,
            chain_id: "D",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    let live = store
        .list_active_refresh_tokens_for_user(uid, Utc::now())
        .await
        .unwrap();
    assert_eq!(live.len(), 2, "A-head + D, not B/C/A-root");
    let hashes: Vec<&str> = live.iter().map(|r| r.token_hash.as_str()).collect();
    assert!(hashes.contains(&"a1"), "A-head present");
    assert!(hashes.contains(&"d0"), "D present");
}

#[tokio::test]
async fn delete_expired_purges_only_terminated_rows() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let past = Utc::now() - Duration::minutes(1);

    // Expired + rotated → DELETE.
    let r1 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "r1",
            user_id: uid,
            chain_id: "X",
            parent_id: None,
            expires_at: past,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    store.mark_refresh_token_rotated(r1.id, past).await.unwrap();

    // Expired + revoked → DELETE.
    let r2 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "r2",
            user_id: uid,
            chain_id: "Y",
            parent_id: None,
            expires_at: past,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    store.revoke_refresh_token(r2.id, past).await.unwrap();

    // Expired but still "head" (rotated_at NULL, revoked_at
    // NULL) → KEEP — the /admin/sessions UI wants to show it.
    let _r3 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "r3",
            user_id: uid,
            chain_id: "Z",
            parent_id: None,
            expires_at: past,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    // Live + rotated (impossible in production — we always
    // rotate already-expired tokens — but the sweeper must
    // not crash). KEEP because expires_at > now.
    let r4 = store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "r4",
            user_id: uid,
            chain_id: "W",
            parent_id: None,
            expires_at: Utc::now() + Duration::days(1),
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();
    store
        .mark_refresh_token_rotated(r4.id, Utc::now())
        .await
        .unwrap();

    let n = store
        .delete_expired_refresh_tokens(Utc::now())
        .await
        .unwrap();
    assert_eq!(n, 2, "only r1 + r2");

    for hash in ["r3", "r4"] {
        let row = store.get_refresh_token_by_hash(hash).await.unwrap();
        assert!(row.is_some(), "{hash} should be kept");
    }
    for hash in ["r1", "r2"] {
        let row = store.get_refresh_token_by_hash(hash).await.unwrap();
        assert!(row.is_none(), "{hash} should be deleted");
    }
}

#[tokio::test]
async fn cascade_delete_when_user_dropped() {
    let (store, _tmp) = fresh_store().await;
    let uid = make_user(&store, "alice", Role::Operator).await;
    let exp = Utc::now() + Duration::days(30);

    store
        .insert_refresh_token(NewRefreshToken {
            token_hash: "casc",
            user_id: uid,
            chain_id: "C",
            parent_id: None,
            expires_at: exp,
            user_agent: None,
            ip: None,
        })
        .await
        .unwrap();

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(uid)
        .execute(store.pool())
        .await
        .unwrap();

    let row = store.get_refresh_token_by_hash("casc").await.unwrap();
    assert!(row.is_none(), "ON DELETE CASCADE dropped the refresh row");
}
