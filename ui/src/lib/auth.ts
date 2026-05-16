// M-Admin Phase 0 — bearer-token persistence + admin-auth header
// injection.
//
// Rules:
// - The token lives only in `localStorage["nexus_admin_token"]`. The
//   topbar field in `main.ts` is the only writer.
// - Empty/missing token = loopback mode. The engine's admin-auth
//   layer accepts loopback writes without a token per the rules in
//   `crates/nexus-engine/src/api.rs::auth_bootstrap`.
// - Non-empty token is injected into every API call as
//   `Authorization: Bearer <token>` by `client.ts::request()`.
//
// The status pill next to the token field is driven by
// `reportRequestOutcome` — green on the first 2xx of a gated write
// (PUT/POST/DELETE/PATCH) when a token is set, red on any 401/403,
// unknown otherwise. GETs do not flip the pill green because the
// engine answers many GETs without auth, which would give a false
// positive that the token is wired up.

const STORAGE_KEY = "nexus_admin_token";

export type AuthStatus = "unknown" | "ok" | "unauthorized";
type Listener = (s: AuthStatus) => void;

const listeners = new Set<Listener>();
let status: AuthStatus = "unknown";

export function getToken(): string | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v && v.trim() !== "" ? v.trim() : null;
  } catch {
    return null;
  }
}

export function setToken(token: string | null): void {
  try {
    if (token == null || token.trim() === "") {
      localStorage.removeItem(STORAGE_KEY);
    } else {
      localStorage.setItem(STORAGE_KEY, token.trim());
    }
  } catch {
    // localStorage may be disabled (private mode) — silent no-op so
    // the rest of the UI keeps working with an in-memory-only token.
  }
  // Reset to unknown; the next gated-write outcome will repopulate.
  publish("unknown");
}

export function authHeader(): Record<string, string> {
  const t = getToken();
  return t ? { Authorization: `Bearer ${t}` } : {};
}

export function getAuthStatus(): AuthStatus {
  return status;
}

export function onAuthStatusChange(fn: Listener): () => void {
  listeners.add(fn);
  fn(status);
  return () => {
    listeners.delete(fn);
  };
}

/// Called by `client.ts::request()` after every fetch. Method is
/// the HTTP verb so we can distinguish gated writes from anonymous
/// GETs (see header comment for the rationale).
export function reportRequestOutcome(method: string, httpStatus: number): void {
  if (httpStatus === 401 || httpStatus === 403) {
    publish("unauthorized");
    return;
  }
  if (httpStatus >= 200 && httpStatus < 300) {
    const m = method.toUpperCase();
    if (m === "PUT" || m === "POST" || m === "DELETE" || m === "PATCH") {
      publish(getToken() ? "ok" : "unknown");
    }
  }
}

function publish(next: AuthStatus): void {
  if (next === status) return;
  status = next;
  for (const fn of listeners) fn(status);
}
