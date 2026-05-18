// Typed fetch wrapper. Every method takes/returns a typed payload from
// `./types.ts`, so callers can never confuse the API shape with the UI's
// own state.

import { authHeader, reportRequestOutcome } from "../lib/auth.js";
import type {
  AlertEvent,
  BackendsResponse,
  CameraConfig,
  CameraId,
  ClipId,
  DiscoverySession,
  MotionEventRow,
  MotionHistogramBucket,
  OAuthStartReq,
  OAuthStartResp,
  OAuthStatusResp,
  ProbeRtspReq,
  ProbeRtspResult,
  PutBackendReq,
  PutColdReq,
  RuleConfig,
  RuleId,
  RuleValidateResponse,
  ScanReq,
  SessionCreatedResp,
  StorageBackendOut,
  StorageLocalResponse,
  StorageResponse,
  FrameMetadata,
} from "./types.js";

const BASE = "/api";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const method = (init?.method ?? "GET").toUpperCase();
  const res = await fetch(BASE + path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...authHeader(),
      ...(init?.headers ?? {}),
    },
  });
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
  },

  events: {
    recent: (limit = 100) =>
      request<AlertEvent[]>(`/events?limit=${limit}`),
  },

  backends: () => request<BackendsResponse>("/backends"),

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
        `/v1/admin/discovery/${encodeURIComponent(id)}`,
      ),
    /// Inline Verify probe — RTSP OPTIONS + DESCRIBE with optional
    /// Digest auth. `sessionId` exists purely so the engine can
    /// audit the probe under the same correlation id.
    probeRtsp: (sessionId: string, req: ProbeRtspReq) =>
      request<ProbeRtspResult>(
        `/v1/admin/discovery/${encodeURIComponent(sessionId)}/probe-rtsp`,
        { method: "POST", body: JSON.stringify(req) },
      ),
  },
};
