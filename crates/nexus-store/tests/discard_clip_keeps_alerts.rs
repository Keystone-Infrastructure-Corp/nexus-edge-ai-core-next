//! A clip the recorder fails (empty, or under the minimum duration) is
//! discarded with `Store::discard_clip_metadata`. The alerts that fired
//! while it was open are linked to it (`events.clip_id`, CASCADE since
//! 0003) and must survive the discard together with their undelivered
//! outbox rows; only the clip row goes.

use std::path::PathBuf;

use chrono::Utc;
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{NewClip, OutboxStatus, Store};
use nexus_types::{AlertEvent, Artifacts, Severity};
use url::Url;
use uuid::Uuid;

#[tokio::test]
async fn discarding_an_empty_clip_keeps_its_alerts_and_their_outbox_rows() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let store = Store::open(&StoreConfig {
        url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
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
            detector: Default::default(),
            behavior: Default::default(),
            onvif: Default::default(),
            talk_down: Default::default(),
            zones: vec![],
        })
        .await
        .unwrap();

    let clip_id = store
        .open_clip(&NewClip {
            camera_id: 1,
            started_at: Utc::now(),
            hot_path: "1/empty.partial.mp4".into(),
            codec: "h264".into(),
            container: "mp4".into(),
            hot_handle: "local".into(),
            frame_width: 960,
            frame_height: 540,
        })
        .await
        .unwrap();
    let alert = AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id: 1,
        rule_id: "r1".into(),
        track_id: Some(7),
        label: "person".into(),
        severity: Severity::High,
        bbox: None,
        frame_id: 1,
        captured_at: Utc::now(),
        trace_id: "trace".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
        frame_w: 0,
        frame_h: 0,
    };
    let event_id = alert.event_id.to_string();
    store
        .record_event_and_enqueue(&alert, &["webhook:slack"])
        .await
        .unwrap();
    store.link_event_to_clip(&event_id, clip_id).await.unwrap();

    store.discard_clip_metadata(clip_id).await.unwrap();

    assert!(
        store.get_clip(clip_id).await.unwrap().is_none(),
        "the failed clip must be gone so it is never replicated"
    );
    assert!(
        store.get_event(&event_id).await.unwrap().is_some(),
        "the alert must survive its empty clip"
    );
    assert_eq!(store.get_event_clip_id(&event_id).await.unwrap(), None);
    let outbox = store.outbox_for_event(&event_id).await.unwrap();
    assert_eq!(outbox.len(), 1, "the undelivered outbox row must survive");
    assert_eq!(outbox[0].status, OutboxStatus::Pending);
}
