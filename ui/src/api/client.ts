// HTTP client with bearer auth + transparent 401 -> refresh -> retry-once.
//
// Auth flow (matches engine /api/v1/auth/* contract):
//   1. Every request reads access_token from the in-memory session.
//   2. On 401, we try POST /api/v1/auth/refresh ONCE. If that succeeds,
//      the original request is retried with the new access_token.
//   3. If refresh itself 401s, we clear the session and emit an event
//      that the AuthProvider picks up to bounce the user to /login.
//
// We deliberately do NOT call the refresh endpoint pre-emptively on TTL
// expiry. Server time and client time can drift; 401-driven refresh is
// the correct trigger.
//
// 204 No Content is returned as `undefined`. Callers must type their
// return as `Promise<void>` for endpoints like POST /v1/auth/logout
// and POST /v1/auth/change-password.

import type { TokenResponse } from "@/api/types";

const BASE = "/api";

// In-memory mirror of the session. Owned by AuthProvider; this module
// just reads/writes via setters so we don't introduce a React import
// in non-component code.
let currentAccessToken: string | null = null;
let currentRefreshToken: string | null = null;
let onSessionRotated: ((tokens: TokenResponse) => void) | null = null;
let onSessionCleared: (() => void) | null = null;

export function configureClient(opts: {
  accessToken: string | null;
  refreshToken: string | null;
  onRotate: (tokens: TokenResponse) => void;
  onClear: () => void;
}) {
  currentAccessToken = opts.accessToken;
  currentRefreshToken = opts.refreshToken;
  onSessionRotated = opts.onRotate;
  onSessionCleared = opts.onClear;
}

export function setTokens(access: string | null, refresh: string | null) {
  currentAccessToken = access;
  currentRefreshToken = refresh;
}

/** Returns the current in-memory access token, or null if logged out.
 *  Used by callers that need to attach the bearer token to a raw
 *  `fetch()` (e.g. binary streaming endpoints that can't use
 *  `api.get()` because they need access to the Response headers). */
export function getAccessToken(): string | null {
  return currentAccessToken;
}

export class ApiError extends Error {
  status: number;
  body: unknown;
  constructor(status: number, message: string, body?: unknown) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }
}

export interface RequestOptions {
  method?: string;
  body?: unknown;
  headers?: Record<string, string>;
  signal?: AbortSignal;
  /** When true, do not auto-refresh on 401 (used by the refresh call itself). */
  skipRefresh?: boolean;
  /** When true, parse response as JPEG blob URL (for camera frames). */
  asBlob?: boolean;
  /** When set, query string is appended to the path. */
  query?: Record<string, string | number | boolean | undefined>;
}

function buildUrl(path: string, query?: RequestOptions["query"]): string {
  const url = path.startsWith("/api/") ? path : `${BASE}${path}`;
  if (!query) return url;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null) continue;
    params.set(k, String(v));
  }
  const qs = params.toString();
  return qs ? `${url}?${qs}` : url;
}

async function doFetch<T>(path: string, opts: RequestOptions): Promise<T> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    ...opts.headers,
  };
  if (opts.body !== undefined && !(opts.body instanceof FormData)) {
    headers["Content-Type"] = "application/json";
  }
  if (currentAccessToken && !headers.Authorization) {
    headers.Authorization = `Bearer ${currentAccessToken}`;
  }

  const res = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? "GET",
    headers,
    body:
      opts.body === undefined
        ? undefined
        : opts.body instanceof FormData
          ? opts.body
          : JSON.stringify(opts.body),
    signal: opts.signal,
  });

  if (res.status === 401 && !opts.skipRefresh && currentRefreshToken) {
    const refreshed = await tryRefresh();
    if (refreshed) {
      return doFetch<T>(path, { ...opts, skipRefresh: true });
    }
  }

  if (!res.ok) {
    let body: unknown = undefined;
    try {
      body = await res.json();
    } catch {
      // body wasn't JSON
    }
    const message =
      (body && typeof body === "object" && "error" in body
        ? String((body as { error: unknown }).error)
        : null) ?? `HTTP ${res.status}`;
    throw new ApiError(res.status, message, body);
  }

  if (res.status === 204) {
    return undefined as T;
  }

  if (opts.asBlob) {
    return (await res.blob()) as unknown as T;
  }

  const ct = res.headers.get("content-type") ?? "";
  if (ct.includes("application/json")) {
    return (await res.json()) as T;
  }
  return (await res.text()) as unknown as T;
}

async function tryRefresh(): Promise<boolean> {
  if (!currentRefreshToken) return false;
  try {
    const res = await fetch(`${BASE}/v1/auth/refresh`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ refresh_token: currentRefreshToken }),
    });
    if (!res.ok) {
      // v0.1.36 — distinguish idle_expired from generic refresh
      // failure so the AuthProvider can show a friendlier toast
      // ("signed out after 20 min idle") instead of the generic
      // "session expired" copy.
      let code: string | null = null;
      try {
        const body = (await res.clone().json()) as { code?: unknown };
        if (typeof body.code === "string") code = body.code;
      } catch {
        // body wasn't JSON
      }
      onSessionCleared?.();
      if (code === "idle_expired") {
        window.dispatchEvent(new CustomEvent("nexus:idle-expired"));
      }
      return false;
    }
    const tokens = (await res.json()) as TokenResponse;
    currentAccessToken = tokens.access_token;
    currentRefreshToken = tokens.refresh_token;
    onSessionRotated?.(tokens);
    return true;
  } catch {
    onSessionCleared?.();
    return false;
  }
}

export const api = {
  get: <T>(path: string, opts?: Omit<RequestOptions, "method" | "body">) =>
    doFetch<T>(path, { ...opts, method: "GET" }),
  post: <T>(path: string, body?: unknown, opts?: Omit<RequestOptions, "method" | "body">) =>
    doFetch<T>(path, { ...opts, method: "POST", body }),
  put: <T>(path: string, body?: unknown, opts?: Omit<RequestOptions, "method" | "body">) =>
    doFetch<T>(path, { ...opts, method: "PUT", body }),
  delete: <T = void>(path: string, opts?: Omit<RequestOptions, "method" | "body">) =>
    doFetch<T>(path, { ...opts, method: "DELETE" }),
};

/** Returns the absolute URL for a streaming endpoint (used by EventSource / video src). */
export function streamUrl(path: string, query?: RequestOptions["query"]): string {
  return buildUrl(path, query);
}
