//! M7 Step 6F2 — dev-only event-injection endpoint.
//!
//! Gated behind the `test-injection` cargo feature on
//! `nexus-engine` so this code is compiled OUT of any normal
//! (production / release) build. The Playwright e2e suite builds
//! with `--features test-injection` to expose
//! `POST /api/v1/_test/inject_event`, which lets a test write a
//! ready-made `AlertEvent` straight into the outbox via the same
//! `Store::record_event_and_enqueue` path the rule engine uses in
//! production.
//!
//! Why an endpoint and not a CLI tool / direct DB write:
//!
//! * Goes through `record_event_and_enqueue`, so the FK to
//!   `cameras` is checked and the per-sink fan-out is identical
//!   to a real rule fire (cascade policy on the dispatcher side
//!   runs unchanged — that's the whole point of the test).
//! * Survives sqlite WAL boundaries: a separate process writing
//!   to the DB would race the engine's pool; an HTTP call serialises
//!   through the same connection pool.
//!
//! Security posture:
//!
//! * Feature-gated → endpoint literally does not exist in the
//!   compiled binary unless the operator opts in at build time.
//! * Skips the admin-auth gate by design — the e2e fixture runs
//!   under `auth.mode = "none"` on loopback. If someone ever wants
//!   to enable this in a half-secured environment they should add
//!   their own gate before the route registration in `api.rs`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use nexus_types::AlertEvent;
use serde::Serialize;

use crate::api::{ApiError, ApiState};

#[derive(Serialize)]
pub(crate) struct InjectEventResp {
    /// Echo back the event id the caller supplied so they don't
    /// have to parse it out of their own request body.
    pub event_id: String,
    /// Number of outbox rows enqueued (one per live sink).
    pub sinks: usize,
}

/// `POST /api/v1/_test/inject_event`
///
/// Body: full `AlertEvent` JSON. The caller is responsible for
/// supplying a unique `event_id` (the store uses it as the
/// primary key on `events`).
///
/// Response: `200 { "event_id": "...", "sinks": N }` where `N` is
/// the count of outbox rows just inserted.
pub(crate) async fn post_inject_event(
    State(s): State<ApiState>,
    Json(ev): Json<AlertEvent>,
) -> Result<(StatusCode, Json<InjectEventResp>), ApiError> {
    // Snapshot the live registry → string-ids in the format the
    // store expects. The injected event is not tied to a loaded rule,
    // so it routes to ALL configured sinks (no per-rule `sinks`
    // filtering). The production fire path in the per-camera
    // supervisor resolves the same registry via `EngineSinkRouter`
    // and additionally narrows to the firing rule's `sinks`
    // allow-list — see `nexus-engine/src/sink_router.rs`.
    let sink_ids: Vec<String> = s
        .sink_registry
        .ids()
        .iter()
        .map(|id| id.to_string())
        .collect();
    let sink_refs: Vec<&str> = sink_ids.iter().map(String::as_str).collect();
    let n = sink_refs.len();

    let event_id_str = ev.event_id.to_string();
    s.store
        .record_event_and_enqueue(&ev, &sink_refs)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("inject_event: {e}"),
            )
        })?;

    Ok((
        StatusCode::OK,
        Json(InjectEventResp {
            event_id: event_id_str,
            sinks: n,
        }),
    ))
}

/// `POST /api/v1/_test/setup_reset`
///
/// Test-only knob that clears the `engine_runtime_settings.setup_complete`
/// latch so the next `GET /v1/setup/status` reports
/// `setup_complete: false` and the SPA router redirects logged-in
/// users to `/setup`. Used by the wizard e2e spec to exercise
/// the empty-DB redirect path without spawning a second engine.
///
/// Same security posture as `inject_event`: behind the
/// `test-injection` cargo feature, no auth gate, never compiled
/// into a production binary.
pub(crate) async fn post_setup_reset(State(s): State<ApiState>) -> Result<StatusCode, ApiError> {
    s.store
        .write_runtime_setting(crate::setup::KEY_SETUP_COMPLETE, None)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("setup_reset: {e}"),
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}
