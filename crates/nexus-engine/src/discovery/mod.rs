//! Camera discovery for M-Admin Phase 1B.
//!
//! Two probe modes share a single in-memory **session registry**:
//!
//! * **`scan`** — bounded-concurrency CIDR sweep that TCP-probes
//!   ports 554 (RTSP `OPTIONS`) + 80 / 8080 (ONVIF SOAP
//!   `GetDeviceInformation`). Implemented in [`scan`].
//! * **`onvif`** — WS-Discovery Probe over UDP multicast. Stubbed
//!   in this commit (Step C); [`onvif::run_session`] currently
//!   marks the session done with zero devices.
//!
//! Plus a third, **inline** endpoint:
//!
//! * **`probe-rtsp`** — `OPTIONS` + `DESCRIBE` against a single
//!   `(host, port, path, optional creds)` to confirm a stream is
//!   reachable before the operator clicks **Add** in the UI.
//!   Stubbed in this commit (Step D); [`rtsp_probe::probe`]
//!   currently returns `ok=false, status=0`.
//!
//! All four HTTP routes are admin-gated by the existing
//! [`crate::admin_auth`] layer. See
//! [`docs/M_ADMIN.md`](../../../../docs/M_ADMIN.md#phase-1b--camera-discovery-onvif--ip-scan)
//! for the design rationale, the engine-side CIDR guardrails
//! (reject `< /22` unconditionally, require `confirm: true` for
//! `/22`), and the audit-log contract.

pub mod onvif;
pub mod onvif_media;
pub mod rtsp_probe;
pub mod scan;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{ApiError, ApiState};

/// How long a finished session lives before the eviction sweep
/// drops it. Matches the figure documented in
/// `docs/M_ADMIN.md` (10 min).
pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);

/// How often the background sweep checks for expired sessions.
/// Cheap (DashMap iteration); a generous interval keeps idle CPU
/// at zero.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Smallest CIDR prefix the operator may scan WITHOUT supplying
/// `confirm: true`. Anything stricter than `/22` is rejected
/// unconditionally — even with `confirm: true`. A `/22` is
/// 1024 hosts, ~16 s at 64-way concurrency and the 250 ms median
/// per-host RTT we budget for.
pub const MIN_PREFIX_LEN: u8 = 22;
pub const CONFIRM_REQUIRED_AT_PREFIX_LEN: u8 = 22;

// ---------------------------------------------------------------------------
// Wire types — kept in lock-step with the TypeScript declarations
// in `ui/src/api/types.ts` (M_ADMIN.md §Wire types).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryKind {
    Onvif,
    Scan,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryState {
    Running,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// RTSP `OPTIONS` returned a `Public:` line.
    Rtsp,
    /// ONVIF SOAP `GetDeviceInformation` returned a fault or 200,
    /// OR WS-Discovery ProbeMatch was received.
    Onvif,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub ip: String,
    pub port: u16,
    pub kind: DeviceKind,
    /// Port to actually talk RTSP to. Distinct from `port` because
    /// a single physical camera typically exposes its ONVIF /
    /// web service on `:80` AND its RTSP service on `:554`. When
    /// the CIDR scan finds both, we surface the ONVIF entry (more
    /// metadata) but remember the RTSP port here so the probe and
    /// the final camera URL hit the right socket.
    ///
    /// `None` when discovery couldn't confirm an RTSP port — the
    /// UI then defaults to the RFC-standard 554, which is right
    /// for the overwhelming majority of IP cameras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtsp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Verbatim `<wsd:XAddrs>` value from the WS-Discovery
    /// ProbeMatch (or `None` for CIDR-scan-only finds with no
    /// SOAP service detected). Used by the inline
    /// `onvif-streams` probe to call ONVIF Media `GetProfiles`
    /// then `GetStreamUri` and skip the brute-force RTSP path
    /// sweep — vendor-correct URIs in two SOAP round-trips
    /// beats guessing through 13 candidate paths.
    ///
    /// Whitespace-separated when the camera advertises multiple
    /// device-service endpoints (e.g. IPv4 + IPv6); the consumer
    /// picks the first parseable URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onvif_xaddrs: Option<String>,
    /// RTSP paths the operator can plug straight into a camera
    /// URL. Populated by the inline probe-rtsp handler post-Verify;
    /// empty on initial probe.
    ///
    /// Always serialised — even as `[]` — so the UI can do a
    /// plain `d.rtsp_paths[0]` indexed access without a defensive
    /// `?? []`. Skipping the field when empty caused the Verify
    /// click handler to throw "Cannot read properties of
    /// undefined (reading '0')" on every initial-probe device.
    #[serde(default)]
    pub rtsp_paths: Vec<String>,
    /// Typed video codec captured opportunistically during the
    /// CIDR scan / ONVIF media probe. `None` until autodetect
    /// resolves it (e.g. the host answered RTSP `OPTIONS` but
    /// hasn't been DESCRIBE-probed yet, or it required creds).
    /// Seeded into the camera-create form's codec selector by
    /// the UI; the operator can still override with the
    /// `_plus` SVC labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<nexus_types::CodecKind>,
}

/// Request body for `POST /api/v1/admin/discovery/scan`.
#[derive(Debug, Clone, Deserialize)]
pub struct ScanReq {
    pub cidr: String,
    /// Defaults to `[554, 80, 8080]` when missing.
    #[serde(default)]
    pub ports: Option<Vec<u16>>,
    /// Defaults to 64 when missing.
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Required to scan `/22`; ignored on `/23` and stricter.
    #[serde(default)]
    pub confirm: Option<bool>,
}

/// Response body for `POST /api/v1/admin/discovery/scan` (and
/// the ONVIF variant, with `total_targets: None`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionCreatedResp {
    pub session_id: Uuid,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_targets: Option<u32>,
}

/// Wire shape returned by `GET /api/v1/admin/discovery/sessions/:id`.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoverySessionView {
    pub session_id: Uuid,
    pub state: DiscoveryState,
    pub kind: DiscoveryKind,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_targets: Option<u32>,
    pub scanned: u32,
    pub found: Vec<DiscoveredDevice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `true` when the session was terminated by an explicit
    /// `POST .../sessions/:id/cancel` rather than running to
    /// completion. Distinct from `state: error` (operator-driven,
    /// not a fault). UI uses it to render an "operator cancelled"
    /// affordance and skip the "No cameras matched" empty-state
    /// copy. Always serialised so callers can do plain field reads.
    #[serde(default)]
    pub cancelled: bool,
}

/// Response body for `POST /api/v1/admin/discovery/sessions/:id/cancel`.
#[derive(Debug, Clone, Serialize)]
pub struct CancelSessionResp {
    /// `true` when the cancel actually transitioned a Running
    /// session to Done; `false` when the session was already
    /// terminal (idempotent — second cancel is not an error).
    pub cancelled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeRtspReq {
    pub host: String,
    pub port: u16,
    pub path: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// Request body for `POST /api/v1/admin/discovery/sessions/:session_id/onvif-streams`.
///
/// `xaddr` is the verbatim WS-Discovery XAddrs URL
/// (`http://<host>:<port>/onvif/device_service` in the common
/// case, or whatever vendor-specific path the camera advertised
/// — see `DiscoveredDevice::onvif_xaddrs`). Username + password
/// are mandatory: ONVIF Media `GetProfiles` is gated on every
/// camera I've seen, anonymous calls always return
/// `NotAuthorized`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeOnvifReq {
    pub xaddr: String,
    pub username: String,
    pub password: String,
}

/// Response body for the ONVIF Media probe. On success,
/// `streams` contains one entry per camera profile with the
/// authoritative RTSP URI the camera advertised. On failure,
/// `streams` is empty and `error` carries the operator-facing
/// reason (typically `"NotAuthorized"` for wrong creds or
/// `"connect failed"` for network issues).
///
/// The UI treats any failure as a signal to fall back to the
/// brute-force RTSP path sweep (`POST .../probe-rtsp`); the
/// `error` string is logged but not surfaced to the operator
/// unless the path sweep ALSO fails.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeOnvifResult {
    pub ok: bool,
    pub streams: Vec<onvif_media::MediaStream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SdpStream {
    pub codec: String,
    /// Typed codec parsed from `a=rtpmap:` lines
    /// (`H264/90000` → `H264`, `HEVC/90000` and `H265/90000`
    /// → `H265`). `None` for codecs we don't enumerate
    /// (`JPEG`, `MPEG4-GENERIC`, audio, …) so the UI can show
    /// the raw string in `codec` but skip the typed selector.
    /// Autodetect never emits a `_plus` variant — vendor SVC
    /// shares the base wire format with H.264 / H.265.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_kind: Option<nexus_types::CodecKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
}

/// Per-path summary returned in [`ProbeRtspResult::streams`].
///
/// One entry per RTSP path that answered DESCRIBE with `200 OK` +
/// a parseable SDP body. `codec` / `resolution` come from the
/// first video track in that path's SDP (typically the H.264 or
/// H.265 main track). Used by the UI to populate a path dropdown
/// per discovered camera with human-readable labels like
/// `"/Streaming/Channels/101 — H264 1920x1080"`.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeStream {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Typed codec from the first video track of this path's
    /// SDP. Mirrors `SdpStream::codec_kind`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_kind: Option<nexus_types::CodecKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeRtspResult {
    pub ok: bool,
    pub status: u16,
    pub sdp_streams: Vec<SdpStream>,
    /// Typed codec of the winning path's first video track, lifted
    /// out of `streams[0].codec_kind` so the UI can seed the codec
    /// selector without iterating. `None` when the probe failed or
    /// the camera publishes only codecs we don't enumerate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<nexus_types::CodecKind>,
    /// RTSP path that actually answered with a 200 + SDP body.
    /// `Some("/cam/realmonitor?channel=1&subtype=0")` when path
    /// discovery succeeded (or when the operator's explicit path
    /// worked); `None` on failure. Echoed back to the UI so the
    /// Add flow can build a complete `rtsp://host:port<path>` URL
    /// without the operator hand-typing the vendor-specific path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Full list of working RTSP paths discovered during this
    /// probe, in the order the candidates were tried (typically
    /// main-stream first, sub-stream second). For an explicit
    /// operator-supplied path this contains at most one entry.
    /// For the empty/`"/"` path-discovery case the probe runs all
    /// [`DEFAULT_PATHS`](rtsp_probe::DEFAULT_PATHS) in parallel
    /// and reports every one that DESCRIBE-200'd, letting the UI
    /// surface a "main vs sub" picker on cameras that publish
    /// both at once (Hikvision / Dahua / Reolink all do).
    #[serde(default)]
    pub streams: Vec<ProbeStream>,
}

// ---------------------------------------------------------------------------
// Session registry
// ---------------------------------------------------------------------------

/// Mutable per-session state held under a single
/// `parking_lot::Mutex` inside the registry. Cheap to lock; the
/// only writers are (a) the spawned discovery task appending
/// devices and (b) the eviction sweep.
#[derive(Debug)]
pub(crate) struct SessionInner {
    pub state: DiscoveryState,
    pub kind: DiscoveryKind,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<Instant>,
    pub total_targets: Option<u32>,
    pub scanned: u32,
    pub found: Vec<DiscoveredDevice>,
    pub error: Option<String>,
    /// Set to `true` by [`DiscoverySessions::cancel`] when the
    /// operator hits `POST .../sessions/:id/cancel`. The spawned
    /// scan/onvif task is NOT torn down (the work is already
    /// bounded — onvif is a 5 s listen window, a /22 scan caps
    /// at ~30 s at concurrency=64), but `mark_finished` is
    /// idempotent so the task's terminal-state write is dropped
    /// when it eventually runs.
    pub cancelled: bool,
}

impl SessionInner {
    fn snapshot(&self, session_id: Uuid) -> DiscoverySessionView {
        DiscoverySessionView {
            session_id,
            state: self.state,
            kind: self.kind,
            started_at: self.started_at,
            total_targets: self.total_targets,
            scanned: self.scanned,
            found: self.found.clone(),
            error: self.error.clone(),
            cancelled: self.cancelled,
        }
    }
}

/// Lock-free registry shared via `ApiState`. The handler clones
/// the `Arc<DiscoverySessions>` into spawned tasks so the per-
/// session task can update progress without holding any borrow
/// on the request handler.
#[derive(Debug, Clone, Default)]
pub struct DiscoverySessions {
    inner: Arc<DashMap<Uuid, Arc<Mutex<SessionInner>>>>,
}

impl DiscoverySessions {
    /// Construct an empty registry. Equivalent to
    /// `DiscoverySessions::default()` but provided as a named
    /// constructor so call sites (e.g. `ApiState` setup in
    /// `main.rs`) read more clearly.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a new running session and returns the id +
    /// the shared `Arc<Mutex<...>>` the spawned task should
    /// hold to push progress.
    pub(crate) fn start(
        &self,
        kind: DiscoveryKind,
        total_targets: Option<u32>,
    ) -> (Uuid, Arc<Mutex<SessionInner>>) {
        let id = Uuid::now_v7();
        let inner = Arc::new(Mutex::new(SessionInner {
            state: DiscoveryState::Running,
            kind,
            started_at: Utc::now(),
            finished_at: None,
            total_targets,
            scanned: 0,
            found: Vec::new(),
            error: None,
            cancelled: false,
        }));
        self.inner.insert(id, inner.clone());
        (id, inner)
    }

    pub fn get(&self, id: &Uuid) -> Option<DiscoverySessionView> {
        let entry = self.inner.get(id)?;
        let snap = entry.value().lock().snapshot(*id);
        Some(snap)
    }

    /// Mark a Running session as cancelled. Returns:
    /// * `Some(true)`  — the session existed AND was Running;
    ///   state transitioned to `Done` with `cancelled = true`.
    /// * `Some(false)` — the session existed but was already
    ///   terminal (Done or Error); no-op, idempotent.
    /// * `None`        — no such session (TTL-evicted or never
    ///   existed); the HTTP handler maps this to 404.
    pub fn cancel(&self, id: &Uuid) -> Option<bool> {
        let entry = self.inner.get(id)?;
        let mut guard = entry.value().lock();
        if guard.state != DiscoveryState::Running {
            return Some(false);
        }
        guard.state = DiscoveryState::Done;
        guard.cancelled = true;
        guard.finished_at = Some(Instant::now());
        // Explicitly clear any prior error string — a cancelled
        // session is operator-driven, not a fault.
        guard.error = None;
        Some(true)
    }

    /// Drop every session whose `finished_at` is older than
    /// `SESSION_TTL`. Called by the background sweep launched
    /// from [`spawn_eviction_sweep`].
    fn evict_expired(&self, now: Instant) {
        let mut to_remove = Vec::new();
        for entry in self.inner.iter() {
            let guard = entry.value().lock();
            if let Some(finished) = guard.finished_at {
                if now.saturating_duration_since(finished) > SESSION_TTL {
                    to_remove.push(*entry.key());
                }
            }
        }
        for id in to_remove {
            self.inner.remove(&id);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Spawn the background eviction sweep. Should be called once at
/// engine boot. Holds a weak-ref-free clone — the sweep keeps the
/// registry alive for the engine lifetime, which is what we want
/// (the registry IS engine-process-scoped).
pub fn spawn_eviction_sweep(sessions: DiscoverySessions) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        // Skip the initial tick (fires immediately) so the first
        // eviction pass happens one SWEEP_INTERVAL after boot, not
        // at boot itself.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            sessions.evict_expired(Instant::now());
        }
    });
}

/// Helper used by the discovery tasks to mark a session terminal
/// (Done or Error). Also stamps `finished_at` so the eviction
/// sweep can drop it after `SESSION_TTL`.
///
/// **Idempotent**: a no-op when the session is already terminal.
/// This lets [`DiscoverySessions::cancel`] flip a Running session
/// to Done synchronously without racing the spawned task — when
/// the task eventually finishes its bounded work and calls
/// `mark_finished`, this branch drops the second write so the
/// `cancelled: true` flag survives.
pub(crate) fn mark_finished(inner: &Mutex<SessionInner>, error: Option<String>) {
    let mut guard = inner.lock();
    if guard.state != DiscoveryState::Running {
        return;
    }
    guard.state = if error.is_some() {
        DiscoveryState::Error
    } else {
        DiscoveryState::Done
    };
    guard.finished_at = Some(Instant::now());
    guard.error = error;
}

// ---------------------------------------------------------------------------
// HTTP handlers
//
// All four routes are mounted under the existing admin sub-router
// in `api::router()`; the `admin_auth_layer` already gates them.
// See `api.rs` for the wiring.
// ---------------------------------------------------------------------------

/// `POST /api/v1/admin/discovery/onvif`
///
/// Spawns a 5-second WS-Discovery probe. Returns the session id
/// immediately; the UI polls `GET /sessions/:session_id` for
/// results.
pub async fn post_discovery_onvif(
    State(s): State<ApiState>,
) -> Result<Json<SessionCreatedResp>, ApiError> {
    let (session_id, inner) = s.discovery_sessions.start(DiscoveryKind::Onvif, None);
    let started_at = inner.lock().started_at;

    // Audit-log mirrors the pattern used by the storage-admin
    // mutations. ONVIF doesn't carry a target argument; the
    // session id is enough to correlate.
    s.store
        .write_audit(
            "api",
            "discovery_onvif_start",
            &format!("session/{session_id}"),
            &serde_json::json!({}),
        )
        .await?;

    let sessions = s.discovery_sessions.clone();
    tokio::spawn(async move {
        onvif::run_session(sessions, session_id, inner).await;
    });

    Ok(Json(SessionCreatedResp {
        session_id,
        started_at,
        total_targets: None,
    }))
}

/// `POST /api/v1/admin/discovery/scan`
///
/// Validates the CIDR against the engine-side guardrails
/// ([`MIN_PREFIX_LEN`] / [`CONFIRM_REQUIRED_AT_PREFIX_LEN`]),
/// audit-logs the start, then spawns the sweep. Returns the
/// session id + projected host count.
pub async fn post_discovery_scan(
    State(s): State<ApiState>,
    Json(req): Json<ScanReq>,
) -> Result<Json<SessionCreatedResp>, ApiError> {
    let plan = validate_scan_req(&req).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;

    let total_targets = plan.total_targets;
    let (session_id, inner) = s
        .discovery_sessions
        .start(DiscoveryKind::Scan, Some(total_targets));
    let started_at = inner.lock().started_at;

    s.store
        .write_audit(
            "api",
            "discovery_scan_start",
            &format!("session/{session_id}"),
            &serde_json::json!({
                "cidr": &req.cidr,
                "ports": &plan.ports,
                "concurrency": plan.concurrency,
                "total_targets": total_targets,
            }),
        )
        .await?;

    let sessions = s.discovery_sessions.clone();
    tokio::spawn(async move {
        scan::run_session(sessions, session_id, inner, plan).await;
    });

    Ok(Json(SessionCreatedResp {
        session_id,
        started_at,
        total_targets: Some(total_targets),
    }))
}

/// `GET /api/v1/admin/discovery/sessions/:session_id`
pub async fn get_discovery_session(
    State(s): State<ApiState>,
    Path(session_id): Path<Uuid>,
) -> Result<Json<DiscoverySessionView>, ApiError> {
    s.discovery_sessions
        .get(&session_id)
        .map(Json)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("session {session_id} not found"),
            )
        })
}

/// `POST /api/v1/admin/discovery/sessions/:session_id/probe-rtsp`
///
/// Runs inline (≤5 s) against the supplied `(host, port, path)`.
/// Does NOT allocate a new session; the path parameter is only
/// used to correlate the audit log entry. A 404 on the session
/// is intentionally tolerated — operators may probe an IP after
/// the parent session has been evicted by TTL.
pub async fn post_probe_rtsp(
    State(s): State<ApiState>,
    Path(session_id): Path<Uuid>,
    Json(req): Json<ProbeRtspReq>,
) -> Result<Json<ProbeRtspResult>, ApiError> {
    s.store
        .write_audit(
            "api",
            "discovery_probe_rtsp",
            &format!("session/{session_id}"),
            &serde_json::json!({
                "host": &req.host,
                "port": req.port,
                "path": &req.path,
            }),
        )
        .await?;
    Ok(Json(rtsp_probe::probe(&req).await))
}

/// `POST /api/v1/admin/discovery/sessions/:session_id/onvif-streams`
///
/// Runs inline (≤5 s per profile, typically <2 s total for 2–4
/// profiles) against the supplied ONVIF Media service URL.
/// Authoritative replacement for the brute-force path sweep:
/// the camera tells us its exact profile list + RTSP URIs.
///
/// On any failure (auth, network, malformed response) returns
/// `{ ok: false, streams: [], error: "..." }` with HTTP 200 —
/// the UI uses the empty streams list as a signal to fall back
/// to `probe-rtsp`. Returning an HTTP error here would force
/// the UI to distinguish "endpoint missing" from "camera said
/// no"; one shape is simpler.
pub async fn post_probe_onvif(
    State(s): State<ApiState>,
    Path(session_id): Path<Uuid>,
    Json(req): Json<ProbeOnvifReq>,
) -> Result<Json<ProbeOnvifResult>, ApiError> {
    s.store
        .write_audit(
            "api",
            "discovery_probe_onvif",
            &format!("session/{session_id}"),
            &serde_json::json!({
                "xaddr": &req.xaddr,
                // Username only for audit trail; password is
                // never logged.
                "username": &req.username,
            }),
        )
        .await?;
    match onvif_media::query_streams(&req.xaddr, &req.username, &req.password).await {
        Ok(streams) => Ok(Json(ProbeOnvifResult {
            ok: !streams.is_empty(),
            streams,
            error: None,
        })),
        Err(err) => Ok(Json(ProbeOnvifResult {
            ok: false,
            streams: Vec::new(),
            error: Some(err),
        })),
    }
}

/// `POST /api/v1/admin/discovery/sessions/:session_id/cancel`
///
/// Operator-driven early termination. Marks a Running session as
/// `Done` with `cancelled: true` so the UI's poll loop exits
/// cleanly when the operator closes the discover dialog mid-scan.
/// The spawned scan/onvif task itself is NOT torn down (the work
/// is already bounded \u2014 onvif is a 5 s listen window, a /22 scan
/// caps at ~30 s at concurrency=64); [`mark_finished`] is
/// idempotent so the task's eventual terminal-state write is a
/// no-op.
///
/// Idempotent: a second cancel against the same session returns
/// `{ cancelled: false }` with HTTP 200, NOT a 4xx error \u2014 the
/// operator may have double-clicked or two cloud-side requests
/// may have raced.
///
/// Returns 404 only when the session id is unknown (TTL-evicted
/// or never existed).
pub async fn post_cancel_session(
    State(s): State<ApiState>,
    Path(session_id): Path<Uuid>,
) -> Result<Json<CancelSessionResp>, ApiError> {
    s.store
        .write_audit(
            "api",
            "discovery_session_cancel",
            &format!("session/{session_id}"),
            &serde_json::json!({}),
        )
        .await?;
    match s.discovery_sessions.cancel(&session_id) {
        Some(cancelled) => Ok(Json(CancelSessionResp { cancelled })),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("session {session_id} not found"),
        )),
    }
}

// ---------------------------------------------------------------------------
// CIDR guardrail validation
// ---------------------------------------------------------------------------

/// Resolved + validated scan request. The CIDR has been parsed,
/// ports defaulted, host count computed.
#[derive(Debug, Clone)]
pub(crate) struct ScanPlan {
    pub cidr: ipnet::Ipv4Net,
    pub ports: Vec<u16>,
    pub concurrency: usize,
    pub total_targets: u32,
}

/// Apply the M_ADMIN.md guardrails to a [`ScanReq`]. Returns a
/// validated [`ScanPlan`] or a human-readable error string the
/// HTTP handler echoes back as the 400 body.
pub(crate) fn validate_scan_req(req: &ScanReq) -> Result<ScanPlan, String> {
    let cidr: ipnet::Ipv4Net = req
        .cidr
        .parse()
        .map_err(|e| format!("malformed CIDR `{}`: {}", req.cidr, e))?;

    if cidr.prefix_len() < MIN_PREFIX_LEN {
        return Err(format!(
            "scan refused: prefix /{} is shorter than the minimum allowed (/{})",
            cidr.prefix_len(),
            MIN_PREFIX_LEN
        ));
    }
    if cidr.prefix_len() == CONFIRM_REQUIRED_AT_PREFIX_LEN && !req.confirm.unwrap_or(false) {
        return Err(format!(
            "scan refused: /{} requires `confirm: true` (1024 hosts)",
            CONFIRM_REQUIRED_AT_PREFIX_LEN
        ));
    }

    let ports = req
        .ports
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![554, 80, 8080]);

    // Operator-requested concurrency is the upper bound, but we
    // also clamp by the actual FD soft limit so the sweep never
    // exhausts the process descriptor table. Each in-flight host
    // races `ports.len()` parallel TCP probes (RTSP + ONVIF ×
    // ports), so socket fan-out is `concurrency * ports.len()`.
    // We reserve `FD_RESERVE` descriptors for the rest of the
    // engine (HTTP server clients, sqlite pool, gstreamer
    // pipelines, logging, tracing exporter, OS bookkeeping) and
    // divide the remainder by `ports.len()`. With ports=[554,80,
    // 8080] and a 256-FD limit that yields 64 — but the typical
    // post-`raise_fd_soft_limit` cap of 65_536 still saturates at
    // the operator-requested ceiling.
    const FD_RESERVE: u64 = 256;
    let requested = req.concurrency.filter(|&n| n > 0).unwrap_or(64).min(256);
    let port_count = ports.len().max(1) as u64;
    let concurrency = {
        #[cfg(unix)]
        let fd_soft = crate::fd_limit::current_fd_soft_limit();
        #[cfg(not(unix))]
        let fd_soft = u64::MAX;
        let fd_budget = fd_soft.saturating_sub(FD_RESERVE);
        let fd_cap = (fd_budget / port_count).max(4) as usize;
        requested.min(fd_cap)
    };

    // `ipnet::Ipv4Net::hosts()` excludes network + broadcast for
    // /30 and shorter prefixes, and yields the two addresses for
    // /31, and a single address for /32. For our guardrail-
    // bounded range (/22..=/32) `hosts().count()` is what the
    // operator means by "number of IPs we'll TCP-probe".
    let total_targets = u32::try_from(cidr.hosts().count())
        .map_err(|_| "CIDR host count exceeds u32::MAX".to_string())?;

    Ok(ScanPlan {
        cidr,
        ports,
        concurrency,
        total_targets,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_prefix_shorter_than_22() {
        // /21 = 2046 hosts. Hard refused even with confirm: true.
        for cidr in ["10.0.0.0/8", "10.0.0.0/16", "10.0.0.0/20", "10.0.0.0/21"] {
            let err = validate_scan_req(&ScanReq {
                cidr: cidr.to_string(),
                ports: None,
                concurrency: None,
                confirm: Some(true),
            })
            .unwrap_err();
            assert!(
                err.contains("prefix") && err.contains("shorter"),
                "expected guardrail rejection for {cidr}, got: {err}"
            );
        }
    }

    #[test]
    fn validate_requires_confirm_for_22() {
        let req = ScanReq {
            cidr: "192.168.0.0/22".to_string(),
            ports: None,
            concurrency: None,
            confirm: None,
        };
        let err = validate_scan_req(&req).unwrap_err();
        assert!(err.contains("confirm: true"), "got: {err}");

        // With confirm: true, /22 is allowed. `ipnet::hosts()`
        // strips the network + broadcast addresses, so a /22 (1024
        // total) sweeps 1022 probable hosts.
        let plan = validate_scan_req(&ScanReq {
            confirm: Some(true),
            ..req
        })
        .unwrap();
        assert_eq!(plan.total_targets, 1022);
    }

    #[test]
    fn validate_accepts_24_without_confirm() {
        // Ensure the test has enough FD headroom to hit the
        // requested concurrency cap — macOS's default 256-FD
        // soft limit would otherwise clamp concurrency to ~0
        // and turn this assertion into a flake. Best-effort:
        // the assertion below tolerates either outcome.
        #[cfg(unix)]
        {
            let _ = crate::fd_limit::raise_fd_soft_limit();
        }

        let plan = validate_scan_req(&ScanReq {
            cidr: "192.168.1.0/24".to_string(),
            ports: None,
            concurrency: None,
            confirm: None,
        })
        .unwrap();
        // /24 hosts() yields .1 ..= .254 → 254.
        assert_eq!(plan.total_targets, 254);
        assert_eq!(plan.ports, vec![554, 80, 8080]);
        // Concurrency is the lesser of the operator request (64
        // here, the default) and the FD-budget cap. On any
        // sensibly-provisioned dev/CI host we expect 64; on
        // pathologically tight FD caps we settle for ≥ 4.
        assert!(
            plan.concurrency >= 4 && plan.concurrency <= 64,
            "concurrency outside expected range: {}",
            plan.concurrency
        );
    }

    #[test]
    fn validate_defaults_empty_ports_to_canonical_set() {
        let plan = validate_scan_req(&ScanReq {
            cidr: "192.168.1.0/24".to_string(),
            ports: Some(vec![]),
            concurrency: None,
            confirm: None,
        })
        .unwrap();
        assert_eq!(plan.ports, vec![554, 80, 8080]);
    }

    #[test]
    fn validate_clamps_concurrency_to_256() {
        // Raise FDs first so the operator ceiling (256), not the
        // FD budget, is the binding constraint we're asserting on.
        #[cfg(unix)]
        {
            let _ = crate::fd_limit::raise_fd_soft_limit();
        }
        let plan = validate_scan_req(&ScanReq {
            cidr: "192.168.1.0/24".to_string(),
            ports: None,
            concurrency: Some(99_999),
            confirm: None,
        })
        .unwrap();
        // Operator request 99_999 → clamped to the 256-host ceiling
        // first, then further clamped by the FD budget. On a host
        // with the typical raised limit (≥ 65_536) the FD budget
        // permits well above 256, so the ceiling wins.
        assert!(
            plan.concurrency >= 4 && plan.concurrency <= 256,
            "concurrency outside expected range: {}",
            plan.concurrency
        );
    }

    #[test]
    fn validate_zero_concurrency_falls_back_to_default() {
        #[cfg(unix)]
        {
            let _ = crate::fd_limit::raise_fd_soft_limit();
        }
        let plan = validate_scan_req(&ScanReq {
            cidr: "192.168.1.0/24".to_string(),
            ports: None,
            concurrency: Some(0),
            confirm: None,
        })
        .unwrap();
        // Same range tolerance as the other concurrency tests:
        // typical hosts hit 64, FD-starved hosts settle for ≥ 4.
        assert!(
            plan.concurrency >= 4 && plan.concurrency <= 64,
            "concurrency outside expected range: {}",
            plan.concurrency
        );
    }

    #[test]
    fn validate_rejects_malformed_cidr() {
        let err = validate_scan_req(&ScanReq {
            cidr: "not a cidr".to_string(),
            ports: None,
            concurrency: None,
            confirm: None,
        })
        .unwrap_err();
        assert!(err.contains("malformed CIDR"), "got: {err}");
    }

    #[tokio::test]
    async fn registry_round_trip() {
        let reg = DiscoverySessions::new();
        let (id, inner) = reg.start(DiscoveryKind::Scan, Some(10));
        assert_eq!(reg.len(), 1);

        let view = reg.get(&id).expect("session present");
        assert_eq!(view.state, DiscoveryState::Running);
        assert_eq!(view.total_targets, Some(10));
        assert_eq!(view.scanned, 0);
        assert!(view.found.is_empty());

        // Simulate the spawned task reporting progress.
        {
            let mut guard = inner.lock();
            guard.scanned = 5;
            guard.found.push(DiscoveredDevice {
                ip: "192.168.1.10".to_string(),
                port: 554,
                kind: DeviceKind::Rtsp,
                rtsp_port: Some(554),
                vendor: None,
                model: None,
                hardware: None,
                firmware: None,
                mac: None,
                onvif_xaddrs: None,
                rtsp_paths: Vec::new(),
                codec: None,
            });
        }
        let view = reg.get(&id).unwrap();
        assert_eq!(view.scanned, 5);
        assert_eq!(view.found.len(), 1);

        mark_finished(&inner, None);
        let view = reg.get(&id).unwrap();
        assert_eq!(view.state, DiscoveryState::Done);
        assert!(view.error.is_none());
    }

    #[tokio::test]
    async fn eviction_drops_finished_sessions_past_ttl() {
        let reg = DiscoverySessions::new();
        let (id, inner) = reg.start(DiscoveryKind::Onvif, None);

        // Pretend the session finished SESSION_TTL + 30s ago.
        {
            let mut guard = inner.lock();
            guard.state = DiscoveryState::Done;
            guard.finished_at = Instant::now().checked_sub(SESSION_TTL + Duration::from_secs(30));
        }
        assert_eq!(reg.len(), 1);
        reg.evict_expired(Instant::now());
        assert_eq!(reg.len(), 0, "expired session was not evicted");

        // Eviction only touches finished sessions; running ones
        // stay regardless of age.
        let (id2, _) = reg.start(DiscoveryKind::Onvif, None);
        reg.evict_expired(Instant::now() + SESSION_TTL * 100);
        assert!(
            reg.get(&id2).is_some(),
            "running session must not be evicted"
        );
        // Sanity check the evicted id really is gone.
        assert!(reg.get(&id).is_none());
    }

    #[tokio::test]
    async fn cancel_transitions_running_to_done_with_flag() {
        let reg = DiscoverySessions::new();
        let (id, _inner) = reg.start(DiscoveryKind::Scan, Some(1024));

        // First cancel: Running -> Done, cancelled = true.
        assert_eq!(reg.cancel(&id), Some(true));
        let view = reg.get(&id).expect("session still present");
        assert_eq!(view.state, DiscoveryState::Done);
        assert!(view.cancelled, "cancelled flag must be true");
        assert!(view.error.is_none(), "cancel must clear error");

        // Second cancel is idempotent: returns Some(false), no
        // state change.
        assert_eq!(reg.cancel(&id), Some(false));
        let view2 = reg.get(&id).unwrap();
        assert_eq!(view2.state, DiscoveryState::Done);
        assert!(view2.cancelled);
    }

    #[tokio::test]
    async fn cancel_unknown_session_returns_none() {
        let reg = DiscoverySessions::new();
        assert_eq!(reg.cancel(&Uuid::now_v7()), None);
    }

    #[tokio::test]
    async fn mark_finished_is_idempotent_after_cancel() {
        // Models the race: operator cancels mid-scan, then the
        // spawned task eventually completes its bounded work and
        // calls mark_finished. The cancelled flag must survive.
        let reg = DiscoverySessions::new();
        let (id, inner) = reg.start(DiscoveryKind::Scan, Some(1024));

        assert_eq!(reg.cancel(&id), Some(true));
        let view_before = reg.get(&id).unwrap();
        let finished_before = view_before
            .cancelled
            .then_some(view_before.state)
            .expect("must be cancelled");

        // Task races in with its terminal-state write.
        mark_finished(&inner, Some("network gone".to_string()));

        let view_after = reg.get(&id).unwrap();
        assert_eq!(
            view_after.state, finished_before,
            "post-cancel mark_finished must NOT overwrite state"
        );
        assert!(view_after.cancelled, "cancelled flag must survive race");
        assert!(
            view_after.error.is_none(),
            "post-cancel mark_finished must NOT overwrite the cleared error"
        );
    }

    #[tokio::test]
    async fn cancel_after_completion_returns_false() {
        let reg = DiscoverySessions::new();
        let (id, inner) = reg.start(DiscoveryKind::Onvif, None);
        mark_finished(&inner, None);

        assert_eq!(reg.cancel(&id), Some(false));
        let view = reg.get(&id).unwrap();
        assert!(!view.cancelled, "natural completion must NOT set cancelled");
        assert_eq!(view.state, DiscoveryState::Done);
    }
}
