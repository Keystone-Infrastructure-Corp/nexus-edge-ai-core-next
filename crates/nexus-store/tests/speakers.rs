//! Speakers and their camera assignments (ADR-091). One speaker serves any
//! number of cameras, a camera has at most one speaker, and deleting either
//! side clears the link.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use nexus_config::{CameraConfig, SpeakerConfig, SpeakerDriver, SpeakerId, StoreConfig};
use nexus_store::{Store, StoreError};
use nexus_types::CameraId;
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

async fn with_cameras(ids: &[CameraId]) -> (Store, TempDir) {
    let (store, dir) = fresh_store().await;
    for &id in ids {
        let cam: CameraConfig = serde_json::from_value(serde_json::json!({
            "id": id,
            "name": format!("c{id}"),
            "url": "virtual://local",
        }))
        .expect("a minimal camera");
        store.upsert_camera(&cam).await.expect("store a camera");
    }
    (store, dir)
}

async fn create(store: &Store, name: &str) -> SpeakerConfig {
    let mut sp: SpeakerConfig =
        serde_json::from_value(serde_json::json!({ "name": name, "host": "192.0.2.10" }))
            .expect("a minimal speaker");
    let mut tx = store.begin_tx().await.unwrap();
    store.create_speaker_tx(&mut tx, &mut sp).await.unwrap();
    Store::commit_tx(tx).await.unwrap();
    sp
}

async fn assign(store: &Store, speaker: SpeakerId, cams: &[CameraId]) -> Result<(), StoreError> {
    let mut tx = store.begin_tx().await?;
    store.set_speaker_cameras_tx(&mut tx, speaker, cams).await?;
    Store::commit_tx(tx).await
}

async fn set_camera(
    store: &Store,
    camera: CameraId,
    speaker: Option<SpeakerId>,
) -> Result<(), StoreError> {
    let mut tx = store.begin_tx().await?;
    store
        .set_camera_speaker_tx(&mut tx, camera, speaker)
        .await?;
    Store::commit_tx(tx).await
}

async fn ids(store: &Store, speaker: SpeakerId) -> Vec<CameraId> {
    store.speaker_camera_ids(speaker).await.unwrap()
}

#[tokio::test]
async fn speakers_round_trip_through_create_list_update_delete() {
    let (store, _dir) = fresh_store().await;
    let a = create(&store, "lobby").await;
    let b = create(&store, "dock").await;
    assert!(a.id > 0 && b.id > a.id, "ids are assigned: {a:?} {b:?}");
    assert_eq!((a.port, a.driver), (5060, SpeakerDriver::Sip));
    assert_eq!(
        store.list_speakers().await.unwrap(),
        vec![a.clone(), b.clone()]
    );

    let mut edited = a.clone();
    edited.name = "front lobby".into();
    edited.password = Some("s3cret".into());
    let mut tx = store.begin_tx().await.unwrap();
    store.update_speaker_tx(&mut tx, &edited).await.unwrap();
    Store::commit_tx(tx).await.unwrap();
    assert_eq!(store.get_speaker(a.id).await.unwrap(), Some(edited));

    let mut tx = store.begin_tx().await.unwrap();
    store.delete_speaker_tx(&mut tx, a.id).await.unwrap();
    Store::commit_tx(tx).await.unwrap();
    assert_eq!(store.get_speaker(a.id).await.unwrap(), None);
    assert_eq!(store.list_speakers().await.unwrap(), vec![b]);

    // An unknown id is reported, not silently ignored.
    let mut tx = store.begin_tx().await.unwrap();
    assert!(matches!(
        store.delete_speaker_tx(&mut tx, a.id).await,
        Err(StoreError::NotFound(_))
    ));
    let mut ghost = a.clone();
    ghost.id = 999;
    assert!(matches!(
        store.update_speaker_tx(&mut tx, &ghost).await,
        Err(StoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn assigning_cameras_replaces_the_speakers_set() {
    let (store, _dir) = with_cameras(&[1, 2, 3]).await;
    let a = create(&store, "a").await;
    let b = create(&store, "b").await;

    assign(&store, a.id, &[2, 1]).await.unwrap();
    assert_eq!(ids(&store, a.id).await, vec![1, 2]);

    // Camera 2 moves from A to B; camera 1 stays on A.
    assign(&store, b.id, &[2, 3]).await.unwrap();
    assert_eq!(ids(&store, b.id).await, vec![2, 3]);
    assert_eq!(ids(&store, a.id).await, vec![1]);
    assert_eq!(store.speaker_for_camera(2).await.unwrap(), Some(b.clone()));

    // A camera left out of the list is unassigned.
    assign(&store, b.id, &[3]).await.unwrap();
    assert_eq!(store.speaker_for_camera(2).await.unwrap(), None);

    // An unknown camera fails the whole set and changes nothing.
    assert!(matches!(
        assign(&store, a.id, &[3, 99]).await,
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(ids(&store, a.id).await, vec![1]);
    assert_eq!(ids(&store, b.id).await, vec![3]);

    assign(&store, a.id, &[]).await.unwrap();
    assert_eq!(ids(&store, a.id).await, Vec::<CameraId>::new());
}

#[tokio::test]
async fn deleting_a_speaker_clears_its_assignments() {
    let (store, _dir) = with_cameras(&[1, 2]).await;
    let a = create(&store, "a").await;
    assign(&store, a.id, &[1, 2]).await.unwrap();

    let mut tx = store.begin_tx().await.unwrap();
    store.delete_speaker_tx(&mut tx, a.id).await.unwrap();
    Store::commit_tx(tx).await.unwrap();

    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM speaker_cameras")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(store.speaker_for_camera(1).await.unwrap(), None);
}

#[tokio::test]
async fn deleting_a_camera_clears_its_assignment() {
    let (store, _dir) = with_cameras(&[1, 2]).await;
    let a = create(&store, "a").await;
    assign(&store, a.id, &[1, 2]).await.unwrap();

    store.delete_camera(1).await.unwrap();
    assert_eq!(ids(&store, a.id).await, vec![2]);
}

#[tokio::test]
async fn a_camera_has_at_most_one_speaker() {
    let (store, _dir) = with_cameras(&[1]).await;
    let a = create(&store, "a").await;
    let b = create(&store, "b").await;
    set_camera(&store, 1, Some(a.id)).await.unwrap();

    // The schema refuses a second row for the camera ...
    let dup = sqlx::query("INSERT INTO speaker_cameras (camera_id, speaker_id) VALUES (1, ?)")
        .bind(b.id)
        .execute(store.pool())
        .await;
    assert!(dup.is_err(), "a second speaker for camera 1 was stored");

    // ... so assigning another speaker moves the camera.
    set_camera(&store, 1, Some(b.id)).await.unwrap();
    assert_eq!(ids(&store, a.id).await, Vec::<CameraId>::new());
    assert_eq!(ids(&store, b.id).await, vec![1]);

    set_camera(&store, 1, None).await.unwrap();
    assert_eq!(store.speaker_for_camera(1).await.unwrap(), None);

    for (camera, speaker) in [(99, Some(a.id)), (99, None), (1, Some(999))] {
        assert!(
            matches!(
                set_camera(&store, camera, speaker).await,
                Err(StoreError::NotFound(_))
            ),
            "camera {camera} speaker {speaker:?}"
        );
    }
}
