//! `nexus-doctor` — operator smoke-test runner (M-Install Checkpoint 1).
//!
//! Walks the eight-step verification gate from `docs/INSTALL.md §9`
//! against a live engine (default: `http://localhost:8089`) and prints
//! a color-coded pass/fail report. Exit 0 when every check is
//! pass / warn / skip; exit 1 on any failure.
//!
//! Designed to keep the per-bake iteration loop on a fresh box short:
//!
//! ```bash
//! cargo run --bin nexus-engine -- --config config/single-camera.toml &
//! sleep 5
//! nexus-doctor                      # all-green -> exit 0
//! ```
//!
//! Each check carries a one-line "hint" pointing at the
//! `INSTALL.md §11` troubleshooting row most likely to apply.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde_json::Value;

#[path = "../time_sync.rs"]
mod time_sync;

// Phase 6.14b — filesystem posture probe for the directories that
// hold edge-side secrets at rest (`/etc/nexus/tls` for mTLS keys,
// `/var/lib/nexus` for the SQLite admin secret + cloud enrollment
// row). The parser is portable; the runtime probe is Linux-only
// (`/proc/self/mountinfo`).
#[path = "../fs_posture.rs"]
mod fs_posture;

const DEFAULT_BASE_URL: &str = "http://localhost:8089";

#[derive(Debug, Parser)]
#[command(
    name = "nexus-doctor",
    version,
    about = "Run the INSTALL.md §9 smoke checks against a live engine"
)]
struct Cli {
    /// Engine base URL.
    #[arg(long, env = "NEXUS_DOCTOR_URL", default_value = DEFAULT_BASE_URL)]
    base_url: String,

    /// HTTP timeout per request (seconds).
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,

    /// Disable ANSI color (auto-disabled when stdout isn't a TTY).
    #[arg(long)]
    no_color: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let use_color = !cli.no_color && atty_stdout();

    let client = match Client::builder()
        .timeout(Duration::from_secs(cli.timeout_secs.max(1)))
        // Doctor always points at a single short-lived endpoint;
        // pooling would only mask connection errors on the first
        // call. Keep the client simple.
        .pool_max_idle_per_host(0)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nexus-doctor: failed to build HTTP client: {e}");
            return ExitCode::from(2);
        }
    };

    let base = cli.base_url.trim_end_matches('/').to_string();
    let theme = Theme::new(use_color);

    println!(
        "{}nexus-doctor{} → {}{}{}",
        theme.bold, theme.reset, theme.dim, base, theme.reset
    );
    println!();

    let outcomes = run_checks(&client, &base);

    for o in &outcomes {
        o.print(&theme);
    }

    let (pass, warn, fail, skip) = tally(&outcomes);
    println!();
    println!(
        "{}summary{}: {}{} pass{}, {}{} warn{}, {}{} fail{}, {}{} skip{}",
        theme.bold,
        theme.reset,
        theme.green,
        pass,
        theme.reset,
        theme.yellow,
        warn,
        theme.reset,
        theme.red,
        fail,
        theme.reset,
        theme.dim,
        skip,
        theme.reset,
    );

    if fail > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ---------------------------------------------------------------------------
// Outcome plumbing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug)]
struct Outcome {
    /// Step number from INSTALL.md §9 (e.g. "9.1").
    step: &'static str,
    /// Short machine-readable name.
    name: &'static str,
    expected: String,
    actual: String,
    status: Status,
    /// Hint pointing at INSTALL.md §11 troubleshooting (or empty).
    hint: &'static str,
}

impl Outcome {
    fn pass(step: &'static str, name: &'static str, expected: &str, actual: String) -> Self {
        Self {
            step,
            name,
            expected: expected.into(),
            actual,
            status: Status::Pass,
            hint: "",
        }
    }

    fn fail(
        step: &'static str,
        name: &'static str,
        expected: &str,
        actual: String,
        hint: &'static str,
    ) -> Self {
        Self {
            step,
            name,
            expected: expected.into(),
            actual,
            status: Status::Fail,
            hint,
        }
    }

    fn warn(
        step: &'static str,
        name: &'static str,
        expected: &str,
        actual: String,
        hint: &'static str,
    ) -> Self {
        Self {
            step,
            name,
            expected: expected.into(),
            actual,
            status: Status::Warn,
            hint,
        }
    }

    fn skip(step: &'static str, name: &'static str, reason: String) -> Self {
        Self {
            step,
            name,
            expected: "n/a".into(),
            actual: reason,
            status: Status::Skip,
            hint: "",
        }
    }

    fn print(&self, t: &Theme) {
        let (sym, color) = match self.status {
            Status::Pass => ("✓", &t.green),
            Status::Warn => ("!", &t.yellow),
            Status::Fail => ("✗", &t.red),
            Status::Skip => ("-", &t.dim),
        };
        println!(
            "  {}{}{} {}{}{} {}",
            color, sym, t.reset, t.bold, self.step, t.reset, self.name,
        );
        println!("      expected: {}", self.expected);
        println!("      actual:   {}", self.actual);
        if !self.hint.is_empty() {
            println!("      {}hint:     {}{}", t.dim, self.hint, t.reset);
        }
    }
}

fn tally(outcomes: &[Outcome]) -> (usize, usize, usize, usize) {
    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    let mut skip = 0;
    for o in outcomes {
        match o.status {
            Status::Pass => pass += 1,
            Status::Warn => warn += 1,
            Status::Fail => fail += 1,
            Status::Skip => skip += 1,
        }
    }
    (pass, warn, fail, skip)
}

// ---------------------------------------------------------------------------
// The checks (mirrors INSTALL.md §9)
// ---------------------------------------------------------------------------

fn run_checks(client: &Client, base: &str) -> Vec<Outcome> {
    let mut out = Vec::with_capacity(12);

    // 9.0 — local clock sync posture (Phase 1.15). Runs first so
    // an operator sees the row even when the engine HTTP listener
    // is down; the cloud actor_token verifier and edge-gateway
    // both reject stale-clock traffic before any handler runs, so
    // a desynced box is doctor-relevant before HTTP is up.
    out.push(check_time_sync());

    // 9.1 — engine HTTP responds.
    let (health, health_body) = check_health(client, base);
    let engine_up = matches!(health.status, Status::Pass);
    out.push(health);

    // If the engine isn't even up, skip the rest — every other check
    // would otherwise return the same connection error.
    if !engine_up {
        for (step, name) in [
            ("9.2", "ui_loads"),
            ("9.3", "cameras_listed"),
            ("9.4", "snapshot"),
            ("9.5", "backends_ready"),
            ("9.6", "storage_local"),
            ("9.7", "motion_recent"),
            ("9.8", "events_recent"),
        ] {
            out.push(Outcome::skip(
                step,
                name,
                "skipped — engine HTTP not reachable".into(),
            ));
        }
        // NPU runtime + filesystem-posture checks are platform-only
        // (don't need the engine to be up) — still run them so the
        // operator gets the libze1 / mount-options hint on a box
        // where the engine is wedged for unrelated reasons.
        out.push(check_npu_runtime());
        out.push(check_filesystem_posture());
        return out;
    }

    out.push(check_ui_loads(client, base));

    // 9.3 lists cameras and reports which are enabled. The actual
    // "is the camera connected to its RTSP source?" signal is
    // §9.4 (snapshot returns image bytes), so chain them.
    let cameras = fetch_cameras(client, base);
    let (cameras_outcome, enabled_ids) = build_cameras_outcome(&cameras);
    out.push(cameras_outcome);

    out.push(check_snapshots(client, base, &enabled_ids));
    out.push(fail_on_engine_issues(
        check_backends_ready(client, base),
        health_body.as_ref(),
        "detector",
        "The detector failed to build, so this engine reports zero detections; the engine log line `DETECTION DISABLED` names the missing model (check `[inference.model]` preset / pack_path).",
    ));
    out.push(fail_on_engine_issues(
        check_storage_local(client, base),
        health_body.as_ref(),
        "recorder",
        "For `recorder_stub`, set `[runtime.clips] recorder = \"gstreamer\"` in /etc/nexus/nexus.toml and restart (INSTALL.md §9, \"Recorder writes 0-byte mp4 files\").",
    ));
    out.push(check_motion_recent(client, base, &enabled_ids));
    out.push(check_events_recent(client, base));
    out.push(check_npu_runtime());
    out.push(check_filesystem_posture());

    out
}

/// Phase 1.15 — surface `time.sync_state` and `time.skew_ms` from
/// the local chrony / timesyncd posture. Pass = clock is locked
/// onto an upstream source AND any reported skew is within the
/// actor_token verifier's ±30 000 ms window. Warn covers the
/// "locked but drifting close to the threshold" case so an
/// operator can fix it before the cloud starts rejecting RPCs.
fn check_time_sync() -> Outcome {
    let ts = time_sync::probe();
    let actual = match ts.skew_ms {
        Some(ms) => format!(
            "time.sync_state={} time.skew_ms={} ({})",
            ts.state.as_str(),
            ms,
            ts.detail
        ),
        None => format!(
            "time.sync_state={} time.skew_ms=unknown ({})",
            ts.state.as_str(),
            ts.detail
        ),
    };
    match ts.state {
        time_sync::SyncState::Synchronized => {
            // Synchronized but drifting → warn before cloud rejects.
            if let Some(ms) = ts.skew_ms {
                if ms.unsigned_abs() > 30_000 {
                    return Outcome::fail(
                        "9.0",
                        "time_sync",
                        "synchronized AND |skew_ms| <= 30000",
                        actual,
                        "actor_token verifier rejects ±30 s skew; check `chronyc tracking` and the upstream NTP source's reachability",
                    );
                }
                if ms.unsigned_abs() > 15_000 {
                    return Outcome::warn(
                        "9.0",
                        "time_sync",
                        "synchronized AND |skew_ms| <= 30000",
                        actual,
                        "approaching the ±30 s skew threshold; investigate NTP source quality before cloud starts dropping RPCs",
                    );
                }
            }
            Outcome::pass(
                "9.0",
                "time_sync",
                "synchronized AND |skew_ms| <= 30000",
                actual,
            )
        }
        time_sync::SyncState::Unsynchronized => Outcome::fail(
            "9.0",
            "time_sync",
            "synchronized AND |skew_ms| <= 30000",
            actual,
            "run `sudo systemctl restart chrony && chronyc tracking` — leap status must be `Normal`",
        ),
        time_sync::SyncState::Unavailable => Outcome::warn(
            "9.0",
            "time_sync",
            "synchronized AND |skew_ms| <= 30000",
            actual,
            "install chrony (`sudo apt install chrony`) or use systemd-timesyncd; doctor cannot verify clock sync without one of them",
        ),
    }
}

/// 9.1, plus the body as JSON when it parses: the engine's own health
/// roll-up, which [`fail_on_engine_issues`] grades later checks against.
fn check_health(client: &Client, base: &str) -> (Outcome, Option<Value>) {
    let url = format!("{base}/api/v1/health");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            if status == StatusCode::OK && !body.trim().is_empty() {
                (
                    Outcome::pass(
                        "9.1",
                        "engine_http",
                        "200 OK with non-empty body",
                        truncate(&body, 80),
                    ),
                    serde_json::from_str(&body).ok(),
                )
            } else {
                (
                    Outcome::fail(
                        "9.1",
                        "engine_http",
                        "200 OK with non-empty body",
                        format!("HTTP {status}; body={}", truncate(&body, 80)),
                        "INSTALL.md §11 row 1 (engine isn't up — check `systemctl status nexus-engine` / `docker compose ps`).",
                    ),
                    None,
                )
            }
        }
        Err(e) => (
            Outcome::fail(
                "9.1",
                "engine_http",
                "200 OK with non-empty body",
                format!("request failed: {e}"),
                "INSTALL.md §11 row 1 (engine isn't up — check `systemctl status nexus-engine` / `docker compose ps`).",
            ),
            None,
        ),
    }
}

/// Grade a check against the engine's own verdict for `component`: the
/// `issues[]` of `GET /api/v1/health`, which the engine computes from the
/// same rule as its heartbeat. Doctor reads that verdict rather than
/// re-deriving it, so it cannot pass a box the engine reports degraded for
/// the thing the check covers (BUG-225). A check that already failed keeps
/// its own reason. A body with no `issues` list says nothing either way, so
/// it cannot back a pass.
fn fail_on_engine_issues(
    check: Outcome,
    health: Option<&Value>,
    component: &str,
    hint: &'static str,
) -> Outcome {
    if check.status == Status::Fail {
        return check;
    }
    let expected = format!(
        "{}, and no {component} issue in /api/v1/health",
        check.expected
    );
    let Some(issues) = health
        .and_then(|h| h.get("issues"))
        .and_then(Value::as_array)
    else {
        return Outcome::warn(
            check.step,
            check.name,
            &expected,
            format!("{}; /api/v1/health carried no issues list", check.actual),
            "Doctor could not read the engine's own health verdict: `curl -fsS http://localhost:8089/api/v1/health`.",
        );
    };
    let codes: Vec<&str> = issues
        .iter()
        .filter(|i| i.get("component").and_then(Value::as_str) == Some(component))
        .map(|i| i.get("code").and_then(Value::as_str).unwrap_or("?"))
        .collect();
    if codes.is_empty() {
        return check;
    }
    Outcome::fail(
        check.step,
        check.name,
        &expected,
        format!("{}; engine reports {}", check.actual, codes.join(", ")),
        hint,
    )
}

fn check_ui_loads(client: &Client, base: &str) -> Outcome {
    let url = format!("{base}/");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if status == StatusCode::OK && ct.starts_with("text/html") {
                Outcome::pass(
                    "9.2",
                    "ui_loads",
                    "200 OK, content-type text/html",
                    format!("200 OK, content-type {ct}"),
                )
            } else {
                Outcome::fail(
                    "9.2",
                    "ui_loads",
                    "200 OK, content-type text/html",
                    format!("HTTP {status}, content-type {ct:?}"),
                    "INSTALL.md §11 row 2 (`ui_root` mismatch — confirm `[server].ui_root` points at a directory containing `index.html`).",
                )
            }
        }
        Err(e) => Outcome::fail(
            "9.2",
            "ui_loads",
            "200 OK, content-type text/html",
            format!("request failed: {e}"),
            "INSTALL.md §11 row 2 (`ui_root` mismatch — confirm `[server].ui_root` points at a directory containing `index.html`).",
        ),
    }
}

/// Result of `GET /api/v1/cameras` parsed enough to decide which IDs to
/// snapshot / motion-poll. We deliberately don't import
/// `nexus-config::CameraConfig` to keep the doctor crate's compile
/// graph small; a serde_json::Value walk is enough.
struct CameraSummary {
    total: usize,
    enabled: Vec<i64>,
    raw_ok: bool,
    error: Option<String>,
}

fn fetch_cameras(client: &Client, base: &str) -> CameraSummary {
    let url = format!("{base}/api/v1/cameras");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status != StatusCode::OK {
                return CameraSummary {
                    total: 0,
                    enabled: vec![],
                    raw_ok: false,
                    error: Some(format!("HTTP {status}")),
                };
            }
            match resp.json::<Value>() {
                Ok(Value::Array(arr)) => {
                    let mut enabled = Vec::new();
                    for cam in &arr {
                        let id = cam.get("id").and_then(|v| v.as_i64());
                        let is_enabled = cam
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if is_enabled {
                            if let Some(i) = id {
                                enabled.push(i);
                            }
                        }
                    }
                    CameraSummary {
                        total: arr.len(),
                        enabled,
                        raw_ok: true,
                        error: None,
                    }
                }
                Ok(other) => CameraSummary {
                    total: 0,
                    enabled: vec![],
                    raw_ok: false,
                    error: Some(format!("unexpected JSON shape: {}", short(&other))),
                },
                Err(e) => CameraSummary {
                    total: 0,
                    enabled: vec![],
                    raw_ok: false,
                    error: Some(format!("invalid JSON: {e}")),
                },
            }
        }
        Err(e) => CameraSummary {
            total: 0,
            enabled: vec![],
            raw_ok: false,
            error: Some(format!("request failed: {e}")),
        },
    }
}

fn build_cameras_outcome(cams: &CameraSummary) -> (Outcome, Vec<i64>) {
    if !cams.raw_ok {
        let msg = cams.error.clone().unwrap_or_else(|| "unknown".into());
        let outcome = Outcome::fail(
            "9.3",
            "cameras_listed",
            "200 OK, JSON array of camera configs",
            msg,
            "INSTALL.md §11 row 1 (engine HTTP layer / store unhealthy).",
        );
        return (outcome, vec![]);
    }
    if cams.total == 0 {
        let outcome = Outcome::warn(
            "9.3",
            "cameras_listed",
            "≥ 1 camera configured",
            "0 cameras configured".into(),
            "Add cameras via UI or `[[cameras]]` in `nexus.toml` (INSTALL.md §8.1).",
        );
        return (outcome, vec![]);
    }
    if cams.enabled.is_empty() {
        let outcome = Outcome::warn(
            "9.3",
            "cameras_listed",
            "≥ 1 enabled camera",
            format!("{} configured, 0 enabled", cams.total),
            "Set `enabled = true` on at least one camera in `nexus.toml` (INSTALL.md §8.1).",
        );
        return (outcome, vec![]);
    }
    let outcome = Outcome::pass(
        "9.3",
        "cameras_listed",
        "≥ 1 enabled camera",
        format!(
            "{} configured, {} enabled (ids: {:?})",
            cams.total,
            cams.enabled.len(),
            cams.enabled
        ),
    );
    (outcome, cams.enabled.clone())
}

fn check_snapshots(client: &Client, base: &str, enabled_ids: &[i64]) -> Outcome {
    if enabled_ids.is_empty() {
        return Outcome::skip(
            "9.4",
            "snapshot",
            "skipped — no enabled cameras to query".into(),
        );
    }
    let mut details = Vec::with_capacity(enabled_ids.len());
    let mut failures = 0;
    let mut empty_bodies = 0;
    for id in enabled_ids {
        let url = format!("{base}/api/v1/cameras/{id}/frames/latest");
        match client.get(&url).send() {
            Ok(resp) => {
                let status = resp.status();
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = resp.bytes().ok().map(|b| b.len()).unwrap_or(0);
                if status == StatusCode::OK && ct.starts_with("image/jpeg") && bytes > 0 {
                    details.push(format!("cam{id}: {bytes}B jpeg"));
                } else if status == StatusCode::OK && bytes == 0 {
                    empty_bodies += 1;
                    details.push(format!("cam{id}: 200 but empty body"));
                } else {
                    failures += 1;
                    details.push(format!("cam{id}: HTTP {status}, ct={ct:?}, {bytes}B"));
                }
            }
            Err(e) => {
                failures += 1;
                details.push(format!("cam{id}: request failed: {e}"));
            }
        }
    }
    let actual = details.join(" | ");
    if failures > 0 {
        Outcome::fail(
            "9.4",
            "snapshot",
            "200 image/jpeg with body > 0 for every enabled camera",
            actual,
            "INSTALL.md §11 row 3 (camera stuck on `connecting` — RTSP transport / credentials / VAAPI group membership).",
        )
    } else if empty_bodies > 0 {
        // 200 with empty body = camera is connected but no frame in
        // the cache yet. Real for the first ~5s after boot. Doctor
        // treats this as warn so an over-eager run after a fresh
        // restart doesn't lie about a healthy box.
        Outcome::warn(
            "9.4",
            "snapshot",
            "200 image/jpeg with body > 0 for every enabled camera",
            actual,
            "Camera is connected but no frame has reached the cache yet — wait 5 s and rerun (INSTALL.md §9.4).",
        )
    } else {
        Outcome::pass(
            "9.4",
            "snapshot",
            "200 image/jpeg with body > 0 for every enabled camera",
            actual,
        )
    }
}

fn check_backends_ready(client: &Client, base: &str) -> Outcome {
    let url = format!("{base}/api/v1/backends");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status != StatusCode::OK {
                return Outcome::fail(
                    "9.5",
                    "backends_ready",
                    "200 OK with all slots ready",
                    format!("HTTP {status}"),
                    "INSTALL.md §11 row 6 (NVIDIA Container Toolkit) or row 4 (`render` group membership).",
                );
            }
            let body: Value = match resp.json() {
                Ok(v) => v,
                Err(e) => {
                    return Outcome::fail(
                        "9.5",
                        "backends_ready",
                        "200 OK with all slots ready",
                        format!("invalid JSON: {e}"),
                        "INSTALL.md §11 row 6 (NVIDIA Container Toolkit) or row 4 (`render` group membership).",
                    );
                }
            };
            let mode = body.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
            let slots = body
                .get("slots")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // `mode = "in_process"` returns slots: [] — the engine is
            // intentionally bypassing the pool (M0/dev path). Treat as
            // pass-with-note rather than fail; pool mode is still the
            // canonical production deployment.
            if mode == "in_process" {
                return Outcome::warn(
                    "9.5",
                    "backends_ready",
                    "pool mode with all slots ready",
                    "mode=in_process (no DetectorPool slots; legitimate for M0 dev runs)".into(),
                    "Switch `inference.backend = \"pool\"` in nexus.toml for production (INSTALL.md §6.3).",
                );
            }
            let total = slots.len();
            let mut not_ready = Vec::new();
            for s in &slots {
                let st = s.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                if st != "ready" {
                    not_ready.push(format!(
                        "slot {} state={st}",
                        s.get("id")
                            .and_then(|v| v.as_i64())
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "?".into())
                    ));
                }
            }
            if total == 0 {
                Outcome::warn(
                    "9.5",
                    "backends_ready",
                    "≥ 1 ready slot",
                    "mode=pool but 0 slots reported".into(),
                    "Set `inference.workers ≥ 1` in nexus.toml (INSTALL.md §6.3).",
                )
            } else if not_ready.is_empty() {
                Outcome::pass(
                    "9.5",
                    "backends_ready",
                    "all slots ready",
                    format!("mode=pool, {total}/{total} slots ready"),
                )
            } else {
                Outcome::fail(
                    "9.5",
                    "backends_ready",
                    "all slots ready",
                    format!("mode=pool, {} not ready: {}", not_ready.len(), not_ready.join(", ")),
                    "INSTALL.md §11 row 9 (model sha256 mismatch) or row 6 (NVIDIA Container Toolkit).",
                )
            }
        }
        Err(e) => Outcome::fail(
            "9.5",
            "backends_ready",
            "200 OK with all slots ready",
            format!("request failed: {e}"),
            "INSTALL.md §11 row 1 (engine HTTP unreachable).",
        ),
    }
}

fn check_storage_local(client: &Client, base: &str) -> Outcome {
    let url = format!("{base}/api/v1/storage/local");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status != StatusCode::OK {
                return Outcome::fail(
                    "9.6",
                    "storage_local",
                    "200 OK, panic == false",
                    format!("HTTP {status}"),
                    "INSTALL.md §11 row 8 (storage panic — `df -h /var/lib/nexus/clips`).",
                );
            }
            let body: Value = match resp.json() {
                Ok(v) => v,
                Err(e) => {
                    return Outcome::fail(
                        "9.6",
                        "storage_local",
                        "200 OK, panic == false",
                        format!("invalid JSON: {e}"),
                        "INSTALL.md §11 row 8 (storage panic — `df -h /var/lib/nexus/clips`).",
                    );
                }
            };
            let panic = body.get("panic").and_then(|v| v.as_bool()).unwrap_or(true);
            let free_pct = body
                .get("free_pct")
                .and_then(|v| v.as_f64())
                .map(|p| format!("{:.1}%", p))
                .unwrap_or_else(|| "n/a".into());
            let recorder_kind = body
                .get("recorder_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let actual =
                format!("recorder_kind={recorder_kind}, free_pct={free_pct}, panic={panic}");
            if panic {
                Outcome::fail(
                    "9.6",
                    "storage_local",
                    "panic == false",
                    actual,
                    "INSTALL.md §11 row 8 (storage panic — `df -h /var/lib/nexus/clips`, lower retention or grow disk).",
                )
            } else {
                Outcome::pass("9.6", "storage_local", "panic == false", actual)
            }
        }
        Err(e) => Outcome::fail(
            "9.6",
            "storage_local",
            "200 OK, panic == false",
            format!("request failed: {e}"),
            "INSTALL.md §11 row 1 (engine HTTP unreachable).",
        ),
    }
}

fn check_motion_recent(client: &Client, base: &str, enabled_ids: &[i64]) -> Outcome {
    if enabled_ids.is_empty() {
        return Outcome::skip(
            "9.7",
            "motion_recent",
            "skipped — no enabled cameras to query".into(),
        );
    }
    // Look at the last 5 minutes — INSTALL.md §9.7 asks the operator
    // to walk in front of one camera; doctor reports counts but
    // doesn't fail on zero (no walker on the machine running CI).
    let to = chrono::Utc::now();
    let from = to - chrono::Duration::minutes(5);
    let from_s = to_rfc3339(from);
    let to_s = to_rfc3339(to);

    let mut details = Vec::with_capacity(enabled_ids.len());
    let mut http_failures = 0;
    let mut total_events = 0usize;
    for id in enabled_ids {
        let url = format!("{base}/api/v1/cameras/{id}/motion?from={from_s}&to={to_s}");
        match client.get(&url).send() {
            Ok(resp) => {
                let status = resp.status();
                if status != StatusCode::OK {
                    http_failures += 1;
                    details.push(format!("cam{id}: HTTP {status}"));
                    continue;
                }
                match resp.json::<Value>() {
                    Ok(Value::Array(arr)) => {
                        total_events += arr.len();
                        details.push(format!("cam{id}: {} events", arr.len()));
                    }
                    Ok(_) => details.push(format!("cam{id}: unexpected JSON shape")),
                    Err(e) => details.push(format!("cam{id}: invalid JSON: {e}")),
                }
            }
            Err(e) => {
                http_failures += 1;
                details.push(format!("cam{id}: request failed: {e}"));
            }
        }
    }
    let actual = format!("(last 5 min) {}", details.join(" | "));
    if http_failures > 0 {
        Outcome::fail(
            "9.7",
            "motion_recent",
            "200 OK from /api/v1/cameras/{id}/motion for every enabled camera",
            actual,
            "INSTALL.md §11 row 1 (engine HTTP) or §11 row 4 (camera RTSP stream).",
        )
    } else if total_events == 0 {
        // Operationally OK on a quiet box — informational only.
        Outcome::warn(
            "9.7",
            "motion_recent",
            "≥ 1 motion event in the last 5 min (informational)",
            actual,
            "Walk in front of a camera and re-run, or wait until the box sees real motion (INSTALL.md §9.7).",
        )
    } else {
        Outcome::pass(
            "9.7",
            "motion_recent",
            "motion events recorded in the last 5 min",
            actual,
        )
    }
}

fn check_events_recent(client: &Client, base: &str) -> Outcome {
    let url = format!("{base}/api/v1/events?limit=1");
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status != StatusCode::OK {
                return Outcome::fail(
                    "9.8",
                    "events_recent",
                    "200 OK from /api/v1/events",
                    format!("HTTP {status}"),
                    "INSTALL.md §11 row 1 (engine HTTP unreachable).",
                );
            }
            match resp.json::<Value>() {
                Ok(Value::Array(arr)) => {
                    if arr.is_empty() {
                        Outcome::warn(
                            "9.8",
                            "events_recent",
                            "≥ 1 alert event recorded (informational)",
                            "0 events in store".into(),
                            "Trigger the seeded `person_in_zone` rule by standing in frame for ≥ 2 s (INSTALL.md §9.8).",
                        )
                    } else {
                        Outcome::pass(
                            "9.8",
                            "events_recent",
                            "≥ 1 alert event in store",
                            format!("{} event(s) returned", arr.len()),
                        )
                    }
                }
                Ok(_) => Outcome::fail(
                    "9.8",
                    "events_recent",
                    "200 OK, JSON array",
                    "unexpected JSON shape".into(),
                    "INSTALL.md §11 row 1 (engine HTTP unreachable / store unhealthy).",
                ),
                Err(e) => Outcome::fail(
                    "9.8",
                    "events_recent",
                    "200 OK, JSON array",
                    format!("invalid JSON: {e}"),
                    "INSTALL.md §11 row 1 (engine HTTP unreachable / store unhealthy).",
                ),
            }
        }
        Err(e) => Outcome::fail(
            "9.8",
            "events_recent",
            "200 OK from /api/v1/events",
            format!("request failed: {e}"),
            "INSTALL.md §11 row 1 (engine HTTP unreachable).",
        ),
    }
}

/// 9.9 — Intel NPU runtime is wired correctly for OpenVINO.
///
/// When the box has an NPU (`/dev/accel/accel0` present) the OpenVINO
/// NPU plugin needs the oneAPI Level Zero loader (`libze1` →
/// `libze_loader.so.1`) to enumerate the device. Without it,
/// `OpenVINOExecutionProvider` registers fine — ORT happily logs
/// `ep_registered=["npu(via-openvino)", "cpu"]` — but the first
/// inference falls back to CPU because OV can't open the NPU. The
/// only journal hint is a single line:
///
/// ```text
/// [OpenVINO] You have selected wrong configuration value for the key 'device_type'.
/// ```
///
/// Verified in the field on Lunar Lake boxes provisioned with
/// the pre-libze1 install.sh. `scripts/lib/install-common.sh` now
/// installs `libze1` from `ppa:kobuk-team/intel-graphics`; this
/// check is the operator-visible repair signal for boxes that were
/// already provisioned before that landed.
///
/// Skips on non-Linux and on Linux boxes without `/dev/accel/accel0`
/// (no NPU hardware).
fn check_npu_runtime() -> Outcome {
    #[cfg(not(target_os = "linux"))]
    {
        Outcome::skip("9.9", "npu_runtime", "skipped — Linux-only check".into())
    }
    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/dev/accel/accel0").exists() {
            return Outcome::skip(
                "9.9",
                "npu_runtime",
                "skipped — no Intel NPU detected (/dev/accel/accel0 absent)".into(),
            );
        }
        if libze_loader_present() {
            if !kobuk_ppa_configured() {
                return Outcome::fail(
                    "9.9",
                    "npu_runtime",
                    "libze1 sourced from kobuk-team PPA (OV 2025.4.x NPU ABI)",
                    "libze1 installed but kobuk-team PPA missing — libze1 is the Ubuntu archive's 1.16.1, OV 2025.4.x NPU plugin reports 'Device NPU is not available'".into(),
                    "sudo add-apt-repository -y ppa:kobuk-team/intel-graphics && sudo apt update && sudo apt install -y libze1 && sudo systemctl restart nexus-engine — see INSTALL.md §5.3.",
                );
            }
            if let Some(missing) = openvino_npu_registry_problem() {
                return Outcome::fail(
                    "9.9",
                    "npu_runtime",
                    "OpenVINO plugins.xml registers NPU plugin",
                    missing,
                    "Re-stage the OpenVINO libs (release tarball >= the libze1 fix) or write /opt/nexus/current/lib/onnxruntime/plugins.xml referencing libopenvino_intel_npu_plugin.so — see INSTALL.md §5.3.",
                );
            }
            Outcome::pass(
                "9.9",
                "npu_runtime",
                "libze1 installed AND OV plugins.xml registers NPU",
                "libze_loader.so.1 resolvable, kobuk-team PPA configured, plugins.xml present and references libopenvino_intel_npu_plugin.so".into(),
            )
        } else {
            Outcome::fail(
                "9.9",
                "npu_runtime",
                "libze1 (Level Zero loader) installed",
                "/dev/accel/accel0 present but libze1 missing — OpenVINO NPU plugin will fall back to CPU".into(),
                "sudo apt install libze1 (from ppa:kobuk-team/intel-graphics) && sudo systemctl restart nexus-engine — see INSTALL.md §5.3.",
            )
        }
    }
}

/// Best-effort probe for the "OV libs present but plugins.xml missing"
/// failure mode. Returns `None` when everything checks out, or
/// `Some(actual)` describing the problem.
///
/// We probe both the release-staged path (`/opt/nexus/current/lib/
/// onnxruntime/`) and the dev path (`./lib/onnxruntime/` relative to
/// the running engine, in case the operator launched out of a build
/// tree). If neither directory exists, we skip — this check is only
/// meaningful when the engine binary is the release layout.
#[cfg(target_os = "linux")]
fn openvino_npu_registry_problem() -> Option<String> {
    use std::path::PathBuf;
    let candidates = [
        PathBuf::from("/opt/nexus/current/lib/onnxruntime"),
        PathBuf::from("/opt/nexus/lib/onnxruntime"),
    ];
    let dir = candidates.iter().find(|p| p.is_dir())?;
    let plugin_so = dir.join("libopenvino_intel_npu_plugin.so");
    let plugins_xml = dir.join("plugins.xml");
    if !plugin_so.exists() {
        return Some(format!(
            "{} missing (re-stage OV libs from release tarball)",
            plugin_so.display()
        ));
    }
    if !plugins_xml.exists() {
        return Some(format!(
            "{} missing (OV Core cannot enumerate NPU without it)",
            plugins_xml.display()
        ));
    }
    let body = std::fs::read_to_string(&plugins_xml).ok()?;
    if !body.contains("libopenvino_intel_npu_plugin.so") {
        return Some(format!(
            "{} present but does not reference libopenvino_intel_npu_plugin.so",
            plugins_xml.display()
        ));
    }
    None
}

#[cfg(target_os = "linux")]
fn libze_loader_present() -> bool {
    // Prefer dpkg-query — it's the canonical Debian/Ubuntu interface
    // for "is this package installed" and matches how install.sh
    // gates the installation.
    if let Ok(out) = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", "libze1"])
        .output()
    {
        if out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains("install ok installed")
        {
            return true;
        }
    }
    // Fallback for non-dpkg distros (Fedora / NixOS / a manual
    // /opt install) — probe the dynamic linker's cache directly.
    if let Ok(out) = std::process::Command::new("ldconfig").arg("-p").output() {
        if out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains("libze_loader.so.1")
        {
            return true;
        }
    }
    // Last-resort direct path check for the file the linker would
    // resolve, in case `ldconfig` is unavailable.
    [
        "/usr/lib/x86_64-linux-gnu/libze_loader.so.1",
        "/usr/lib64/libze_loader.so.1",
        "/usr/lib/libze_loader.so.1",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists())
}

/// True when `ppa:kobuk-team/intel-graphics` (or its private mirror)
/// is configured as an apt source. The Ubuntu noble archive ships
/// `libze1` 1.16.1, which the OpenVINO 2025.4.x NPU plugin (bundled
/// in `onnxruntime-openvino` ≥ 1.24.x) cannot drive — OV reports
/// `Device NPU is not available` at session create. The kobuk-team
/// PPA ships a newer `libze1` matched to `intel-level-zero-npu`
/// 1.32.x. When `libze1` is installed but the PPA is absent, the
/// loader is almost certainly the stale archive build.
#[cfg(target_os = "linux")]
fn kobuk_ppa_configured() -> bool {
    for dir in ["/etc/apt/sources.list.d", "/etc/apt"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let ext_ok = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "list" | "sources"));
            let name_ok = p.file_name().and_then(|n| n.to_str()) == Some("sources.list");
            if !ext_ok && !name_ok {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&p) {
                if body.contains("kobuk-team/intel-graphics") {
                    return true;
                }
            }
        }
    }
    false
}

/// Phase 6.14b — surface the mount-option posture of the two
/// directories that hold edge-side secrets at rest:
///
/// * `/etc/nexus/tls/` — mTLS client cert + key used by
///   `nexus_cloud_client::tunnel`.
/// * `/var/lib/nexus/` — SQLite database holding the local-auth
///   admin secret and cloud enrollment row.
///
/// Both directories are mode-locked by the installer
/// (`/etc/nexus/tls` → `2750 root:nexus`, `/var/lib/nexus` →
/// `0750 nexus:nexus`).  Mount-option hardening (`nodev`,
/// `nosuid`) is defence-in-depth: it costs nothing at runtime and
/// blocks a local attacker who chrooted in from honouring device
/// nodes or setuid binaries dropped under either path.
///
/// Severity is **warn**, never fail — Ubuntu Server's default
/// single-root-mount layout has neither flag on `/`, and that is
/// not actually broken (the file modes still keep the secrets
/// private).  The probe surfaces the gap so an operator who *can*
/// add the flags via `/etc/fstab` knows the contract.
///
/// Skips on non-Linux (`/proc/self/mountinfo` does not exist on
/// macOS / Windows).
fn check_filesystem_posture() -> Outcome {
    #[cfg(not(target_os = "linux"))]
    {
        Outcome::skip(
            "9.10",
            "filesystem_posture",
            "skipped — Linux-only check (no /proc/self/mountinfo)".into(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;
        let paths: [&Path; 2] = [Path::new("/etc/nexus/tls"), Path::new("/var/lib/nexus")];
        let postures = fs_posture::probe_paths(&paths);

        // Render a one-line `actual` summary that names each
        // inspected path and the flags that are present.
        let mut summary_parts = Vec::with_capacity(postures.len());
        let mut missing_lines = Vec::new();
        let mut any_missing = false;
        let mut probe_failed = false;
        for p in &postures {
            match &p.mount_point {
                Some(mp) => {
                    let flags = if p.is_hardened() {
                        "nodev,nosuid".to_string()
                    } else if p.missing_flags().is_empty() {
                        "ok".to_string()
                    } else {
                        format!("MISSING={}", p.missing_flags().join(","))
                    };
                    summary_parts.push(format!(
                        "{} (on {}): {}",
                        p.path.display(),
                        mp.display(),
                        flags,
                    ));
                    if !p.is_hardened() {
                        any_missing = true;
                        missing_lines.push(format!(
                            "{} on {} missing {}",
                            p.path.display(),
                            mp.display(),
                            p.missing_flags().join(","),
                        ));
                    }
                }
                None => {
                    probe_failed = true;
                    summary_parts.push(format!(
                        "{}: no matching mount ({})",
                        p.path.display(),
                        p.detail,
                    ));
                }
            }
        }

        let actual = summary_parts.join(" | ");
        if probe_failed {
            return Outcome::warn(
                "9.10",
                "filesystem_posture",
                "every secret path resolves to a mountinfo entry",
                actual,
                "could not resolve a mount point for one or more secret paths — verify /var/lib/nexus and /etc/nexus/tls exist (re-run `scripts/install.sh`).",
            );
        }
        if any_missing {
            Outcome::warn(
                "9.10",
                "filesystem_posture",
                "/etc/nexus/tls and /var/lib/nexus on a mount with nodev,nosuid",
                actual,
                "INSTALL.md §9 — add `nodev,nosuid` to the fstab line covering the secret paths and `sudo mount -o remount` (single-root-mount layouts can be ignored on appliances where the operator is the only local user).",
            )
        } else {
            Outcome::pass(
                "9.10",
                "filesystem_posture",
                "/etc/nexus/tls and /var/lib/nexus on a mount with nodev,nosuid",
                actual,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tiny helpers (color, formatting, TTY detect)
// ---------------------------------------------------------------------------

/// ANSI codes; an empty struct field becomes a no-op when color is
/// disabled. Cheaper than threading a bool everywhere.
struct Theme {
    bold: &'static str,
    dim: &'static str,
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    reset: &'static str,
}

impl Theme {
    fn new(use_color: bool) -> Self {
        if use_color {
            Self {
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                red: "\x1b[31m",
                green: "\x1b[32m",
                yellow: "\x1b[33m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                bold: "",
                dim: "",
                red: "",
                green: "",
                yellow: "",
                reset: "",
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        trimmed.into()
    } else {
        let mut out: String = trimmed.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    truncate(&s, 80)
}

fn to_rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    // Use a `Z` suffix for cleaner URL-encodable bytes (no `+00:00`).
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Cheap TTY probe via `std::io::IsTerminal` (stable since 1.70). No
/// extra dep — the workspace MSRV is 1.88.
fn atty_stdout() -> bool {
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A live engine as 9.5 and 9.6 see it: every backend slot ready, storage
    /// not in panic, and `issues` as its `GET /api/v1/health` roll-up. Every
    /// other route 404s, so the checks that read them fail; no test here
    /// asserts on those.
    struct FakeEngine {
        server: MockServer,
        // Declared after `server` so the server drops while its runtime is
        // still up.
        _rt: tokio::runtime::Runtime,
    }

    fn fake_engine(health: Value) -> FakeEngine {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(async {
            let server = MockServer::start().await;
            for (route, body) in [
                ("/api/v1/health", health),
                (
                    "/api/v1/storage/local",
                    json!({ "recorder_kind": "stub", "panic": false, "free_pct": 80.0 }),
                ),
                (
                    "/api/v1/backends",
                    json!({ "mode": "pool", "slots": [{ "id": 0, "state": "ready" }] }),
                ),
            ] {
                Mock::given(method("GET"))
                    .and(path(route))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .mount(&server)
                    .await;
            }
            server
        });
        FakeEngine { server, _rt: rt }
    }

    /// `GET /api/v1/health` as the engine builds it: `degraded` iff an issue
    /// is open.
    fn health_with(issues: &[(&str, &str)]) -> Value {
        let issues: Vec<Value> = issues
            .iter()
            .map(|(component, code)| json!({ "component": component, "code": code }))
            .collect();
        json!({
            "status": if issues.is_empty() { "ok" } else { "degraded" },
            "version": "test",
            "issues": issues,
        })
    }

    fn run(engine: &FakeEngine) -> Vec<Outcome> {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client");
        run_checks(&client, &engine.server.uri())
    }

    fn status_of(outcomes: &[Outcome], step: &str) -> Status {
        outcomes
            .iter()
            .find(|o| o.step == step)
            .unwrap_or_else(|| panic!("no {step} outcome in {outcomes:?}"))
            .status
    }

    /// BUG-225: a stub recorder behind enabled cameras keeps no video, and the
    /// engine says so in its health roll-up. The storage check read only the
    /// panic flag, so it passed that box.
    #[test]
    fn a_recorder_issue_in_the_engine_health_fails_the_storage_check() {
        let engine = fake_engine(health_with(&[("recorder", "recorder_stub")]));
        let outcomes = run(&engine);
        assert_eq!(status_of(&outcomes, "9.6"), Status::Fail, "{outcomes:?}");
        assert_eq!(
            status_of(&outcomes, "9.5"),
            Status::Pass,
            "a recorder issue is not a backend failure: {outcomes:?}"
        );
    }

    /// The same defect for the detector: a detector that failed to build is
    /// replaced by one that reports nothing, and its slot still reads `ready`.
    #[test]
    fn a_detector_issue_in_the_engine_health_fails_the_backends_check() {
        let engine = fake_engine(health_with(&[("detector", "detector_unavailable")]));
        let outcomes = run(&engine);
        assert_eq!(status_of(&outcomes, "9.5"), Status::Fail, "{outcomes:?}");
        assert_eq!(
            status_of(&outcomes, "9.6"),
            Status::Pass,
            "a detector issue is not a storage failure: {outcomes:?}"
        );
    }

    #[test]
    fn an_engine_reporting_no_issues_passes_the_storage_and_backends_checks() {
        let engine = fake_engine(health_with(&[]));
        let outcomes = run(&engine);
        assert_eq!(status_of(&outcomes, "9.5"), Status::Pass, "{outcomes:?}");
        assert_eq!(status_of(&outcomes, "9.6"), Status::Pass, "{outcomes:?}");
    }

    /// A health body with no `issues` list says nothing about the recorder or
    /// the detector, so it cannot back a pass for either.
    #[test]
    fn a_health_body_without_an_issues_list_cannot_pass_either_check() {
        let engine = fake_engine(json!({ "status": "ok" }));
        let outcomes = run(&engine);
        assert_eq!(status_of(&outcomes, "9.5"), Status::Warn, "{outcomes:?}");
        assert_eq!(status_of(&outcomes, "9.6"), Status::Warn, "{outcomes:?}");
    }
}
