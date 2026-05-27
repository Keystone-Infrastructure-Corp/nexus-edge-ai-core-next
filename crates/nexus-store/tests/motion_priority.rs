//! Phase 2 Step 2.1c — `motion_clips.priority` ordering for the cold
//! replicator + helper coverage for the Expedite RPC handler.
//!
//! Migration 0015 adds an `INTEGER NOT NULL DEFAULT 0` column and
//! changes `clips_pending_cold_upload` to `ORDER BY priority DESC,
//! ended_at ASC` so the cloud's `POST /admin/clips/.../replicate`
//! can jump a single clip to the head of the queue without
//! disrupting the rest of the backfill.
//!
//! Exercises:
//!
//! * Two pending clips inserted oldest-first → ordering matches FIFO.
//! * After `bump_clip_priority` on the newer one → it leads.
//! * `bump_clip_priority` is idempotent (second call on the already-
//!   bumped row returns `Ok(false)`; nonexistent id also returns
//!   `Ok(false)`).
//! * `pending_cold_upload_position` reports the right 1-indexed slot
//!   and `None` for ids that are not pending.

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
        },
        detector: CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
        },
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
async fn pending_orders_by_priority_then_ended_at() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let t0 = Utc::now() - Duration::minutes(30);
    let t1 = t0 + Duration::minutes(10);

    let id_old = insert_pending_clip(&store, 1, t0).await;
    let id_new = insert_pending_clip(&store, 1, t1).await;

    // Default priority = 0 → oldest-first.
    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].id, id_old);
    assert_eq!(pending[1].id, id_new);
    assert_eq!(pending[0].priority, 0);

    // Expedite the newer one.
    let bumped = store.bump_clip_priority(id_new, 1).await.unwrap();
    assert!(bumped, "first bump must report a change");

    // Now the newer clip leads despite older ended_at on the other.
    let pending = store.clips_pending_cold_upload(10).await.unwrap();
    assert_eq!(pending[0].id, id_new);
    assert_eq!(pending[0].priority, 1);
    assert_eq!(pending[1].id, id_old);
}

#[tokio::test]
async fn bump_clip_priority_is_idempotent_and_no_op_on_missing() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let t0 = Utc::now() - Duration::minutes(5);
    let id = insert_pending_clip(&store, 1, t0).await;

    assert!(store.bump_clip_priority(id, 1).await.unwrap());
    // Same level → no row updated → false.
    assert!(!store.bump_clip_priority(id, 1).await.unwrap());
    // Lower level → strict-less filter → false.
    assert!(!store.bump_clip_priority(id, 0).await.unwrap());
    // Non-existent id → false (no panic).
    assert!(!store.bump_clip_priority(999_999, 1).await.unwrap());
}

#[tokio::test]
async fn pending_position_reflects_priority_ordering() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let t0 = Utc::now() - Duration::minutes(30);
    let t1 = t0 + Duration::minutes(10);
    let t2 = t1 + Duration::minutes(10);

    let id_a = insert_pending_clip(&store, 1, t0).await;
    let id_b = insert_pending_clip(&store, 1, t1).await;
    let id_c = insert_pending_clip(&store, 1, t2).await;

    // FIFO baseline: a, b, c at positions 1, 2, 3.
    assert_eq!(
        store.pending_cold_upload_position(id_a).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        store.pending_cold_upload_position(id_b).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        store.pending_cold_upload_position(id_c).await.unwrap(),
        Some(3)
    );

    // Expedite c → c=1, a=2, b=3.
    store.bump_clip_priority(id_c, 1).await.unwrap();
    assert_eq!(
        store.pending_cold_upload_position(id_c).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        store.pending_cold_upload_position(id_a).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        store.pending_cold_upload_position(id_b).await.unwrap(),
        Some(3)
    );

    // Unknown id → None.
    assert_eq!(
        store.pending_cold_upload_position(999_999).await.unwrap(),
        None
    );
}
