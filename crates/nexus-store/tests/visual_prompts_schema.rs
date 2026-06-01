//! M3.1: visual_prompts + camera_visual_prompts schema and CRUD
//! coverage. Pairs with `crates/nexus-store/src/visual_prompts.rs`.
//!
//! Each test boots a fresh tmpdir-backed `Store` so migration 0012
//! exercises end-to-end (we don't reuse a shared DB).

use std::path::PathBuf;

use nexus_config::{CameraBehavior, CameraConfig, CameraDetector, CameraIngest, StoreConfig};
use nexus_store::{NewVisualPrompt, Store, VisualPromptError};
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
        zones: vec![],
    }
}

fn embed_a() -> Vec<f32> {
    (0..512).map(|i| (i as f32) / 1024.0).collect()
}

fn embed_b() -> Vec<f32> {
    (0..512).map(|i| ((i as f32) - 256.0) / 512.0).collect()
}

// 64-char hex strings used as placeholder image_sha256 values across
// tests. Kept as constants so the `&NewVisualPrompt` borrow lives for
// the duration of each test (a `String::repeat(8)` temporary would
// drop at the end of the call expression).
const SHA_A: &str = "abc12345abc12345abc12345abc12345abc12345abc12345abc12345abc12345";
const SHA_B: &str = "def01234def01234def01234def01234def01234def01234def01234def01234";

#[tokio::test]
async fn create_and_get_visual_prompt_round_trips_embedding() {
    let (store, _dir) = fresh_store().await;

    let embedding = embed_a();
    let id = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: Some("blue Amazon delivery vehicle"),
            image_path: "amazon_van/1_abc12345.jpg",
            image_sha256: SHA_A,
            embedding: &embedding,
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create_visual_prompt");

    let (row, fetched) = store
        .get_visual_prompt(id)
        .await
        .expect("get_visual_prompt")
        .expect("row should exist");

    assert_eq!(row.id, id);
    assert_eq!(row.name, "amazon_van");
    assert_eq!(
        row.description.as_deref(),
        Some("blue Amazon delivery vehicle")
    );
    assert_eq!(row.embedding_dim as usize, embedding.len());
    assert_eq!(row.encoder_model_id, "yoloe26_s_image_encoder");
    assert_eq!(fetched, embedding);
}

#[tokio::test]
async fn duplicate_visual_prompt_name_returns_name_taken() {
    let (store, _dir) = fresh_store().await;
    let _ = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/1.jpg",
            image_sha256: SHA_A,
            embedding: &embed_a(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("first insert ok");

    let err = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/2.jpg",
            image_sha256: SHA_B,
            embedding: &embed_b(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect_err("second insert must error");
    assert!(matches!(err, VisualPromptError::NameTaken), "got {err:?}");
}

#[tokio::test]
async fn empty_embedding_is_rejected() {
    let (store, _dir) = fresh_store().await;
    let err = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "empty_prompt",
            description: None,
            image_path: "empty/1.jpg",
            image_sha256: SHA_A,
            embedding: &[],
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect_err("empty embedding must error");
    assert!(
        matches!(err, VisualPromptError::EmptyEmbedding),
        "got {err:?}"
    );
}

#[tokio::test]
async fn list_visual_prompts_returns_attachment_count() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .expect("upsert camera 1");
    store
        .upsert_camera(&sample_camera(2, "back"))
        .await
        .expect("upsert camera 2");
    let id_a = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/1.jpg",
            image_sha256: SHA_A,
            embedding: &embed_a(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create a");
    let _id_b = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "ups_truck",
            description: None,
            image_path: "ups_truck/1.jpg",
            image_sha256: SHA_B,
            embedding: &embed_b(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create b");
    store
        .attach_camera_visual_prompt(1, id_a)
        .await
        .expect("attach 1<-a");
    store
        .attach_camera_visual_prompt(2, id_a)
        .await
        .expect("attach 2<-a");

    let list = store.list_visual_prompts().await.expect("list");
    assert_eq!(list.len(), 2);
    // Returned in NOCASE name order: amazon_van < ups_truck.
    assert_eq!(list[0].name, "amazon_van");
    assert_eq!(list[0].attached_camera_count, 2);
    assert_eq!(list[1].name, "ups_truck");
    assert_eq!(list[1].attached_camera_count, 0);
}

#[tokio::test]
async fn delete_attached_prompt_returns_conflict() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .expect("upsert camera");
    let id = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/1.jpg",
            image_sha256: SHA_A,
            embedding: &embed_a(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create");
    store
        .attach_camera_visual_prompt(1, id)
        .await
        .expect("attach");

    let err = store
        .delete_visual_prompt(id)
        .await
        .expect_err("must conflict");
    assert!(matches!(err, VisualPromptError::Conflict(1)), "got {err:?}");

    store
        .detach_camera_visual_prompt(1, id)
        .await
        .expect("detach");
    store
        .delete_visual_prompt(id)
        .await
        .expect("delete after detach");
    assert!(store
        .get_visual_prompt(id)
        .await
        .expect("get after delete")
        .is_none());
}

#[tokio::test]
async fn deleting_camera_cascades_attachment_only() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .expect("upsert camera");
    let id = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/1.jpg",
            image_sha256: SHA_A,
            embedding: &embed_a(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create");
    store
        .attach_camera_visual_prompt(1, id)
        .await
        .expect("attach");

    // Cascade the camera away.
    store.delete_camera(1).await.expect("delete camera");

    // The visual prompt row survives — and now lists zero
    // attachments because the join was cascaded.
    let list = store.list_visual_prompts().await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].attached_camera_count, 0);
}

#[tokio::test]
async fn list_camera_visual_prompts_returns_embedding_in_name_order() {
    let (store, _dir) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .expect("upsert camera");
    let id_b = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "ups_truck",
            description: None,
            image_path: "ups_truck/1.jpg",
            image_sha256: SHA_B,
            embedding: &embed_b(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create b");
    let id_a = store
        .create_visual_prompt(&NewVisualPrompt {
            name: "amazon_van",
            description: None,
            image_path: "amazon_van/1.jpg",
            image_sha256: SHA_A,
            embedding: &embed_a(),
            encoder_model_id: "yoloe26_s_image_encoder",
        })
        .await
        .expect("create a");
    store
        .attach_camera_visual_prompt(1, id_a)
        .await
        .expect("attach a");
    store
        .attach_camera_visual_prompt(1, id_b)
        .await
        .expect("attach b");

    let attached = store
        .list_camera_visual_prompts(1)
        .await
        .expect("list_camera_visual_prompts");
    assert_eq!(attached.len(), 2);
    // NOCASE alpha: amazon_van before ups_truck.
    assert_eq!(attached[0].0.name, "amazon_van");
    assert_eq!(attached[0].1, embed_a());
    assert_eq!(attached[1].0.name, "ups_truck");
    assert_eq!(attached[1].1, embed_b());
}
