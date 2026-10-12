//! Streaming diagnostics tarball: `GET /v1/admin/diagnostics/export`.
//! Split out of `admin_runtime`; `diag_collect` is its cloud-triggered consumer.

use std::io::Write;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use chrono::{Duration as ChronoDuration, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use nexus_config::AuthMode;
use nexus_store::audit::{AuditFilter, AuditOutcome};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::AdminContext;

// ===========================================================
// Section 3 — Streaming diagnostics tarball
// ===========================================================

const DEFAULT_AUDIT_LIMIT: i64 = 1000;
const DEFAULT_MOTION_LIMIT: i64 = 100;
/// Default number of recent journal lines to bundle when the caller
/// doesn't override `log_lines`. ~10k lines is a few hours on a busy
/// multi-camera box and gzips to well under a megabyte.
const DEFAULT_LOG_LINES: i64 = 10_000;
/// Hard ceiling on requested journal lines. Guards the local streaming
/// download (which, unlike the cloud collector, has no `max_bytes`
/// budget) against a pathological `?log_lines=` value.
const MAX_LOG_LINES: i64 = 200_000;
/// Absolute byte cap on the collected + redacted journal text baked into
/// the tarball, independent of the line cap. The most-recent bytes are
/// kept; anything older is dropped with a marker at the top.
const MAX_LOG_BYTES: usize = 32 * 1024 * 1024;
/// systemd unit whose journal the diagnostics bundle captures.
const ENGINE_UNIT: &str = "nexus-engine.service";

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    /// Cap on the number of recent audit rows to include.
    /// Default 1000; capped at 10 000 to keep the tarball
    /// bounded.
    #[serde(default)]
    pub audit_limit: Option<i64>,
    /// Cap on the number of recent motion events. Default
    /// 100; capped at 10 000.
    #[serde(default)]
    pub motion_limit: Option<i64>,
    /// Operator opt-in: include a secret-scrubbed copy of the
    /// live SQLite state DB (`state/nexus-state.sqlite`) in the
    /// tarball. Default false. The snapshot drops every
    /// secret-bearing table and redacts every secret column
    /// (see `nexus_store::diag_snapshot`); if it can't be built
    /// the export still streams without it and the
    /// `X-Nexus-Diag-Sqlite` response header reports `failed`.
    #[serde(default)]
    pub include_sqlite: Option<bool>,
    /// Cap on the number of recent engine journal lines
    /// (`journalctl -u nexus-engine`, 24h window) to bundle into
    /// `nexus-engine.log`. Default 10 000; capped at 200 000. `0`
    /// omits the log body (a short note is written instead).
    #[serde(default)]
    pub log_lines: Option<i64>,
}

pub async fn get_diagnostics_export(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let audit_limit = q
        .audit_limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(0, 10_000);
    let motion_limit = q
        .motion_limit
        .unwrap_or(DEFAULT_MOTION_LIMIT)
        .clamp(0, 10_000);
    let include_sqlite = q.include_sqlite.unwrap_or(false);
    let log_lines = q
        .log_lines
        .unwrap_or(DEFAULT_LOG_LINES)
        .clamp(0, MAX_LOG_LINES);

    // Gather everything that needs the tokio runtime BEFORE
    // we hand off to spawn_blocking. The tar writer itself
    // runs sync; pre-computing the bytes here keeps the
    // blocking task pure-CPU and avoids smuggling a Handle
    // across the thread boundary.
    let snapshot = build_snapshot(&s, audit_limit, motion_limit, include_sqlite, log_lines).await;
    let sqlite_status = snapshot.sqlite_status;

    audit_admin_action(
        &s.store,
        Some(&admin.0),
        &headers,
        peer.ip(),
        "diagnostics.export",
        "admin/diagnostics",
        Some("export"),
        AuditOutcome::Success,
        None,
        Some(
            &serde_json::json!({
                "audit_limit": audit_limit,
                "motion_limit": motion_limit,
                "audit_count": snapshot.audit_count,
                "motion_count": snapshot.motion_count,
                "redacted": true,
                "include_sqlite": include_sqlite,
                "sqlite_included": sqlite_status == SqliteSnapshotStatus::Included,
                "log_lines": log_lines,
                "log_status": snapshot.journal_status.as_str(),
            })
            .to_string(),
        ),
    )
    .await;

    let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);

    tokio::task::spawn_blocking(move || {
        let writer = MpscWriter::new(tx.clone());
        let gz = GzEncoder::new(writer, Compression::default());
        let mut tar = tar::Builder::new(gz);
        if let Err(e) = write_tar_entries(&mut tar, &snapshot) {
            tracing::warn!(error = %e, "diagnostics tar build failed mid-stream");
            // Force the channel closed so the client gets a
            // truncated body instead of hanging.
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("tar build: {e}"))));
            return;
        }
        // `into_inner()` on tar::Builder flushes the trailer;
        // GzEncoder's drop flushes its footer when the
        // builder drops. `finish()` is the explicit form.
        let gz = match tar.into_inner() {
            Ok(gz) => gz,
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::other(format!("tar trailer: {e}"))));
                return;
            }
        };
        if let Err(e) = gz.finish() {
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("gzip finish: {e}"))));
        }
    });

    let stream = ReceiverStream::new(rx);
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("nexus-diagnostics-{ts}.tar.gz");

    let body = Body::from_stream(stream);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header("X-Content-Type-Options", "nosniff")
        // Tells the cloud-side collector (`diag_collect.rs`) whether the
        // optional sqlite snapshot made it into the stream, so it can set
        // `diag_ready.error_code = include_sqlite_unavailable` on a
        // partial success without re-parsing the tarball.
        .header("X-Nexus-Diag-Sqlite", sqlite_status.header_value())
        .body(body)
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("response build: {e}"),
            )
        })?;
    Ok(resp)
}

struct DiagnosticsSnapshot {
    redacted_config_toml: String,
    system_metrics_json: String,
    audit_json: String,
    motion_events_json: String,
    storage_backends_json: String,
    build_info_json: String,
    /// Per-camera frame throughput and decode health. The host metrics
    /// above are box-wide, so before this the bundle could not answer the
    /// first question a stale-video report raises: which cameras, and is
    /// the picture being damaged before the decoder or recycled after it
    /// (BUG-071).
    camera_stats_json: String,
    /// Recent engine journal (`journalctl -u nexus-engine`), URL
    /// credentials redacted, size-capped. Always present — on a read
    /// failure it holds a short explanatory note instead of log lines.
    journal_log: String,
    /// Whether the journal was collected, empty, or unavailable —
    /// surfaced in the export audit record.
    journal_status: JournalStatus,
    /// Secret-scrubbed copy of the live SQLite state DB. `Some` only
    /// when `include_sqlite` was requested AND the snapshot succeeded.
    sqlite_bytes: Option<Vec<u8>>,
    /// Whether the sqlite snapshot was omitted, included, or attempted
    /// and failed — surfaced to the collector via a response header.
    sqlite_status: SqliteSnapshotStatus,
    audit_count: usize,
    motion_count: usize,
    generated_at: chrono::DateTime<chrono::Utc>,
}

/// Disposition of the optional sqlite snapshot for one export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqliteSnapshotStatus {
    /// Not requested (`include_sqlite=false`).
    Omitted,
    /// Requested and bundled.
    Included,
    /// Requested but the scrubbed snapshot could not be built; the
    /// tarball streams without it (partial success).
    Failed,
}

impl SqliteSnapshotStatus {
    /// Stable `X-Nexus-Diag-Sqlite` header value.
    fn header_value(self) -> &'static str {
        match self {
            Self::Omitted => "omitted",
            Self::Included => "included",
            Self::Failed => "failed",
        }
    }
}

/// Disposition of the bundled engine journal for one export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalStatus {
    /// `journalctl` returned log lines that were bundled.
    Collected,
    /// `journalctl` ran but produced no entries for the window (fresh
    /// boot, or the caller passed `log_lines=0`).
    Empty,
    /// `journalctl` could not be run or was denied read access to the
    /// system journal. The tarball still ships, with an explanatory note
    /// in place of the log body.
    Unavailable,
}

impl JournalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Collected => "collected",
            Self::Empty => "empty",
            Self::Unavailable => "unavailable",
        }
    }
}

async fn build_snapshot(
    s: &ApiState,
    audit_limit: i64,
    motion_limit: i64,
    include_sqlite: bool,
    log_lines: i64,
) -> DiagnosticsSnapshot {
    let now = Utc::now();

    let redacted_config_toml = build_redacted_config_toml(s).await;

    let system_metrics_json = {
        let snap = crate::system_metrics::snapshot();
        serde_json::to_string_pretty(&*snap).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };

    let (audit_json, audit_count) = match s
        .store
        .list_audit_filtered(&AuditFilter::default(), audit_limit, 0)
        .await
    {
        Ok(rows) => {
            let n = rows.len();
            let body = serde_json::to_string_pretty(&rows)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            // Audit payloads are wholesale serialisations of the mutated
            // object, so pre-fix camera rows still at rest carry
            // `user:pass@`. Same scrub every log line already gets.
            (redact_url_credentials(&body), n)
        }
        Err(e) => (format!("{{\"error\":\"{e}\"}}"), 0),
    };

    let (motion_events_json, motion_count) = {
        // 24h window, newest-first cap of `motion_limit`. The
        // store API doesn't have a "give me the last N rows
        // across all cameras" call without a time bound; 24h
        // matches the existing UI default for the events feed
        // and keeps the JSON well under a few MB for typical
        // deployments.
        let to = now;
        let from = now - ChronoDuration::hours(24);
        match s
            .store
            .list_motion_events_across_cameras(None, from, to, motion_limit)
            .await
        {
            Ok(rows) => {
                let n = rows.len();
                let body = serde_json::to_string_pretty(&rows)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                (body, n)
            }
            Err(e) => (format!("{{\"error\":\"{e}\"}}"), 0),
        }
    };

    let storage_backends_json = {
        // `Registry::snapshot()` returns `Vec<(handle, kind)>`.
        // Backend-specific config is intentionally omitted —
        // many backends include OAuth refresh tokens or API
        // keys; we surface only the operator-visible label +
        // kind so support can map the bundle back to the
        // storage admin tab without leaking secrets.
        let entries = s.registry.snapshot();
        let view: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|(handle, kind)| {
                serde_json::json!({
                    "handle": handle,
                    "kind": kind,
                })
            })
            .collect();
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };

    // Per-camera throughput + decode health, sorted by camera id so two
    // bundles from the same box diff cleanly. Counters only — no URL, name
    // or credential — so this needs no redaction pass.
    let camera_stats_json = {
        let mut rows: Vec<(nexus_types::CameraId, nexus_pipeline::CameraFrameStats)> =
            s.frame_stats.snapshot_all().into_iter().collect();
        rows.sort_by_key(|(id, _)| *id);
        let view: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(id, st)| {
                let dh = s.decode_health.snapshot(id).unwrap_or_default();
                serde_json::json!({
                    "camera_id": id,
                    "last_frame_at": st.last_frame_at,
                    "last_frame_age_ms": st.last_frame_age_ms(std::time::Instant::now()),
                    "fps_ema": st.fps_ema,
                    "frames_emitted": st.frames_emitted,
                    "frames_dropped": st.frames_dropped,
                    "frames_backpressure_dropped": st.frames_backpressure_dropped,
                    "analysed_fps": st.analysed_fps,
                    "source_width": st.source_width,
                    "source_height": st.source_height,
                    "decoder_input_drops": dh.decoder_input_drops,
                    "decoder_output_frames": dh.decoder_output_frames,
                    "sampled_frames": dh.sampled_frames,
                    "duplicate_frames": dh.duplicate_frames,
                    "duplicate_per_mille": dh.duplicate_per_mille(),
                })
            })
            .collect();
        serde_json::to_string_pretty(&view).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };

    let build_info_json = serde_json::to_string_pretty(&serde_json::json!({
        // See `build.rs` — release-tag at CI build-time, falls back
        // to `CARGO_PKG_VERSION` for local dev builds.
        "engine_version": env!("NEXUS_BUILD_VERSION"),
        "generated_at": now.to_rfc3339(),
        "current_bind": s.current_bind,
        "auth_mode": format!("{:?}", s.auth_mode),
        "recorder_kind": s.recorder.kind(),
        "low_watermark_pct": s.low_watermark_pct,
        "panic_watermark_pct": s.panic_watermark_pct,
    }))
    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));

    // Optional secret-scrubbed sqlite snapshot. On any failure we log
    // and downgrade to a partial success (Failed) rather than aborting
    // the whole export — the logs/metrics are still worth shipping.
    let (sqlite_bytes, sqlite_status) = if include_sqlite {
        match s.store.export_scrubbed_snapshot().await {
            Ok(bytes) => (Some(bytes), SqliteSnapshotStatus::Included),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "diagnostics sqlite snapshot failed; streaming bundle without it"
                );
                (None, SqliteSnapshotStatus::Failed)
            }
        }
    } else {
        (None, SqliteSnapshotStatus::Omitted)
    };

    // Recent engine journal for stream-drop / pipeline troubleshooting.
    // Never aborts the export: a failure yields an explanatory note.
    let (journal_log, journal_status) = collect_journal_log(log_lines).await;

    DiagnosticsSnapshot {
        redacted_config_toml,
        system_metrics_json,
        audit_json,
        motion_events_json,
        storage_backends_json,
        build_info_json,
        journal_log,
        journal_status,
        sqlite_bytes,
        sqlite_status,
        camera_stats_json,
        audit_count,
        motion_count,
        generated_at: now,
    }
}

async fn build_redacted_config_toml(s: &ApiState) -> String {
    // We don't hold the parsed `Config` on ApiState. The
    // operator-visible view is reconstructed from the bits
    // we DO have: current bind, auth mode (sans secrets),
    // recorder kind, watermarks. This is a stripped projection
    // for support — operators who want the byte-exact
    // `nexus.toml` can read it off disk; this snapshot exists
    // so the bundle is self-contained without paths.
    let mut s_out = String::new();
    s_out.push_str("# Redacted runtime view — secrets stripped\n");
    s_out.push_str("# Generated by GET /v1/admin/diagnostics/export\n\n");
    s_out.push_str("[server]\n");
    s_out.push_str(&format!("api_bind = \"{}\"\n\n", s.current_bind));
    s_out.push_str("[auth]\n");
    s_out.push_str(&format!("mode = \"{}\"\n", auth_mode_str(s.auth_mode)));
    if s.oidc_display_name.is_some() {
        s_out.push_str("oidc = { issuer = \"<redacted>\", audience = \"<redacted>\" }\n");
    }
    s_out.push_str("\n[runtime.clips]\n");
    s_out.push_str(&format!("low_watermark_pct = {}\n", s.low_watermark_pct));
    s_out.push_str(&format!(
        "panic_watermark_pct = {}\n",
        s.panic_watermark_pct
    ));
    s_out.push_str(&format!("recorder = \"{}\"\n", s.recorder.kind()));
    if let Some(label) = s.preferred_usb_label.get() {
        s_out.push_str(&format!("preferred_usb_label = \"{label}\"\n"));
    }
    s_out
}

fn auth_mode_str(m: AuthMode) -> &'static str {
    match m {
        AuthMode::Local => "local",
        AuthMode::Oidc => "oidc",
        AuthMode::Hybrid => "hybrid",
    }
}

/// Strip `user:pass@` credentials from any `scheme://userinfo@host` URL
/// embedded in `input`, replacing the userinfo with `<redacted>`.
///
/// The diagnostics tarball uploads to cloud Blob (edge Hard Rule 6 — no
/// camera credentials over the tunnel). The engine keys its own pipeline
/// logs on `camera_id` rather than the URL, but a GStreamer / reqwest
/// error string could still echo an endpoint with embedded credentials,
/// so every log (and stderr) line is run through this scrub before it can
/// enter the bundle.
///
/// The authority component ends at the first `/`, `?`, `#`, or any
/// whitespace / quote / delimiter — mirroring how a URL parser tokenises
/// it — so a stray `@` inside a query string (`?to=a@b`) is never
/// mistaken for userinfo.
pub(crate) fn redact_url_credentials(input: &str) -> String {
    const MARKER: &str = "://";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(MARKER) {
        let after = pos + MARKER.len();
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        let authority_end = tail
            .find(|c: char| {
                matches!(
                    c,
                    '/' | '?'
                        | '#'
                        | '"'
                        | '\''
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                ) || c.is_whitespace()
            })
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        // Gap G4: use the LAST '@' as the userinfo/host boundary, per RFC
        // 3986 (`userinfo = *( unreserved / pct-encoded / sub-delims / ":" )`,
        // where `@` is itself a legal — if unusual — password character).
        // Splitting on the FIRST '@' instead would treat a password like
        // `p@ss` as if `ss@host` were the host, leaking the tail.
        match authority.rfind('@') {
            // Userinfo present → keep only the host after the last '@'.
            Some(at) => {
                out.push_str("<redacted>@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Keep at most the last `max_bytes` of `s`, trimming from the front on a
/// UTF-8 char + line boundary and prefixing a marker noting how much was
/// dropped. The tail (most-recent lines) is what matters when reading
/// back logs, so we drop the oldest.
fn cap_tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    if let Some(nl) = s[start..].find('\n') {
        start += nl + 1;
    }
    format!(
        "# … {start} earlier bytes truncated to stay within the {max_bytes}-byte log cap …\n{}",
        &s[start..]
    )
}

/// Collect the most-recent engine journal lines for the diagnostics
/// bundle via `journalctl -u nexus-engine`.
///
/// The engine logs to the systemd journal (stdout → journald); that is
/// the only durable log store on the box and, crucially, it survives the
/// process restarts a crash-looping stream drop can trigger — an
/// in-process ring buffer would lose exactly the pre-crash lines an
/// operator needs. Read access comes from the `systemd-journal`
/// supplementary group granted in `deploy/systemd/nexus-engine.service`.
///
/// Never fails the export: on a missing binary, non-zero exit, or denied
/// journal access the returned text is a short human-readable note and
/// the status is [`JournalStatus::Unavailable`]. Output is credential-
/// scrubbed ([`redact_url_credentials`]) and byte-capped
/// ([`cap_tail_bytes`]) before it can enter the tarball.
async fn collect_journal_log(max_lines: i64) -> (String, JournalStatus) {
    let header = format!(
        "# nexus-engine journal — unit {ENGINE_UNIT}, last {max_lines} lines, 24h window, \
         URL credentials redacted\n# generated {}\n\n",
        Utc::now().to_rfc3339(),
    );

    if max_lines <= 0 {
        return (
            format!("{header}(log collection disabled: log_lines=0)\n"),
            JournalStatus::Empty,
        );
    }

    // `journalctl` reads the persistent/volatile system journal directly;
    // filtering by `--unit` needs system-journal read access, which the
    // service's `systemd-journal` supplementary group provides. `LC_ALL=C`
    // + `SYSTEMD_COLORS=0` keep the output stable and ANSI-free.
    let output = tokio::process::Command::new("journalctl")
        .arg(format!("--unit={ENGINE_UNIT}"))
        .arg("--since=24 hours ago")
        .arg(format!("--lines={max_lines}"))
        .arg("--output=short-iso")
        .arg("--no-pager")
        .arg("--quiet")
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            if raw.trim().is_empty() {
                return (
                    format!("{header}(no journal entries in the last 24h)\n"),
                    JournalStatus::Empty,
                );
            }
            let capped = cap_tail_bytes(&redact_url_credentials(&raw), MAX_LOG_BYTES);
            (format!("{header}{capped}"), JournalStatus::Collected)
        }
        Ok(out) => {
            let code = out.status.code();
            let stderr = redact_url_credentials(String::from_utf8_lossy(&out.stderr).trim());
            tracing::warn!(
                exit_code = ?code,
                "diagnostics journal collection failed; bundling without logs"
            );
            let detail = if stderr.is_empty() {
                "no stderr".to_string()
            } else {
                stderr
            };
            (
                format!("{header}(journalctl exited with status {code:?}: {detail})\n"),
                JournalStatus::Unavailable,
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not run journalctl for diagnostics bundle");
            (
                format!("{header}(journalctl unavailable: {e})\n"),
                JournalStatus::Unavailable,
            )
        }
    }
}

fn write_tar_entries<W: Write>(
    tar: &mut tar::Builder<W>,
    snap: &DiagnosticsSnapshot,
) -> std::io::Result<()> {
    let mtime = snap.generated_at.timestamp().max(0) as u64;
    write_entry(
        tar,
        "nexus.toml.redacted",
        &snap.redacted_config_toml,
        mtime,
    )?;
    write_entry(tar, "system-metrics.json", &snap.system_metrics_json, mtime)?;
    write_entry(tar, "audit-log.json", &snap.audit_json, mtime)?;
    write_entry(tar, "motion-events.json", &snap.motion_events_json, mtime)?;
    write_entry(
        tar,
        "storage-backends.json",
        &snap.storage_backends_json,
        mtime,
    )?;
    write_entry(tar, "build-info.json", &snap.build_info_json, mtime)?;
    write_entry(tar, "camera-stats.json", &snap.camera_stats_json, mtime)?;
    write_entry(tar, "nexus-engine.log", &snap.journal_log, mtime)?;
    if let Some(bytes) = &snap.sqlite_bytes {
        write_entry_bytes(tar, "state/nexus-state.sqlite", bytes, mtime)?;
    }
    Ok(())
}

fn write_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    body: &str,
    mtime: u64,
) -> std::io::Result<()> {
    write_entry_bytes(tar, name, body.as_bytes(), mtime)
}

fn write_entry_bytes<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
    mtime: u64,
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_path(name)?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    header.set_cksum();
    tar.append(&header, bytes)
}

// Tiny std::io::Write impl that ships bytes into a tokio
// mpsc channel as `bytes::Bytes`. Blocking-side (the tar
// writer runs in spawn_blocking) uses `blocking_send` so
// backpressure from a slow client surfaces here as a wait
// rather than silently dropping data. If the channel is
// closed mid-stream (client disconnected) we return
// `BrokenPipe` so the tar builder bails out instead of
// looping forever.
struct MpscWriter {
    tx: mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
}

impl MpscWriter {
    fn new(tx: mpsc::Sender<Result<bytes::Bytes, std::io::Error>>) -> Self {
        Self { tx }
    }
}

impl Write for MpscWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len();
        let chunk = bytes::Bytes::copy_from_slice(buf);
        self.tx.blocking_send(Ok(chunk)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected")
        })?;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // mpsc has no flush; the next write will block on
        // capacity anyway.
        Ok(())
    }
}

// Note on backpressure: when the client is reading slowly
// the blocking sender naturally backs up. tokio::sync::mpsc's
// `blocking_send` doesn't take a timeout, so we lean on the
// upstream HTTP server's per-request body timeout to bound
// the worst case under pathologically slow clients.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_url_userinfo() {
        // RTSP with embedded creds — the classic camera-URL leak.
        assert_eq!(
            redact_url_credentials("connect rtsp://admin:s3cret@10.0.0.5:554/stream failed"),
            "connect rtsp://<redacted>@10.0.0.5:554/stream failed"
        );
        // No userinfo → untouched.
        assert_eq!(
            redact_url_credentials("GET https://host.example/path?a=1"),
            "GET https://host.example/path?a=1"
        );
        // A stray '@' in a query string is NOT userinfo — must survive.
        assert_eq!(
            redact_url_credentials("https://host/cb?to=a@b.com"),
            "https://host/cb?to=a@b.com"
        );
        // Two URLs on one line, both scrubbed independently.
        assert_eq!(
            redact_url_credentials("a rtsp://u:p@h1/s and http://x:y@h2/ b"),
            "a rtsp://<redacted>@h1/s and http://<redacted>@h2/ b"
        );
        // Nothing URL-shaped → identity.
        assert_eq!(redact_url_credentials("plain log line"), "plain log line");
        // Gap G4: a password containing '@' must not leave a credential
        // tail after the redaction. The authority's LAST '@' is the
        // userinfo/host boundary (RFC 3986) — splitting on the FIRST '@'
        // instead treats everything after it, up to the real boundary, as
        // part of the host and leaks it verbatim. Built with `format!`
        // rather than a literal so the '@'-in-password shape survives
        // copy/paste tooling that likes to mangle credential-looking URLs.
        let leaky = format!("rtsp://admin:s3cret{at}more@10.0.0.5:554/stream", at = '@');
        assert_eq!(
            redact_url_credentials(&leaky),
            "rtsp://<redacted>@10.0.0.5:554/stream"
        );
    }

    #[test]
    fn cap_tail_keeps_recent_and_marks_truncation() {
        // 20-byte, 4-line input.
        let s = "aaaa\nbbbb\ncccc\ndddd\n";
        // Under the cap → returned verbatim.
        assert_eq!(cap_tail_bytes(s, 1000), s);
        // Over the cap → oldest dropped, tail kept, marker prepended.
        let capped = cap_tail_bytes(s, 8);
        assert!(capped.starts_with("# \u{2026}"), "got: {capped:?}");
        assert!(capped.ends_with("dddd\n"), "got: {capped:?}");
        assert!(!capped.contains("aaaa"), "oldest line must be dropped");
    }

    #[tokio::test]
    async fn journal_log_disabled_when_zero_lines() {
        // log_lines=0 must not shell out — returns the note + Empty.
        let (text, status) = collect_journal_log(0).await;
        assert_eq!(status, JournalStatus::Empty);
        assert!(text.contains("log_lines=0"), "got: {text}");
        assert!(text.contains("nexus-engine journal"), "got: {text}");
    }

    #[test]
    fn tar_entries_include_engine_log() {
        let snap = DiagnosticsSnapshot {
            redacted_config_toml: "cfg".into(),
            system_metrics_json: "{}".into(),
            camera_stats_json: "[]".into(),
            audit_json: "[]".into(),
            motion_events_json: "[]".into(),
            storage_backends_json: "[]".into(),
            build_info_json: "{}".into(),
            journal_log: "2026-07-31T00:00:00+0000 host nexus-engine[1]: rtsp session failed\n"
                .into(),
            journal_status: JournalStatus::Collected,
            sqlite_bytes: None,
            sqlite_status: SqliteSnapshotStatus::Omitted,
            audit_count: 0,
            motion_count: 0,
            generated_at: Utc::now(),
        };

        let mut buf = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut buf);
            write_tar_entries(&mut tar, &snap).unwrap();
            tar.finish().unwrap();
        }

        let mut names = Vec::new();
        let mut archive = tar::Archive::new(&buf[..]);
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            names.push(entry.path().unwrap().to_string_lossy().into_owned());
        }
        assert!(
            names.iter().any(|n| n == "nexus-engine.log"),
            "engine log missing from bundle: {names:?}"
        );
        // The host metrics in this bundle are box-wide; without the
        // per-camera file a stale-video report cannot be triaged from the
        // bundle alone (BUG-071).
        assert!(
            names.iter().any(|n| n == "camera-stats.json"),
            "per-camera decode health missing from bundle: {names:?}"
        );
        // sqlite omitted → snapshot entry must not appear.
        assert!(
            !names.iter().any(|n| n.contains("nexus-state.sqlite")),
            "unexpected sqlite entry: {names:?}"
        );
    }
}
