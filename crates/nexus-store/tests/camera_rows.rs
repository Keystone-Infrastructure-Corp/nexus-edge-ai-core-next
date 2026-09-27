//! `Store::list_readable_cameras` returns the camera rows this build can read
//! and the ids of those it cannot. Boot and the reconciler read with it, so a
//! row it cannot read must be reported, never a panic that takes them down;
//! `list_cameras`, which needs every row, must fail on it, not panic.

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
