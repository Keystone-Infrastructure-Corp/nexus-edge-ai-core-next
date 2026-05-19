//! M6 Phase 2 Step 2.1 — schema + CRUD coverage for the `users`
//! table (migration `0010_local_users.sql`) and `nexus-store::users`.
//!
//! Exercises:
//!
//! * Migration 0010 applies cleanly and registers itself in
//!   `schema_migrations`.
//! * `create_user` round-trip: lookups by id / username
//!   (case-insensitive) / oidc_subject all return the same row.
//! * `UNIQUE NOCASE` on username — `"Alice"` and `"alice"` collide.
//! * `UNIQUE` on oidc_subject — second linkage of the same hash fails.
//! * CHECK constraint — a user with neither password nor OIDC
//!   subject is rejected (and the function returns
//!   `NoAuthMethod` before touching SQL too).
//! * `update_user_role` trips `LastAdmin` when demoting the sole
//!   admin, succeeds when a second admin exists.
//! * `set_user_disabled(true)` trips `LastAdmin` for the sole
//!   admin, succeeds when a peer admin exists. Re-enable
//!   (`set_user_disabled(false)`) never trips.
//! * `soft_delete_user` trips `LastAdmin` for the sole admin,
//!   AND renames the username to `<id>:deleted-<ts>` so the
//!   slot is reusable.
//! * `update_user_password` clears or sets `force_password_reset`.
//! * `record_login_success` / `record_login_failure` /
//!   `clear_lockout` update the right columns.
//! * `list_users(false)` excludes soft-deleted; `list_users(true)`
//!   surfaces them.
//! * `get_password_hash_for_login` returns `(id, hash)` for
//!   active users only.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::StoreConfig;
use nexus_store::{NewUser, Store, UsersError};
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

#[tokio::test]
async fn users_migration_registers() {
    let (store, _tmp) = fresh_store().await;
    let row: (String,) =
        sqlx::query_as("SELECT id FROM schema_migrations WHERE id = '0010_local_users'")
            .fetch_one(store.pool())
            .await
            .expect("migration registered");
    assert_eq!(row.0, "0010_local_users");
}

#[tokio::test]
async fn create_user_round_trips_by_id_username_and_oidc() {
    let (store, _tmp) = fresh_store().await;

    let id = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def"),
            oidc_subject_hash: Some("sha256-alice"),
            force_password_reset: true,
        })
        .await
        .expect("create");
    assert!(id > 0);

    let by_id = store
        .get_user_by_id(id)
        .await
        .unwrap()
        .expect("found by id");
    assert_eq!(by_id.username, "alice");
    assert_eq!(by_id.role, Role::Admin);
    assert!(by_id.has_password);
    assert!(by_id.has_oidc);
    assert!(by_id.force_password_reset);
    assert!(!by_id.disabled);
    assert_eq!(by_id.failed_login_count, 0);
    assert!(by_id.deleted_at.is_none());

    // Case-insensitive username lookup matches the UNIQUE NOCASE index.
    let by_upper = store
        .get_user_by_username("ALICE")
        .await
        .unwrap()
        .expect("found by uppercase username");
    assert_eq!(by_upper.id, id);

    let by_oidc = store
        .get_user_by_oidc_subject("sha256-alice")
        .await
        .unwrap()
        .expect("found by oidc subject hash");
    assert_eq!(by_oidc.id, id);
}

#[tokio::test]
async fn username_unique_case_insensitive() {
    let (store, _tmp) = fresh_store().await;

    store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Operator,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();

    let err = store
        .create_user(&NewUser {
            username: "Alice",
            role: Role::Viewer,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .expect_err("UNIQUE NOCASE must reject");
    assert!(matches!(err, UsersError::UsernameTaken));
}

#[tokio::test]
async fn oidc_subject_unique_across_users() {
    let (store, _tmp) = fresh_store().await;

    store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            oidc_subject_hash: Some("sha256-shared"),
            ..Default::default()
        })
        .await
        .unwrap();

    let err = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Viewer,
            oidc_subject_hash: Some("sha256-shared"),
            ..Default::default()
        })
        .await
        .expect_err("oidc UNIQUE must reject");
    assert!(matches!(err, UsersError::OidcSubjectTaken));
}

#[tokio::test]
async fn create_user_without_auth_method_rejected() {
    let (store, _tmp) = fresh_store().await;
    let err = store
        .create_user(&NewUser {
            username: "ghost",
            role: Role::Viewer,
            ..Default::default()
        })
        .await
        .expect_err("must reject");
    assert!(matches!(err, UsersError::NoAuthMethod));
}

#[tokio::test]
async fn last_admin_protection_blocks_role_downgrade() {
    let (store, _tmp) = fresh_store().await;

    let admin = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    // Sole admin → demotion must fail.
    let err = store
        .update_user_role(admin, Role::Operator)
        .await
        .expect_err("last-admin protection trips");
    assert!(matches!(err, UsersError::LastAdmin));

    // Add a second admin and the demotion now succeeds.
    let _bob = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_user_role(admin, Role::Operator)
        .await
        .expect("downgrade now allowed");

    let alice = store.get_user_by_id(admin).await.unwrap().unwrap();
    assert_eq!(alice.role, Role::Operator);
}

#[tokio::test]
async fn last_admin_protection_blocks_disable_but_allows_reenable() {
    let (store, _tmp) = fresh_store().await;

    let admin = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    let err = store
        .set_user_disabled(admin, true)
        .await
        .expect_err("cannot disable sole admin");
    assert!(matches!(err, UsersError::LastAdmin));

    // Add peer admin → disabling alice is now allowed.
    let _bob = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    store.set_user_disabled(admin, true).await.unwrap();
    // Re-enabling never trips last-admin protection.
    store.set_user_disabled(admin, false).await.unwrap();

    let alice = store.get_user_by_id(admin).await.unwrap().unwrap();
    assert!(!alice.disabled);
}

#[tokio::test]
async fn soft_delete_user_renames_username_and_frees_slot() {
    let (store, _tmp) = fresh_store().await;

    // Two admins so the delete is allowed.
    let _bob = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    let alice = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();

    store.soft_delete_user(alice).await.unwrap();

    // The deleted row is still present under a renamed handle.
    let raw: (String, Option<String>) = sqlx::query_as(
        "SELECT username, deleted_at FROM users WHERE id = ?",
    )
    .bind(alice)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        raw.0.starts_with(&format!("{alice}:deleted-")),
        "username renamed to {} (expected '{alice}:deleted-...')",
        raw.0
    );
    assert!(raw.1.is_some(), "deleted_at set");

    // The slot is now reusable for a fresh user.
    let new_alice = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Operator,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .expect("slot reusable post-rename");
    assert_ne!(new_alice, alice);

    // Soft-deleted user is NOT findable by original username.
    let by_username = store.get_user_by_username("alice").await.unwrap().unwrap();
    assert_eq!(by_username.id, new_alice, "lookup hits the new user, not the tombstone");
}

#[tokio::test]
async fn soft_delete_trips_last_admin_protection() {
    let (store, _tmp) = fresh_store().await;
    let admin = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    let err = store
        .soft_delete_user(admin)
        .await
        .expect_err("cannot delete sole admin");
    assert!(matches!(err, UsersError::LastAdmin));
}

#[tokio::test]
async fn update_user_password_flips_force_reset_flag() {
    let (store, _tmp) = fresh_store().await;
    let id = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Operator,
            password_hash: Some("$argon2id$old"),
            force_password_reset: true,
            ..Default::default()
        })
        .await
        .unwrap();
    // User-initiated change clears the flag.
    store
        .update_user_password(id, "$argon2id$new", false)
        .await
        .unwrap();
    let after = store.get_user_by_id(id).await.unwrap().unwrap();
    assert!(!after.force_password_reset);

    // Admin reset sets it back.
    store
        .update_user_password(id, "$argon2id$reset", true)
        .await
        .unwrap();
    let after2 = store.get_user_by_id(id).await.unwrap().unwrap();
    assert!(after2.force_password_reset);
}

#[tokio::test]
async fn login_success_resets_lockout_counters() {
    let (store, _tmp) = fresh_store().await;
    let id = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Viewer,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();

    let lock = Utc::now() + Duration::minutes(15);
    store.record_login_failure(id, 1, None).await.unwrap();
    store.record_login_failure(id, 2, None).await.unwrap();
    store.record_login_failure(id, 3, Some(lock)).await.unwrap();

    let before = store.get_user_by_id(id).await.unwrap().unwrap();
    assert_eq!(before.failed_login_count, 3);
    assert!(before.locked_until.is_some());
    assert!(before.last_failed_login_at.is_some());
    assert!(before.last_login_at.is_none());

    store.record_login_success(id).await.unwrap();

    let after = store.get_user_by_id(id).await.unwrap().unwrap();
    assert_eq!(after.failed_login_count, 0);
    assert!(after.locked_until.is_none());
    assert!(after.last_login_at.is_some());
}

#[tokio::test]
async fn clear_lockout_zeroes_failed_counter() {
    let (store, _tmp) = fresh_store().await;
    let id = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Viewer,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .record_login_failure(id, 1, Some(Utc::now() + Duration::minutes(15)))
        .await
        .unwrap();
    store
        .record_login_failure(id, 2, Some(Utc::now() + Duration::minutes(15)))
        .await
        .unwrap();

    store.clear_lockout(id).await.unwrap();
    let row = store.get_user_by_id(id).await.unwrap().unwrap();
    assert_eq!(row.failed_login_count, 0);
    assert!(row.locked_until.is_none());

    // Unknown id is `NotFound`.
    let err = store.clear_lockout(999_999).await.expect_err("not found");
    assert!(matches!(err, UsersError::NotFound));
}

#[tokio::test]
async fn list_users_hides_soft_deleted_by_default() {
    let (store, _tmp) = fresh_store().await;

    let _ = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    let bob = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    store.soft_delete_user(bob).await.unwrap();

    let visible = store.list_users(false).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].username, "alice");

    let all = store.list_users(true).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn get_password_hash_for_login_returns_active_users_only() {
    let (store, _tmp) = fresh_store().await;

    // OIDC-only user has no hash → None.
    let _oidc = store
        .create_user(&NewUser {
            username: "carol",
            role: Role::Viewer,
            oidc_subject_hash: Some("sha256-carol"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(store
        .get_password_hash_for_login("carol")
        .await
        .unwrap()
        .is_none());

    // Local user has hash → Some((id, hash)).
    let id = store
        .create_user(&NewUser {
            username: "alice",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    // Make sure soft-delete is allowed (need a second admin).
    let _bob = store
        .create_user(&NewUser {
            username: "bob",
            role: Role::Admin,
            password_hash: Some("$argon2id$dummy"),
            ..Default::default()
        })
        .await
        .unwrap();
    let pair = store
        .get_password_hash_for_login("ALICE")
        .await
        .unwrap()
        .expect("found");
    assert_eq!(pair.0, id);
    assert_eq!(pair.1, "$argon2id$dummy");

    // Soft-delete alice → no longer returned.
    store.soft_delete_user(id).await.unwrap();
    assert!(store
        .get_password_hash_for_login("alice")
        .await
        .unwrap()
        .is_none());
}
