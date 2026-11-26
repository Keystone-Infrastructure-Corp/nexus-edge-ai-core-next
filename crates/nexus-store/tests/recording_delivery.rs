//! SPEC-090 — authoritative recording delivery accounting.
//!
//! * Queue partitions are mutually exclusive, cover every unreplicated
//!   finalized row, and the `ready` partition is exactly what the
//!   uploaders select at the same clock and enrollment floor.
//! * Minute buckets count generation at finalization and delivery at the
//!   durable completion, once each, across duplicates, retries and
//!   restarts; eviction before upload is an explicit loss total.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{
    AlertClipColdMark, ClipClose, ClipColdMark, DeliveryCounters, DeliveryWindow, NewAlertClip,
    NewClip, QueuePartition, RecordingDeliverySnapshot, RecordingSource, Store,
};
use tempfile::TempDir;
use url::Url;

async fn open_store(dir: &Path) -> Store {
    Store::open(&StoreConfig {
        url: format!("sqlite:{}?mode=rwc", dir.join("nexus.db").display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    })
    .await
    .expect("Store::open")
}

async fn fresh_store() -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let store = open_store(dir.path()).await;
    for id in [1, 2] {
        store.upsert_camera(&sample_camera(id)).await.unwrap();
    }
    (store, dir)
}

fn sample_camera(id: i64) -> CameraConfig {
    CameraConfig {
        id,
        name: format!("cam{id}"),
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
    }
}

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 24, h, m, s).unwrap()
}

async fn open_clip(store: &Store, camera_id: i64, started: DateTime<Utc>) -> i64 {
    store
        .open_clip(&NewClip {
            camera_id,
            started_at: started,
            hot_path: format!("cam{camera_id}/{}.mp4", started.timestamp_millis()),
            codec: "h264".into(),
            container: "mp4".into(),
            hot_handle: "local".into(),
            frame_width: 960,
            frame_height: 540,
        })
        .await
        .unwrap()
}

/// Open and finalize a clip; `sha` false leaves it unhashed.
async fn closed_clip(
    store: &Store,
    camera_id: i64,
    started: DateTime<Utc>,
    size_bytes: i64,
    sha: bool,
) -> i64 {
    let id = open_clip(store, camera_id, started).await;
    store
        .close_clip_at(
            id,
            &close(started, size_bytes, sha),
            started + Duration::seconds(30),
        )
        .await
        .unwrap();
    id
}

fn close(started: DateTime<Utc>, size_bytes: i64, sha: bool) -> ClipClose {
    ClipClose {
        ended_at: started + Duration::seconds(30),
        duration_ms: 30_000,
        size_bytes,
        hot_path: None,
        sha256: sha.then(|| "a".repeat(64)),
    }
}

fn cold_mark(path: &str, when: DateTime<Utc>) -> ClipColdMark {
    ClipColdMark {
        cold_handle: "cold".into(),
        cold_path: path.into(),
        cold_uploaded_at: when,
    }
}

async fn alert_clip(store: &Store, camera_id: i64, started: DateTime<Utc>) -> i64 {
    store
        .insert_alert_clip(&NewAlertClip {
            camera_id,
            started_at: started,
            path: format!("alert/{camera_id}/{}.mp4", started.timestamp_millis()),
        })
        .await
        .unwrap()
}

fn partitions(rows: Vec<(i64, QueuePartition)>) -> BTreeMap<i64, QueuePartition> {
    let n = rows.len();
    let map: BTreeMap<_, _> = rows.into_iter().collect();
    assert_eq!(map.len(), n, "a row appeared in two partitions");
    map
}

fn ids_in(map: &BTreeMap<i64, QueuePartition>, p: QueuePartition) -> BTreeSet<i64> {
    map.iter()
        .filter(|(_, v)| **v == p)
        .map(|(k, _)| *k)
        .collect()
}

/// D-1: one row per partition, two priorities, an unknown size and an
/// open file. Ready is exactly the uploader's selection at the same
/// fixed clock and floor, for both the ordinary and the alert queue.
#[tokio::test]
async fn recording_delivery_partitions_match_uploader_selection() {
    let (store, _dir) = fresh_store().await;
    let now = at(12, 0, 0);
    let floor = at(8, 0, 0);
    store
        .upsert_storage_backend("cold", "lan", "{\"root\":\"/tmp/c\"}")
        .await
        .unwrap();

    // Ordinary queue.
    let quarantined = closed_clip(&store, 1, at(9, 0, 0), 100, true).await;
    store
        .quarantine_clip(quarantined, now, "too big")
        .await
        .unwrap();
    // Quarantine wins over the floor: partitions are evaluated in order.
    let quarantined_old = closed_clip(&store, 1, at(7, 0, 0), 100, true).await;
    store
        .quarantine_clip(quarantined_old, now, "too big")
        .await
        .unwrap();
    let before_floor = closed_clip(&store, 1, at(7, 30, 0), 200, false).await;
    let missing_media = closed_clip(&store, 1, at(9, 10, 0), 300, false).await;
    let backoff = closed_clip(&store, 1, at(9, 20, 0), 400, true).await;
    store
        .record_cold_upload_failure(backoff, now, "boom", Some(now + Duration::hours(1)), false)
        .await
        .unwrap();
    let ready_low = closed_clip(&store, 1, at(9, 30, 0), 1_000, true).await;
    let ready_unknown = closed_clip(&store, 2, at(9, 40, 0), 0, true).await;
    assert!(store.bump_clip_priority(ready_unknown, 1).await.unwrap());
    let ready_elapsed = closed_clip(&store, 2, at(9, 50, 0), 2_000, true).await;
    store
        .record_cold_upload_failure(ready_elapsed, now, "boom", Some(now), false)
        .await
        .unwrap();
    let open = open_clip(&store, 1, at(11, 59, 0)).await;
    let replicated = closed_clip(&store, 1, at(10, 0, 0), 500, true).await;
    store
        .mark_cold_replicated(replicated, &cold_mark("r.mp4", now))
        .await
        .unwrap();

    let parts = partitions(
        store
            .upload_queue_partitions(RecordingSource::Ordinary, now, Some(floor))
            .await
            .unwrap(),
    );
    assert_eq!(
        parts.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            quarantined,
            quarantined_old,
            before_floor,
            missing_media,
            backoff,
            ready_low,
            ready_unknown,
            ready_elapsed
        ]),
        "every unreplicated finalized row, and only those"
    );
    assert!(!parts.contains_key(&open) && !parts.contains_key(&replicated));
    assert_eq!(parts[&quarantined], QueuePartition::Quarantined);
    assert_eq!(parts[&quarantined_old], QueuePartition::Quarantined);
    assert_eq!(parts[&before_floor], QueuePartition::BeforeEnrollment);
    assert_eq!(parts[&missing_media], QueuePartition::MissingLocalMedia);
    assert_eq!(parts[&backoff], QueuePartition::Backoff);

    let selected = store
        .clips_pending_cold_upload_at(100, Some(floor), now)
        .await
        .unwrap();
    assert_eq!(
        ids_in(&parts, QueuePartition::Ready),
        selected.iter().map(|c| c.id).collect::<BTreeSet<_>>(),
        "ready must be exactly the uploader's selection"
    );
    assert_eq!(
        selected.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![ready_unknown, ready_low, ready_elapsed],
        "priority orders the selection; it does not change its members"
    );

    // Alert queue.
    let a_building = alert_clip(&store, 1, at(11, 0, 0)).await;
    let a_ready = alert_clip(&store, 1, at(9, 0, 0)).await;
    store
        .mark_alert_clip_ready(a_ready, 5_000, 50, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let a_unhashed = alert_clip(&store, 1, at(9, 5, 0)).await;
    store
        .mark_alert_clip_ready(a_unhashed, 5_000, 60, None)
        .await
        .unwrap();
    let a_before = alert_clip(&store, 2, at(7, 0, 0)).await;
    store
        .mark_alert_clip_ready(a_before, 5_000, 70, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let a_backoff = alert_clip(&store, 2, at(9, 10, 0)).await;
    store
        .mark_alert_clip_ready(a_backoff, 5_000, 80, Some(&"b".repeat(64)))
        .await
        .unwrap();
    store
        .record_alert_clip_cold_failure(a_backoff, now - Duration::seconds(30), "boom")
        .await
        .unwrap();

    let alert_parts = partitions(
        store
            .upload_queue_partitions(RecordingSource::Alert, now, Some(floor))
            .await
            .unwrap(),
    );
    assert!(!alert_parts.contains_key(&a_building));
    assert_eq!(alert_parts[&a_before], QueuePartition::BeforeEnrollment);
    assert_eq!(alert_parts[&a_unhashed], QueuePartition::MissingLocalMedia);
    assert_eq!(alert_parts[&a_backoff], QueuePartition::Backoff);
    let alert_selected = store
        .alert_clips_pending_cold_upload(100, Some(floor), now - Duration::seconds(60))
        .await
        .unwrap();
    assert_eq!(
        ids_in(&alert_parts, QueuePartition::Ready),
        alert_selected.iter().map(|c| c.id).collect::<BTreeSet<_>>(),
    );
    assert_eq!(
        ids_in(&alert_parts, QueuePartition::Ready),
        BTreeSet::from([a_ready])
    );

    // The snapshot aggregates the same partitions in one read.
    let snap = store
        .recording_delivery_stats(|| now, Some(floor))
        .await
        .unwrap();
    assert_eq!(snap.schema_version, 1);
    assert_eq!(snap.sampled_at, now);
    let o = &snap.ordinary;
    let q = o
        .quarantined
        .as_ref()
        .expect("motion clips can be quarantined");
    assert_eq!((q.count, q.known_bytes), (2, 200));
    assert_eq!(q.oldest_capture_at, Some(at(7, 0, 0)));
    assert_eq!(
        (o.before_enrollment.count, o.before_enrollment.known_bytes),
        (1, 200)
    );
    assert_eq!(o.missing_local_media.count, 1);
    assert_eq!((o.backoff.count, o.backoff.known_bytes), (1, 400));
    assert_eq!(o.ready.count, 3);
    assert_eq!(
        o.ready.known_bytes, 3_000,
        "an unknown size is not counted as 0 bytes"
    );
    assert_eq!(o.ready.unknown_size_count, 1);
    assert_eq!(o.ready.oldest_capture_at, Some(at(9, 30, 0)));
    assert_eq!(
        (o.open.count, o.open.known_bytes, o.open.unknown_size_count),
        (1, 0, 1)
    );
    assert_eq!(o.open.oldest_capture_at, Some(at(11, 59, 0)));
    let a = &snap.alert;
    assert!(a.quarantined.is_none(), "alert clips have no quarantine");
    assert_eq!((a.ready.count, a.ready.known_bytes), (1, 50));
    assert_eq!((a.backoff.count, a.backoff.known_bytes), (1, 80));
    assert_eq!(a.missing_local_media.count, 1);
    assert_eq!(a.before_enrollment.count, 1);
    assert_eq!(a.open.count, 1);
}

fn hour(snap: &RecordingDeliverySnapshot) -> &DeliveryWindow {
    assert_eq!(snap.windows[0].window_seconds, 3_600);
    &snap.windows[0]
}

fn day(snap: &RecordingDeliverySnapshot) -> &DeliveryWindow {
    assert_eq!(snap.windows[1].window_seconds, 86_400);
    &snap.windows[1]
}

async fn enroll(store: &Store, floor: DateTime<Utc>) {
    store
        .set_cloud_enrollment(&nexus_store::cloud::CloudEnrollment {
            core_id: "11111111-2222-3333-4444-555555555555".into(),
            gateway_url: "wss://gateway.test/v1/tunnel".into(),
            cert_pem: "x".into(),
            private_key_pem: "x".into(),
            ca_chain_pem: "x".into(),
            entitlement_jwt: "x".into(),
            signing_key_pem: None,
            signing_kid: None,
            enrolled_at: floor,
            attach_replay_after: None,
            server_cert_pem: None,
            server_private_key_pem: None,
            ssh_ca_public_key: None,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE cloud_enrollment SET enrolled_at = ?")
        .bind(floor.to_rfc3339())
        .execute(store.pool())
        .await
        .unwrap();
}

async fn cold_backend(store: &Store) {
    store
        .upsert_storage_backend("cold", "lan", "{\"root\":\"/tmp/c\"}")
        .await
        .unwrap();
}

/// A duplicate completion and an overwritten receipt move the pointer
/// but count the delivery once, in the minute of the first completion.
#[tokio::test]
async fn recording_delivery_counts_delivery_once_across_duplicate_and_overwritten_receipts() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let id = closed_clip(&store, 1, at(10, 0, 0), 1_000, true).await;
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(10, 5, 10)))
        .await
        .unwrap();
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(10, 5, 10)))
        .await
        .unwrap();
    store
        .mark_cold_replicated(id, &cold_mark("b.mp4", at(10, 7, 0)))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_clip(id)
            .await
            .unwrap()
            .unwrap()
            .cold_path
            .as_deref(),
        Some("b.mp4"),
        "the receipt still overwrites the pointer"
    );

    let snap = store
        .recording_delivery_stats(|| at(10, 30, 0), None)
        .await
        .unwrap();
    let ord = hour(&snap).core.ordinary;
    assert_eq!(ord.generated_bytes, 1_000);
    assert_eq!(ord.recorded_ms, 30_000);
    assert_eq!(ord.delivered_bytes, 1_000, "delivered once");
    assert_eq!(hour(&snap).core.alert, DeliveryCounters::default());
    let at_overwrite = store
        .recording_delivery_stats(|| at(10, 8, 0), None)
        .await
        .unwrap();
    let late = store
        .recording_delivery_stats(|| at(10, 6, 0), None)
        .await
        .unwrap();
    assert_eq!(
        hour(&at_overwrite).core.ordinary.delivered_bytes,
        hour(&late).core.ordinary.delivered_bytes,
        "nothing was counted at the overwrite minute"
    );
}

/// A crash between attempt and completion, and a restart after it, never
/// double counts: the re-close and re-mark after reopening are no-ops,
/// while the second transfer is a real second attempt.
#[tokio::test]
async fn recording_delivery_survives_restart_without_double_counting() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let store = open_store(dir.path()).await;
        store.upsert_camera(&sample_camera(1)).await.unwrap();
        cold_backend(&store).await;
        let id = closed_clip(&store, 1, at(10, 0, 0), 1_000, true).await;
        store
            .record_upload_attempt(RecordingSource::Ordinary, id, at(10, 1, 0), 1_000)
            .await
            .unwrap();
        store.pool().close().await;
        id
    };
    let store = open_store(dir.path()).await;
    store
        .close_clip_at(id, &close(at(10, 0, 0), 1_000, true), at(10, 0, 30))
        .await
        .unwrap();
    store
        .record_upload_attempt(RecordingSource::Ordinary, id, at(10, 2, 0), 1_000)
        .await
        .unwrap();
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(10, 2, 30)))
        .await
        .unwrap();
    store.pool().close().await;
    let store = open_store(dir.path()).await;
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(10, 3, 0)))
        .await
        .unwrap();

    let snap = store
        .recording_delivery_stats(|| at(10, 30, 0), None)
        .await
        .unwrap();
    let ord = hour(&snap).core.ordinary;
    assert_eq!(ord.generated_bytes, 1_000, "re-close after restart");
    assert_eq!(ord.attempted_bytes, 2_000, "both transfers were attempted");
    assert_eq!(ord.retry_count, 0, "no failure was recorded before either");
    assert_eq!(ord.delivered_bytes, 1_000, "re-mark after restart");
}

/// Attempted bytes count every transfer whatever its outcome; a transfer
/// after a recorded failure is a retry.
#[tokio::test]
async fn recording_delivery_counts_every_retry_attempt_and_failure() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let id = closed_clip(&store, 1, at(10, 0, 0), 1_000, true).await;
    for minute in [1, 2] {
        store
            .record_upload_attempt(RecordingSource::Ordinary, id, at(10, minute, 0), 1_000)
            .await
            .unwrap();
        store
            .record_cold_upload_failure(
                id,
                at(10, minute, 5),
                "boom",
                Some(at(10, minute, 30)),
                false,
            )
            .await
            .unwrap();
    }
    store
        .record_upload_attempt(RecordingSource::Ordinary, id, at(10, 3, 0), 1_000)
        .await
        .unwrap();
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(10, 3, 10)))
        .await
        .unwrap();

    let ord = hour(
        &store
            .recording_delivery_stats(|| at(10, 30, 0), None)
            .await
            .unwrap(),
    )
    .core
    .ordinary;
    assert_eq!(ord.attempted_bytes, 3_000);
    assert_eq!(ord.retry_count, 2);
    assert_eq!(ord.failure_count, 2);
    assert_eq!(ord.delivered_bytes, 1_000);

    let alert = alert_clip(&store, 1, at(10, 0, 0)).await;
    store
        .mark_alert_clip_ready(alert, 5_000, 40, Some(&"b".repeat(64)))
        .await
        .unwrap();
    store
        .record_upload_attempt(RecordingSource::Alert, alert, at(10, 4, 0), 40)
        .await
        .unwrap();
    store
        .record_alert_clip_cold_failure(alert, at(10, 4, 1), "boom")
        .await
        .unwrap();
    store
        .record_upload_attempt(RecordingSource::Alert, alert, at(10, 5, 0), 40)
        .await
        .unwrap();
    let snap = store
        .recording_delivery_stats(|| at(10, 30, 0), None)
        .await
        .unwrap();
    let a = hour(&snap).core.alert;
    assert_eq!(
        (a.attempted_bytes, a.retry_count, a.failure_count),
        (80, 1, 1)
    );
    assert_eq!(
        hour(&snap).core.ordinary.attempted_bytes,
        3_000,
        "kept apart"
    );
}

/// Retention / watermark eviction of a queued clip is an explicit loss;
/// a delivered clip, a before-enrollment clip and an open file are not.
/// Alert eviction (including the missing-file scan's) counts once.
#[tokio::test]
async fn recording_delivery_counts_eviction_before_upload() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let now = Utc::now();
    enroll(&store, now - Duration::hours(3)).await;

    let queued = closed_clip(&store, 1, now - Duration::hours(2), 500, true).await;
    let quarantined_unknown = closed_clip(&store, 1, now - Duration::hours(2), 0, true).await;
    store
        .quarantine_clip(quarantined_unknown, now, "corrupt")
        .await
        .unwrap();
    let delivered = closed_clip(&store, 1, now - Duration::hours(2), 700, true).await;
    store
        .mark_cold_replicated(delivered, &cold_mark("d.mp4", now - Duration::hours(1)))
        .await
        .unwrap();
    let before_floor = closed_clip(&store, 1, now - Duration::hours(4), 900, true).await;
    let open = open_clip(&store, 1, now - Duration::hours(2)).await;
    for id in [queued, quarantined_unknown, delivered, before_floor, open] {
        store.cascade_delete_clip_metadata(id).await.unwrap();
    }
    assert!(store.cascade_delete_clip_metadata(queued).await.is_err());

    let a_lost = alert_clip(&store, 2, now - Duration::hours(1)).await;
    store
        .mark_alert_clip_ready(a_lost, 5_000, 60, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let a_delivered = alert_clip(&store, 2, now - Duration::hours(1)).await;
    store
        .mark_alert_clip_ready(a_delivered, 5_000, 70, Some(&"b".repeat(64)))
        .await
        .unwrap();
    store
        .mark_alert_clip_cold_replicated(
            a_delivered,
            &AlertClipColdMark {
                cold_handle: "cold".into(),
                cold_path: "a.mp4".into(),
                cold_uploaded_at: now,
            },
        )
        .await
        .unwrap();
    let a_before = alert_clip(&store, 2, now - Duration::hours(4)).await;
    store
        .mark_alert_clip_ready(a_before, 5_000, 80, Some(&"b".repeat(64)))
        .await
        .unwrap();
    for id in [a_lost, a_lost, a_delivered, a_before] {
        store.mark_alert_clip_evicted(id).await.unwrap();
    }

    let snap = store
        .recording_delivery_stats(|| Utc::now() + Duration::minutes(2), None)
        .await
        .unwrap();
    let w = hour(&snap);
    assert_eq!(w.core.ordinary.evicted_before_upload_bytes, 500);
    assert_eq!(
        w.core.ordinary.evicted_before_upload_count, 2,
        "an unknown-size loss still counts"
    );
    assert_eq!(w.core.alert.evicted_before_upload_bytes, 60);
    assert_eq!(w.core.alert.evicted_before_upload_count, 1);
    assert_eq!(w.cameras.len(), 2);
}

/// Deleting a camera cascades its clips away without the evictors; its
/// undelivered queue is still an explicit loss, and an alert clip that
/// was already evicted is not counted again.
#[tokio::test]
async fn recording_delivery_counts_camera_deletion_as_eviction_before_upload() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let now = Utc::now();
    closed_clip(&store, 1, now - Duration::hours(1), 500, true).await;
    let delivered = closed_clip(&store, 1, now - Duration::hours(1), 700, true).await;
    store
        .mark_cold_replicated(delivered, &cold_mark("d.mp4", now))
        .await
        .unwrap();
    let a_lost = alert_clip(&store, 1, now - Duration::hours(1)).await;
    store
        .mark_alert_clip_ready(a_lost, 5_000, 60, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let a_evicted = alert_clip(&store, 1, now - Duration::hours(1)).await;
    store
        .mark_alert_clip_ready(a_evicted, 5_000, 80, Some(&"b".repeat(64)))
        .await
        .unwrap();
    store.mark_alert_clip_evicted(a_evicted).await.unwrap();

    store.delete_camera(1).await.unwrap();

    let snap = store
        .recording_delivery_stats(|| Utc::now() + Duration::minutes(2), None)
        .await
        .unwrap();
    let w = hour(&snap);
    assert_eq!(
        (
            w.core.ordinary.evicted_before_upload_bytes,
            w.core.ordinary.evicted_before_upload_count
        ),
        (500, 1)
    );
    assert_eq!(
        (
            w.core.alert.evicted_before_upload_bytes,
            w.core.alert.evicted_before_upload_count
        ),
        (140, 2),
        "the earlier eviction (80) plus the cascaded clip (60), once each"
    );
    assert_eq!(snap.ordinary.ready.count + snap.alert.ready.count, 0);
}

/// The minute holding the sample is outside every window, so a loss in it
/// is reported in `since_window_end`: an eviction at 12:00:05 sampled at
/// 12:00:20 empties the queue while the complete windows show no loss.
#[tokio::test]
async fn recording_delivery_reports_loss_since_the_window_end() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let id = closed_clip(&store, 1, Utc::now() - Duration::hours(1), 500, true).await;
    store.cascade_delete_clip_metadata(id).await.unwrap();
    let minute: i64 = sqlx::query_scalar(
        "SELECT minute_utc FROM recording_minute_buckets WHERE evicted_before_upload_count > 0",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    let sampled = Utc.timestamp_opt(minute + 20, 0).unwrap();

    let snap = store
        .recording_delivery_stats(|| sampled, None)
        .await
        .unwrap();
    assert_eq!(
        snap.ordinary.ready.count, 0,
        "the queue already reads empty"
    );
    for w in &snap.windows {
        assert_eq!(w.core.ordinary.evicted_before_upload_count, 0);
    }
    let since = snap.since_window_end.ordinary;
    assert_eq!(
        (
            since.evicted_before_upload_bytes,
            since.evicted_before_upload_count
        ),
        (500, 1)
    );
    assert_eq!(snap.since_window_end.alert, DeliveryCounters::default());
}

/// The review's boundary race: the sample is read at 18:00:59.999 and an
/// eviction commits at 18:01 before the snapshot's first read. The sample
/// time is read after the snapshot is pinned, so the clip is still queued
/// there or its loss is counted, never neither (an empty queue with no
/// loss reads "No backlog"); a later sample counts the loss in
/// `since_window_end`.
#[tokio::test(flavor = "multi_thread")]
async fn recording_delivery_samples_after_pinning_its_snapshot() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let id = closed_clip(&store, 1, Utc::now() - Duration::hours(1), 500, true).await;
    let rt = tokio::runtime::Handle::current();
    let clock = || {
        let minute = Utc::now().timestamp().div_euclid(60) * 60;
        let sampled = Utc.timestamp_opt(minute, 0).unwrap() - Duration::milliseconds(1);
        // Commits right after the clock read, in a later minute than it.
        tokio::task::block_in_place(|| rt.block_on(store.cascade_delete_clip_metadata(id)))
            .unwrap();
        sampled
    };
    let snap = store.recording_delivery_stats(clock, None).await.unwrap();
    let lost = snap.since_window_end.ordinary.evicted_before_upload_count
        + hour(&snap).core.ordinary.evicted_before_upload_count;
    assert_eq!(
        snap.ordinary.ready.count + lost,
        1,
        "the clip is queued or lost in the snapshot, never neither"
    );

    let minute: i64 = sqlx::query_scalar(
        "SELECT minute_utc FROM recording_minute_buckets WHERE evicted_before_upload_count > 0",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    let last_ms = Utc.timestamp_opt(minute + 60, 0).unwrap() - Duration::milliseconds(1);
    let snap = store
        .recording_delivery_stats(|| last_ms, None)
        .await
        .unwrap();
    assert_eq!(snap.ordinary.ready.count, 0);
    assert_eq!(
        snap.since_window_end.ordinary.evicted_before_upload_count,
        1
    );
}

/// A row being transferred is a marked subset of the partition it was
/// selected from, never an extra total: it stays there until its
/// completion commits. `in_flight` is null until the queue's uploader is
/// tracked.
#[tokio::test]
async fn recording_delivery_marks_in_flight_rows_without_moving_totals() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let now = at(12, 0, 0);
    let sending = closed_clip(&store, 1, at(11, 0, 0), 1_000, true).await;
    let unknown = closed_clip(&store, 1, at(11, 10, 0), 0, true).await;
    closed_clip(&store, 2, at(11, 20, 0), 2_000, true).await;
    let snap =
        |store: Store| async move { store.recording_delivery_stats(|| now, None).await.unwrap() };

    let before = snap(store.clone()).await;
    assert_eq!(before.ordinary.in_flight, None, "no uploader: cannot tell");
    store.track_transfers(RecordingSource::Ordinary);
    let idle = snap(store.clone()).await;
    let idle_flight = idle.ordinary.in_flight.clone().unwrap();
    assert_eq!(idle_flight.count, 0);
    assert_eq!(
        idle.alert.in_flight, None,
        "the alert uploader is not tracked"
    );

    let t1 = store.begin_transfer(RecordingSource::Ordinary, sending);
    let t2 = store.begin_transfer(RecordingSource::Ordinary, unknown);
    let busy = snap(store.clone()).await;
    let f = busy.ordinary.in_flight.clone().unwrap();
    assert_eq!(
        (f.count, f.known_bytes, f.unknown_size_count),
        (2, 1_000, 1)
    );
    assert_eq!(busy.ordinary.ready, idle.ordinary.ready, "totals unchanged");

    // Completion commits before the transfer ends: no longer queued, so
    // no longer in flight either.
    store
        .mark_cold_replicated(sending, &cold_mark("s.mp4", now))
        .await
        .unwrap();
    let f = snap(store.clone()).await.ordinary.in_flight.unwrap();
    assert_eq!((f.count, f.known_bytes), (1, 0));
    drop((t1, t2));
    assert_eq!(snap(store).await.ordinary.in_flight.unwrap().count, 0);
}

/// Windows are whole UTC minutes ending at the start of the sample's
/// minute; a clip finalized one millisecond before the hour belongs to
/// the previous hour.
#[tokio::test]
async fn recording_delivery_windows_align_to_minute_and_hour_boundaries() {
    let (store, _dir) = fresh_store().await;
    let ms = Duration::milliseconds(1);
    // close() ends a clip 30 s after it starts.
    closed_clip(&store, 1, at(10, 59, 30) - ms, 100, true).await; // ends 10:59:59.999
    closed_clip(&store, 1, at(10, 59, 30), 20, true).await; // ends 11:00:00.000

    let gen = |snap: &RecordingDeliverySnapshot| hour(snap).core.ordinary.generated_bytes;
    let snap = store
        .recording_delivery_stats(|| at(11, 0, 30), None)
        .await
        .unwrap();
    assert_eq!(
        (hour(&snap).start, hour(&snap).end),
        (at(10, 0, 0), at(11, 0, 0))
    );
    assert_eq!(gen(&snap), 100, "the current minute is excluded");
    let snap = store
        .recording_delivery_stats(|| at(11, 0, 59), None)
        .await
        .unwrap();
    assert_eq!(gen(&snap), 100);
    let snap = store
        .recording_delivery_stats(|| at(11, 59, 59) + Duration::milliseconds(999), None)
        .await
        .unwrap();
    assert_eq!(gen(&snap), 120);
    let snap = store
        .recording_delivery_stats(|| at(12, 0, 0), None)
        .await
        .unwrap();
    assert_eq!(gen(&snap), 20);
    assert_eq!(day(&snap).core.ordinary.generated_bytes, 120);
    assert!(
        !hour(&snap).complete && !day(&snap).complete,
        "windows before accounting began are partial"
    );

    let later = snap.accounting_since + Duration::minutes(61);
    let snap = store
        .recording_delivery_stats(|| later, None)
        .await
        .unwrap();
    assert!(hour(&snap).complete);
    assert!(!day(&snap).complete);
}

/// Per-camera rows add up to the core totals, and alert bytes never leak
/// into the ordinary totals.
#[tokio::test]
async fn recording_delivery_totals_two_cameras_and_keeps_alert_separate() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let one = closed_clip(&store, 1, at(10, 0, 0), 1_000, true).await;
    closed_clip(&store, 2, at(10, 1, 0), 2_000, true).await;
    closed_clip(&store, 2, at(10, 2, 0), 0, true).await;
    store
        .mark_cold_replicated(one, &cold_mark("1.mp4", at(10, 3, 0)))
        .await
        .unwrap();
    let alert = alert_clip(&store, 2, at(10, 4, 0)).await;
    // `mark_alert_clip_ready` stamps the real clock; read it back.
    store
        .mark_alert_clip_ready(alert, 4_000, 300, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let ready_at = store
        .get_alert_clip(alert)
        .await
        .unwrap()
        .unwrap()
        .ready_at
        .unwrap();

    let snap = store
        .recording_delivery_stats(|| at(10, 30, 0), None)
        .await
        .unwrap();
    let w = hour(&snap);
    assert_eq!(w.core.ordinary.generated_bytes, 3_000);
    assert_eq!(w.core.ordinary.generated_unknown_size_count, 1);
    assert_eq!(w.core.ordinary.delivered_bytes, 1_000);
    let cams: Vec<_> = w.cameras.iter().map(|c| c.camera_id).collect();
    assert_eq!(cams, vec![1, 2]);
    assert_eq!(w.cameras[0].counters.ordinary.generated_bytes, 1_000);
    assert_eq!(w.cameras[1].counters.ordinary.generated_bytes, 2_000);
    assert_eq!(w.cameras[1].counters.ordinary.delivered_bytes, 0);

    let snap = store
        .recording_delivery_stats(|| ready_at + Duration::minutes(1), None)
        .await
        .unwrap();
    let w = hour(&snap);
    assert_eq!(w.core.alert.generated_bytes, 300);
    assert_eq!(w.core.alert.recorded_ms, 4_000);
    assert_eq!(
        w.cameras.last().unwrap().counters.alert.generated_bytes,
        300
    );
    assert_eq!(w.core.ordinary.generated_bytes, 0, "alert is not ordinary");
}

/// A write prunes buckets more than seven days older than itself.
#[tokio::test]
async fn recording_delivery_prunes_buckets_older_than_seven_days() {
    let (store, _dir) = fresh_store().await;
    let t0 = Utc.with_ymd_and_hms(2026, 11, 10, 10, 0, 0).unwrap();
    closed_clip(&store, 1, t0, 1, true).await; // minute t0 + 30 s -> t0
    closed_clip(&store, 1, t0 + Duration::days(7), 1, true).await;
    let minutes = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT minute_utc FROM recording_minute_buckets ORDER BY minute_utc",
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
    };
    assert_eq!(
        minutes().await,
        vec![t0.timestamp(), (t0 + Duration::days(7)).timestamp()],
        "exactly seven days old is kept"
    );
    closed_clip(
        &store,
        1,
        t0 + Duration::days(7) + Duration::minutes(1),
        1,
        true,
    )
    .await;
    assert_eq!(
        minutes().await,
        vec![
            (t0 + Duration::days(7)).timestamp(),
            (t0 + Duration::days(7) + Duration::minutes(1)).timestamp()
        ]
    );
}

/// Rows that exist when migration 0041 runs are a baseline: no bucket is
/// invented for them, finished work is never counted later, and the
/// backlog counts when it actually finalizes or delivers.
#[tokio::test]
async fn recording_delivery_baselines_existing_rows_without_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("nexus.db");
    {
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE schema_migrations (
                 id TEXT PRIMARY KEY,
                 applied_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut files: Vec<_> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.file_name().unwrap().to_str().unwrap() < "0041")
                .collect();
        files.sort();
        assert_eq!(files.len(), 40, "pre-0041 baseline");
        for f in &files {
            sqlx::raw_sql(&std::fs::read_to_string(f).unwrap())
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("apply {}: {e}", f.display()));
            sqlx::query("INSERT INTO schema_migrations (id) VALUES (?)")
                .bind(f.file_stem().unwrap().to_str().unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        for sql in [
            "INSERT INTO cameras (id, name, url, config_json) VALUES (1, 'c', 'rtsp://h/s', '{}')",
            "INSERT INTO storage_backends (handle, kind, config_json) VALUES ('cold', 'lan', '{}')",
            "INSERT INTO motion_clips (id, camera_id, started_at, ended_at, size_bytes, duration_ms,
                 hot_handle, hot_path, sha256, cold_handle, cold_path, cold_uploaded_at)
             VALUES (1, 1, '2026-11-24T09:00:00+00:00', '2026-11-24T09:00:30+00:00', 100, 30000,
                 'local', '1.mp4', 'h', 'cold', '1.mp4', '2026-11-24T09:01:00+00:00'),
                    (2, 1, '2026-11-24T09:10:00+00:00', '2026-11-24T09:10:30+00:00', 200, 30000,
                 'local', '2.mp4', 'h', NULL, NULL, NULL),
                    (3, 1, '2026-11-24T09:20:00+00:00', NULL, 0, 0,
                 'local', '3.mp4', NULL, NULL, NULL, NULL)",
            "INSERT INTO alert_clips (id, camera_id, path, started_at, ready_at, state, size_bytes,
                 sha256, cold_handle, cold_path, cold_uploaded_at)
             VALUES (1, 1, 'a1.mp4', '2026-11-24T09:00:00+00:00', '2026-11-24T09:00:10+00:00',
                 'ready', 10, 'h', 'cold', 'a1.mp4', '2026-11-24T09:01:00+00:00'),
                    (2, 1, 'a2.mp4', '2026-11-24T09:00:00+00:00', '2026-11-24T09:00:10+00:00',
                 'ready', 20, 'h', NULL, NULL, NULL)",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        pool.close().await;
    }

    let store = open_store(dir.path()).await;
    let buckets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recording_minute_buckets")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(buckets, 0, "no historical buckets are invented");

    let t = at(10, 0, 0);
    let alert_mark = |p: &str| AlertClipColdMark {
        cold_handle: "cold".into(),
        cold_path: p.into(),
        cold_uploaded_at: t,
    };
    store
        .mark_cold_replicated(1, &cold_mark("1.mp4", t))
        .await
        .unwrap();
    store
        .mark_cold_replicated(2, &cold_mark("2.mp4", t))
        .await
        .unwrap();
    store
        .close_clip_at(1, &close(at(9, 0, 0), 100, true), at(9, 0, 30))
        .await
        .unwrap();
    store
        .close_clip_at(3, &close(at(9, 59, 30), 300, true), at(10, 0, 0))
        .await
        .unwrap();
    store
        .mark_alert_clip_cold_replicated(1, &alert_mark("a1.mp4"))
        .await
        .unwrap();
    store
        .mark_alert_clip_cold_replicated(2, &alert_mark("a2.mp4"))
        .await
        .unwrap();

    let snap = store
        .recording_delivery_stats(|| at(10, 30, 0), None)
        .await
        .unwrap();
    let w = hour(&snap);
    assert_eq!(
        w.core.ordinary.delivered_bytes, 200,
        "only the backlog clip"
    );
    assert_eq!(
        w.core.ordinary.generated_bytes, 300,
        "only the clip open at upgrade"
    );
    assert_eq!(w.core.alert.delivered_bytes, 20);
    assert!(snap.accounting_since > at(10, 30, 0));
}

/// A clip whose last frame was captured at 12:59:59 but finalized at
/// 13:00:05 is generated in the 13:00 hour, where its delivery lands.
#[tokio::test]
async fn recording_delivery_counts_generation_at_finalization_not_last_frame() {
    let (store, _dir) = fresh_store().await;
    cold_backend(&store).await;
    let id = open_clip(&store, 1, at(12, 59, 0)).await;
    let mut c = close(at(12, 59, 0), 3_600, true);
    c.ended_at = at(12, 59, 59);
    store.close_clip_at(id, &c, at(13, 0, 5)).await.unwrap();
    store
        .mark_cold_replicated(id, &cold_mark("a.mp4", at(13, 10, 0)))
        .await
        .unwrap();

    let minute: i64 = sqlx::query_scalar(
        "SELECT minute_utc FROM recording_minute_buckets
          WHERE source_kind = 'ordinary' AND generated_bytes = 3600",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(minute, at(13, 0, 0).timestamp(), "bucketed at finalization");
}
