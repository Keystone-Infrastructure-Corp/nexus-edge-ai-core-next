//! Boot the real `nexus-engine` binary against a store holding a camera row
//! this build cannot read.
//!
//! BUG-225: `main` read the cameras with `list_cameras`, which fails on such a
//! row (one a newer release wrote with a codec this build has no variant
//! for), so the engine exited at boot and crash-looped under systemd after
//! any restart or rollback. It now reads row by row: the readable cameras
//! run, and the roll-up names the row it could not read from the first
//! health answer. No unit test reaches `main`'s read, so this runs the
//! binary, found through cargo's `CARGO_BIN_EXE_nexus-engine`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::Store;
use serde_json::Value;

/// The engine under test, killed when the test ends, pass or fail.
struct Engine {
    child: Child,
    log: PathBuf,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Engine {
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().expect("poll the engine process")
    }

    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }
}

fn camera(id: i64) -> CameraConfig {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": format!("c{id}"),
        "url": "virtual://local",
        "enabled": true,
        "max_fps": 5,
    }))
    .expect("a minimal camera")
}

/// Cameras 1 and 2, with camera 2 given a codec this build has no variant
/// for, so the camera list can no longer be read in one piece.
async fn seed_the_store(db: &Path) {
    let store = Store::open(&StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    })
    .await
    .expect("Store::open");
    for id in [1, 2] {
        store
            .upsert_camera(&camera(id))
            .await
            .expect("store a camera");
    }
    sqlx::query(
        "UPDATE cameras SET config_json = json_set(config_json, '$.codec', 'av1') WHERE id = 2",
    )
    .execute(store.pool())
    .await
    .expect("rewrite camera 2's row");
    assert!(
        store.list_cameras().await.is_err(),
        "fixture: the camera list can no longer be read in one piece",
    );
    store.pool().close().await;
}

fn camera_config_unreadable(health: &Value) -> Option<&Value> {
    health["issues"]
        .as_array()?
        .iter()
        .find(|i| i["code"] == "camera_config_unreadable")
}

#[tokio::test]
async fn the_engine_boots_past_a_camera_row_it_cannot_read_and_reports_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("nexus.db");
    seed_the_store(&db).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("an ephemeral port")
        .port();
    let config = dir.path().join("nexus.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[runtime]
state_dir = "{state}"

[runtime.clips]
clips_dir = "{clips}"

[server]
api_bind = "127.0.0.1:{port}"
ui_root = "{ui}"

[store]
url = "sqlite:{db}?mode=rwc"
seed_from_config = false

[telemetry]
log_level = "info"

[auth]
mode = "local"

[inference]
backend = "in_process"
workers = 1

[inference.model]
kind = "mock"
input_width = 640
input_height = 480
"#,
            state = dir.path().join("state").display(),
            clips = dir.path().join("clips").display(),
            ui = dir.path().join("ui-unused").display(),
            db = db.display(),
        ),
    )
    .expect("write the config");
    let log = dir.path().join("engine.log");
    let out = File::create(&log).expect("create the engine log");
    let child = Command::new(env!("CARGO_BIN_EXE_nexus-engine"))
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(out.try_clone().expect("clone the log handle"))
        .stderr(out)
        .spawn()
        .expect("spawn nexus-engine");
    let mut engine = Engine { child, log };
    let base = format!("http://127.0.0.1:{port}/api/v1");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client");

    // Boot reads the cameras before the API listens, so the first answer
    // must already name the row. The reconciler's first pass, which would
    // find it too, runs 30 s after boot.
    let deadline = Instant::now() + Duration::from_secs(60);
    let first = loop {
        if let Some(status) = engine.exited() {
            panic!(
                "the engine exited at boot ({status}):\n{}",
                engine.log_tail()
            );
        }
        if let Ok(r) = http.get(format!("{base}/health")).send().await {
            break r.json::<Value>().await.expect("a health body");
        }
        assert!(
            Instant::now() < deadline,
            "no health answer within 60 s:\n{}",
            engine.log_tail(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let issue = camera_config_unreadable(&first)
        .unwrap_or_else(|| panic!("the first health answer must report the row: {first}"));
    assert_eq!(issue["component"], "store", "{first}");
    assert_eq!(first["status"], "degraded", "{first}");

    // The detail, which names the camera, goes to a signed-in caller only.
    let token = http
        .post(format!("{base}/auth/first-run-setup"))
        .json(&serde_json::json!({
            "username": "admin",
            "password": "boot-past-an-unreadable-camera-row",
        }))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .expect("first-run setup")
        .json::<Value>()
        .await
        .expect("a token body")["access_token"]
        .as_str()
        .expect("an access token")
        .to_string();
    let signed_in = http
        .get(format!("{base}/health"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("signed-in health")
        .json::<Value>()
        .await
        .expect("a health body");
    let detail = camera_config_unreadable(&signed_in)
        .and_then(|i| i["detail"].as_str())
        .unwrap_or_else(|| panic!("a signed-in caller gets the detail: {signed_in}"));
    assert!(detail.ends_with(": 2"), "names camera 2: {detail}");

    // The readable camera runs: its supervisor has put frame stats up.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let stats = http
            .get(format!("{base}/cameras/1/stats"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("camera 1's stats");
        if stats.status().is_success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "camera 1 must run beside the unreadable row (stats: {}):\n{}",
            stats.status(),
            engine.log_tail(),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        engine.exited(),
        None,
        "the engine must keep running:\n{}",
        engine.log_tail(),
    );
}
