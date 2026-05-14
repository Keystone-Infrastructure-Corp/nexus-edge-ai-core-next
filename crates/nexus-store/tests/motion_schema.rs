//! M2.1 Stage A — schema + CRUD coverage for `motion_clips` and
//! `motion_events`. Exercises:
//!
//! * Both migrations apply cleanly on a fresh DB (`schema_migrations`
//!   shows `0001_initial` + `0002_motion_clips`).
//! * Re-opening the same DB is idempotent (no double-apply of
//!   `ALTER TABLE events ADD COLUMN clip_id`).
//! * `open_clip` / `close_clip` / `get_clip` round-trip.
//! * `oldest_clip_for_camera` returns the right row by `started_at`.
//! * `cameras_with_clips` enumerates camera ids deterministically.
//! * `cascade_delete_clip_metadata` deletes the clip AND every linked
//!   `motion_events` row in a single statement (FK cascade), and
//!   `events.clip_id` flips to NULL via `ON DELETE SET NULL`.
//! * `link_event_to_clip` cross-stamps the alert row.
//! * `clips_older_than` returns rows in `started_at ASC` order.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{ClipClose, EventStore, MotionEventKind, NewClip, NewMotionEvent, Store};
use nexus_types::{AlertEvent, Artifacts, BBox, Severity};
use sqlx::Row;
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;

/// Build an isolated `Store` rooted at a tmpdir. The DB file is dropped
/// when the `TempDir` is dropped at the end of the test.
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
        url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
        enabled: true,
        prompts: vec![],
        model_override: None,
        zones: vec![],
        max_fps: 0,
        parking_lot_mode: false,
    }
}

fn sample_clip(camera_id: i64, started: chrono::DateTime<Utc>) -> NewClip {
    NewClip {
        camera_id,
        started_at: started,
        path: format!("cam{camera_id}/{}.mp4", started.timestamp()),
        codec: "h264".into(),
        container: "mp4".into(),
        backend_id: "local".into(),
    }
}

fn sample_motion_event(
    camera_id: i64,
    clip_id: i64,
    track_id: u64,
    kind: MotionEventKind,
    captured_at: chrono::DateTime<Utc>,
) -> NewMotionEvent {
    NewMotionEvent {
        camera_id,
        clip_id,
        track_id,
        kind,
        captured_at,
        bbox: BBox {
            x1: 10.0,
            y1: 20.0,
            x2: 110.0,
            y2: 220.0,
        },
        label: "person".into(),
        confidence: 0.87,
        attributes_json: r#"{"motion.speed_class":"walking"}"#.into(),
    }
}

#[tokio::test]
async fn migrations_apply_and_are_idempotent() {
    let (store, dir) = fresh_store().await;

    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM schema_migrations WHERE id IN ('0001_initial','0002_motion_clips')",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(row.0, 2, "both migrations should be recorded");

    // Re-open the same DB. `ALTER TABLE events ADD COLUMN clip_id` would
    // fail on a second run; if it doesn't, the migration runner correctly
    // skipped the file based on `schema_migrations`.
    drop(store);
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    let _store2 = Store::open(&cfg)
        .await
        .expect("re-open should be idempotent");
}

#[tokio::test]
async fn motion_clips_round_trip_and_oldest_pick() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let t0 = Utc::now() - Duration::minutes(30);
    let t1 = t0 + Duration::minutes(10);
    let t2 = t1 + Duration::minutes(10);

    let id_old = store.open_clip(&sample_clip(1, t0)).await.unwrap();
    let id_mid = store.open_clip(&sample_clip(1, t1)).await.unwrap();
    let _id_new = store.open_clip(&sample_clip(1, t2)).await.unwrap();
    assert!(id_mid > id_old);

    // Close the oldest with synthetic stats.
    store
        .close_clip(
            id_old,
            &ClipClose {
                ended_at: t0 + Duration::seconds(15),
                duration_ms: 15_000,
                size_bytes: 1_234_567,
            },
        )
        .await
        .unwrap();

    let oldest = store.oldest_clip_for_camera(1).await.unwrap().unwrap();
    assert_eq!(oldest.id, id_old);
    assert_eq!(oldest.duration_ms, 15_000);
    assert_eq!(oldest.size_bytes, 1_234_567);
    assert!(oldest.ended_at.is_some());

    // get_clip on a deleted id returns None.
    let missing = store.get_clip(99_999).await.unwrap();
    assert!(missing.is_none());

    // cameras_with_clips enumerates exactly cam 1.
    let cams = store.cameras_with_clips().await.unwrap();
    assert_eq!(cams, vec![1]);
}

#[tokio::test]
async fn cascade_delete_drops_motion_events_and_nulls_event_clip() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let t0 = Utc::now() - Duration::minutes(5);
    let clip_id = store.open_clip(&sample_clip(1, t0)).await.unwrap();

    // Three motion events linked to the clip.
    for (i, kind) in [
        MotionEventKind::Born,
        MotionEventKind::Updated,
        MotionEventKind::Died,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .insert_motion_event(&sample_motion_event(
                1,
                clip_id,
                42,
                kind,
                t0 + Duration::seconds(i as i64 * 2),
            ))
            .await
            .unwrap();
    }

    // One alert that references the clip via `events.clip_id`.
    let alert = AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id: 1,
        rule_id: "rule.test".into(),
        track_id: Some(42),
        label: "person".into(),
        severity: Severity::Medium,
        bbox: None,
        frame_id: 100,
        captured_at: t0,
        trace_id: "trace-1".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
    };
    store.record_event(&alert).await.unwrap();
    store
        .link_event_to_clip(&alert.event_id.to_string(), clip_id)
        .await
        .unwrap();

    // Sanity: 3 motion events + 1 event row with the FK populated.
    let n_motion: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM motion_events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_motion.0, 3);
    let event_clip: (Option<i64>,) =
        sqlx::query_as("SELECT clip_id FROM events WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(event_clip.0, Some(clip_id));

    // The single-DELETE eviction.
    store.cascade_delete_clip_metadata(clip_id).await.unwrap();

    // motion_events: gone (FK ON DELETE CASCADE).
    let n_motion_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM motion_events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_motion_after.0, 0);

    // events row: still there, clip_id NULLed (FK ON DELETE SET NULL).
    let event_clip_after: (Option<i64>,) =
        sqlx::query_as("SELECT clip_id FROM events WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert!(event_clip_after.0.is_none());

    // Clip itself: gone.
    assert!(store.get_clip(clip_id).await.unwrap().is_none());

    // Foreign-key sanity: PRAGMA reports no orphans across the schema.
    let fk_check: Vec<(String,)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(
        fk_check.is_empty(),
        "PRAGMA foreign_key_check returned {fk_check:?}"
    );
}

#[tokio::test]
async fn list_motion_events_for_camera_window_and_order() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let now = Utc::now();
    let clip_id = store
        .open_clip(&sample_clip(1, now - Duration::minutes(5)))
        .await
        .unwrap();

    for i in 0..5 {
        store
            .insert_motion_event(&sample_motion_event(
                1,
                clip_id,
                100 + i as u64,
                MotionEventKind::Born,
                now - Duration::seconds(i * 30),
            ))
            .await
            .unwrap();
    }

    let rows = store
        .list_motion_events_for_camera(
            1,
            now - Duration::seconds(91),
            now + Duration::seconds(1),
            100,
        )
        .await
        .unwrap();
    // Only the 4 events within the last 91s (i=0..3 → 0/30/60/90s old).
    // The i=4 event is 120s old and falls outside the window.
    assert_eq!(rows.len(), 4);
    // ASC order by captured_at.
    for w in rows.windows(2) {
        assert!(w[0].captured_at <= w[1].captured_at);
    }
    // BBox + attributes_json round-trip cleanly.
    assert_eq!(rows[0].label, "person");
    assert!((rows[0].confidence - 0.87).abs() < 1e-5);
    assert_eq!(rows[0].bbox.x1, 10.0);
}

#[tokio::test]
async fn clips_older_than_returns_oldest_first() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let now = Utc::now();
    let _new = store
        .open_clip(&sample_clip(1, now - Duration::days(1)))
        .await
        .unwrap();
    let mid = store
        .open_clip(&sample_clip(1, now - Duration::days(40)))
        .await
        .unwrap();
    let old = store
        .open_clip(&sample_clip(1, now - Duration::days(60)))
        .await
        .unwrap();

    let rows = store
        .clips_older_than(now - Duration::days(30), 10)
        .await
        .unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    // `mid` (40d) and `old` (60d) qualify; `_new` (1d) does not.
    // Returned oldest-first → old before mid.
    assert_eq!(ids, vec![old, mid]);
}

#[tokio::test]
async fn motion_events_clip_id_is_not_null() {
    // Schema-level invariant: motion_events.clip_id NOT NULL. Trying to
    // insert NULL must fail at the DB layer, even if we route around the
    // typed Rust API.
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let res = sqlx::query(
        "INSERT INTO motion_events
             (camera_id, clip_id, track_id, kind, captured_at,
              bbox_x1, bbox_y1, bbox_x2, bbox_y2,
              label, confidence)
         VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(1_i64)
    .bind(7_i64)
    .bind("born")
    .bind(Utc::now().to_rfc3339())
    .bind(0.0_f64)
    .bind(0.0_f64)
    .bind(1.0_f64)
    .bind(1.0_f64)
    .bind("person")
    .bind(0.5_f64)
    .execute(store.pool())
    .await;
    let err = res.expect_err("inserting NULL clip_id should fail (NOT NULL constraint)");
    let msg = format!("{err}");
    assert!(
        msg.contains("NOT NULL") || msg.contains("constraint"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn schema_migrations_table_records_apply_order() {
    let (store, _dir) = fresh_store().await;
    let rows = sqlx::query("SELECT id FROM schema_migrations ORDER BY id")
        .fetch_all(store.pool())
        .await
        .unwrap();
    let ids: Vec<String> = rows.into_iter().map(|r| r.get::<String, _>(0)).collect();
    assert_eq!(
        ids,
        vec!["0001_initial".to_string(), "0002_motion_clips".to_string()]
    );
}
