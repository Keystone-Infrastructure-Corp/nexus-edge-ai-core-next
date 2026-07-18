//! M-Alert-Clip P2/P3 — `alert_clips` table CRUD, the
//! `events.alert_clip_id` link, and the P6 eviction gate.

use std::path::PathBuf;

use chrono::Utc;
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{NewAlertClip, Store};
use nexus_types::{AlertEvent, Artifacts, Severity};
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;

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
            ..Default::default()
        },
        onvif: Default::default(),
        talk_down: Default::default(),
        zones: vec![],
    }
}

fn sample_alert(camera_id: i64, rule: &str) -> AlertEvent {
    AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id,
        rule_id: rule.into(),
        track_id: Some(7),
        label: "person".into(),
        severity: Severity::High,
        bbox: None,
        frame_id: 1,
        captured_at: Utc::now(),
        trace_id: "trace-ac".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
        frame_w: 0,
        frame_h: 0,
    }
}

/// Full lifecycle: two burst-coalesced events share one alert clip; the
/// clip is only evictable once it's `ready` AND every sink of every
/// linked event has reached a terminal outbox state.
#[tokio::test]
async fn alert_clip_lifecycle_and_eviction_gate() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    // One burst clip, two alerts sharing it (burst coalescing).
    let clip_id = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/2026-07-17/123.mp4".into(),
        })
        .await
        .unwrap();

    let a1 = sample_alert(1, "r1");
    let a2 = sample_alert(1, "r2");
    let e1 = a1.event_id.to_string();
    let e2 = a2.event_id.to_string();
    store
        .record_event_and_enqueue(&a1, &["webhook:s1"])
        .await
        .unwrap();
    store
        .record_event_and_enqueue(&a2, &["webhook:s2"])
        .await
        .unwrap();
    store.link_event_alert_clip(&e1, clip_id).await.unwrap();
    store.link_event_alert_clip(&e2, clip_id).await.unwrap();

    // Either event resolves to the same clip; still building.
    let resolved = store
        .get_event_alert_clip(&e1)
        .await
        .unwrap()
        .expect("event linked to a clip");
    assert_eq!(resolved.id, clip_id);
    assert!(!resolved.is_ready(), "clip is still building");

    // A cutoff far in the FUTURE makes the cold-replication grace gate
    // non-blocking, so these assertions isolate the *delivery* gate.
    let grace_elapsed = Utc::now() + chrono::Duration::hours(1);
    // A cutoff in the PAST keeps the grace gate BLOCKING (the clip's
    // ready_at is ~now), so a ready-but-not-cold clip is held.
    let grace_blocking = Utc::now() - chrono::Duration::hours(1);

    // Two pending deliveries across the two events; not evictable.
    assert_eq!(
        store.alert_clip_pending_deliveries(clip_id).await.unwrap(),
        2
    );
    assert!(store
        .alert_clips_evictable(10, grace_elapsed)
        .await
        .unwrap()
        .is_empty());

    // Builder finishes → ready with real duration/size (sha256 None:
    // this test does not exercise cold replication).
    store
        .mark_alert_clip_ready(clip_id, 8_000, 1_234_567, None)
        .await
        .unwrap();
    let resolved = store.get_event_alert_clip(&e2).await.unwrap().unwrap();
    assert!(resolved.is_ready());
    assert_eq!(resolved.duration_ms, 8_000);
    assert_eq!(resolved.size_bytes, 1_234_567);
    assert!(resolved.ready_at.is_some());
    // Ready, but deliveries still pending → still NOT evictable.
    assert!(store
        .alert_clips_evictable(10, grace_elapsed)
        .await
        .unwrap()
        .is_empty());

    // Deliver every sink of both events.
    for row in store.outbox_pending(100).await.unwrap() {
        store.outbox_mark_sent(row.id).await.unwrap();
    }
    assert_eq!(
        store.alert_clip_pending_deliveries(clip_id).await.unwrap(),
        0
    );

    // Delivered, but NOT cold-replicated and within the grace window →
    // still NOT evictable (M-Alert-Clip cloud-delivery gate).
    assert!(store
        .alert_clips_evictable(10, grace_blocking)
        .await
        .unwrap()
        .is_empty());

    // Past the grace window → evictable; eviction flips state to
    // 'evicted' (row kept for audit).
    let evictable = store
        .alert_clips_evictable(10, grace_elapsed)
        .await
        .unwrap();
    assert_eq!(evictable.len(), 1);
    assert_eq!(evictable[0].id, clip_id);
    store.mark_alert_clip_evicted(clip_id).await.unwrap();
    assert!(store
        .alert_clips_evictable(10, grace_elapsed)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.get_alert_clip(clip_id).await.unwrap().unwrap().state,
        "evicted"
    );
}

/// Reclaiming an alert clip (ON DELETE SET NULL) must NOT delete the
/// durable alert event — the opposite of the motion-clip cascade.
#[tokio::test]
async fn deleting_alert_clip_nulls_link_but_keeps_event() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let clip_id = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/2026-07-17/1.mp4".into(),
        })
        .await
        .unwrap();
    let a = sample_alert(1, "r1");
    let eid = a.event_id.to_string();
    store.record_event_and_enqueue(&a, &[]).await.unwrap();
    store.link_event_alert_clip(&eid, clip_id).await.unwrap();

    // Hard-delete the alert_clips row (simulates a future retention
    // sweep). The FK is ON DELETE SET NULL.
    sqlx::query("DELETE FROM alert_clips WHERE id = ?")
        .bind(clip_id)
        .execute(store.pool())
        .await
        .unwrap();

    // Event survives; its link is nulled; resolving the clip yields None.
    assert!(store.get_event_alert_clip(&eid).await.unwrap().is_none());
    let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE event_id = ?")
        .bind(&eid)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(still_there, 1, "the durable alert event must survive");
}

/// M-Alert-Clip cloud delivery — the cold-replication working set,
/// pointer stamping, retry gate, and how cold-replication interacts
/// with the eviction grace window.
#[tokio::test]
async fn alert_clip_cold_replication_lifecycle() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();
    let sha = "b".repeat(64);
    let clip_id = store
        .insert_alert_clip(&NewAlertClip {
            camera_id: 1,
            started_at: Utc::now(),
            path: "alert/1/2026-07-17/9.mp4".into(),
        })
        .await
        .unwrap();

    let future_retry = Utc::now() + chrono::Duration::hours(1);

    // 'building' → not in the cold working set (only 'ready' rows are).
    assert!(store
        .alert_clips_pending_cold_upload(10, None, future_retry)
        .await
        .unwrap()
        .is_empty());

    // Ready but sha256 = None → still excluded (cloud envelope needs it).
    store
        .mark_alert_clip_ready(clip_id, 4_000, 100_000, None)
        .await
        .unwrap();
    assert!(store
        .alert_clips_pending_cold_upload(10, None, future_retry)
        .await
        .unwrap()
        .is_empty());

    // Re-stamp with a sha256 (simulates the builder hashing the MP4).
    // mark_alert_clip_ready only transitions 'building', so set it
    // directly to model a hashed ready row.
    sqlx::query("UPDATE alert_clips SET sha256 = ? WHERE id = ?")
        .bind(&sha)
        .bind(clip_id)
        .execute(store.pool())
        .await
        .unwrap();

    // Now eligible: ready + hashed + not cold.
    let pending = store
        .alert_clips_pending_cold_upload(10, None, future_retry)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, clip_id);
    assert_eq!(pending[0].sha256.as_deref(), Some(sha.as_str()));

    // A pre-enrollment floor AFTER the clip's started_at excludes it.
    let floor_future = Utc::now() + chrono::Duration::hours(2);
    assert!(store
        .alert_clips_pending_cold_upload(10, Some(floor_future), future_retry)
        .await
        .unwrap()
        .is_empty());

    // Record a failure → the retry gate holds it out until the cutoff
    // moves past the attempt time.
    store
        .record_alert_clip_cold_failure(clip_id, Utc::now(), "backend 503")
        .await
        .unwrap();
    let past_retry = Utc::now() - chrono::Duration::hours(1);
    assert!(
        store
            .alert_clips_pending_cold_upload(10, None, past_retry)
            .await
            .unwrap()
            .is_empty(),
        "a just-failed clip is gated out until retry_after elapses"
    );
    assert_eq!(
        store
            .alert_clips_pending_cold_upload(10, None, future_retry)
            .await
            .unwrap()
            .len(),
        1,
        "past the retry gate it is eligible again"
    );
    assert_eq!(
        store
            .get_alert_clip(clip_id)
            .await
            .unwrap()
            .unwrap()
            .cold_attempts,
        1
    );

    // A ready clip that is NOT cold-replicated is held from eviction
    // while inside the grace window (cutoff in the PAST).
    let grace_blocking = Utc::now() - chrono::Duration::hours(1);
    assert!(store
        .alert_clips_evictable(10, grace_blocking)
        .await
        .unwrap()
        .is_empty());

    // Stamp the cold pointer → out of the pending set AND immediately
    // evictable even inside the grace window (the cloud has it now).
    store
        .upsert_storage_backend("azure", "azure_blob", "{}")
        .await
        .unwrap();
    store
        .mark_alert_clip_cold_replicated(
            clip_id,
            &nexus_store::AlertClipColdMark {
                cold_handle: "azure".into(),
                cold_path: "org/core/alert-9.mp4".into(),
                cold_uploaded_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    assert!(store
        .alert_clips_pending_cold_upload(10, None, future_retry)
        .await
        .unwrap()
        .is_empty());
    let evictable = store
        .alert_clips_evictable(10, grace_blocking)
        .await
        .unwrap();
    assert_eq!(evictable.len(), 1, "cold-replicated → evictable pre-grace");
    assert_eq!(evictable[0].id, clip_id);
    assert!(evictable[0].cold_uploaded_at.is_some());
}
