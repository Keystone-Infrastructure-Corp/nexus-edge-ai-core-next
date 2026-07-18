//! Delivery-settings store round-trip, focused on the M-Alert-Clip
//! `attach_alert_clip` toggle (migration 0028): it defaults ON for a
//! fresh install and round-trips through put/get without disturbing the
//! other delivery fields.

use std::path::PathBuf;

use nexus_config::StoreConfig;
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

#[tokio::test]
async fn attach_alert_clip_defaults_on_and_round_trips() {
    let (store, _tmp) = fresh_store().await;

    // Fresh install: the singleton row seeds attach_alert_clip = true
    // (migration 0028 `DEFAULT 1`), so alert clips are on by default.
    let ds = store.delivery_settings_get().await.unwrap();
    assert!(ds.attach_alert_clip, "alert clips must default ON");

    // Disable it → persists; unrelated fields untouched.
    let mut off = ds.clone();
    off.attach_alert_clip = false;
    store.delivery_settings_put(&off).await.unwrap();
    let ds_off = store.delivery_settings_get().await.unwrap();
    assert!(!ds_off.attach_alert_clip, "disable must persist");
    assert_eq!(ds_off.enabled, ds.enabled);
    assert_eq!(ds_off.timezone, ds.timezone);

    // Re-enable → persists.
    let mut on = ds_off.clone();
    on.attach_alert_clip = true;
    store.delivery_settings_put(&on).await.unwrap();
    assert!(
        store
            .delivery_settings_get()
            .await
            .unwrap()
            .attach_alert_clip,
        "re-enable must persist"
    );
}
