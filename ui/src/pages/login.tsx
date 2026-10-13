// Login page. Mode-aware:
//   - local      → username/password form
//   - oidc       → OIDC sign-in button only
//   - hybrid     → form + OIDC button below
//
// First-run aware: when /auth/info reports `first_run_pending: true`
// (engine has no admin user yet, local mode allowed), the page
// renders a "Set up the initial admin password" form that POSTs
// to /auth/first-run-setup and auto-signs-in on success.
//
// Auth modes "none" and "dev_token" are intentionally NOT supported
// in the new UI; the engine still accepts them for a transition
// window but operators must migrate per the Phase 0 plan.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Loader2, ShieldCheck } from "lucide-react";
import { useState } from "react";

import { authApi } from "@/api/auth";
import { ApiError } from "@/api/client";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useAuth } from "@/lib/auth";
import { MIN_PASSWORD_LEN } from "@/lib/password-policy";

export function LoginPage() {
  const navigate = useNavigate();
  const { setSessionFromTokens } = useAuth();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [error, setError] = useState<string | null>(null);

  const info = useQuery({
    queryKey: ["auth-info"],
    queryFn: () => authApi.info(),
    // Short stale time on the login page so a concurrent
    // first-run-setup elsewhere (or the operator's own
    // submit) flips the form back to the normal sign-in form
    // without a hard refresh.
    staleTime: 5_000,
    retry: 1,
  });

  const login = useMutation({
    mutationFn: (vars: { username: string; password: string }) =>
      authApi.login(vars.username, vars.password),
    onSuccess: (tokens) => {
      setSessionFromTokens(tokens);
      navigate({ to: "/dashboard" });
    },
    onError: (e: unknown) => {
      if (e instanceof ApiError) {
        setError(e.message);
      } else {
        setError("Unable to sign in. Check the engine is reachable.");
      }
    },
  });

  const firstRunSetup = useMutation({
    mutationFn: (vars: { username?: string; password: string }) =>
      authApi.firstRunSetup(vars.password, vars.username),
    onSuccess: (tokens) => {
      setSessionFromTokens(tokens);
      navigate({ to: "/dashboard" });
    },
    onError: (e: unknown) => {
      if (e instanceof ApiError) {
        // 409 == another setup beat us, or DB was already
        // populated since we loaded /auth/info. Re-fetch so
        // the form flips back to the normal sign-in form on
        // the next render.
        if (e.status === 409) {
          setError(
            "Another setup completed first; refresh and sign in with the password just created.",
          );
          info.refetch();
        } else {
          setError(e.message);
        }
      } else {
        setError("Unable to complete setup. Check the engine is reachable.");
      }
    },
  });

  // OIDC: POST to /auth/oidc/start to mint state+PKCE server-side,
  // then hand the browser to the IdP authorization URL it returns.
  // A bare GET would 405 — the start endpoint is POST only so the
  // optional `redirect_to` body field can constrain the post-login
  // landing path.
  const oidcStart = useMutation({
    mutationFn: () => authApi.oidcStart("/"),
    onSuccess: (res) => {
      window.location.assign(res.authorization_url);
    },
    onError: (e: unknown) => {
      const msg = e instanceof ApiError ? e.message : String(e);
      setError(`OIDC sign-in failed: ${msg}`);
    },
  });

  const allowsLocal = info.data?.allows_local ?? info.data?.mode !== "oidc";
  const allowsOidc = info.data?.allows_oidc ?? info.data?.mode === "oidc";
  const oidcLabel = info.data?.oidc_display_name ?? "single sign-on";
  // First-run mode is mutually exclusive with the regular login
  // form: when true the operator hasn't picked a password yet so
  // the "Sign in" form would always 401.
  const firstRunPending = Boolean(info.data?.first_run_pending) && allowsLocal;

  return (
    <div className="flex h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-sm">
        <CardHeader className="space-y-3">
          <div className="flex items-center gap-2 text-primary">
            <ShieldCheck className="h-5 w-5" />
            <span className="text-sm font-semibold tracking-tight">Nexus Edge AI</span>
          </div>
          <CardTitle>{firstRunPending ? "Set up the admin account" : "Sign in"}</CardTitle>
          <CardDescription>
            {firstRunPending
              ? "No admin user exists yet. Choose the initial admin password — anyone with network access to this device can claim it until you do."
              : "Access the local edge appliance console."}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {info.isError ? (
            <div className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
              Could not reach the engine. Is `nexus-engine` running?
            </div>
          ) : null}

          {firstRunPending ? (
            <form
              className="space-y-3"
              onSubmit={(e) => {
                e.preventDefault();
                setError(null);
                if (password.length < MIN_PASSWORD_LEN) {
                  setError(`Password must be at least ${MIN_PASSWORD_LEN} characters.`);
                  return;
                }
                if (password !== confirmPassword) {
                  setError("Passwords do not match.");
                  return;
                }
                firstRunSetup.mutate({
                  // Default to "admin" when blank — matches the
                  // engine-side default and keeps the form a
                  // single-field happy path.
                  username: username.trim() || undefined,
                  password,
                });
              }}
            >
              <div className="space-y-1.5">
                <Label htmlFor="username">Username</Label>
                <Input
                  id="username"
                  autoComplete="username"
                  placeholder="admin"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="password">New password</Label>
                <Input
                  id="password"
                  type="password"
                  autoComplete="new-password"
                  required
                  minLength={MIN_PASSWORD_LEN}
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  Minimum {MIN_PASSWORD_LEN} characters.
                </p>
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="confirm">Confirm password</Label>
                <Input
                  id="confirm"
                  type="password"
                  autoComplete="new-password"
                  required
                  minLength={MIN_PASSWORD_LEN}
                  value={confirmPassword}
                  onChange={(e) => setConfirmPassword(e.target.value)}
                />
              </div>
              {error ? (
                <div className="rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
                  {error}
                </div>
              ) : null}
              <Button
                type="submit"
                className="w-full"
                disabled={firstRunSetup.isPending}
              >
                {firstRunSetup.isPending ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  "Create admin account"
                )}
              </Button>
            </form>
          ) : allowsLocal ? (
            <form
              className="space-y-3"
              onSubmit={(e) => {
                e.preventDefault();
                setError(null);
                login.mutate({ username, password });
              }}
            >
              <div className="space-y-1.5">
                <Label htmlFor="username">Username</Label>
                <Input
                  id="username"
                  autoComplete="username"
                  required
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="password">Password</Label>
                <Input
                  id="password"
                  type="password"
                  autoComplete="current-password"
                  required
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              </div>
              {error ? (
                <div className="rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
                  {error}
                </div>
              ) : null}
              <Button type="submit" className="w-full" disabled={login.isPending}>
                {login.isPending ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  "Sign in"
                )}
              </Button>
            </form>
          ) : null}

          {allowsOidc ? (
            <Button
              variant="outline"
              className="w-full"
              disabled={oidcStart.isPending}
              onClick={() => {
                setError(null);
                oidcStart.mutate();
              }}
            >
              {oidcStart.isPending ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <>Continue with {oidcLabel}</>
              )}
            </Button>
          ) : null}
        </CardContent>
      </Card>
    </div>
  );
}
