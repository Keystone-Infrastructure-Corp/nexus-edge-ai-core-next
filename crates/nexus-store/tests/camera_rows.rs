//! `Store::list_readable_cameras` returns the camera rows this build can read
//! and the ids of those it cannot. Boot and the reconciler read with it, so a
//! row it cannot read must be reported, never a panic that takes them down;
//! `list_cameras`, which needs every row, must fail on it, not panic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::Store;
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

fn camera(id: i64) -> CameraConfig {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": format!("c{id}"),
        "url": "virtual://local",
    }))
    .expect("a minimal camera")
}

/// A row that does not decode (a codec from a newer release) and a row whose
/// `config_json` is not text at all (what a hand repair in the sqlite shell
/// can leave) are both reported by id, and the readable row is returned.
#[tokio::test]
async fn a_row_this_build_cannot_read_is_reported_by_id() {
    let (store, _dir) = fresh_store().await;
    for id in [1, 2, 3] {
        store
            .upsert_camera(&camera(id))
            .await
            .expect("store a camera");
    }
    sqlx::query("UPDATE cameras SET config_json = CAST(config_json AS BLOB) WHERE id = 2")
        .execute(store.pool())
        .await
        .expect("store camera 2's row as a blob");
    sqlx::query(
        "UPDATE cameras SET config_json = json_set(config_json, '$.codec', 'av1') WHERE id = 3",
    )
    .execute(store.pool())
    .await
    .expect("give camera 3 a codec this build has no variant for");

    let (readable, unreadable) = store
        .list_readable_cameras()
        .await
        .expect("the rows are read one by one");
    assert_eq!(readable.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(unreadable, vec![2, 3]);

    // The reads that need every row fail on them, rather than panic the
    // task that called them (the roster's, say).
    sqlx::query(
        "UPDATE cameras SET config_json = json_set(config_json, '$.codec', 'h264') WHERE id = 3",
    )
    .execute(store.pool())
    .await
    .expect("make camera 3 readable again");
    assert!(
        store.list_cameras().await.is_err(),
        "list_cameras fails on the blob row"
    );
}

fn revision_of(camera: &CameraConfig) -> String {
    camera.behavior.recording_policy_revision.clone()
}

/// The revisions of the rows this build can read, in id order.
async fn readable_revisions(store: &Store) -> Vec<String> {
    let (readable, _) = store.list_readable_cameras().await.unwrap();
    readable.iter().map(revision_of).collect()
}

async fn reopen(store: Store, dir: &TempDir) -> Store {
    store.pool().close().await;
    Store::open(&StoreConfig {
        url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    })
    .await
    .expect("reopen the store")
}

/// SPEC-089 — a create assigns the camera a new opaque revision whatever the
/// body carried; a restart keeps it, and a camera deleted and created again
/// gets another.
#[tokio::test]
async fn a_created_camera_gets_a_revision_that_a_restart_keeps() {
    async fn create(store: &Store) -> CameraConfig {
        let mut cam = camera(0);
        cam.behavior.recording_policy_revision = "client-chosen".into();
        let mut tx = store.begin_tx().await.unwrap();
        store.create_camera_tx(&mut tx, &mut cam).await.unwrap();
        Store::commit_tx(tx).await.unwrap();
        cam
    }
    let (store, dir) = fresh_store().await;
    let created = create(&store).await;
    let first = revision_of(&created);
    assert_ne!(first, "client-chosen", "a create keeps no client revision");
    assert!(
        first.len() == 32 && first.chars().all(|c| c.is_ascii_hexdigit()),
        "{first}"
    );
    assert_eq!(revision_of(&store.list_cameras().await.unwrap()[0]), first);

    let store = reopen(store, &dir).await;
    assert_eq!(
        revision_of(&store.list_cameras().await.unwrap()[0]),
        first,
        "a restart keeps the revision"
    );

    store.delete_camera(created.id).await.unwrap();
    let recreated = create(&store).await;
    assert_ne!(revision_of(&recreated), first, "a re-create gets a new one");
    assert_eq!(
        revision_of(&store.list_cameras().await.unwrap()[0]),
        revision_of(&recreated)
    );
}

/// SPEC-089 — a camera an older build saved without a revision gets exactly
/// one, persisted when the store is opened: every read and every later open
/// report that one. A row this build cannot read does not stop the open.
#[tokio::test]
async fn a_camera_saved_without_a_revision_gets_one_persisted_on_first_load() {
    let (store, dir) = fresh_store().await;
    for id in [1, 2] {
        store.upsert_camera(&camera(id)).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO cameras (id, name, url, enabled, config_json) \
         VALUES (3, 'blob', 'virtual://local', 1, CAST('{\"id\":3}' AS BLOB))",
    )
    .execute(store.pool())
    .await
    .unwrap();
    let stored: Vec<String> = sqlx::query_scalar("SELECT config_json FROM cameras WHERE id < 3")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|j| !j.contains("recording_policy_revision")),
        "precondition: saved without a revision, as an older build did: {stored:?}"
    );

    let store = reopen(store, &dir).await;
    let first: Vec<String> = readable_revisions(&store).await;
    assert!(first.iter().all(|r| r.len() == 32), "{first:?}");
    assert_ne!(first[0], first[1], "each camera gets its own");
    assert_eq!(readable_revisions(&store).await, first, "reads invent none");

    let store = reopen(store, &dir).await;
    assert_eq!(
        readable_revisions(&store).await,
        first,
        "a later open keeps the persisted revision"
    );
    let (_, unreadable) = store.list_readable_cameras().await.unwrap();
    assert_eq!(unreadable, vec![3], "the blob row is left as it was");
}

/// SPEC-089 — cameras seeded from the TOML config are stored with a revision.
#[tokio::test]
async fn the_toml_seed_gives_each_camera_a_revision() {
    let (store, _dir) = fresh_store().await;
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cfg = nexus_config::Config::load(repo_root.join("config/single-camera.toml"))
        .expect("load the default config");
    let mut second = cfg.cameras[0].clone();
    second.id += 1;
    cfg.cameras.push(second);
    store.seed_from_config_if_empty(&cfg).await.unwrap();
    let revisions: Vec<String> = store
        .list_cameras()
        .await
        .unwrap()
        .iter()
        .map(revision_of)
        .collect();
    assert_eq!(revisions.len(), 2);
    assert!(revisions.iter().all(|r| r.len() == 32), "{revisions:?}");
    assert_ne!(revisions[0], revisions[1]);
}
