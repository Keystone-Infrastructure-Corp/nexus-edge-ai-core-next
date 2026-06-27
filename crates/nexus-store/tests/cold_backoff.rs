//! Migration 0023 — cold-replicator backoff + quarantine gate on
//! `clips_pending_cold_upload` (and the `cold_replica_stats`
//! pending count).
//!
//! Exercises:
//!
//! * A freshly-closed pending clip is eligible by default
//!   (`cold_attempts == 0`, not quarantined).
//! * `record_cold_upload_failure` with a FUTURE `cold_next_attempt_at`
//!   gates the clip out of the pending working set; a PAST value
//!   re-admits it; `cold_attempts` increments on every call.
//! * `record_cold_upload_failure(.., quarantined = true)` and
//!   `quarantine_clip` both drop the clip from the pending set AND
//!   from `cold_replica_stats().pending_count` permanently.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::{CameraBehavior, CameraConfig, CameraDetector, CameraIngest, StoreConfig};
use nexus_store::{ClipClose, NewClip, Store};
use tempfile::TempDir;
use url::Url;

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

fn sample_camera(id: i64, name: &str) -> CameraConfig {
    CameraConfig {
        id,
        name: name.into(),
        ingest: CameraIngest {
            url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
            enabled: true,
            max_fps: 0,
            codec: None,
        },
        detector: CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    }
}

/// Open a clip at `started`, immediately close it as
/// pending-cold-eligible (`sha256` set, `cold_handle` still NULL).
async fn insert_pending_clip(store: &Store, camera_id: i64, started: chrono::DateTime<Utc>) -> i64 {
    let id = store
        .open_clip(&NewClip {
            camera_id,
            started_at: started,
            hot_path: format!("cam{camera_id}/{}.mp4", started.timestamp()),
            codec: "h264".into(),
            container: "mp4".into(),
            hot_handle: "local".into(),
            frame_width: 960,
            frame_height: 540,
        })
        .await
        .unwrap();
    store
        .close_clip(
            id,
            &ClipClose {
                ended_at: started + Duration::seconds(15),
                duration_ms: 15_000,
                size_bytes: 1_000_000,
                hot_path: None,
                sha256: Some("a".repeat(64)),
            },
        )
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn backoff_gate_excludes_then_readmits_pending_clip() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let id = insert_pending_clip(&store, 1, Utc::now() - Duration::minutes(5)).await;

    // Eligible by default, no attempts yet.
    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert!(pending.iter().any(|c| c.id == id));
    let row = pending.iter().find(|c| c.id == id).unwrap();
    assert_eq!(row.cold_attempts, 0);
    assert!(!row.cold_quarantined);

    // Fail with a FUTURE next-attempt → gated out; attempts -> 1.
    let now = Utc::now();
    store
        .record_cold_upload_failure(id, now, "boom", Some(now + Duration::hours(1)), false)
        .await
        .unwrap();
    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert!(
        !pending.iter().any(|c| c.id == id),
        "backoff-gated clip must be excluded from the pending set"
    );

    // The row still carries the bookkeeping.
    let row = store.get_clip(id).await.unwrap().unwrap();
    assert_eq!(row.cold_attempts, 1);
    assert_eq!(row.cold_last_error.as_deref(), Some("boom"));
    assert!(row.cold_next_attempt_at.is_some());
    assert!(row.cold_last_attempt_at.is_some());
    assert!(!row.cold_quarantined);

    // Backoff elapsed (PAST next-attempt) → eligible again; attempts -> 2.
    let now2 = Utc::now();
    store
        .record_cold_upload_failure(id, now2, "boom2", Some(now2 - Duration::hours(1)), false)
        .await
        .unwrap();
    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert!(
        pending.iter().any(|c| c.id == id),
        "clip must be eligible again once its backoff window has elapsed"
    );
    let row = store.get_clip(id).await.unwrap().unwrap();
    assert_eq!(row.cold_attempts, 2);
    assert_eq!(row.cold_last_error.as_deref(), Some("boom2"));
}

#[tokio::test]
async fn quarantine_removes_clip_from_pending_and_stats() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let id = insert_pending_clip(&store, 1, Utc::now() - Duration::minutes(5)).await;

    // Sanity: exactly one pending clip before quarantine.
    assert_eq!(
        store.cold_replica_stats().await.unwrap().pending_count,
        1,
        "freshly-closed clip should count as pending"
    );

    store
        .quarantine_clip(id, Utc::now(), "too big")
        .await
        .unwrap();

    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert!(
        pending.is_empty(),
        "quarantined clip must be excluded from the pending set"
    );
    assert_eq!(
        store.cold_replica_stats().await.unwrap().pending_count,
        0,
        "quarantined clip must not count toward the pending backlog"
    );

    let row = store.get_clip(id).await.unwrap().unwrap();
    assert!(row.cold_quarantined);
    assert_eq!(row.cold_last_error.as_deref(), Some("too big"));
    // quarantine_clip does NOT consume an attempt.
    assert_eq!(row.cold_attempts, 0);
}

#[tokio::test]
async fn record_failure_with_quarantine_flag_excludes_clip() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let id = insert_pending_clip(&store, 1, Utc::now() - Duration::minutes(5)).await;

    let now = Utc::now();
    store
        .record_cold_upload_failure(id, now, "max attempts reached", None, true)
        .await
        .unwrap();

    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert!(pending.is_empty(), "quarantined clip must be excluded");

    let row = store.get_clip(id).await.unwrap().unwrap();
    assert!(row.cold_quarantined);
    assert_eq!(row.cold_attempts, 1);
    assert!(
        row.cold_next_attempt_at.is_none(),
        "a quarantined clip has no scheduled next attempt"
    );
}
