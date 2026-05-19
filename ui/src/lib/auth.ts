// M6 Phase 2 Step 2.9 — session + legacy bearer state.
//
// Two coexisting auth shapes:
//
// 1. **Legacy dev token** (Phase 0): a single string in
//    `localStorage["nexus_admin_token"]`, set via the topbar
//    paste-field. Used under `auth.mode in {none, dev_token}`.
//
// 2. **Session** (this step): an `access_token + refresh_token +
//    user` triple in `localStorage["nexus_session"]`, set by
//    the login overlay. Used under `auth.mode in {local, oidc,
//    hybrid}`. The access token is short-lived (15 min by
//    default); on 401 we silently refresh once before retrying.
//
// `authHeader()` prefers the session over the legacy token —
// once a user has logged in, the topbar paste-field is hidden
// anyway, but the precedence makes the boot order irrelevant.
//
// The status pill next to the (mode-dependent) topbar control
// is driven by `reportRequestOutcome`: green on the first 2xx
// of a gated write when *some* credential is set, red on any
// 401/403, unknown otherwise.

import { auth as authApi } from "../api/auth.js";
import type {
  AuthInfoResponse,
  AuthMode,
  SessionUser,
  TokenResponse,
} from "../api/types.js";

const LEGACY_KEY = "nexus_admin_token";
const SESSION_KEY = "nexus_session";

export type AuthStatus = "unknown" | "ok" | "unauthorized";
type StatusListener = (s: AuthStatus) => void;
type SessionListener = (s: Session | null) => void;
type AuthInfoListener = (info: AuthInfoResponse | null) => void;

const statusListeners = new Set<StatusListener>();
const sessionListeners = new Set<SessionListener>();
const authInfoListeners = new Set<AuthInfoListener>();

let status: AuthStatus = "unknown";

/// Locally-cached projection of the engine's `/v1/auth/info`
/// probe. Populated once at boot by `loadAuthInfo()`. Mutating
/// `auth.mode` in `nexus.toml` requires an engine restart AND a
/// page reload to take effect on the SPA — same posture as
/// every other static config.
let cachedAuthInfo: AuthInfoResponse | null = null;

/// In-flight refresh promise dedupe — under burst load the SPA
/// can fire many parallel 401s; we only want one refresh round-
/// trip. Every subsequent caller awaits the same promise.
let inflightRefresh: Promise<Session | null> | null = null;

// ---------------------------------------------------------------------------
// Session shape (browser-side only — the wire types live in `api/types.ts`).
// ---------------------------------------------------------------------------

export interface Session {
  access_token: string;
  refresh_token: string;
  /// Epoch-millis after which the access token is expired. We
  /// store the absolute deadline (not `expires_in`) so clock
  /// drift after a tab sleep doesn't quietly keep using a dead
  /// token.
  access_expires_at: number;
  refresh_expires_at: number;
  user: SessionUser;
}

// ---------------------------------------------------------------------------
// Legacy bearer (Phase 0) — unchanged contract.
// ---------------------------------------------------------------------------

export function getToken(): string | null {
  try {
    const v = localStorage.getItem(LEGACY_KEY);
    return v && v.trim() !== "" ? v.trim() : null;
  } catch {
    return null;
  }
}

export function setToken(token: string | null): void {
  try {
    if (token == null || token.trim() === "") {
      localStorage.removeItem(LEGACY_KEY);
    } else {
      localStorage.setItem(LEGACY_KEY, token.trim());
    }
  } catch {
    // localStorage may be disabled (private mode) — silent
    // no-op so the rest of the UI keeps working with an
    // in-memory-only token.
  }
  publishStatus("unknown");
}

// ---------------------------------------------------------------------------
// Session (Step 2.9) — read/write/observe.
// ---------------------------------------------------------------------------

export function getSession(): Session | null {
  try {
    const raw = localStorage.getItem(SESSION_KEY);
    if (!raw) return null;
    const s = JSON.parse(raw) as Session;
    // Validate the shape just enough to fail closed on a hand-
    // mangled value (e.g. someone bumped the schema).
    if (
      typeof s !== "object" ||
      typeof s.access_token !== "string" ||
      typeof s.refresh_token !== "string" ||
      typeof s.access_expires_at !== "number" ||
      typeof s.refresh_expires_at !== "number" ||
      typeof s.user !== "object" ||
      s.user == null
    ) {
      return null;
    }
    return s;
  } catch {
    return null;
  }
}

export function setSession(s: Session | null): void {
  try {
    if (s == null) {
      localStorage.removeItem(SESSION_KEY);
    } else {
      localStorage.setItem(SESSION_KEY, JSON.stringify(s));
    }
  } catch {
    // Private mode — silent no-op.
  }
  publishSession(s);
  publishStatus(s ? "ok" : "unknown");
}

/// Build a Session from the engine's `TokenResponse`. Pure
/// helper — does NOT persist.
export function sessionFromTokenResponse(t: TokenResponse): Session {
  const now = Date.now();
  return {
    access_token: t.access_token,
    refresh_token: t.refresh_token,
    access_expires_at: now + t.expires_in * 1000,
    refresh_expires_at: now + t.refresh_expires_in * 1000,
    user: t.user,
  };
}

export function onSessionChange(fn: SessionListener): () => void {
  sessionListeners.add(fn);
  fn(getSession());
  return () => {
    sessionListeners.delete(fn);
  };
}

function publishSession(s: Session | null): void {
  for (const fn of sessionListeners) fn(s);
}

// ---------------------------------------------------------------------------
// Outgoing header — session wins over legacy bearer.
// ---------------------------------------------------------------------------

export function authHeader(): Record<string, string> {
  const s = getSession();
  if (s) {
    // Send the bearer even if access_expires_at is past — the
    // engine will return 401 and `client.ts::request` will
    // call `tryRefresh()` then retry. If we suppressed the
    // bearer here the audit log would carry "no bearer" for
    // every refresh-window request, which is misleading.
    return { Authorization: `Bearer ${s.access_token}` };
  }
  const t = getToken();
  return t ? { Authorization: `Bearer ${t}` } : {};
}

// ---------------------------------------------------------------------------
// Auto-refresh on 401 — called by `client.ts::request`.
// ---------------------------------------------------------------------------

/// Returns the new Session on success, or null on hard failure
/// (no session present, expired refresh, server error). Caller
/// is expected to retry their original request exactly once
/// when the result is non-null; on null they should drop the
/// session and prompt re-login.
export async function tryRefresh(): Promise<Session | null> {
  if (inflightRefresh) return inflightRefresh;
  inflightRefresh = (async () => {
    const cur = getSession();
    if (!cur) return null;
    if (Date.now() >= cur.refresh_expires_at) {
      setSession(null);
      return null;
    }
    try {
      const fresh = await authApi.refresh({ refresh_token: cur.refresh_token });
      const next = sessionFromTokenResponse(fresh);
      setSession(next);
      return next;
    } catch {
      setSession(null);
      return null;
    }
  })();
  try {
    return await inflightRefresh;
  } finally {
    inflightRefresh = null;
  }
}

// ---------------------------------------------------------------------------
// Cached auth-mode probe — populated by `loadAuthInfo()` at boot.
// ---------------------------------------------------------------------------

export function getAuthInfo(): AuthInfoResponse | null {
  return cachedAuthInfo;
}

export function getAuthMode(): AuthMode | null {
  return cachedAuthInfo?.mode ?? null;
}

export function onAuthInfoChange(fn: AuthInfoListener): () => void {
  authInfoListeners.add(fn);
  fn(cachedAuthInfo);
  return () => {
    authInfoListeners.delete(fn);
  };
}

/// Fetch + cache the public probe. Safe to call repeatedly;
/// only the most recent result is retained. On network error
/// the cache is cleared (so the UI falls back to its default
/// "show everything" posture rather than locking the user out
/// from a transient failure).
export async function loadAuthInfo(): Promise<AuthInfoResponse | null> {
  try {
    const info = await authApi.info();
    cachedAuthInfo = info;
    for (const fn of authInfoListeners) fn(info);
    return info;
  } catch {
    cachedAuthInfo = null;
    for (const fn of authInfoListeners) fn(null);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Status pill — unchanged contract from Phase 0.
// ---------------------------------------------------------------------------

export function getAuthStatus(): AuthStatus {
  return status;
}

export function onAuthStatusChange(fn: StatusListener): () => void {
  statusListeners.add(fn);
  fn(status);
  return () => {
    statusListeners.delete(fn);
  };
}

/// Called by `client.ts::request()` after every fetch. Method
/// is the HTTP verb so we can distinguish gated writes from
/// anonymous GETs. A 2xx GET doesn't flip the pill green —
/// many engine GETs answer without auth and would give a false
/// positive that the credential is wired up.
export function reportRequestOutcome(method: string, httpStatus: number): void {
  if (httpStatus === 401 || httpStatus === 403) {
    publishStatus("unauthorized");
    return;
  }
  if (httpStatus >= 200 && httpStatus < 300) {
    const m = method.toUpperCase();
    if (m === "PUT" || m === "POST" || m === "DELETE" || m === "PATCH") {
      const haveCred = getSession() != null || getToken() != null;
      publishStatus(haveCred ? "ok" : "unknown");
    }
  }
}

function publishStatus(next: AuthStatus): void {
  if (next === status) return;
  status = next;
  for (const fn of statusListeners) fn(status);
}

// ---------------------------------------------------------------------------
// M6 Phase 3 Step 3.3 UI — OIDC handoff-cookie hydration.
//
// After a successful OIDC callback, the engine 302-redirects
// the browser back to `/` (or whatever `redirect_to` was) and
// attaches a short-lived `nexus_oidc_handoff` cookie containing
// the same `TokenResponse` shape the local-login JSON endpoint
// returns. The cookie is base64url-no-pad-encoded JSON, NOT
// HttpOnly (the SPA needs to read it from JS).
//
// On first paint we look for the cookie, decode it, install
// the session in localStorage, and immediately delete the
// cookie so a reload doesn't re-hydrate a stale token.
//
// Posture choices:
//
// * Silently no-op on any failure path (cookie absent,
//   malformed base64, JSON parse error, missing fields). A
//   bad handoff cookie shouldn't crash the boot sequence; the
//   user just falls through to the login overlay and tries
//   again.
// * Decode + validate FULLY before calling `setSession` so a
//   half-formed handoff can't half-populate localStorage.
// * Clear the cookie unconditionally on the success path so a
//   page reload during the 60s TTL window doesn't re-fire the
//   onSessionChange listeners with the same value.
// ---------------------------------------------------------------------------

const HANDOFF_COOKIE = "nexus_oidc_handoff";

/// Read the OIDC handoff cookie (if any), install the resulting
/// session, and clear the cookie. Returns true iff a session
/// was successfully installed. Safe to call unconditionally on
/// every boot — a no-op when the cookie isn't present.
export function hydrateFromOidcHandoff(): boolean {
  const raw = readCookie(HANDOFF_COOKIE);
  if (raw == null) return false;
  // Whatever happens below, drop the cookie so it can't fire
  // twice. Set this BEFORE decoding so a malformed payload
  // doesn't leave a poison value in the jar.
  clearCookie(HANDOFF_COOKIE);
  try {
    const json = base64UrlDecodeToString(raw);
    const tr = JSON.parse(json) as TokenResponse;
    if (
      typeof tr !== "object" ||
      tr == null ||
      typeof tr.access_token !== "string" ||
      typeof tr.refresh_token !== "string" ||
      typeof tr.expires_in !== "number" ||
      typeof tr.refresh_expires_in !== "number" ||
      typeof tr.user !== "object" ||
      tr.user == null
    ) {
      return false;
    }
    setSession(sessionFromTokenResponse(tr));
    return true;
  } catch {
    return false;
  }
}

function readCookie(name: string): string | null {
  if (typeof document === "undefined") return null;
  const cookies = document.cookie ? document.cookie.split(";") : [];
  for (const c of cookies) {
    const trimmed = c.trim();
    const eq = trimmed.indexOf("=");
    if (eq <= 0) continue;
    if (trimmed.slice(0, eq) === name) {
      return trimmed.slice(eq + 1);
    }
  }
  return null;
}

function clearCookie(name: string): void {
  if (typeof document === "undefined") return;
  // Mirror the engine's `Path=/` so the deletion actually
  // targets the same cookie. `Max-Age=0` and a past
  // `Expires=` are belt-and-suspenders for older browsers.
  document.cookie =
    `${name}=; Path=/; Max-Age=0; ` +
    `Expires=Thu, 01 Jan 1970 00:00:00 GMT; SameSite=Lax`;
}

function base64UrlDecodeToString(input: string): string {
  // RFC 4648 §5: url-safe base64 swaps `+` -> `-`, `/` -> `_`,
  // and the engine emits the no-padding variant. atob() needs
  // standard base64 with padding.
  let b64 = input.replace(/-/g, "+").replace(/_/g, "/");
  const pad = b64.length % 4;
  if (pad === 2) b64 += "==";
  else if (pad === 3) b64 += "=";
  else if (pad !== 0) throw new Error("invalid base64url length");
  const bin = atob(b64);
  // Decode the UTF-8 bytes back to a JS string. `atob` gives
  // us a binary string; we need to walk it byte-by-byte to
  // reconstruct the original codepoints.
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
}

// ---------------------------------------------------------------------------
// M6 Phase 3 Step 3.3 UI — surface `?oidc_error=...` from the
// callback URL exactly once per page load. The engine
// 302-redirects to `/?oidc_error=<code>` on IdP-side cancel /
// failure so the SPA can render a friendly toast.
// ---------------------------------------------------------------------------

/// Pluck `?oidc_error=<code>` out of the current URL and strip
/// it from the address bar. Returns the raw error code (e.g.
/// `access_denied`, `bad_request`, `unmapped_role`) or null
/// when not present. Safe to call multiple times — only the
/// first call returns a value; subsequent calls return null
/// because the query string is rewritten.
export function consumeOidcError(): string | null {
  if (typeof window === "undefined") return null;
  const url = new URL(window.location.href);
  const err = url.searchParams.get("oidc_error");
  if (err == null) return null;
  url.searchParams.delete("oidc_error");
  // Preserve hash (route) but strip the now-empty query so
  // hitting back doesn't re-trigger the toast.
  const next = url.pathname + (url.search || "") + url.hash;
  try {
    window.history.replaceState(null, "", next);
  } catch {
    // Some embeds disable history mutation; silently skip.
  }
  return err;
}

// ---------------------------------------------------------------------------
// Top-level logout helper. Best-effort POST + always clear the
// local session.
// ---------------------------------------------------------------------------

export async function logout(): Promise<void> {
  const s = getSession();
  if (s) {
    try {
      await authApi.logout({ refresh_token: s.refresh_token }, s.access_token);
    } catch {
      // Network/engine failure — we still clear local state
      // below so the UI returns to the login overlay either
      // way. Worst case the refresh chain stays alive in the
      // DB until its expiry; not a security issue (the SPA no
      // longer has the secret).
    }
  }
  setSession(null);
}
