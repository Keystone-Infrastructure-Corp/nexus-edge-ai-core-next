// M6 Phase 2 Step 2.9 — typed auth-endpoint client.
//
// Lives outside `./client.ts::api` because the auth endpoints
// drive the session lifecycle that `client.ts::request()`
// itself depends on (auto-refresh on 401). Importing them
// from inside `client.ts` would create a cycle the bundler
// can untangle but humans can't read.
//
// All five endpoints live OUTSIDE the admin gate:
//   GET  /api/v1/auth/info             (public probe)
//   POST /api/v1/auth/login
//   POST /api/v1/auth/refresh
//   POST /api/v1/auth/logout
//   POST /api/v1/auth/change-password
//
// The thin `rawRequest` wrapper deliberately does NOT call
// `reportRequestOutcome` or attempt auto-refresh — login by
// definition runs without a session, refresh by definition is
// the recovery path that auto-refresh would call into, and
// change-password takes its own bearer.

import type {
  AuthInfoResponse,
  ChangePasswordRequest,
  LoginRequest,
  LogoutRequest,
  OidcStartRequest,
  OidcStartResponse,
  RefreshRequest,
  TokenResponse,
} from "./types.js";

const BASE = "/api";

async function rawRequest<T>(
  path: string,
  init?: RequestInit,
): Promise<T> {
  const res = await fetch(BASE + path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init?.headers ?? {}),
    },
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`${res.status} ${res.statusText}: ${text}`);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const auth = {
  /// Public probe — anonymous fetch, used on first paint to
  /// decide which login form to render.
  info: () => rawRequest<AuthInfoResponse>("/v1/auth/info"),

  /// Trade username+password for a (access, refresh) pair.
  /// On 401 the engine returns `{"error": "invalid_credentials"}`
  /// for every failure mode (unknown user, bad password, locked,
  /// disabled) — by design. Surface a generic message.
  login: (req: LoginRequest) =>
    rawRequest<TokenResponse>("/v1/auth/login", {
      method: "POST",
      body: JSON.stringify(req),
    }),

  /// Single-use refresh. The old `refresh_token` is invalidated
  /// the moment the new pair is minted; if you ever issue two
  /// refreshes concurrently with the same secret, the second
  /// will revoke the whole chain (replay protection).
  refresh: (req: RefreshRequest) =>
    rawRequest<TokenResponse>("/v1/auth/refresh", {
      method: "POST",
      body: JSON.stringify(req),
    }),

  /// 204 No Content. The engine also clears the session cookie
  /// in the response. If `refresh_token` is omitted, only the
  /// access-token side dies (refresh chain stays usable) —
  /// always send the current refresh.
  logout: (req: LogoutRequest = {}, accessToken: string) =>
    rawRequest<void>("/v1/auth/logout", {
      method: "POST",
      body: JSON.stringify(req),
      headers: { Authorization: `Bearer ${accessToken}` },
    }),

  /// 200 with a fresh (access, refresh) pair on success. Clears
  /// `users.force_password_reset` server-side. The caller MUST
  /// replace its stored session with the response — every prior
  /// refresh token for this user is revoked atomically.
  changePassword: (req: ChangePasswordRequest, accessToken: string) =>
    rawRequest<TokenResponse>("/v1/auth/change-password", {
      method: "POST",
      body: JSON.stringify(req),
      headers: { Authorization: `Bearer ${accessToken}` },
    }),

  /// M6 Phase 3 Step 3.3 UI — mint PKCE verifier + state +
  /// nonce server-side, get back the authorization URL to
  /// redirect the browser to. The engine ALSO sets a
  /// `__Host-nexus_oidc_state` cookie that the callback
  /// verifies; we don't have to do anything with the returned
  /// `state` string. On success the caller should
  /// `window.location.assign(authorization_url)` to hand off
  /// the browser.
  oidcStart: (req: OidcStartRequest = {}) =>
    rawRequest<OidcStartResponse>("/v1/auth/oidc/start", {
      method: "POST",
      body: JSON.stringify(req),
    }),
};
