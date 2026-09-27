// Top bar: brand + system-health pill + cloud-tunnel pill + user
// menu + sign-out button. Both pills poll their respective
// /api endpoints every 10s. The cloud pill is unauthenticated
// (the underlying engine route is too) so it renders correctly
// even on the login overlay.

import { useMutation, useQuery } from "@tanstack/react-query";
import { LogOut, ShieldCheck, User } from "lucide-react";
import { useState } from "react";

import { api } from "@/api/client";
import { authApi } from "@/api/auth";
import { getHealth } from "@/api/system";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useAuth, useSession } from "@/lib/auth";
import { engineHealth, issueCodes } from "@/lib/engineHealth";
import { cn } from "@/lib/utils";

interface CloudStatusResponse {
  enrolled: boolean;
  connected: boolean;
}

export function TopBar() {
  const session = useSession();
  const { clearSession } = useAuth();
  const [menuOpen, setMenuOpen] = useState(false);

  const health = useQuery({
    queryKey: ["health"],
    queryFn: getHealth,
    refetchInterval: 10_000,
    retry: 1,
  });

  const cloud = useQuery({
    queryKey: ["cloud-status"],
    queryFn: () => api.get<CloudStatusResponse>("/cloud/status"),
    refetchInterval: 10_000,
    retry: 1,
  });

  const logout = useMutation({
    mutationFn: () => authApi.logout(session?.refresh_token).catch(() => undefined),
    onSettled: () => {
      clearSession();
      // Hard reload to ensure no in-flight queries hold stale auth state.
      window.location.assign("/");
    },
  });

  const engine = engineHealth(health);
  const healthVariant =
    engine.verdict === "unreachable"
      ? "destructive"
      : engine.verdict === "ok"
        ? "success"
        : "warning";
  const healthLabel =
    engine.verdict === "unreachable"
      ? "engine unreachable"
      : engine.verdict === "unread"
        ? "starting\u2026"
        : engine.verdict === "ok"
          ? `online \u2022 ${engine.read.version}`
          : `degraded \u2022 ${issueCodes(engine.read.issues)}`;
  // Hover text for a degraded pill: every issue, with its detail when the
  // engine sent one.
  const healthTitle =
    engine.verdict === "degraded"
      ? engine.read.issues
          .map((i) => (i.detail ? `${i.code}: ${i.detail}` : i.code))
          .join("\n")
      : undefined;

  // Cloud-tunnel pill — three observable states (plus one for
  // transient errors and one for "not answered yet"), mapped to the
  // same Badge variants used by the health pill so the two read as a
  // unit. An unanswered query is not "not enrolled".
  const cloudVariant: "secondary" | "success" | "warning" = cloud.isError
    ? "secondary"
    : !cloud.data?.enrolled
      ? "secondary"
      : cloud.data.connected
        ? "success"
        : "warning";
  const cloudLabel = cloud.isError
    ? "cloud: ?"
    : !cloud.isSuccess
      ? "cloud: \u2026"
      : !cloud.data?.enrolled
        ? "cloud: not enrolled"
        : cloud.data.connected
          ? "cloud: connected"
          : "cloud: reconnecting\u2026";

  return (
    <header className="flex h-12 shrink-0 items-center justify-between border-b border-border bg-card px-4">
      <div className="flex items-center gap-3">
        <div className="flex h-7 w-7 items-center justify-center rounded bg-primary/20 text-primary">
          <ShieldCheck className="h-4 w-4" />
        </div>
        <div className="text-sm font-semibold tracking-tight">Nexus Edge AI</div>
        <Badge variant={healthVariant} className="ml-2" title={healthTitle}>
          {healthLabel}
        </Badge>
        <Badge variant={cloudVariant} className="ml-2">
          {cloudLabel}
        </Badge>
      </div>

      <div className="relative flex items-center gap-2">
        <Button
          variant="ghost"
          size="sm"
          className="gap-2"
          onClick={() => setMenuOpen((v) => !v)}
        >
          <User className="h-4 w-4" />
          <span className="text-sm">{session?.user.username ?? "guest"}</span>
          <span
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase",
              session?.user.role === "admin"
                ? "bg-primary/20 text-primary"
                : "bg-secondary text-muted-foreground",
            )}
          >
            {session?.user.role ?? "?"}
          </span>
        </Button>

        {menuOpen ? (
          <div
            className="absolute right-0 top-full z-50 mt-1 w-48 rounded-md border border-border bg-popover p-1 shadow-md"
            onMouseLeave={() => setMenuOpen(false)}
          >
            <button
              className="flex w-full items-center gap-2 rounded-sm px-2 py-1.5 text-left text-sm hover:bg-secondary"
              onClick={() => logout.mutate()}
              disabled={logout.isPending}
            >
              <LogOut className="h-4 w-4" />
              Sign out
            </button>
          </div>
        ) : null}
      </div>
    </header>
  );
}
