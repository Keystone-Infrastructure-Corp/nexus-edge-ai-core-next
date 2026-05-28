// First-boot setup wizard (M-Install Checkpoint 3c).
//
// Multi-step flow shown only when `engine_runtime_settings.setup_complete`
// is unset. Router redirects here after login until the operator clicks
// Finish (which POSTs /v1/setup/complete and flips the latch).
//
// Steps:
//   1. Welcome       — show hostname/version, set expectations.
//   2. Password      — MANDATORY when the bootstrap OTP is still in use
//                      (session_force_password_reset === true). Engine
//                      clears the on-disk bootstrap sentinel file on
//                      successful change-password.
//   3. Cameras       — show current count, offer "Add cameras" deep-link
//                      to /cameras (camera CRUD lives there in full).
//                      Skippable.
//   4. Rules         — analogous to cameras; deep-link to /rules. Skippable.
//   5. Finish        — POST /v1/setup/complete, redirect to /dashboard.
//
// Operator-only: backend require_role::AdminContext gates the complete
// endpoint, so non-admins seeing this route can read status but cannot
// finish. In practice the only account on a fresh install IS the
// bootstrap admin, so this is more defense-in-depth than common case.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
  CheckCircle2,
  ChevronRight,
  KeyRound,
  Loader2,
  ShieldCheck,
  Video,
  Workflow,
} from "lucide-react";
import { useState } from "react";

import { authApi } from "@/api/auth";
import { ApiError } from "@/api/client";
import { setupApi } from "@/api/setup";
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

type Step = "welcome" | "password" | "cameras" | "rules" | "finish";

const STEP_LABELS: Record<Step, string> = {
  welcome: "Welcome",
  password: "Password",
  cameras: "Cameras",
  rules: "Rules",
  finish: "Finish",
};

export function SetupPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { session } = useAuth();

  const status = useQuery({
    queryKey: ["setup-status"],
    queryFn: () => setupApi.status(),
    // Refetch when the user comes back from /cameras or /rules in another
    // tab — the wizard should reflect newly-added rows live.
    refetchOnWindowFocus: true,
    staleTime: 5_000,
  });

  // The bootstrap OTP forces a password change. We trust the server's
  // session_force_password_reset flag rather than the local session
  // mirror, because the operator may have created a fresh user from a
  // different browser session.
  const mustChangePassword =
    status.data?.session_force_password_reset ?? Boolean(session?.user.force_password_reset);

  // Only show the password step when the operator is on the
  // installer-minted bootstrap OTP. Operators who chose their
  // own password via the UI first-run-setup flow already picked
  // a permanent one — asking them to "change" it immediately
  // after creation is just busywork.
  const stepOrder: Step[] = mustChangePassword
    ? ["welcome", "password", "cameras", "rules", "finish"]
    : ["welcome", "cameras", "rules", "finish"];

  const [step, setStep] = useState<Step>("welcome");

  const goNext = () => {
    const i = stepOrder.indexOf(step);
    if (i >= 0 && i < stepOrder.length - 1) {
      const next = stepOrder[i + 1];
      if (next) setStep(next);
    }
  };

  const goBack = () => {
    const i = stepOrder.indexOf(step);
    if (i > 0) {
      const prev = stepOrder[i - 1];
      if (prev) setStep(prev);
    }
  };

  const complete = useMutation({
    mutationFn: () => setupApi.complete(),
    onSuccess: async () => {
      // Refresh the gating query so appRoute.beforeLoad sees the latch
      // flipped before the next navigation.
      await queryClient.invalidateQueries({ queryKey: ["setup-status"] });
      navigate({ to: "/dashboard" });
    },
  });

  return (
    <div className="min-h-screen bg-background p-4 sm:p-8">
      <div className="mx-auto max-w-2xl space-y-6">
        <StepRail current={step} order={stepOrder} mustChangePassword={mustChangePassword} />

        {status.isPending ? (
          <Card>
            <CardContent className="flex items-center justify-center py-12">
              <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
            </CardContent>
          </Card>
        ) : status.isError ? (
          <Card>
            <CardContent className="py-6 text-sm text-destructive">
              Could not load setup status. Is the engine reachable?
            </CardContent>
          </Card>
        ) : step === "welcome" ? (
          <WelcomeStep
            hostname={status.data!.hostname}
            version={status.data!.version}
            onNext={goNext}
          />
        ) : step === "password" ? (
          <PasswordStep
            required={mustChangePassword}
            onDone={() => {
              // Refresh status so the latch state is fresh, then advance.
              queryClient.invalidateQueries({ queryKey: ["setup-status"] });
              goNext();
            }}
            onBack={goBack}
          />
        ) : step === "cameras" ? (
          <CountStep
            icon={<Video className="h-5 w-5" />}
            title="Add your cameras"
            description="Cameras stream into the engine over RTSP / ONVIF. You can also add them later from the Cameras page."
            count={status.data!.cameras_count}
            countLabel="cameras configured"
            ctaLabel="Add cameras"
            ctaPath="/cameras"
            onSkip={goNext}
            onBack={goBack}
            navigate={navigate}
          />
        ) : step === "rules" ? (
          <CountStep
            icon={<Workflow className="h-5 w-5" />}
            title="Wire up rules"
            description="Rules turn detections into events (motion clips, alerts, webhooks). Skip and add them later if you want to see the dashboard first."
            count={status.data!.rules_count}
            countLabel="rules configured"
            ctaLabel="Add rules"
            ctaPath="/rules"
            onSkip={goNext}
            onBack={goBack}
            navigate={navigate}
          />
        ) : (
          <FinishStep
            cameras={status.data!.cameras_count}
            rules={status.data!.rules_count}
            onBack={goBack}
            onFinish={() => complete.mutate()}
            pending={complete.isPending}
            error={
              complete.isError
                ? complete.error instanceof ApiError
                  ? complete.error.message
                  : String(complete.error)
                : null
            }
          />
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Step rail
// ---------------------------------------------------------------------------

function StepRail({
  current,
  order,
  mustChangePassword,
}: {
  current: Step;
  order: Step[];
  mustChangePassword: boolean;
}) {
  const currentIdx = order.indexOf(current);
  return (
    <div className="flex items-center gap-2 text-primary">
      <ShieldCheck className="h-5 w-5" />
      <span className="text-sm font-semibold tracking-tight">
        Nexus Edge AI &mdash; first-boot setup
      </span>
      <div className="ml-auto flex items-center gap-1 text-xs text-muted-foreground">
        {order.map((s, i) => {
          const done = i < currentIdx;
          const isCurrent = i === currentIdx;
          const label =
            s === "password" && mustChangePassword ? `${STEP_LABELS[s]} *` : STEP_LABELS[s];
          return (
            <span key={s} className="flex items-center gap-1">
              <span
                className={
                  done
                    ? "text-foreground"
                    : isCurrent
                      ? "font-medium text-foreground"
                      : "text-muted-foreground"
                }
              >
                {label}
              </span>
              {i < order.length - 1 ? (
                <ChevronRight className="h-3 w-3 text-muted-foreground/60" />
              ) : null}
            </span>
          );
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Welcome
// ---------------------------------------------------------------------------

function WelcomeStep({
  hostname,
  version,
  onNext,
}: {
  hostname: string;
  version: string;
  onNext: () => void;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Welcome</CardTitle>
        <CardDescription>
          Let&rsquo;s get this appliance ready to use. This takes about a minute.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
          <dt className="text-muted-foreground">Hostname</dt>
          <dd className="font-mono">{hostname}</dd>
          <dt className="text-muted-foreground">Engine version</dt>
          <dd className="font-mono">{version}</dd>
        </dl>
        <p className="text-sm text-muted-foreground">
          You&rsquo;ll change the bootstrap password, optionally add a
          camera or two, optionally wire up a rule, and then land on
          the dashboard.
        </p>
        <div className="flex justify-end">
          <Button onClick={onNext}>Get started</Button>
        </div>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Password (always shown; mandatory when bootstrap OTP still in use)
// ---------------------------------------------------------------------------

function PasswordStep({
  required,
  onDone,
  onBack,
}: {
  required: boolean;
  onDone: () => void;
  onBack: () => void;
}) {
  const [oldPassword, setOldPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);

  const change = useMutation({
    mutationFn: (vars: { old: string; new_: string }) =>
      authApi.changePassword(vars.old, vars.new_),
    onSuccess: () => {
      // 204 — no body. Engine has already cleared the bootstrap sentinel
      // file. The cached access_token is still valid until TTL; advance.
      setError(null);
      onDone();
    },
    onError: (e: unknown) => {
      if (e instanceof ApiError) {
        setError(e.message);
      } else {
        setError(String(e));
      }
    },
  });

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (newPassword.length < 12) {
      setError("New password must be at least 12 characters.");
      return;
    }
    if (newPassword !== confirm) {
      setError("New passwords don't match.");
      return;
    }
    change.mutate({ old: oldPassword, new_: newPassword });
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          <KeyRound className="h-5 w-5 text-primary" />
          <CardTitle>Change your password</CardTitle>
        </div>
        <CardDescription>
          {required
            ? "You're using the one-time bootstrap password printed by the installer. Choose a permanent one before continuing."
            : "Choose a new password for the admin account, or skip if you're already using a permanent one."}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-3" onSubmit={submit}>
          <div className="space-y-1.5">
            <Label htmlFor="old">Current password</Label>
            <Input
              id="old"
              type="password"
              autoComplete="current-password"
              required
              value={oldPassword}
              onChange={(e) => setOldPassword(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="new">New password</Label>
            <Input
              id="new"
              type="password"
              autoComplete="new-password"
              required
              minLength={12}
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">
              At least 12 characters. Longer is better &mdash; consider a passphrase.
            </p>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="confirm">Confirm new password</Label>
            <Input
              id="confirm"
              type="password"
              autoComplete="new-password"
              required
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
          </div>
          {error ? (
            <div className="rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
              {error}
            </div>
          ) : null}
          <div className="flex justify-between">
            <Button type="button" variant="ghost" onClick={onBack}>
              Back
            </Button>
            <div className="flex gap-2">
              {!required ? (
                <Button type="button" variant="outline" onClick={onDone}>
                  Skip
                </Button>
              ) : null}
              <Button type="submit" disabled={change.isPending}>
                {change.isPending ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  "Change password"
                )}
              </Button>
            </div>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Cameras / Rules (shared shape — count + CTA + skip)
// ---------------------------------------------------------------------------

function CountStep({
  icon,
  title,
  description,
  count,
  countLabel,
  ctaLabel,
  ctaPath,
  onSkip,
  onBack,
  navigate,
}: {
  icon: React.ReactNode;
  title: string;
  description: string;
  count: number;
  countLabel: string;
  ctaLabel: string;
  ctaPath: "/cameras" | "/rules";
  onSkip: () => void;
  onBack: () => void;
  navigate: ReturnType<typeof useNavigate>;
}) {
  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          {icon}
          <CardTitle>{title}</CardTitle>
        </div>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="rounded-md border bg-muted/30 px-3 py-2 text-sm">
          <span className="font-semibold tabular-nums">{count}</span>{" "}
          <span className="text-muted-foreground">{countLabel}</span>
        </div>
        <div className="flex justify-between">
          <Button type="button" variant="ghost" onClick={onBack}>
            Back
          </Button>
          <div className="flex gap-2">
            <Button type="button" variant="outline" onClick={onSkip}>
              Skip for now
            </Button>
            <Button type="button" onClick={() => navigate({ to: ctaPath })}>
              {ctaLabel}
            </Button>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Finish
// ---------------------------------------------------------------------------

function FinishStep({
  cameras,
  rules,
  onBack,
  onFinish,
  pending,
  error,
}: {
  cameras: number;
  rules: number;
  onBack: () => void;
  onFinish: () => void;
  pending: boolean;
  error: string | null;
}) {
  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          <CheckCircle2 className="h-5 w-5 text-primary" />
          <CardTitle>You&rsquo;re all set</CardTitle>
        </div>
        <CardDescription>
          Click Finish to mark setup complete and head to the dashboard.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
          <dt className="text-muted-foreground">Cameras</dt>
          <dd className="font-mono">{cameras}</dd>
          <dt className="text-muted-foreground">Rules</dt>
          <dd className="font-mono">{rules}</dd>
        </dl>
        {cameras === 0 || rules === 0 ? (
          <p className="text-xs text-muted-foreground">
            You can add{" "}
            {cameras === 0 && rules === 0
              ? "cameras and rules"
              : cameras === 0
                ? "cameras"
                : "rules"}{" "}
            at any time from the sidebar.
          </p>
        ) : null}
        {error ? (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {error}
          </div>
        ) : null}
        <div className="flex justify-between">
          <Button type="button" variant="ghost" onClick={onBack} disabled={pending}>
            Back
          </Button>
          <Button type="button" onClick={onFinish} disabled={pending}>
            {pending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Finish setup"}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
