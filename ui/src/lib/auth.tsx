// AuthProvider \u2014 the single source of truth for the logged-in session.
//
// Storage:  localStorage["nexus_session"] (JSON serialised PersistedSession).
// In-memory: React state that mirrors localStorage so children re-render.
// Client:    on every mount the api/client module is wired with our setters
//            so its 401-driven refresh loop can rotate tokens without going
//            through React.
//
// 204 on POST /v1/auth/change-password: handled in <ChangePasswordForm>;
// we DO NOT call setSession on a 204 \u2014 the cached session already has
// what we need and the next request will exercise refresh naturally.

import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";

import { configureClient } from "@/api/client";
import type { AuthUser, TokenResponse } from "@/api/types";
import { useIdleLogout } from "@/lib/idle";

const STORAGE_KEY = "nexus_session";

export interface PersistedSession {
  access_token: string;
  refresh_token: string;
  access_expires_at: number;
  refresh_expires_at: number;
  user: AuthUser;
}

function loadSession(): PersistedSession | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as PersistedSession;
    if (
      typeof parsed.access_token === "string" &&
      typeof parsed.refresh_token === "string" &&
      parsed.user &&
      typeof parsed.user.username === "string"
    ) {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

function saveSession(s: PersistedSession | null) {
  if (s === null) {
    localStorage.removeItem(STORAGE_KEY);
  } else {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  }
}

function sessionFromTokens(t: TokenResponse): PersistedSession {
  const now = Date.now();
  return {
    access_token: t.access_token,
    refresh_token: t.refresh_token,
    access_expires_at: now + t.expires_in * 1000,
    refresh_expires_at: now + t.refresh_expires_in * 1000,
    user: t.user,
  };
}

interface AuthCtx {
  session: PersistedSession | null;
  setSessionFromTokens: (t: TokenResponse) => void;
  clearSession: () => void;
}

const Ctx = createContext<AuthCtx | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<PersistedSession | null>(() => loadSession());

  const setSessionFromTokens = useCallback((t: TokenResponse) => {
    const s = sessionFromTokens(t);
    saveSession(s);
    setSession(s);
  }, []);

  const clearSession = useCallback(() => {
    saveSession(null);
    setSession(null);
  }, []);

  // Wire the imperative HTTP client every time the session shape changes.
  // It captures the setters so the 401-refresh loop can rotate tokens
  // without going through React state mutation in the hot path.
  useEffect(() => {
    configureClient({
      accessToken: session?.access_token ?? null,
      refreshToken: session?.refresh_token ?? null,
      onRotate: (t) => setSessionFromTokens(t),
      onClear: () => clearSession(),
    });
  }, [session, setSessionFromTokens, clearSession]);

  // v0.1.36 — 20-minute client-side idle nudge. Server is the
  // authoritative gate; this just clears local state slightly
  // before the next refresh would have been refused so the UX
  // is a clean redirect to /login instead of a failed request.
  useIdleLogout(session !== null, clearSession);

  // Also listen for the global idle_expired event the api client
  // dispatches when the refresh endpoint returned the 401 first
  // (e.g. the user came back after a long lunch with a stale tab).
  useEffect(() => {
    const handler = () => clearSession();
    window.addEventListener("nexus:idle-expired", handler);
    return () => window.removeEventListener("nexus:idle-expired", handler);
  }, [clearSession]);

  const value = useMemo(
    () => ({ session, setSessionFromTokens, clearSession }),
    [session, setSessionFromTokens, clearSession],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

// The hooks below are co-located with <AuthProvider> intentionally so
// the Context type doesn't have to be re-exported and re-imported.
// react-refresh wants component-only exports for full HMR; these hooks
// are stable enough that a manual page reload on auth changes is fine.
// eslint-disable-next-line react-refresh/only-export-components
export function useAuth() {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useAuth() must be used inside <AuthProvider>");
  return ctx;
}

// eslint-disable-next-line react-refresh/only-export-components
export function useSession() {
  return useAuth().session;
}

// eslint-disable-next-line react-refresh/only-export-components
export function useIsAdmin() {
  const s = useSession();
  return s?.user.role === "admin";
}
