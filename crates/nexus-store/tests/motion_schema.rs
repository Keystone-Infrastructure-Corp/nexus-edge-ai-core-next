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

use chrono::{Duration, Timelike, Utc};
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
        ingest: nexus_config::CameraIngest {
            url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
            enabled: true,
            max_fps: 0,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: nexus_config::CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
        },
        zones: vec![],
    }
}

fn sample_clip(camera_id: i64, started: chrono::DateTime<Utc>) -> NewClip {
    NewClip {
        camera_id,
        started_at: started,
        hot_path: format!("cam{camera_id}/{}.mp4", started.timestamp()),
        codec: "h264".into(),
        container: "mp4".into(),
        hot_handle: "local".into(),
        frame_width: 960,
        frame_height: 540,
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
        "SELECT COUNT(*) FROM schema_migrations WHERE id IN ('0001_initial','0002_motion_clips','0003_events_clip_cascade','0004_storage_backends')",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(row.0, 4, "all four migrations should be recorded");

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
                hot_path: None,
                sha256: None,
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
async fn cascade_delete_drops_motion_events_and_alert_events() {
    // M2.1 closeout: events.clip_id was originally ON DELETE SET NULL
    // (migration 0002), then flipped to ON DELETE CASCADE in
    // migration 0003 to satisfy the spec invariant that a
    // cascade-delete leaves NO half-deleted state — including no
    // alert rows pointing at a missing clip.
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

    // A second alert NOT linked to any clip — sanity that CASCADE
    // doesn't sweep unrelated rows.
    let unlinked = AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id: 1,
        rule_id: "rule.other".into(),
        track_id: None,
        label: "bird".into(),
        severity: Severity::Low,
        bbox: None,
        frame_id: 200,
        captured_at: t0 + Duration::seconds(30),
        trace_id: "trace-2".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
    };
    store.record_event(&unlinked).await.unwrap();

    // Sanity: 3 motion events + 2 event rows (one with FK populated).
    let n_motion: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM motion_events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_motion.0, 3);
    let n_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_events.0, 2);
    let event_clip: (Option<i64>,) =
        sqlx::query_as("SELECT clip_id FROM events WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(event_clip.0, Some(clip_id));

    // The single-DELETE eviction.
    store.cascade_delete_clip_metadata(clip_id).await.unwrap();

    // motion_events: gone (FK ON DELETE CASCADE from 0002).
    let n_motion_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM motion_events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_motion_after.0, 0);

    // The linked alert row: GONE (FK ON DELETE CASCADE from 0003).
    let linked_after: Option<(String,)> =
        sqlx::query_as("SELECT event_id FROM events WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .fetch_optional(store.pool())
            .await
            .unwrap();
    assert!(
        linked_after.is_none(),
        "linked alert row should be cascade-deleted, found {linked_after:?}"
    );

    // The UNLINKED alert row: still there. CASCADE only eats rows
    // whose clip_id pointed at the evicted clip.
    let unlinked_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE event_id = ?")
        .bind(unlinked.event_id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(unlinked_after.0, 1);

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
async fn list_motion_events_across_cameras_filters_window_and_scope() {
    // Powers `POST /api/rules/preview` — must:
    //   * span multiple cameras when no filter is set,
    //   * apply IN(...) filter when the rule has a camera_filter,
    //   * order DESC by captured_at,
    //   * honour the limit cap.
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    store
        .upsert_camera(&sample_camera(2, "back"))
        .await
        .unwrap();
    let now = Utc::now();
    let clip_a = store
        .open_clip(&sample_clip(1, now - Duration::minutes(10)))
        .await
        .unwrap();
    let clip_b = store
        .open_clip(&sample_clip(2, now - Duration::minutes(10)))
        .await
        .unwrap();

    // 3 events on cam 1, 2 events on cam 2, all within window.
    for i in 0..3 {
        store
            .insert_motion_event(&sample_motion_event(
                1,
                clip_a,
                10 + i as u64,
                MotionEventKind::Born,
                now - Duration::seconds(i * 10),
            ))
            .await
            .unwrap();
    }
    for i in 0..2 {
        store
            .insert_motion_event(&sample_motion_event(
                2,
                clip_b,
                20 + i as u64,
                MotionEventKind::Born,
                now - Duration::seconds(i * 10),
            ))
            .await
            .unwrap();
    }
    // One stale event well outside the window — must NOT appear.
    store
        .insert_motion_event(&sample_motion_event(
            1,
            clip_a,
            99,
            MotionEventKind::Born,
            now - Duration::hours(48),
        ))
        .await
        .unwrap();

    let from = now - Duration::minutes(1);
    let to = now + Duration::seconds(1);

    // No filter ⇒ all 5 in-window rows.
    let all = store
        .list_motion_events_across_cameras(None, from, to, 100)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    // DESC by captured_at.
    for w in all.windows(2) {
        assert!(w[0].captured_at >= w[1].captured_at);
    }

    // Filter to camera 2 only ⇒ exactly 2 rows.
    let cam2 = store
        .list_motion_events_across_cameras(Some(&[2]), from, to, 100)
        .await
        .unwrap();
    assert_eq!(cam2.len(), 2);
    assert!(cam2.iter().all(|r| r.camera_id == 2));

    // Empty filter slice ⇒ treat as "all" (matches None semantics).
    let empty_scope = store
        .list_motion_events_across_cameras(Some(&[]), from, to, 100)
        .await
        .unwrap();
    assert_eq!(empty_scope.len(), 5);

    // Limit cap honoured — DESC order means we keep the newest.
    let limited = store
        .list_motion_events_across_cameras(None, from, to, 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].captured_at, all[0].captured_at);
}

#[tokio::test]
async fn list_motion_histogram_for_camera_buckets_by_seconds() {
    // M2.1 Stage B B7 — Timeline UI grid wants per-hour bars.
    // Verify the SQL bucket math: events scattered across a 3h
    // window with 3600s buckets land in buckets {0, 1, 2} with the
    // right per-bucket counts and distinct clip_count.
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    // Anchor `from` on a wall-clock minute boundary so SQLite's
    // strftime('%s', captured_at) - from_unix maps cleanly into
    // bucket indices instead of bobbing on sub-second drift.
    let from = Utc::now()
        .with_nanosecond(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        - Duration::hours(3);
    let to = from + Duration::hours(3);

    // Two clips so clip_count is observably > 1 in at least one
    // bucket.
    let clip_a = store
        .open_clip(&sample_clip(1, from + Duration::minutes(10)))
        .await
        .unwrap();
    let clip_b = store
        .open_clip(&sample_clip(1, from + Duration::minutes(70)))
        .await
        .unwrap();

    // Bucket 0 (0..60min): 3 events, 1 distinct clip.
    for i in 0..3 {
        store
            .insert_motion_event(&sample_motion_event(
                1,
                clip_a,
                100 + i as u64,
                MotionEventKind::Updated,
                from + Duration::minutes(5 + i * 10),
            ))
            .await
            .unwrap();
    }
    // Bucket 1 (60..120min): 2 events, 2 distinct clips.
    store
        .insert_motion_event(&sample_motion_event(
            1,
            clip_a,
            200,
            MotionEventKind::Updated,
            from + Duration::minutes(65),
        ))
        .await
        .unwrap();
    store
        .insert_motion_event(&sample_motion_event(
            1,
            clip_b,
            201,
            MotionEventKind::Born,
            from + Duration::minutes(75),
        ))
        .await
        .unwrap();
    // Bucket 2 (120..180min): NO events. Expect this bucket to be
    // absent from the response (sparse).

    let buckets = store
        .list_motion_histogram_for_camera(1, from, to, 3600)
        .await
        .unwrap();

    assert_eq!(buckets.len(), 2, "empty bucket 2 must be omitted");
    assert_eq!(buckets[0].bucket, 0);
    assert_eq!(buckets[0].event_count, 3);
    assert_eq!(buckets[0].clip_count, 1);
    assert_eq!(buckets[0].bucket_start, from);
    assert_eq!(buckets[1].bucket, 1);
    assert_eq!(buckets[1].event_count, 2);
    assert_eq!(buckets[1].clip_count, 2);
    assert_eq!(buckets[1].bucket_start, from + Duration::hours(1));
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
        vec![
            "0001_initial".to_string(),
            "0002_motion_clips".to_string(),
            "0003_events_clip_cascade".to_string(),
            "0004_storage_backends".to_string(),
            "0005_runtime_settings".to_string(),
            "0006_alert_sink_outbox".to_string(),
            "0007_delivery_settings".to_string(),
            "0008_rules_delivery_policy".to_string(),
            "0009_audit_log".to_string(),
            "0010_local_users".to_string(),
            "0011_auth_refresh_tokens".to_string(),
            "0012_visual_prompts".to_string(),
            "0013_cloud_enrollment".to_string(),
            "0014_storage_backends_azure_blob".to_string(),
            "0015_motion_clips_priority".to_string(),
            "0016_cloud_enrollment_attach_replay".to_string(),
            "0017_motion_clips_frame_size".to_string(),
            "0018_entity_local_state".to_string(),
            "0019_auth_refresh_idle".to_string(),
            "0020_cloud_enrollment_server_cert".to_string(),
        ]
    );
}

#[tokio::test]
async fn delete_runtime_setting_tx_removes_row_distinct_from_null_write() {
    // The LAN-settings UI relies on the three-state distinction
    // (no row vs. row-with-NULL vs. row-with-value) to know whether
    // to fall back to TOML or surface an operator-persisted "off".
    // If `delete_*` ever silently became a `write None`, the
    // `Reset` action in `admin_runtime::put_server_bind` would
    // mis-translate to `Clear` on the next GET, surprising operators
    // who expected "reset to defaults".
    let (store, _dir) = fresh_store().await;

    // Step 1 — write a value, confirm Some(Some(_)).
    store
        .write_runtime_setting("ui_bind", Some("0.0.0.0:80"))
        .await
        .unwrap();
    let after_set = store.read_runtime_setting("ui_bind").await.unwrap();
    assert_eq!(after_set, Some(Some("0.0.0.0:80".to_string())));

    // Step 2 — write None, confirm Some(None) (operator-cleared).
    store.write_runtime_setting("ui_bind", None).await.unwrap();
    let after_null = store.read_runtime_setting("ui_bind").await.unwrap();
    assert_eq!(after_null, Some(None));

    // Step 3 — delete, confirm None (no row, fall back to TOML).
    let mut tx = store.begin_tx().await.unwrap();
    store
        .delete_runtime_setting_tx(&mut tx, "ui_bind")
        .await
        .unwrap();
    Store::commit_tx(tx).await.unwrap();
    let after_delete = store.read_runtime_setting("ui_bind").await.unwrap();
    assert_eq!(after_delete, None);

    // Step 4 — deleting an already-absent key is idempotent.
    let mut tx = store.begin_tx().await.unwrap();
    store
        .delete_runtime_setting_tx(&mut tx, "ui_bind")
        .await
        .unwrap();
    Store::commit_tx(tx).await.unwrap();
    let still_absent = store.read_runtime_setting("ui_bind").await.unwrap();
    assert_eq!(still_absent, None);
}
