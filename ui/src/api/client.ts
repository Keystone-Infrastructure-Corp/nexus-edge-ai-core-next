// Typed fetch wrapper. Every method takes/returns a typed payload from
// `./types.ts`, so callers can never confuse the API shape with the UI's
// own state.

import { authHeader, reportRequestOutcome, tryRefresh, getSession } from "../lib/auth.js";
import type {
  AlertEvent,
  AuditGlobalFilter,
  AuditPage,
  AuditRow,
  BackendsResponse,
  CameraConfig,
  CameraId,
  ClipId,
  CreateUserRequest,
  CreateUserResponse,
  DiscoverySession,
  ClipTracksResponse,
  DeliverySettings,
  EventClipResponse,
  EventId,
  ModelPromptsResponse,
  MotionEventRow,
  MotionHistogramBucket,
  OAuthStartReq,
  OAuthStartResp,
  OAuthStatusResp,
  OutboxRow,
  ProbeRtspReq,
  ProbeRtspResult,
  PutAdminDeliveryRequest,
  PutBackendReq,
  PutColdReq,
  PutRuleDeliveryRequest,
  ResetPasswordResponse,
  RuleConfig,
  RuleDeliveryResponse,
  RuleId,
  RulePreviewRequest,
  RulePreviewResponse,
  RuleValidateResponse,
  ScanReq,
  SessionCreatedResp,
  SinksHealthResponse,
  StorageBackendOut,
  StorageLocalResponse,
  StorageResponse,
  UpdateUserRequest,
  UserListResponse,
  UserView,
  FrameMetadata,
} from "./types.js";

const BASE = "/api";

/// One-shot fetch wrapper. Auto-refreshes a stale session token
/// once on 401 (M6 Phase 2 Step 2.9) and retries the original
/// request. If refresh fails the second 401 propagates as a
/// thrown Error — the topbar status pill flips red and the
/// auth overlay decides whether to re-prompt for login.
async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const method = (init?.method ?? "GET").toUpperCase();

  const doFetch = (): Promise<Response> =>
    fetch(BASE + path, {
      ...init,
      headers: {
        "Content-Type": "application/json",
        ...authHeader(),
        ...(init?.headers ?? {}),
      },
    });

  let res = await doFetch();

  // Auto-refresh path: only attempt when we hold a session AND
  // the engine actually said 401 (don't try to refresh a 403,
  // which means the bearer is fine but the role is wrong). We
  // also DON'T touch the auth endpoints themselves — they're
  // their own recovery path; refreshing on a 401 from `/login`
  // would be circular.
  if (res.status === 401 && getSession() && !path.startsWith("/v1/auth/")) {
    const refreshed = await tryRefresh();
    if (refreshed) {
      res = await doFetch();
    }
  }

  reportRequestOutcome(method, res.status);
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`${res.status} ${res.statusText}: ${text}`);
  }
  if (res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

export const api = {
  health: () => request<{ status: string; version: string }>("/health"),

  cameras: {
    list: () => request<CameraConfig[]>("/cameras"),
    upsert: (cam: CameraConfig) =>
      request<CameraConfig>(`/cameras/${cam.id}`, {
        method: "PUT",
        body: JSON.stringify(cam),
      }),
    remove: (id: CameraId) =>
      request<void>(`/cameras/${id}`, { method: "DELETE" }),
    latestSnapshotUrl: (id: CameraId, ts = Date.now()) =>
      `${BASE}/cameras/${id}/frames/latest?t=${ts}`,
    latestMetadata: (id: CameraId) =>
      request<FrameMetadata>(`/cameras/${id}/frames/latest.json`),
  },

  rules: {
    list: () => request<RuleConfig[]>("/rules"),
    upsert: (rule: RuleConfig) =>
      request<RuleConfig>(`/rules/${rule.id}`, {
        method: "PUT",
        body: JSON.stringify(rule),
      }),
    remove: (id: RuleId) =>
      request<void>(`/rules/${id}`, { method: "DELETE" }),
    /// M-Admin Phase 5 — compile-only CEL validation. Always
    /// resolves with `{ok, error?}`; the engine returns 200 even
    /// for invalid expressions so the form can render the parser
    /// message inline. Network failures still reject the promise.
    validate: (when: string) =>
      request<RuleValidateResponse>("/rules/validate", {
        method: "POST",
        body: JSON.stringify({ when }),
      }),
    /// "What would this rule have fired on?" — replays a candidate
    /// rule against the last 24h (default) of motion_events. Doesn't
    /// touch the persisted ruleset. See `preview_rule` for semantics
    /// (no debounce/cooldown applied; raw predicate matches only).
    preview: (req: RulePreviewRequest) =>
      request<RulePreviewResponse>("/rules/preview", {
        method: "POST",
        body: JSON.stringify(req),
      }),
  },

  events: {
    recent: (limit = 100) =>
      request<AlertEvent[]>(`/events?limit=${limit}`),
    /// Look up the clip the supervisor linked to an alert event.
    /// 404 when the event has no linked clip (either the alert
    /// fired on a frame with no open recorder, or the SSE arrived
    /// at the UI before the supervisor's `link_event_to_clip`
    /// call landed). Callers should treat 404 as "no clip yet".
    clip: (eventId: EventId) =>
      request<EventClipResponse>(`/v1/events/${eventId}/clip`),
  },

  backends: () => request<BackendsResponse>("/backends"),

  // M-Admin Phase 5 — detector prompt catalog. Snapshot taken at
  // engine boot of every detector kind the router knows about
  // plus its vocabulary. The camera + rules forms call this to
  // render kind-appropriate label pickers (closed-vocab chip strip
  // for COCO; free-text + suggestions for open-vocab yolo_world).
  // Promise rejects on transport failure; callers should fall back
  // to "no suggestions" rather than hard-fail the form.
  models: {
    prompts: () => request<ModelPromptsResponse>("/v1/models/prompts"),
  },

  // M2.1 Stage B (B5) — motion timeline + on-disk clip storage.
  // Binary endpoints return URLs the caller embeds in <video>/<img>;
  // the engine streams them with HTTP Range support so seeking works.
  storage: {
    local: () => request<StorageLocalResponse>("/v1/storage/local"),
    /// M2.2 Phase 5 — combined hot + cold + backends snapshot. The
    /// admin/storage page polls this; the existing storage tab uses
    /// it to render the cold-tier health pill alongside the M2.1
    /// hot strip.
    full: () => request<StorageResponse>("/v1/storage"),
  },

  /// M2.2 Phase 5 — cold-replication admin mutations. Every method
  /// returns once the engine has audited + republished the change
  /// to the bus, so the UI can refetch `storage.full()` immediately
  /// after and see the new state.
  adminStorage: {
    /// Switch the active cold backend (or disable cold replication
    /// by passing `handle: null`). The handle MUST exist in
    /// `storage_backends` — a 4xx surfaces if it doesn't.
    cold: (req: PutColdReq) =>
      request<{ handle: string | null; throttle_bps: number }>(
        "/v1/admin/storage/cold",
        { method: "PUT", body: JSON.stringify(req) },
      ),
    /// Register or update a backend. Body is validated by the
    /// engine via `nexus_storage::build_backend` before the row is
    /// inserted, so an invalid `kind` or `config` surfaces as 400
    /// without dirtying the table.
    upsertBackend: (handle: string, body: PutBackendReq) =>
      request<StorageBackendOut>(
        `/v1/admin/storage/backends/${encodeURIComponent(handle)}`,
        { method: "PUT", body: JSON.stringify(body) },
      ),
    /// Delete a backend. Returns 409 if the backend is referenced
    /// by any `motion_clips` row OR is the active cold replica;
    /// returns 400 for the implicit `'local'` backend.
    removeBackend: (handle: string) =>
      request<void>(
        `/v1/admin/storage/backends/${encodeURIComponent(handle)}`,
        { method: "DELETE" },
      ),
    /// M2.2 closeout — set/clear the runtime preferred USB label.
    /// Sending `{label: null}` clears the preference; the engine
    /// persists the choice in `engine_runtime_settings` AND updates
    /// the shared `PreferredUsbLabel` handle so the next clip
    /// honours it without restart. In-flight clips finish where
    /// they started.
    usbPreferred: (label: string | null) =>
      request<{ label: string | null }>(
        "/v1/admin/runtime/usb_preferred",
        { method: "PUT", body: JSON.stringify({ label }) },
      ),

    /// M2.2 closeout — OAuth auth-code flow for cloud cold
    /// backends (gdrive, onedrive). The UI never sees the
    /// refresh_token directly; the engine encrypts it before
    /// upserting the row. Flow: caller hits `oauthStart` with the
    /// same client_id/secret it would otherwise put in the form,
    /// opens `authorize_url` in a popup, then polls `oauthStatus`
    /// every ~2 s with the returned `state` token until
    /// `status === "complete"` or `"error"`.
    oauthStart: (provider: string, body: OAuthStartReq) =>
      request<OAuthStartResp>(
        `/v1/admin/oauth/${encodeURIComponent(provider)}/start`,
        { method: "POST", body: JSON.stringify(body) },
      ),
    /// Poll the pending-session status. 404 once the session has
    /// expired (10 min TTL) or after it terminates and is swept;
    /// the caller should treat 404 after a previous `complete`
    /// as success-already-reported.
    oauthStatus: (state: string) =>
      request<OAuthStatusResp>(
        `/v1/admin/oauth/status?state=${encodeURIComponent(state)}`,
      ),
  },

  motion: {
    /// Camera-scoped motion event window. `from` / `to` accept
    /// RFC3339 timestamps; the engine clamps `limit` to [1, 5000]
    /// and defaults the window to the last hour.
    listForCamera: (
      cameraId: CameraId,
      opts: { from?: string; to?: string; limit?: number } = {},
    ) => {
      const q = new URLSearchParams();
      if (opts.from) q.set("from", opts.from);
      if (opts.to) q.set("to", opts.to);
      if (opts.limit != null) q.set("limit", String(opts.limit));
      const qs = q.toString();
      const suffix = qs ? `?${qs}` : "";
      return request<MotionEventRow[]>(
        `/v1/cameras/${cameraId}/motion${suffix}`,
      );
    },

    /// Bucketed motion-density histogram for the per-camera Timeline
    /// grid. The engine clamps `bucket_seconds` to [60, 86400] and
    /// defaults the window to the last 24h with one-hour buckets.
    /// Returned buckets are sparse: empty intervals are absent.
    histogramForCamera: (
      cameraId: CameraId,
      opts: { from?: string; to?: string; bucket_seconds?: number } = {},
    ) => {
      const q = new URLSearchParams();
      if (opts.from) q.set("from", opts.from);
      if (opts.to) q.set("to", opts.to);
      if (opts.bucket_seconds != null) {
        q.set("bucket_seconds", String(opts.bucket_seconds));
      }
      const qs = q.toString();
      const suffix = qs ? `?${qs}` : "";
      return request<MotionHistogramBucket[]>(
        `/v1/cameras/${cameraId}/motion/histogram${suffix}`,
      );
    },
  },

  clips: {
    streamUrl: (clipId: ClipId) => `${BASE}/v1/clips/${clipId}`,
    thumbnailUrl: (clipId: ClipId) =>
      `${BASE}/v1/clips/${clipId}/thumbnail`,
    // Per-clip bbox overlay payload — the modal player draws
    // bounding boxes on a transparent <canvas> synced to
    // <video>.currentTime using these rows.
    tracks: (clipId: ClipId) =>
      request<ClipTracksResponse>(`/v1/clips/${clipId}/tracks`),
  },

  // M-Admin Phase 1B — camera discovery. Mirrors the four admin
  // routes under `/api/v1/admin/discovery/*`. All four go through
  // the admin-auth layer (loopback or `Authorization: Bearer …`
  // depending on env), so `authHeader()` is already applied above.
  discovery: {
    /// Kick off WS-Discovery on the engine's LAN. Returns a fresh
    /// session id the caller polls with `getSession`. The engine
    /// caps the listen window at 5 s and finishes the session
    /// automatically.
    startOnvif: () =>
      request<SessionCreatedResp>("/v1/admin/discovery/onvif", {
        method: "POST",
        body: "{}",
      }),
    /// Kick off a bounded CIDR sweep. The engine rejects prefixes
    /// shorter than /22 outright; /22 requires `confirm: true`
    /// because that's 1022 hosts × N ports.
    startScan: (req: ScanReq) =>
      request<SessionCreatedResp>("/v1/admin/discovery/scan", {
        method: "POST",
        body: JSON.stringify(req),
      }),
    /// Poll a session for progress + accumulated `found[]`.
    /// Returns 404 once the session is past `SESSION_TTL`.
    getSession: (id: string) =>
      request<DiscoverySession>(
        `/v1/admin/discovery/sessions/${encodeURIComponent(id)}`,
      ),
    /// Inline Verify probe — RTSP OPTIONS + DESCRIBE with optional
    /// Digest auth. `sessionId` exists purely so the engine can
    /// audit the probe under the same correlation id.
    probeRtsp: (sessionId: string, req: ProbeRtspReq) =>
      request<ProbeRtspResult>(
        `/v1/admin/discovery/sessions/${encodeURIComponent(sessionId)}/probe-rtsp`,
        { method: "POST", body: JSON.stringify(req) },
      ),
  },

  // M7 Step 6 — alert delivery policy + sinks health.
  //
  // The two `admin/` routes go through the HS256 bearer gate (same
  // `admin_auth_layer` as Storage Admin). The per-rule / per-event
  // endpoints are ungated to match the rest of `/api/rules/:id`
  // (admin-by-loopback for v1). `authHeader()` is already applied
  // by the shared `request` helper for both cases.
  delivery: {
    /// Read the singleton `delivery_settings` row. Returns the
    /// engine-seeded defaults on a fresh install (`enabled = true`,
    /// `schedule = null`, `timezone = "UTC"`).
    getAdmin: () => request<DeliverySettings>("/v1/admin/delivery"),
    /// Atomic update. Engine validates the IANA timezone at the
    /// API boundary (`400` on unknown) and re-validates the 7×48
    /// grid shape before persisting. Publishes
    /// `delivery.settings.changed` so the dispatcher's
    /// `CascadingPolicy` hot-reloads without restart.
    putAdmin: (req: PutAdminDeliveryRequest) =>
      request<DeliverySettings>("/v1/admin/delivery", {
        method: "PUT",
        body: JSON.stringify(req),
      }),
    /// Per-rule override + cascade-resolved view. `inherited = true`
    /// ⇔ `policy == null`. `effective` always carries the
    /// resolved policy the dispatcher would use (cascade rules:
    /// `enabled = global && (policy ?? true)`,
    /// `schedule = policy.schedule ?? global.schedule`).
    getRule: (ruleId: RuleId) =>
      request<RuleDeliveryResponse>(
        `/v1/rules/${encodeURIComponent(ruleId)}/delivery`,
      ),
    /// Set or clear the per-rule override. `{ policy: null }`
    /// clears (rule reverts to inheriting global). Returns 204
    /// and publishes `rule.delivery_policy.changed`. 404 if the
    /// rule id does not exist.
    putRule: (ruleId: RuleId, req: PutRuleDeliveryRequest) =>
      request<void>(
        `/v1/rules/${encodeURIComponent(ruleId)}/delivery`,
        { method: "PUT", body: JSON.stringify(req) },
      ),
    /// Per-event delivery log — one row per (event × configured
    /// sink), ordered by `id ASC`. Powers the alert-detail
    /// delivery badges.
    listForEvent: (eventId: EventId) =>
      request<OutboxRow[]>(
        `/v1/events/${encodeURIComponent(eventId)}/delivery`,
      ),
    /// Sinks-health card payload. The `sinks` array unions
    /// configured-sinks ∪ historical-outbox-sinks; each row is
    /// tagged with `configured: bool` so the operator sees both
    /// freshly-added quiet sinks AND orphaned counts from deleted
    /// sinks. `counts` is keyed by the window labels in
    /// `windows[].label` (currently `"1h"` + `"24h"`).
    sinksHealth: () =>
      request<SinksHealthResponse>("/v1/admin/sinks/health"),
  },

  // M6 Phase 2 Step 2.9c — admin user CRUD. Every route below
  // sits BEHIND `admin_auth_layer` AND the per-handler
  // `AdminContext` extractor; the SPA can only reach them once
  // the session principal carries `role = admin`. Surface 4xx
  // responses to the operator (the error bodies are stable
  // contracts — see `UsersAdminError::IntoResponse`).
  adminUsers: {
    /// Paginated-by-the-store list (no offset/limit on the wire
    /// today). `includeDeleted` surfaces tombstones — the
    /// admin/users page hides them behind a toggle.
    list: (opts: { includeDeleted?: boolean } = {}) => {
      const qs = opts.includeDeleted ? "?include_deleted=true" : "";
      return request<UserListResponse>(`/v1/admin/users${qs}`);
    },

    /// Create a user. When `password` is omitted, the engine
    /// generates a 192-bit OTP and returns it in
    /// `one_time_password`; either way the new row is flagged
    /// `force_password_reset = true`.
    create: (req: CreateUserRequest) =>
      request<CreateUserResponse>("/v1/admin/users", {
        method: "POST",
        body: JSON.stringify(req),
      }),

    /// Mutate role / disabled. Either field may be omitted.
    /// Last-admin protection: demoting or disabling the only
    /// active admin returns 409 `{"error":"last_admin"}`.
    update: (id: number, req: UpdateUserRequest) =>
      request<UserView>(`/v1/admin/users/${id}`, {
        method: "PUT",
        body: JSON.stringify(req),
      }),

    /// Server-generated OTP only — the admin never picks the
    /// new password directly (avoids social-engineering: the
    /// new password is shown ONCE in the response and never
    /// logged). Also revokes EVERY active refresh chain for
    /// the target user.
    resetPassword: (id: number) =>
      request<ResetPasswordResponse>(
        `/v1/admin/users/${id}/reset-password`,
        { method: "POST" },
      ),

    /// Clears lockout counters + `locked_until`.
    unlock: (id: number) =>
      request<void>(`/v1/admin/users/${id}/unlock`, { method: "POST" }),

    /// Soft-delete: sets `deleted_at`, `disabled = true`, and
    /// renames `username → "<id>:deleted-<iso8601>"` so the
    /// original name can be reused. Revokes every active
    /// refresh chain. Last-admin protection applies.
    remove: (id: number) =>
      request<void>(`/v1/admin/users/${id}`, { method: "DELETE" }),
  },

  // M6 Phase 4 Step 4.2 + 4.3 — audit-log read API. Admin-gated.
  // Per-resource feed powers the "History" panel on each detail
  // view; global filtered feed powers the `/admin/audit` table.
  adminAudit: {
    /// Last N rows for `(resource_kind, resource_id)`,
    /// newest-first. `limit` defaults to 50 server-side, max 200.
    forResource: (kind: string, id: string, limit = 50) => {
      const qs = `?limit=${encodeURIComponent(String(limit))}`;
      const enc = `${encodeURIComponent(kind)}/${encodeURIComponent(id)}`;
      return request<AuditRow[]>(`/v1/admin/audit/resource/${enc}${qs}`);
    },

    /// Global filtered audit feed. Every filter field is
    /// optional; `limit` defaults to 50 server-side (max 500),
    /// `offset` defaults to 0.
    list: (filter: AuditGlobalFilter = {}) => {
      const qs = new URLSearchParams();
      if (filter.actor_id) qs.set("actor_id", filter.actor_id);
      if (filter.action) qs.set("action", filter.action);
      if (filter.resource_kind) qs.set("resource_kind", filter.resource_kind);
      if (filter.resource_id) qs.set("resource_id", filter.resource_id);
      if (filter.outcome) qs.set("outcome", filter.outcome);
      if (filter.since) qs.set("since", filter.since);
      if (filter.until) qs.set("until", filter.until);
      if (filter.limit !== undefined)
        qs.set("limit", String(filter.limit));
      if (filter.offset !== undefined)
        qs.set("offset", String(filter.offset));
      const query = qs.toString();
      const path = query ? `/v1/admin/audit?${query}` : "/v1/admin/audit";
      return request<AuditPage>(path);
    },
  },
};
