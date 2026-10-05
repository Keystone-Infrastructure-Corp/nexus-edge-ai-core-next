//! #759 — durable `clip_replicated` announce state on motion and alert
//! clips: a cold-uploaded clip stays in the re-announce set until the
//! cloud acks it (stored or permanently rejected).

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{AlertClipColdMark, ClipClose, ClipColdMark, NewAlertClip, NewClip, Store};
use tempfile::TempDir;
use url::Url;

async fn fresh_store() -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");
    let store = Store::open(&StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db_path.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    })
    .await
    .expect("Store::open");
    store
        .upsert_camera(&CameraConfig {
            id: 1,
            name: "cam1".into(),
            ingest: nexus_config::CameraIngest {
                url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                analysis_url: None,
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
        })
        .await
        .unwrap();
    store
        .upsert_storage_backend("azure", "azure_blob", "{}")
        .await
        .unwrap();
    (store, dir)
}

/// A closed motion clip, cold-uploaded an hour ago.
async fn cold_motion_clip(store: &Store, n: i64) -> i64 {
    let now = Utc::now();
    let id = store
        .open_clip(&NewClip {
            camera_id: 1,
            started_at: now - Duration::hours(2),
            hot_path: format!("1/clip_{n}.mp4"),
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
                ended_at: now - Duration::hours(2) + Duration::seconds(10),
                duration_ms: 10_000,
                size_bytes: 10,
                hot_path: None,
                sha256: Some(format!("{id:064x}")),
            },
        )
        .await
        .unwrap();
    store
        .mark_cold_replicated(
            id,
            &ClipColdMark {
                cold_handle: "azure".into(),
                cold_path: format!("1/{id}.mp4"),
                cold_uploaded_at: now - Duration::hours(1),
            },
        )
        .await
        .unwrap();
    id
}

fn pending_ids(rows: &[(nexus_store::ClipRow, String, i64)]) -> Vec<i64> {
    rows.iter().map(|(c, _, _)| c.id).collect()
}

#[tokio::test]
async fn unacked_motion_clip_is_pending_until_the_cloud_acks_it() {
    let (store, _tmp) = fresh_store().await;
    let now = Utc::now();

    // Cold-uploaded with no blob URL (LAN backend / pre-#759 row):
    // nothing to announce.
    let lan = cold_motion_clip(&store, 1).await;
    let stored = cold_motion_clip(&store, 2).await;
    let rejected = cold_motion_clip(&store, 3).await;
    let unacked = cold_motion_clip(&store, 4).await;
    let due = now - Duration::minutes(1);
    for (id, msg) in [(stored, "m-2"), (rejected, "m-3"), (unacked, "m-4")] {
        store
            .stamp_clip_announce(id, &format!("https://blob.test/{id}.mp4"), msg)
            .await
            .unwrap();
        store.record_clip_announce_sent(id, due).await.unwrap();
    }

    let pending = store.clips_pending_cloud_announce(10, now).await.unwrap();
    assert_eq!(pending_ids(&pending), vec![stored, rejected, unacked]);
    assert!(!pending_ids(&pending).contains(&lan));
    assert_eq!(pending[0].1, format!("https://blob.test/{stored}.mp4"));
    assert_eq!(pending[0].2, 1, "the stamp counts the send");

    assert_eq!(
        store
            .record_clip_announce_ack("m-2", now, None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .record_clip_announce_ack("m-3", now, Some("unknown camera"))
            .await
            .unwrap(),
        1
    );
    // An ack for an id that was superseded by a re-send matches nothing,
    // and a repeated ack does not overwrite the first.
    for id in ["stale", "m-3"] {
        assert_eq!(
            store.record_clip_announce_ack(id, now, None).await.unwrap(),
            0
        );
    }

    let pending = store.clips_pending_cloud_announce(10, now).await.unwrap();
    assert_eq!(
        pending_ids(&pending),
        vec![unacked],
        "a stored or permanently-rejected clip is never re-announced"
    );

    // A clip is held back until its next attempt is due.
    assert!(store
        .clips_pending_cloud_announce(10, due - Duration::seconds(1))
        .await
        .unwrap()
        .is_empty());
    store
        .stamp_clip_announce(unacked, "https://blob.test/u.mp4", "m-5")
        .await
        .unwrap();
    store
        .record_clip_announce_sent(unacked, now + Duration::hours(1))
        .await
        .unwrap();
    assert!(store
        .clips_pending_cloud_announce(10, now)
        .await
        .unwrap()
        .is_empty());
    let pending = store
        .clips_pending_cloud_announce(10, now + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(pending_ids(&pending), vec![unacked]);
    assert_eq!(pending[0].2, 2);
}

/// #759 re-review fix — `stamp_clip_announce` (called before every send,
/// so the ack can match the id) must not by itself count as an attempt
/// or start the backoff: only [`Store::record_clip_announce_sent`],
/// called after a send actually succeeds, does. A send that fails
/// (tunnel down) therefore leaves the clip due immediately instead of
/// burning an attempt and doubling the backoff for nothing.
#[tokio::test]
async fn stamping_alone_does_not_burn_an_attempt_recording_a_sent_does() {
    let (store, _tmp) = fresh_store().await;
    let now = Utc::now();
    let id = cold_motion_clip(&store, 1).await;

    store
        .stamp_clip_announce(id, "https://blob.test/1.mp4", "env-1")
        .await
        .unwrap();
    let pending = store.clips_pending_cloud_announce(10, now).await.unwrap();
    assert_eq!(pending.len(), 1, "stamping makes the clip pending");
    assert_eq!(
        pending[0].2, 0,
        "stamping the envelope id alone must not burn an attempt"
    );

    let next_at = now + Duration::minutes(5);
    store.record_clip_announce_sent(id, next_at).await.unwrap();
    assert!(
        store
            .clips_pending_cloud_announce(10, now)
            .await
            .unwrap()
            .is_empty(),
        "a recorded send holds the clip back until next_at"
    );
    let pending = store
        .clips_pending_cloud_announce(10, next_at)
        .await
        .unwrap();
    assert_eq!(
        pending[0].2, 1,
        "the successful send counted as exactly one attempt"
    );
}

#[tokio::test]
async fn unacked_alert_clip_is_pending_until_the_cloud_acks_it() {
    let (store, _tmp) = fresh_store().await;
    let now = Utc::now();

    let mut ids = Vec::new();
    for n in 0..2 {
        let id = store
            .insert_alert_clip(&NewAlertClip {
                camera_id: 1,
                started_at: now - Duration::hours(2),
                path: format!("alert/1/{n}.mp4"),
            })
            .await
            .unwrap();
        store
            .mark_alert_clip_ready(id, 5_000, 10, Some(&format!("{id:064x}")))
            .await
            .unwrap();
        store
            .mark_alert_clip_cold_replicated(
                id,
                &AlertClipColdMark {
                    cold_handle: "azure".into(),
                    cold_path: format!("alert/1/alert-{id}.mp4"),
                    cold_uploaded_at: now - Duration::hours(1),
                },
            )
            .await
            .unwrap();
        store
            .stamp_alert_clip_announce(id, "https://blob.test/a.mp4", &format!("a-{id}"))
            .await
            .unwrap();
        store
            .record_alert_clip_announce_sent(id, now)
            .await
            .unwrap();
        ids.push(id);
    }

    let pending = store
        .alert_clips_pending_cloud_announce(10, now)
        .await
        .unwrap();
    assert_eq!(
        pending.iter().map(|(c, _, _)| c.id).collect::<Vec<_>>(),
        ids
    );
    assert_eq!(pending[0].1, "https://blob.test/a.mp4");

    // The shared ack handler reaches alert clips too.
    store
        .record_clip_announce_ack(&format!("a-{}", ids[0]), now, Some("malformed payload"))
        .await
        .unwrap();
    let pending = store
        .alert_clips_pending_cloud_announce(10, now)
        .await
        .unwrap();
    assert_eq!(
        pending.iter().map(|(c, _, _)| c.id).collect::<Vec<_>>(),
        vec![ids[1]]
    );
}
