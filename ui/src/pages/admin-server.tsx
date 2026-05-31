// Admin Server Settings page.
//
// Live editors for the API bind address, storage watermark
// thresholds (low / panic), and the default inference model
// (kind / preset / input dims / score threshold / pack_path).
// All three are restart-based: the engine persists the
// operator's choice to `engine_runtime_settings` via the
// admin endpoints, then re-reads the row at next boot and
// applies it instead of the on-disk `nexus.toml` value.
// Recorder kind and UI root remain on-disk-only. The
// "Restart engine" card at the bottom triggers a graceful
// `execv()` self-restart so the operator can apply all the
// pending changes without shelling into the host.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Brain,
  CheckCircle2,
  Cloud,
  CloudOff,
  Database,
  HardDrive,
  Network,
  Power,
  Save,
  Settings,
  Tag,
} from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";

import {
  getCloudEnrollment,
  getInferenceModel,
  getServerBind,
  getServerIdentity,
  getWatermarks,
  postCloudEnroll,
  putInferenceModel,
  putServerBind,
  putServerIdentity,
  putWatermarks,
  restartEngine,
} from "@/api/admin";
import type { InferenceModelPatch, PostCloudEnrollReq } from "@/api/admin";
import type { UiBindUpdate } from "@/api/admin";
import { authApi } from "@/api/auth";
import { getStorage } from "@/api/storage";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { formatBytes, formatPct } from "@/lib/format";
import {
  defaultSizeForKind,
  describeSize,
  sizesForKind,
} from "@/lib/model-sizes";

export function AdminServerPage() {
  const qc = useQueryClient();

  const authQuery = useQuery({
    queryKey: ["auth", "info"],
    queryFn: () => authApi.info(),
  });

  const storageQuery = useQuery({
    queryKey: ["storage"],
    queryFn: () => getStorage(),
  });

  const bindQuery = useQuery({
    queryKey: ["admin", "server", "bind"],
    queryFn: () => getServerBind(),
  });

  // Local form state for the bind editor. Seeded once from the
  // server response (the persisted-pending value if it exists,
  // otherwise the active bind). Operators can re-edit after a
  // save without losing the input cursor.
  const [bindDraft, setBindDraft] = useState<string>("");
  useEffect(() => {
    if (bindQuery.data && bindDraft === "") {
      setBindDraft(bindQuery.data.pending ?? bindQuery.data.current);
    }
  }, [bindQuery.data, bindDraft]);

  // UI alias listener — three-way action selector + addr draft
  // for the `set` case. "noop" omits `ui_bind` from the PUT body
  // so the persisted row is left alone (the common case where
  // the operator only wants to touch the primary listener).
  type UiBindAction = "noop" | "set" | "clear" | "reset";
  const [uiBindAction, setUiBindAction] = useState<UiBindAction>("noop");
  const [uiBindDraft, setUiBindDraft] = useState<string>("");
  useEffect(() => {
    // Seed the draft once the query resolves. Prefer a pending
    // `set` payload over the active `ui_current` over a blank
    // suggestion so the input never starts empty unless the
    // operator has truly never configured the alias.
    if (bindQuery.data && uiBindDraft === "") {
      const seed =
        (bindQuery.data.ui_pending?.action === "set"
          ? bindQuery.data.ui_pending.addr
          : null)
        ?? bindQuery.data.ui_current
        ?? "0.0.0.0:80";
      setUiBindDraft(seed);
    }
  }, [bindQuery.data, uiBindDraft]);

  const bindMutation = useMutation({
    mutationFn: (input: { addr: string; ui_bind?: UiBindUpdate }) =>
      putServerBind(input.addr, input.ui_bind),
    onSuccess: (res) => {
      const parts = [`Bind address saved: ${res.pending}`];
      if (res.ui_pending?.action === "set") {
        parts.push(`UI alias → ${res.ui_pending.addr}`);
      } else if (res.ui_pending?.action === "clear") {
        parts.push("UI alias disabled");
      }
      toast.success(`${parts.join(" · ")}. Restart engine to apply.`);
      // Reset the action selector so a subsequent submit doesn't
      // accidentally re-apply the same UI bind decision.
      setUiBindAction("noop");
      qc.invalidateQueries({ queryKey: ["admin", "server", "bind"] });
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to save bind address: ${msg}`);
    },
  });

  const pendingDiffers =
    bindQuery.data?.pending !== null
    && bindQuery.data?.pending !== undefined
    && bindQuery.data.pending !== bindQuery.data.current;

  // True when there is a persisted ui_bind override that hasn't
  // taken effect (either an explicit "off" or a `set` whose addr
  // doesn't match the currently-bound alias). Drives the pending
  // banner + the Restart engine card aggregation.
  const uiPendingDiffers = (() => {
    const p = bindQuery.data?.ui_pending;
    if (!p) return false;
    if (p.action === "clear") return bindQuery.data?.ui_current !== null;
    // p.action === "set"
    return p.addr !== bindQuery.data?.ui_current;
  })();

  // Identity (engine display name) ---------------------------------------
  const identityQuery = useQuery({
    queryKey: ["admin", "server", "identity"],
    queryFn: () => getServerIdentity(),
  });
  const [identityDraft, setIdentityDraft] = useState<string>("");
  useEffect(() => {
    if (identityQuery.data && identityDraft === "") {
      setIdentityDraft(identityQuery.data.display_name ?? "");
    }
  }, [identityQuery.data, identityDraft]);
  const identityMutation = useMutation({
    mutationFn: (display_name: string | null) =>
      putServerIdentity(display_name),
    onSuccess: (res) => {
      toast.success(
        res.display_name === null
          ? "Display name cleared."
          : `Display name set to \u201c${res.display_name}\u201d.`,
      );
      qc.invalidateQueries({ queryKey: ["admin", "server", "identity"] });
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to save display name: ${msg}`);
    },
  });

  // Watermarks --------------------------------------------------------------
  const wmQuery = useQuery({
    queryKey: ["admin", "server", "watermarks"],
    queryFn: () => getWatermarks(),
  });

  const [lowDraft, setLowDraft] = useState<string>("");
  const [panicDraft, setPanicDraft] = useState<string>("");
  useEffect(() => {
    if (wmQuery.data && lowDraft === "" && panicDraft === "") {
      setLowDraft(String(wmQuery.data.pending_low_pct ?? wmQuery.data.low_pct));
      setPanicDraft(
        String(wmQuery.data.pending_panic_pct ?? wmQuery.data.panic_pct),
      );
    }
  }, [wmQuery.data, lowDraft, panicDraft]);

  const wmMutation = useMutation({
    mutationFn: ({ low_pct, panic_pct }: { low_pct: number; panic_pct: number }) =>
      putWatermarks(low_pct, panic_pct),
    onSuccess: (res) => {
      toast.success(
        `Watermarks saved: low=${res.pending_low_pct}% panic=${res.pending_panic_pct}%. Restart engine to apply.`,
      );
      qc.invalidateQueries({ queryKey: ["admin", "server", "watermarks"] });
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to save watermarks: ${msg}`);
    },
  });

  const wmPendingDiffers =
    (wmQuery.data?.pending_low_pct !== null
      && wmQuery.data?.pending_low_pct !== undefined)
    || (wmQuery.data?.pending_panic_pct !== null
      && wmQuery.data?.pending_panic_pct !== undefined);

  // Default inference model ------------------------------------------------
  const modelQuery = useQuery({
    queryKey: ["admin", "server", "inference"],
    queryFn: () => getInferenceModel(),
  });

  type ModelDraft = {
    kind: string;
    preset: string;
    score_threshold: string;
    pack_path: string;
  };
  const emptyModelDraft: ModelDraft = {
    kind: "",
    preset: "",
    score_threshold: "",
    pack_path: "",
  };
  const [modelDraft, setModelDraft] = useState<ModelDraft>(emptyModelDraft);
  const [modelSeeded, setModelSeeded] = useState(false);
  useEffect(() => {
    if (modelQuery.data && !modelSeeded) {
      // Seed from pending if present, otherwise current. Width/height
      // are derived from preset (see the per-kind size table in
      // `lib/model-sizes.ts`) — the engine treats `(input_width,
      // input_height) = (Number(preset), Number(preset))` for every
      // shipped square model.
      const src = modelQuery.data.pending ?? modelQuery.data.current;
      setModelDraft({
        kind: src.kind,
        preset: src.preset,
        score_threshold: String(src.score_threshold),
        pack_path: src.pack_path ?? "",
      });
      setModelSeeded(true);
    }
  }, [modelQuery.data, modelSeeded]);

  // Snap preset when kind changes so we never persist a (kind, size)
  // combo the engine doesn't ship a per-size ONNX for. If the current
  // preset is still valid for the new kind, keep it; else pick the
  // first option; else (no-size kind like mock / classifier_ensemble)
  // clear the preset entirely and the engine applies its kind default.
  const switchModelKind = (nextKind: string) => {
    if (nextKind === modelDraft.kind) return;
    const opts = sizesForKind(nextKind);
    let nextPreset = modelDraft.preset;
    if (opts.length === 0) {
      nextPreset = "";
    } else {
      const cur = Number(modelDraft.preset);
      const keep = Number.isFinite(cur) && opts.includes(cur)
        ? cur
        : defaultSizeForKind(nextKind);
      nextPreset = keep === undefined ? "" : String(keep);
    }
    setModelDraft({ ...modelDraft, kind: nextKind, preset: nextPreset });
  };

  const modelMutation = useMutation({
    mutationFn: (patch: InferenceModelPatch) => putInferenceModel(patch),
    onSuccess: (res) => {
      toast.success(
        `Model saved: kind=${res.pending.kind} preset=${res.pending.preset}. Restart engine to apply.`,
      );
      qc.invalidateQueries({ queryKey: ["admin", "server", "inference"] });
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to save model: ${msg}`);
    },
  });

  const modelPendingDiffers =
    modelQuery.data?.pending !== null
    && modelQuery.data?.pending !== undefined;

  // Cloud enrollment -------------------------------------------------------
  //
  // Single round-trip surface: the GET query reports the enrolled /
  // unenrolled state, and the POST mutation runs the same
  // `cloud_enroll::perform_enrollment` flow as the
  // `nexus-engine enroll` CLI subcommand. Restart-required: the WSS
  // tunnel is spawned exactly once at boot from the persisted row.
  // We track an in-session `cloudJustEnrolled` flag so the operator
  // sees the change in the restart summary even though the server-side
  // status query reports the same `enrolled: true` it would have shown
  // before this mutation.
  const cloudQuery = useQuery({
    queryKey: ["admin", "cloud", "enrollment"],
    queryFn: () => getCloudEnrollment(),
  });

  const [cloudJustEnrolled, setCloudJustEnrolled] = useState(false);

  type CloudDraft = {
    code: string;
    cloud_host: string;
    label: string;
    keep_history: boolean;
    history_days: string;
  };
  const emptyCloudDraft: CloudDraft = {
    code: "",
    cloud_host: "",
    label: "",
    keep_history: false,
    history_days: "30",
  };
  const [cloudDraft, setCloudDraft] = useState<CloudDraft>(emptyCloudDraft);
  const [cloudShowAdvanced, setCloudShowAdvanced] = useState(false);

  const cloudMutation = useMutation({
    mutationFn: (req: PostCloudEnrollReq) => postCloudEnroll(req),
    onSuccess: (res) => {
      toast.success(
        `Connected to cloud as ${res.core_id}. Restart engine to activate the tunnel.`,
      );
      setCloudJustEnrolled(true);
      setCloudDraft(emptyCloudDraft);
      qc.invalidateQueries({ queryKey: ["admin", "cloud", "enrollment"] });
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to enroll: ${msg}`);
    },
  });

  // Engine restart ---------------------------------------------------------
  const restartMutation = useMutation({
    mutationFn: () => restartEngine(),
    onSuccess: (res) => {
      toast.success(
        `Engine restart scheduled in ${res.delay_ms}ms — the page will reconnect shortly.`,
      );
      // Schedule a soft page reload a few seconds after the
      // engine's exec() so the SPA picks up any new auth /
      // bind values without the operator hitting F5. Bind
      // changes that move the engine to a different port make
      // the reload a 404, which is acceptable — the operator
      // knows they changed it.
      const totalDelay = res.delay_ms + 4_000;
      window.setTimeout(() => {
        window.location.reload();
      }, totalDelay);
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Failed to schedule restart: ${msg}`);
    },
  });

  return (
    <div className="space-y-6">
      <header>
        <h1 className="text-2xl font-semibold">Server settings</h1>
        <p className="text-sm text-muted-foreground">
          Current runtime configuration. The bind address and storage
          watermark thresholds take effect on the next engine restart.
        </p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Tag className="h-4 w-4 text-muted-foreground" />
            Identity
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4 text-sm">
          <p className="text-xs text-muted-foreground">
            Friendly name surfaced in the cloud console&apos;s cores
            list (and in this engine&apos;s local UI header). Takes
            effect on the next cloud heartbeat — no restart required.
          </p>
          <div className="grid gap-2">
            <Label htmlFor="display-name">Display name</Label>
            <Input
              id="display-name"
              data-testid="identity-display-name"
              value={identityDraft}
              onChange={(e) => setIdentityDraft(e.target.value)}
              placeholder="e.g. Front Office Cam Tower"
              maxLength={80}
              disabled={identityQuery.isLoading || identityMutation.isPending}
            />
          </div>
          <div className="flex gap-2">
            <Button
              type="button"
              size="sm"
              data-testid="identity-save"
              disabled={
                identityQuery.isLoading
                || identityMutation.isPending
                || identityDraft.trim() === (identityQuery.data?.display_name ?? "")
              }
              onClick={() => {
                const next = identityDraft.trim();
                identityMutation.mutate(next === "" ? null : next);
              }}
            >
              <Save className="mr-1 h-3.5 w-3.5" />
              Save
            </Button>
            {identityQuery.data?.display_name ? (
              <Button
                type="button"
                size="sm"
                variant="outline"
                data-testid="identity-clear"
                disabled={identityMutation.isPending}
                onClick={() => {
                  setIdentityDraft("");
                  identityMutation.mutate(null);
                }}
              >
                Clear
              </Button>
            ) : null}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Network className="h-4 w-4 text-muted-foreground" />
            Network
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4 text-sm">
          <Row
            k="Auth mode"
            v={
              authQuery.isLoading ? (
                <Skeleton className="h-4 w-24" />
              ) : (
                <Badge variant="outline">{authQuery.data?.mode}</Badge>
              )
            }
          />
          <Row
            k="Console URL"
            v={<code className="font-mono">{window.location.origin}</code>}
          />
          <Row
            k="Active bind"
            v={
              bindQuery.isLoading ? (
                <Skeleton className="h-4 w-32" />
              ) : (
                <code className="font-mono">{bindQuery.data?.current}</code>
              )
            }
          />
          {pendingDiffers ? (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="bind-pending"
            >
              <p className="font-medium text-warning">Pending restart</p>
              <p className="mt-0.5 text-muted-foreground">
                Engine will listen on{" "}
                <code className="font-mono">{bindQuery.data?.pending}</code>{" "}
                after the next restart.
              </p>
            </div>
          ) : null}

          <Row
            k="Admin/UI alias"
            v={
              bindQuery.isLoading ? (
                <Skeleton className="h-4 w-32" />
              ) : bindQuery.data?.ui_current ? (
                <code className="font-mono">{bindQuery.data.ui_current}</code>
              ) : (
                <span className="text-muted-foreground">disabled</span>
              )
            }
          />
          {uiPendingDiffers ? (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="ui-bind-pending"
            >
              <p className="font-medium text-warning">Pending restart</p>
              <p className="mt-0.5 text-muted-foreground">
                {bindQuery.data?.ui_pending?.action === "set" ? (
                  <>
                    Second listener will bind to{" "}
                    <code className="font-mono">
                      {bindQuery.data.ui_pending.addr}
                    </code>{" "}
                    after the next restart.
                  </>
                ) : (
                  <>Second listener will be disabled after the next restart.</>
                )}
              </p>
            </div>
          ) : null}

          <form
            className="flex flex-col gap-4"
            onSubmit={(e) => {
              e.preventDefault();
              const trimmed = bindDraft.trim();
              if (!trimmed) {
                toast.error("Bind address must be host:port");
                return;
              }
              let ui_bind: UiBindUpdate | undefined;
              switch (uiBindAction) {
                case "noop":
                  ui_bind = undefined;
                  break;
                case "set": {
                  const ui = uiBindDraft.trim();
                  if (!ui) {
                    toast.error("UI alias address must be host:port");
                    return;
                  }
                  if (ui === trimmed) {
                    toast.error(
                      "UI alias address must differ from the primary bind",
                    );
                    return;
                  }
                  ui_bind = { action: "set", addr: ui };
                  break;
                }
                case "clear":
                  ui_bind = { action: "clear" };
                  break;
                case "reset":
                  ui_bind = { action: "reset" };
                  break;
              }
              bindMutation.mutate({ addr: trimmed, ui_bind });
            }}
          >
            <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
              <div className="flex flex-1 flex-col gap-1">
                <Label htmlFor="bind-addr">Primary bind address</Label>
                <Input
                  id="bind-addr"
                  placeholder="0.0.0.0:8089"
                  value={bindDraft}
                  onChange={(e) => setBindDraft(e.target.value)}
                  disabled={bindQuery.isLoading || bindMutation.isPending}
                  data-testid="bind-addr-input"
                />
                <p className="text-xs text-muted-foreground">
                  The engine probe-binds to the new address before persisting
                  so you find out about port conflicts immediately, not on
                  next restart.
                </p>
              </div>
            </div>

            <fieldset className="flex flex-col gap-2 rounded-md border p-3">
              <legend className="px-1 text-xs font-medium text-muted-foreground">
                Admin/UI alias listener (optional second bind)
              </legend>
              <p className="text-xs text-muted-foreground">
                Bind a second listener so the admin console is reachable on
                a different interface or port (e.g. <code>0.0.0.0:80</code>{" "}
                on a management LAN while the primary engine port stays on
                the camera LAN). Both listeners serve the same router; the
                only practical difference is where they're reachable from.
              </p>
              <UiBindRadio
                value={uiBindAction}
                onChange={setUiBindAction}
                disabled={bindQuery.isLoading || bindMutation.isPending}
              />
              {uiBindAction === "set" ? (
                <div className="flex flex-col gap-1 pl-6">
                  <Label htmlFor="ui-bind-addr">UI alias address</Label>
                  <Input
                    id="ui-bind-addr"
                    placeholder="0.0.0.0:80"
                    value={uiBindDraft}
                    onChange={(e) => setUiBindDraft(e.target.value)}
                    disabled={bindMutation.isPending}
                    data-testid="ui-bind-addr-input"
                  />
                  <p className="text-xs text-muted-foreground">
                    Binding ports &lt;1024 on bare-metal needs{" "}
                    <code>CAP_NET_BIND_SERVICE</code> on the systemd unit.
                  </p>
                </div>
              ) : null}
            </fieldset>

            <div className="flex justify-end">
              <Button
                type="submit"
                disabled={bindMutation.isPending}
                data-testid="bind-addr-save"
              >
                <Save className="mr-2 h-4 w-4" />
                {bindMutation.isPending ? "Saving…" : "Save"}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <HardDrive className="h-4 w-4 text-muted-foreground" />
            Storage watermarks
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          {storageQuery.isLoading ? (
            <Skeleton className="h-20 w-full" />
          ) : storageQuery.data ? (
            <>
              <Row
                k="Clips directory"
                v={
                  <code className="font-mono">
                    {storageQuery.data.hot.clips_dir}
                  </code>
                }
              />
              <Row
                k="Low watermark"
                v={formatPct(storageQuery.data.hot.watermark_low_pct, 0)}
              />
              <Row
                k="Panic watermark"
                v={formatPct(storageQuery.data.hot.watermark_panic_pct, 0)}
              />
              <Row
                k="Filesystem"
                v={
                  storageQuery.data.hot.fs_total_bytes !== null &&
                  storageQuery.data.hot.fs_used_bytes !== null ? (
                    <>
                      {formatBytes(storageQuery.data.hot.fs_used_bytes)} of{" "}
                      {formatBytes(storageQuery.data.hot.fs_total_bytes)} used
                      {storageQuery.data.hot.free_pct !== null ? (
                        <>
                          {" "}
                          (
                          <span className="text-muted-foreground">
                            {formatPct(
                              100 - storageQuery.data.hot.free_pct,
                              1,
                            )}{" "}
                            full
                          </span>
                          )
                        </>
                      ) : null}
                    </>
                  ) : (
                    <span className="text-muted-foreground">unknown</span>
                  )
                }
              />
              <Row
                k="Current state"
                v={<WmBadge state={storageQuery.data.hot.watermark_state} />}
              />
            </>
          ) : null}

          {wmPendingDiffers ? (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="watermark-pending"
            >
              <p className="font-medium text-warning">Pending restart</p>
              <p className="mt-0.5 text-muted-foreground">
                Engine will apply low={" "}
                <code className="font-mono">
                  {wmQuery.data?.pending_low_pct ?? wmQuery.data?.low_pct}%
                </code>{" "}
                / panic={" "}
                <code className="font-mono">
                  {wmQuery.data?.pending_panic_pct ?? wmQuery.data?.panic_pct}%
                </code>{" "}
                after the next restart.
              </p>
            </div>
          ) : null}

          <form
            className="flex flex-col gap-3 pt-2 sm:flex-row sm:items-end"
            onSubmit={(e) => {
              e.preventDefault();
              const low = Number(lowDraft);
              const panic = Number(panicDraft);
              if (
                !Number.isFinite(low) || !Number.isFinite(panic)
                || low < 0 || low > 100 || panic < 0 || panic > 100
              ) {
                toast.error("Watermarks must be 0..=100");
                return;
              }
              if (panic <= low) {
                toast.error("panic_pct must be strictly greater than low_pct");
                return;
              }
              wmMutation.mutate({ low_pct: low, panic_pct: panic });
            }}
          >
            <div className="flex flex-1 flex-col gap-1">
              <Label htmlFor="wm-low">Low watermark (%)</Label>
              <Input
                id="wm-low"
                type="number"
                min={0}
                max={100}
                value={lowDraft}
                onChange={(e) => setLowDraft(e.target.value)}
                disabled={wmQuery.isLoading || wmMutation.isPending}
                data-testid="wm-low-input"
              />
            </div>
            <div className="flex flex-1 flex-col gap-1">
              <Label htmlFor="wm-panic">Panic watermark (%)</Label>
              <Input
                id="wm-panic"
                type="number"
                min={0}
                max={100}
                value={panicDraft}
                onChange={(e) => setPanicDraft(e.target.value)}
                disabled={wmQuery.isLoading || wmMutation.isPending}
                data-testid="wm-panic-input"
              />
            </div>
            <Button
              type="submit"
              disabled={wmMutation.isPending}
              data-testid="wm-save"
            >
              <Save className="mr-2 h-4 w-4" />
              {wmMutation.isPending ? "Saving…" : "Save"}
            </Button>
          </form>
          <p className="text-xs text-muted-foreground">
            Panic must be strictly greater than low. Persisted to
            <code className="font-mono"> engine_runtime_settings </code>
            and applied on next engine boot.
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Brain className="h-4 w-4 text-muted-foreground" />
            Default inference model
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          <Row
            k="Active kind"
            v={
              modelQuery.isLoading ? (
                <Skeleton className="h-4 w-24" />
              ) : (
                <code className="font-mono">
                  {modelQuery.data?.current.kind}
                </code>
              )
            }
          />
          <Row
            k="Active preset"
            v={
              modelQuery.isLoading ? (
                <Skeleton className="h-4 w-16" />
              ) : (
                <code className="font-mono">
                  {modelQuery.data?.current.preset}
                </code>
              )
            }
          />
          <Row
            k="Active input size"
            v={
              modelQuery.isLoading ? (
                <Skeleton className="h-4 w-20" />
              ) : (
                <code className="font-mono">
                  {modelQuery.data?.current.input_width}×
                  {modelQuery.data?.current.input_height}
                </code>
              )
            }
          />
          <Row
            k="Active score threshold"
            v={
              modelQuery.isLoading ? (
                <Skeleton className="h-4 w-12" />
              ) : (
                <code className="font-mono">
                  {modelQuery.data?.current.score_threshold.toFixed(2)}
                </code>
              )
            }
          />
          {modelQuery.data?.current.pack_path ? (
            <Row
              k="Active pack path"
              v={
                <code className="font-mono text-xs">
                  {modelQuery.data.current.pack_path}
                </code>
              }
            />
          ) : null}

          {modelPendingDiffers && modelQuery.data?.pending ? (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="model-pending"
            >
              <p className="font-medium text-warning">Pending restart</p>
              <p className="mt-0.5 text-muted-foreground">
                Engine will load{" "}
                <code className="font-mono">
                  {modelQuery.data.pending.kind}
                </code>{" "}
                @{" "}
                <code className="font-mono">
                  {modelQuery.data.pending.input_width}×
                  {modelQuery.data.pending.input_height}
                </code>{" "}
                (score≥
                <code className="font-mono">
                  {modelQuery.data.pending.score_threshold.toFixed(2)}
                </code>
                ) after the next restart.
              </p>
            </div>
          ) : null}

          <form
            className="grid grid-cols-1 gap-3 pt-2 sm:grid-cols-2"
            onSubmit={(e) => {
              e.preventDefault();
              const thr = Number(modelDraft.score_threshold);
              if (!modelDraft.kind.trim()) {
                toast.error("Kind is required");
                return;
              }
              const presetOpts = sizesForKind(modelDraft.kind);
              // For multi-size kinds the operator MUST pick a preset that the
              // pack actually ships at — the engine resolver hard fails on
              // missing per-size files (silent-CPU-fallback trap on Intel NPU).
              if (presetOpts.length > 0) {
                const p = Number(modelDraft.preset);
                if (!presetOpts.includes(p)) {
                  toast.error(
                    `Preset must be one of ${presetOpts.join(" / ")} for kind ${modelDraft.kind}`,
                  );
                  return;
                }
              }
              if (!Number.isFinite(thr) || thr < 0 || thr > 1) {
                toast.error("Score threshold must be in 0.0..=1.0");
                return;
              }
              // Width/height are derived from preset — every shipped model is
              // square (yolo26n_640/960/1280, yolo_world_v2_s_640/960, etc.).
              // For kinds that ship a single fixed-size ONNX (yoloe*,
              // classifier_ensemble, mock) we send the current engine
              // dimensions back unchanged so the PUT contract stays satisfied;
              // changing them on the wire is meaningless because the resolver
              // ignores the request.
              const current = modelQuery.data?.current;
              const presetNum = modelDraft.preset.trim() === ""
                ? null
                : Number(modelDraft.preset);
              const w = presetNum ?? current?.input_width ?? 0;
              const h = presetNum ?? current?.input_height ?? 0;
              modelMutation.mutate({
                kind: modelDraft.kind.trim(),
                preset: modelDraft.preset.trim() === ""
                  ? (current?.preset ?? "")
                  : modelDraft.preset.trim(),
                input_width: w,
                input_height: h,
                score_threshold: thr,
                // Empty string is treated as "clear pack_path"
                // by the engine; omit when the operator hasn't
                // edited the field (i.e. matches current).
                pack_path: modelDraft.pack_path.trim() === ""
                  ? ""
                  : modelDraft.pack_path.trim(),
              });
            }}
          >
            <div className="flex flex-col gap-1">
              <Label htmlFor="model-kind">Kind</Label>
              <select
                id="model-kind"
                className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                value={modelDraft.kind}
                onChange={(e) => switchModelKind(e.target.value)}
                disabled={modelQuery.isLoading || modelMutation.isPending}
                data-testid="model-kind-input"
              >
                {modelQuery.data?.available_kinds.map((k) => (
                  <option key={k} value={k}>
                    {k}
                  </option>
                ))}
                {/* If the active kind isn't in the static
                    known-kinds list (shouldn't happen, but
                    survive forward-compat), keep it
                    selectable so we don't silently rewrite. */}
                {modelDraft.kind
                  && !modelQuery.data?.available_kinds.includes(
                    modelDraft.kind,
                  ) ? (
                  <option value={modelDraft.kind}>{modelDraft.kind}</option>
                ) : null}
              </select>
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor="model-preset">
                Input size {sizesForKind(modelDraft.kind).length === 0 ? "(fixed)" : "(W × H)"}
              </Label>
              {sizesForKind(modelDraft.kind).length > 1 ? (
                <select
                  id="model-preset"
                  className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                  value={modelDraft.preset}
                  onChange={(e) =>
                    setModelDraft({ ...modelDraft, preset: e.target.value })
                  }
                  disabled={modelQuery.isLoading || modelMutation.isPending}
                  data-testid="model-preset-input"
                >
                  {sizesForKind(modelDraft.kind).map((sz) => (
                    <option key={sz} value={String(sz)}>
                      {describeSize(sz)}
                    </option>
                  ))}
                  {/* Forward-compat: keep an unknown current preset selectable. */}
                  {modelDraft.preset
                    && !sizesForKind(modelDraft.kind).includes(
                      Number(modelDraft.preset),
                    ) ? (
                    <option value={modelDraft.preset}>{modelDraft.preset}</option>
                  ) : null}
                </select>
              ) : (
                <div
                  className="flex h-9 w-full items-center rounded-md border border-dashed border-input bg-muted/30 px-3 py-1 text-sm text-muted-foreground"
                  data-testid="model-preset-input"
                >
                  <code className="font-mono">
                    {modelDraft.preset
                      ? `${modelDraft.preset} × ${modelDraft.preset}`
                      : "fixed by model"}
                  </code>
                </div>
              )}
              <p className="text-xs text-muted-foreground">
                Width and height are set automatically from the picked size —
                every shipped detector ONNX is square. The engine's per-kind
                resolver hard fails on a missing per-size file (silent
                CPU-fallback trap on Intel NPU, fixed in v0.1.22), so only
                sizes the kind's pack actually ships are listed here.
              </p>
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor="model-score">Score threshold (0..=1)</Label>
              <Input
                id="model-score"
                type="number"
                min={0}
                max={1}
                step={0.01}
                value={modelDraft.score_threshold}
                onChange={(e) =>
                  setModelDraft({
                    ...modelDraft,
                    score_threshold: e.target.value,
                  })
                }
                disabled={modelQuery.isLoading || modelMutation.isPending}
                data-testid="model-score-input"
              />
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor="model-pack">Pack path (optional)</Label>
              <Input
                id="model-pack"
                type="text"
                placeholder="/models/my-pack"
                value={modelDraft.pack_path}
                onChange={(e) =>
                  setModelDraft({ ...modelDraft, pack_path: e.target.value })
                }
                disabled={modelQuery.isLoading || modelMutation.isPending}
                data-testid="model-pack-input"
              />
            </div>
            <div className="sm:col-span-2">
              <Button
                type="submit"
                disabled={modelMutation.isPending || !modelSeeded}
                data-testid="model-save"
              >
                <Save className="mr-2 h-4 w-4" />
                {modelMutation.isPending ? "Saving…" : "Save model"}
              </Button>
            </div>
          </form>
          <p className="text-xs text-muted-foreground">
            Changing the model requires an engine restart — the
            <code className="font-mono"> InferenceRouter </code>
            walks its known kinds once at boot. Use the
            <strong> Restart engine </strong>
            card below to apply.
          </p>
        </CardContent>
      </Card>

      <Card data-testid="cloud-enrollment-card">
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            {cloudQuery.data?.enrolled ? (
              <Cloud className="h-4 w-4 text-emerald-500" />
            ) : (
              <CloudOff className="h-4 w-4 text-muted-foreground" />
            )}
            Cloud connection
            {cloudQuery.isLoading ? (
              <Skeleton className="ml-2 h-5 w-20" />
            ) : cloudQuery.data?.enrolled ? (
              <Badge
                variant="outline"
                className="ml-2 border-emerald-500/40 text-emerald-500"
                data-testid="cloud-status-enrolled"
              >
                <CheckCircle2 className="mr-1 h-3 w-3" />
                Enrolled
              </Badge>
            ) : (
              <Badge
                variant="outline"
                className="ml-2 border-warning/40 text-warning"
                data-testid="cloud-status-unenrolled"
              >
                Not enrolled
              </Badge>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          {cloudQuery.data?.enrolled ? (
            <>
              <Row
                k="Core ID"
                v={
                  <code className="font-mono text-xs">
                    {cloudQuery.data.core_id}
                  </code>
                }
              />
              <Row
                k="Gateway"
                v={
                  <code className="font-mono text-xs">
                    {cloudQuery.data.gateway_url}
                  </code>
                }
              />
              {cloudQuery.data.enrolled_at ? (
                <Row
                  k="Enrolled at"
                  v={
                    <span className="text-xs text-muted-foreground">
                      {new Date(cloudQuery.data.enrolled_at).toLocaleString()}
                    </span>
                  }
                />
              ) : null}
              <p className="text-xs text-muted-foreground">
                Re-enroll below to point this core at a different cloud
                console or to refresh the mTLS certificate. The current
                enrollment row will be replaced atomically.
              </p>
            </>
          ) : (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="cloud-prompt-unenrolled"
            >
              <p className="font-medium text-warning">
                This core is not connected to a cloud console.
              </p>
              <p className="mt-0.5 text-muted-foreground">
                Enter an enrollment key from your cloud console's
                "Add Core" flow to enable remote control, central
                monitoring, and clip replication. The engine runs
                fully offline without this step.
              </p>
            </div>
          )}

          <form
            className="grid grid-cols-1 gap-3 pt-2 sm:grid-cols-2"
            onSubmit={(e) => {
              e.preventDefault();
              const code = cloudDraft.code.trim();
              const host = cloudDraft.cloud_host.trim();
              if (!code) {
                toast.error("Enrollment key is required");
                return;
              }
              if (!host) {
                toast.error("Cloud host URL is required");
                return;
              }
              if (
                !host.startsWith("https://")
                && !host.startsWith("http://127.0.0.1")
                && !host.startsWith("http://localhost")
              ) {
                toast.error(
                  "Cloud host must start with https:// (or http://localhost for dev)",
                );
                return;
              }
              const req: PostCloudEnrollReq = {
                code,
                cloud_host: host,
              };
              const label = cloudDraft.label.trim();
              if (label) req.label = label;
              if (cloudDraft.keep_history) {
                req.keep_history = true;
                const days = Number(cloudDraft.history_days);
                if (Number.isFinite(days) && days >= 1 && days <= 365) {
                  req.history_days = Math.floor(days);
                } else {
                  toast.error("History days must be in 1..=365");
                  return;
                }
              }
              cloudMutation.mutate(req);
            }}
          >
            <div className="flex flex-col gap-1">
              <Label htmlFor="cloud-code">Enrollment key</Label>
              <Input
                id="cloud-code"
                type="text"
                placeholder="XJ4K-PMQ7-9NAB"
                value={cloudDraft.code}
                onChange={(e) =>
                  setCloudDraft({ ...cloudDraft, code: e.target.value })
                }
                disabled={cloudMutation.isPending}
                data-testid="cloud-code-input"
                autoComplete="off"
                spellCheck={false}
              />
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor="cloud-host">Cloud host URL</Label>
              <Input
                id="cloud-host"
                type="url"
                placeholder="https://cloud.example"
                value={cloudDraft.cloud_host}
                onChange={(e) =>
                  setCloudDraft({
                    ...cloudDraft,
                    cloud_host: e.target.value,
                  })
                }
                disabled={cloudMutation.isPending}
                data-testid="cloud-host-input"
                autoComplete="off"
                spellCheck={false}
              />
            </div>

            <div className="sm:col-span-2">
              <button
                type="button"
                className="text-xs text-muted-foreground underline-offset-2 hover:underline"
                onClick={() => setCloudShowAdvanced((v) => !v)}
                data-testid="cloud-advanced-toggle"
              >
                {cloudShowAdvanced ? "Hide" : "Show"} advanced options
              </button>
            </div>
            {cloudShowAdvanced ? (
              <>
                <div className="flex flex-col gap-1 sm:col-span-2">
                  <Label htmlFor="cloud-label">
                    Label (optional — defaults to hostname)
                  </Label>
                  <Input
                    id="cloud-label"
                    type="text"
                    placeholder="reception-rack-01"
                    value={cloudDraft.label}
                    onChange={(e) =>
                      setCloudDraft({ ...cloudDraft, label: e.target.value })
                    }
                    disabled={cloudMutation.isPending}
                    data-testid="cloud-label-input"
                  />
                </div>
                <div className="flex items-center gap-2 sm:col-span-2">
                  <input
                    id="cloud-keep-history"
                    type="checkbox"
                    className="h-4 w-4 rounded border-input"
                    checked={cloudDraft.keep_history}
                    onChange={(e) =>
                      setCloudDraft({
                        ...cloudDraft,
                        keep_history: e.target.checked,
                      })
                    }
                    disabled={cloudMutation.isPending}
                    data-testid="cloud-keep-history-input"
                  />
                  <Label
                    htmlFor="cloud-keep-history"
                    className="text-xs font-normal text-muted-foreground"
                  >
                    Replay local clip backlog to the cloud after enrollment
                  </Label>
                </div>
                {cloudDraft.keep_history ? (
                  <div className="flex flex-col gap-1">
                    <Label htmlFor="cloud-history-days">
                      History days (1..=365)
                    </Label>
                    <Input
                      id="cloud-history-days"
                      type="number"
                      min={1}
                      max={365}
                      value={cloudDraft.history_days}
                      onChange={(e) =>
                        setCloudDraft({
                          ...cloudDraft,
                          history_days: e.target.value,
                        })
                      }
                      disabled={cloudMutation.isPending}
                      data-testid="cloud-history-days-input"
                    />
                  </div>
                ) : null}
              </>
            ) : null}

            <div className="sm:col-span-2">
              <Button
                type="submit"
                disabled={cloudMutation.isPending}
                data-testid="cloud-enroll-save"
              >
                <Cloud className="mr-2 h-4 w-4" />
                {cloudMutation.isPending
                  ? "Connecting…"
                  : cloudQuery.data?.enrolled
                    ? "Re-enroll"
                    : "Connect to cloud"}
              </Button>
            </div>
          </form>
          <p className="text-xs text-muted-foreground">
            The enrollment round-trip mints a fresh mTLS certificate
            and entitlement token. The WSS tunnel is spawned once at
            boot from the persisted enrollment, so a successful
            connection requires an engine restart to activate.
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Database className="h-4 w-4 text-muted-foreground" />
            Recorder
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          {storageQuery.isLoading ? (
            <Skeleton className="h-12 w-full" />
          ) : storageQuery.data ? (
            <Row
              k="Recorder kind"
              v={
                <code className="font-mono">
                  {storageQuery.data.hot.recorder_kind}
                </code>
              }
            />
          ) : null}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Settings className="h-4 w-4 text-muted-foreground" />
            UI root
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          <p className="text-muted-foreground">
            The SPA you're viewing was served from{" "}
            <code className="font-mono">{window.location.pathname}</code>.
            Changing the static-asset root requires{" "}
            <code className="font-mono">server.ui_root</code> in{" "}
            <code className="font-mono">nexus.toml</code> and a restart.
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Power className="h-4 w-4 text-destructive" />
            Restart engine
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          <p className="text-muted-foreground">
            Applies every pending change above (bind, watermarks,
            model, auth, OIDC). The engine
            <code className="font-mono"> execv()</code>s a fresh
            copy of itself — same PID, same argv, no separate
            supervisor needed. The page will reload automatically
            once the new image is listening.
          </p>
          {(pendingDiffers || uiPendingDiffers || wmPendingDiffers || modelPendingDiffers || cloudJustEnrolled) ? (
            <div
              className="rounded-md border border-warning/40 bg-warning/10 p-3 text-xs"
              data-testid="restart-pending-summary"
            >
              <p className="font-medium text-warning">Pending changes</p>
              <ul className="mt-1 list-disc pl-4 text-muted-foreground">
                {pendingDiffers ? <li>Bind address</li> : null}
                {uiPendingDiffers ? <li>UI alias listener</li> : null}
                {wmPendingDiffers ? <li>Storage watermarks</li> : null}
                {modelPendingDiffers ? <li>Default inference model</li> : null}
                {cloudJustEnrolled ? <li>Cloud enrollment (tunnel not active until restart)</li> : null}
              </ul>
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              No pending changes detected. Restart anyway if you
              edited <code className="font-mono">nexus.toml</code>
              {" "}on disk.
            </p>
          )}
          <Button
            type="button"
            variant="destructive"
            disabled={restartMutation.isPending}
            onClick={() => {
              if (
                window.confirm(
                  "Restart the engine now? Live video streams will drop for a few seconds while the new image takes over.",
                )
              ) {
                restartMutation.mutate();
              }
            }}
            data-testid="restart-engine"
          >
            <Power className="mr-2 h-4 w-4" />
            {restartMutation.isPending
              ? "Restarting…"
              : "Restart engine now"}
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}

function Row({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-border/30 py-1.5 last:border-b-0">
      <span className="text-muted-foreground">{k}</span>
      <div className="text-right">{v}</div>
    </div>
  );
}

/**
 * Four-way radio selector for the UI alias listener change action.
 * Plain `<input type=radio>` styled to match the surrounding
 * shadcn aesthetic since we don't (yet) ship a RadioGroup primitive.
 */
function UiBindRadio({
  value,
  onChange,
  disabled,
}: {
  value: "noop" | "set" | "clear" | "reset";
  onChange: (v: "noop" | "set" | "clear" | "reset") => void;
  disabled?: boolean;
}) {
  const opts: Array<{
    value: "noop" | "set" | "clear" | "reset";
    label: string;
    hint: string;
  }> = [
    {
      value: "noop",
      label: "Leave unchanged",
      hint: "Don't touch the persisted ui_bind row.",
    },
    {
      value: "set",
      label: "Use a specific address",
      hint: "Persist a host:port for the second listener.",
    },
    {
      value: "clear",
      label: "Disable second listener",
      hint: "Persist explicit 'off' — overrides nexus.toml at next boot.",
    },
    {
      value: "reset",
      label: "Use nexus.toml default",
      hint: "Drop the override row — fall back to server.ui_bind in nexus.toml.",
    },
  ];
  return (
    <div className="flex flex-col gap-1.5" data-testid="ui-bind-action-group">
      {opts.map((o) => (
        <label
          key={o.value}
          className="flex cursor-pointer items-start gap-2 text-sm"
        >
          <input
            type="radio"
            name="ui-bind-action"
            value={o.value}
            checked={value === o.value}
            onChange={() => onChange(o.value)}
            disabled={disabled}
            data-testid={`ui-bind-action-${o.value}`}
            className="mt-1"
          />
          <span className="flex flex-col">
            <span>{o.label}</span>
            <span className="text-xs text-muted-foreground">{o.hint}</span>
          </span>
        </label>
      ))}
    </div>
  );
}

function WmBadge({ state }: { state: "ok" | "low" | "panic" }) {
  if (state === "panic") return <Badge variant="destructive">panic</Badge>;
  if (state === "low") return <Badge variant="warning">low</Badge>;
  return <Badge variant="success">ok</Badge>;
}
