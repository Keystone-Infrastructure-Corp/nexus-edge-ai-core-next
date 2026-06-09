// Storage page — hot tier + cold backends + USB.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CheckCircle2,
  Cloud,
  HardDrive,
  Pencil,
  Plus,
  Server,
  Trash2,
  Usb,
  Zap,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import {
  deleteBackend,
  getOAuthStatus,
  getStorage,
  putBackend,
  putColdReplica,
  putUsbPreferred,
  startOAuth,
} from "@/api/storage";
import type {
  BackendKind,
  ColdHealthOut,
  OAuthProvider,
  StorageBackendOut,
  StorageResponse,
  UsbAttached,
  WatermarkState,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Progress } from "@/components/ui/progress";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";

export function StoragePage() {
  const qc = useQueryClient();
  const storageQuery = useQuery({
    queryKey: ["storage", "view"],
    queryFn: getStorage,
    refetchInterval: 10_000,
  });

  const [editing, setEditing] = useState<{
    handle: string;
    kind: BackendKind;
    config: Record<string, unknown>;
  } | null>(null);
  const [adding, setAdding] = useState(false);

  const data = storageQuery.data;

  const onSaved = () => {
    setEditing(null);
    setAdding(false);
    qc.invalidateQueries({ queryKey: ["storage", "view"] });
  };

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Storage</h1>
          <p className="text-sm text-muted-foreground">
            Hot tier (local disk) and cold replicas (LAN, Google Drive,
            OneDrive).
          </p>
        </div>
        <Button onClick={() => setAdding(true)}>
          <Plus className="mr-2 h-4 w-4" />
          Add backend
        </Button>
      </header>

      {storageQuery.isLoading ? (
        <div className="space-y-3">
          <Skeleton className="h-32 w-full" />
          <Skeleton className="h-24 w-full" />
        </div>
      ) : storageQuery.isError || !data ? (
        <Card>
          <CardContent className="py-6 text-center text-sm text-destructive">
            Failed to load storage state.
          </CardContent>
        </Card>
      ) : (
        <>
          <HotTierCard data={data} />
          <ColdReplicaCard data={data} />
          <BackendsCard
            data={data}
            onEdit={(b) =>
              setEditing({
                handle: b.handle,
                kind: b.kind as BackendKind,
                config: b.config,
              })
            }
          />
          <UsbCard data={data} />
        </>
      )}

      {adding ? (
        <BackendSheet
          mode="new"
          existing={data?.backends ?? []}
          onClose={() => setAdding(false)}
          onSaved={onSaved}
        />
      ) : null}
      {editing ? (
        <BackendSheet
          mode="edit"
          initial={editing}
          existing={data?.backends ?? []}
          onClose={() => setEditing(null)}
          onSaved={onSaved}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Hot tier.
// ---------------------------------------------------------------------------

function HotTierCard({ data }: { data: StorageResponse }) {
  const hot = data.hot;
  const pct =
    hot.fs_total_bytes && hot.fs_used_bytes != null
      ? (hot.fs_used_bytes / hot.fs_total_bytes) * 100
      : null;

  return (
    <Card>
      <CardContent className="space-y-4 p-5">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="flex items-center gap-2 text-base font-semibold">
              <HardDrive className="h-4 w-4" />
              Hot tier
            </h2>
            <p className="font-mono text-xs text-muted-foreground">
              {hot.clips_dir}
            </p>
          </div>
          <WatermarkBadge state={hot.watermark_state} panic={hot.panic} />
        </div>

        {pct != null && hot.fs_total_bytes && hot.fs_used_bytes != null ? (
          <div className="space-y-1.5">
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>
                {fmtBytes(hot.fs_used_bytes)} / {fmtBytes(hot.fs_total_bytes)}{" "}
                used
              </span>
              <span>{pct.toFixed(1)}%</span>
            </div>
            <Progress
              value={pct}
              className={
                hot.watermark_state === "panic"
                  ? "[&>div]:bg-destructive"
                  : hot.watermark_state === "low"
                    ? "[&>div]:bg-warning"
                    : undefined
              }
            />
            <p className="text-xs text-muted-foreground">
              Low at {hot.watermark_low_pct}% free · Panic at{" "}
              {hot.watermark_panic_pct}% free
            </p>
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">
            Filesystem stats unavailable on this platform.
          </p>
        )}

        {hot.per_camera.length > 0 ? (
          <div>
            <p className="mb-2 text-xs uppercase text-muted-foreground">
              Per camera
            </p>
            <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-2 lg:grid-cols-3">
              {hot.per_camera.map((c) => (
                <div
                  key={c.camera_id}
                  className="flex items-center justify-between rounded-md border border-border/40 px-2 py-1 text-xs"
                >
                  <span className="font-mono text-muted-foreground">
                    cam {c.camera_id}
                  </span>
                  <span>
                    {c.clip_count} clip{c.clip_count === 1 ? "" : "s"} ·{" "}
                    {fmtBytes(c.bytes)}
                  </span>
                </div>
              ))}
            </div>
          </div>
        ) : null}

        {data.cold_only_count > 0 ? (
          <p className="text-xs text-muted-foreground">
            {data.cold_only_count} clip
            {data.cold_only_count === 1 ? "" : "s"} live on cold tier only
            (evicted from hot).
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function WatermarkBadge({
  state,
  panic,
}: {
  state: WatermarkState;
  panic: boolean;
}) {
  if (panic || state === "panic") {
    return (
      <Badge variant="destructive">
        <AlertTriangle className="mr-1 h-3 w-3" />
        PANIC
      </Badge>
    );
  }
  if (state === "low") {
    return <Badge variant="warning">LOW</Badge>;
  }
  return <Badge variant="success">OK</Badge>;
}

// ---------------------------------------------------------------------------
// Cold replica.
// ---------------------------------------------------------------------------

function ColdReplicaCard({ data }: { data: StorageResponse }) {
  const qc = useQueryClient();
  const [throttleEdit, setThrottleEdit] = useState<string>("");
  const [error, setError] = useState<string | null>(null);

  const cold = data.cold;
  const activeHandle = cold?.handle ?? "";
  const remoteBackends = data.backends.filter(
    (b) => b.kind !== "lan" || true, // all non-local
  );

  const activateMutation = useMutation({
    mutationFn: (handle: string | null) =>
      putColdReplica({ handle, throttle_bps: cold?.throttle_bps }),
    onSuccess: () => {
      setError(null);
      qc.invalidateQueries({ queryKey: ["storage", "view"] });
    },
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const throttleMutation = useMutation({
    mutationFn: (bps: number) =>
      putColdReplica({ handle: cold?.handle ?? null, throttle_bps: bps }),
    onSuccess: () => {
      setError(null);
      setThrottleEdit("");
      qc.invalidateQueries({ queryKey: ["storage", "view"] });
    },
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  return (
    <Card>
      <CardContent className="space-y-4 p-5">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="flex items-center gap-2 text-base font-semibold">
              <Cloud className="h-4 w-4" />
              Cold replica
            </h2>
            <p className="text-xs text-muted-foreground">
              Active backend mirrors clips off the hot tier.
            </p>
          </div>
          {cold ? <ColdHealthBadge health={cold.health} /> : (
            <Badge variant="secondary">disabled</Badge>
          )}
        </div>

        {cold?.health.reason ? (
          <p
            className="text-xs text-muted-foreground"
            data-testid="cold-health-reason"
          >
            {cold.health.reason}
          </p>
        ) : null}

        {error ? (
          <p className="text-xs text-destructive">{error}</p>
        ) : null}

        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div className="space-y-2">
            <Label>Active backend</Label>
            <select
              className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
              value={activeHandle}
              onChange={(e) => {
                const v = e.target.value;
                activateMutation.mutate(v === "" ? null : v);
              }}
              disabled={activateMutation.isPending}
            >
              <option value="">(disabled)</option>
              {remoteBackends.map((b) => (
                <option key={b.handle} value={b.handle}>
                  {b.handle} ({b.kind})
                </option>
              ))}
            </select>
          </div>

          <div className="space-y-2">
            <Label htmlFor="cold-throttle">Throttle (bytes/sec)</Label>
            <div className="flex gap-2">
              <Input
                id="cold-throttle"
                type="number"
                min={0}
                placeholder={cold ? String(cold.throttle_bps) : "0"}
                value={throttleEdit}
                onChange={(e) => setThrottleEdit(e.target.value)}
                disabled={!cold}
              />
              <Button
                variant="outline"
                size="sm"
                onClick={() => {
                  const n = Number.parseInt(throttleEdit, 10);
                  if (!Number.isNaN(n) && n >= 0) {
                    throttleMutation.mutate(n);
                  }
                }}
                disabled={
                  !cold || throttleMutation.isPending || throttleEdit === ""
                }
              >
                <Zap className="mr-1 h-3 w-3" />
                Apply
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              0 = unlimited. Current:{" "}
              {cold ? fmtBytes(cold.throttle_bps) + "/s" : "—"}
            </p>
          </div>
        </div>

        {cold ? (
          <div className="grid grid-cols-2 gap-2 rounded-md border border-border/40 bg-muted/20 p-3 text-xs sm:grid-cols-4">
            <Stat
              label="Pending"
              value={cold.pending_count.toLocaleString()}
            />
            <Stat
              label="Replicated"
              value={cold.replicated_count.toLocaleString()}
            />
            <Stat
              label="Cold-only"
              value={cold.cold_only_count.toLocaleString()}
            />
            <Stat
              label="Lifetime"
              value={fmtBytes(cold.lifetime_uploaded_bytes)}
            />
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

function ColdHealthBadge({ health }: { health: ColdHealthOut }) {
  switch (health.status) {
    case "ok":
      return <Badge variant="success">healthy</Badge>;
    case "read_only":
      return <Badge variant="warning">read-only</Badge>;
    case "unreachable":
      return <Badge variant="destructive">unreachable</Badge>;
    case "not_registered":
      return <Badge variant="secondary">not registered</Badge>;
  }
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <p className="text-muted-foreground">{label}</p>
      <p className="font-mono">{value}</p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Backends list.
// ---------------------------------------------------------------------------

function BackendsCard({
  data,
  onEdit,
}: {
  data: StorageResponse;
  onEdit: (b: StorageBackendOut) => void;
}) {
  return (
    <Card>
      <CardContent className="p-0">
        <div className="flex items-center justify-between px-5 pt-5">
          <h2 className="flex items-center gap-2 text-base font-semibold">
            <Server className="h-4 w-4" />
            Backends
          </h2>
        </div>
        {data.backends.length === 0 ? (
          <p className="px-5 py-6 text-center text-sm text-muted-foreground">
            No backends registered. Add one above.
          </p>
        ) : (
          <div className="overflow-x-auto px-5 pb-5 pt-3">
            <table className="w-full text-sm">
              <thead className="text-xs uppercase text-muted-foreground">
                <tr>
                  <th className="px-2 py-2 text-left">Handle</th>
                  <th className="px-2 py-2 text-left">Kind</th>
                  <th className="px-2 py-2 text-left">Config</th>
                  <th className="px-2 py-2 text-left">Updated</th>
                  <th className="px-2 py-2 text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {data.backends.map((b) => (
                  <BackendRow
                    key={b.handle}
                    backend={b}
                    isActiveCold={data.cold?.handle === b.handle}
                    onEdit={() => onEdit(b)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function BackendRow({
  backend,
  isActiveCold,
  onEdit,
}: {
  backend: StorageBackendOut;
  isActiveCold: boolean;
  onEdit: () => void;
}) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const delMutation = useMutation({
    mutationFn: (h: string) => deleteBackend(h),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["storage", "view"] }),
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const configPreview = formatConfigPreview(backend.kind, backend.config);

  return (
    <tr className="border-t border-border/40">
      <td className="px-2 py-2 font-mono text-xs">{backend.handle}</td>
      <td className="px-2 py-2">
        <Badge variant="outline" className="capitalize">
          {backend.kind}
        </Badge>
      </td>
      <td className="px-2 py-2 text-xs text-muted-foreground">
        {configPreview}
        {error ? (
          <span className="ml-2 text-destructive">{error}</span>
        ) : null}
      </td>
      <td className="px-2 py-2 text-xs text-muted-foreground">
        {backend.updated_at.replace("T", " ").slice(0, 19)}
      </td>
      <td className="px-2 py-2 text-right">
        <div className="flex justify-end gap-1">
          <Button size="sm" variant="ghost" onClick={onEdit}>
            <Pencil className="h-4 w-4" />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              if (isActiveCold) {
                setError(
                  "Active cold replica. Switch to another backend first.",
                );
                return;
              }
              if (confirm(`Delete backend "${backend.handle}"?`)) {
                delMutation.mutate(backend.handle);
              }
            }}
            disabled={delMutation.isPending || isActiveCold}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </td>
    </tr>
  );
}

function formatConfigPreview(
  kind: string,
  config: Record<string, unknown>,
): string {
  if (kind === "lan") {
    return `root: ${String(config.root ?? "—")}`;
  }
  if (kind === "gdrive" || kind === "onedrive") {
    const email = config.account_email ? String(config.account_email) : "—";
    return `account: ${email}`;
  }
  return Object.keys(config).join(", ") || "—";
}

// ---------------------------------------------------------------------------
// USB.
// ---------------------------------------------------------------------------

function UsbCard({ data }: { data: StorageResponse }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const mutation = useMutation({
    mutationFn: (label: string | null) => putUsbPreferred(label),
    onSuccess: () => {
      setError(null);
      qc.invalidateQueries({ queryKey: ["storage", "view"] });
    },
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const u = data.usb;

  return (
    <Card>
      <CardContent className="space-y-3 p-5">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="flex items-center gap-2 text-base font-semibold">
              <Usb className="h-4 w-4" />
              USB volume
            </h2>
            <p className="text-xs text-muted-foreground">
              When a preferred label is set and attached, new clips are
              recorded under that volume.
            </p>
          </div>
          {u.preferred_active ? (
            <Badge variant="success">active</Badge>
          ) : u.preferred_label ? (
            <Badge variant="warning">not attached</Badge>
          ) : null}
        </div>

        {error ? (
          <p className="text-xs text-destructive">{error}</p>
        ) : null}

        <div className="space-y-2">
          <Label>Preferred label</Label>
          <select
            className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
            value={u.preferred_label ?? ""}
            onChange={(e) =>
              mutation.mutate(e.target.value === "" ? null : e.target.value)
            }
            disabled={mutation.isPending}
          >
            <option value="">(none — record to clips_dir root)</option>
            {u.attached.map((v: UsbAttached) => (
              <option key={v.label} value={v.label}>
                {v.label} ({v.mount_relpath})
              </option>
            ))}
            {u.preferred_label &&
            !u.attached.some((v) => v.label === u.preferred_label) ? (
              <option value={u.preferred_label}>
                {u.preferred_label} (not currently attached)
              </option>
            ) : null}
          </select>
        </div>

        {u.attached.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            No USB volumes currently attached.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Backend editor sheet.
// ---------------------------------------------------------------------------

function BackendSheet({
  mode,
  initial,
  existing,
  onClose,
  onSaved,
}: {
  mode: "new" | "edit";
  initial?: { handle: string; kind: BackendKind; config: Record<string, unknown> };
  existing: StorageBackendOut[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const [handle, setHandle] = useState(initial?.handle ?? "");
  const [kind, setKind] = useState<BackendKind>(initial?.kind ?? "lan");
  const [lanRoot, setLanRoot] = useState<string>(
    initial?.kind === "lan" ? String(initial.config.root ?? "") : "",
  );
  const [cloudClientId, setCloudClientId] = useState<string>(
    initial && initial.kind !== "lan"
      ? String(initial.config.client_id ?? "")
      : "",
  );
  const [cloudClientSecret, setCloudClientSecret] = useState<string>("");
  const [cloudAccountEmail, setCloudAccountEmail] = useState<string>(
    initial && initial.kind !== "lan"
      ? String(initial.config.account_email ?? "")
      : "",
  );
  const [rootFolderId, setRootFolderId] = useState<string>("");
  const [oauthState, setOauthState] = useState<string | null>(null);
  const [oauthError, setOauthError] = useState<string | null>(null);
  const [oauthDone, setOauthDone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const popupRef = useRef<Window | null>(null);

  // OAuth polling.
  useEffect(() => {
    if (!oauthState || oauthDone) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const res = await getOAuthStatus(oauthState);
        if (cancelled) return;
        if (res.status === "complete") {
          setOauthDone(true);
          setOauthError(null);
          onSaved();
          return;
        }
        if (res.status === "error") {
          setOauthError(res.message);
          setOauthDone(true);
          return;
        }
      } catch (e) {
        if (cancelled) return;
        setOauthError(e instanceof Error ? e.message : String(e));
        setOauthDone(true);
        return;
      }
      timer = setTimeout(poll, 1000);
    };
    timer = setTimeout(poll, 1000);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [oauthState, oauthDone, onSaved]);

  const saveLanMutation = useMutation({
    mutationFn: () =>
      putBackend(handle, { kind: "lan", config: { root: lanRoot } }),
    onSuccess: onSaved,
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const startOAuthMutation = useMutation({
    mutationFn: () => {
      const redirect_uri = `${window.location.origin}/api/v1/admin/oauth/${kind}/callback`;
      return startOAuth(kind as OAuthProvider, {
        handle,
        client_id: cloudClientId,
        client_secret: cloudClientSecret,
        account_email: cloudAccountEmail || undefined,
        root_folder_id: kind === "gdrive" && rootFolderId ? rootFolderId : undefined,
        redirect_uri,
      });
    },
    onSuccess: (resp) => {
      setOauthError(null);
      setOauthDone(false);
      setOauthState(resp.state);
      popupRef.current = window.open(
        resp.authorize_url,
        "nexus-oauth",
        "width=560,height=720",
      );
    },
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const handleTaken =
    mode === "new" && existing.some((b) => b.handle === handle);

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!handle.trim()) {
      setError("Handle is required.");
      return;
    }
    if (handleTaken) {
      setError("Handle already in use.");
      return;
    }
    if (!/^[a-z0-9][a-z0-9_-]*$/.test(handle)) {
      setError(
        "Handle must start with a letter or digit and contain only [a-z0-9_-].",
      );
      return;
    }
    if (handle === "local") {
      setError('Handle "local" is reserved.');
      return;
    }
    if (kind === "lan") {
      if (!lanRoot.trim()) {
        setError("Root path is required.");
        return;
      }
      saveLanMutation.mutate();
    } else {
      if (!cloudClientId.trim() || !cloudClientSecret.trim()) {
        setError("Client ID and secret are required.");
        return;
      }
      startOAuthMutation.mutate();
    }
  };

  const isNew = mode === "new";

  return (
    <Sheet
      open
      onClose={onClose}
      title={isNew ? "Add backend" : `Edit ${initial?.handle ?? ""}`}
      description="LAN is filesystem-only. Google Drive and OneDrive require an OAuth round-trip."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          {kind === "lan" ? (
            <Button
              onClick={onSubmit}
              disabled={saveLanMutation.isPending}
            >
              {saveLanMutation.isPending ? "Saving…" : "Save"}
            </Button>
          ) : (
            <Button
              onClick={onSubmit}
              disabled={startOAuthMutation.isPending || !!oauthState}
            >
              {oauthState
                ? "Waiting for consent…"
                : startOAuthMutation.isPending
                  ? "Starting…"
                  : "Connect"}
            </Button>
          )}
        </>
      }
    >
      <form onSubmit={onSubmit}>
        {error ? (
          <div className="border-b border-destructive/50 bg-destructive/10 px-5 py-3 text-sm text-destructive">
            {error}
          </div>
        ) : null}

        <SheetSection title="Identity">
          <div className="space-y-2">
            <Label htmlFor="bk-handle">Handle</Label>
            <Input
              id="bk-handle"
              value={handle}
              onChange={(e) => setHandle(e.target.value.toLowerCase())}
              disabled={!isNew}
              placeholder="archive-nas"
              className="font-mono"
            />
            <p className="text-xs text-muted-foreground">
              Lowercase letters, digits, dashes, underscores. Reserved:{" "}
              <code className="font-mono">local</code>.
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="bk-kind">Kind</Label>
            <select
              id="bk-kind"
              className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
              value={kind}
              onChange={(e) => setKind(e.target.value as BackendKind)}
              disabled={!isNew}
            >
              <option value="lan">LAN (mounted filesystem)</option>
              <option value="gdrive">Google Drive</option>
              <option value="onedrive">OneDrive</option>
            </select>
          </div>
        </SheetSection>

        {kind === "lan" ? (
          <SheetSection title="LAN config">
            <div className="space-y-2">
              <Label htmlFor="bk-root">Root path</Label>
              <Input
                id="bk-root"
                value={lanRoot}
                onChange={(e) => setLanRoot(e.target.value)}
                placeholder="/mnt/nas-archive"
                className="font-mono"
              />
            </div>
          </SheetSection>
        ) : (
          <>
            <SheetSection
              title="OAuth app credentials"
              description={
                kind === "gdrive"
                  ? "From Google Cloud Console → APIs & Services → Credentials."
                  : "From Azure → App Registrations → your app."
              }
            >
              <div className="space-y-2">
                <Label htmlFor="bk-cid">Client ID</Label>
                <Input
                  id="bk-cid"
                  value={cloudClientId}
                  onChange={(e) => setCloudClientId(e.target.value)}
                  className="font-mono"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="bk-csecret">Client secret</Label>
                <Input
                  id="bk-csecret"
                  type="password"
                  value={cloudClientSecret}
                  onChange={(e) => setCloudClientSecret(e.target.value)}
                  className="font-mono"
                  placeholder={
                    !isNew ? "(unchanged unless re-entered)" : undefined
                  }
                />
                <p className="text-xs text-muted-foreground">
                  Not stored on disk in cleartext; encrypted via the engine
                  admin secret.
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="bk-email">Account email (optional)</Label>
                <Input
                  id="bk-email"
                  value={cloudAccountEmail}
                  onChange={(e) => setCloudAccountEmail(e.target.value)}
                  placeholder="ops@example.com"
                />
              </div>
              {kind === "gdrive" ? (
                <div className="space-y-2">
                  <Label htmlFor="bk-folder">Root folder ID (optional)</Label>
                  <Input
                    id="bk-folder"
                    value={rootFolderId}
                    onChange={(e) => setRootFolderId(e.target.value)}
                    placeholder="Leave blank for app root"
                    className="font-mono"
                  />
                </div>
              ) : null}
            </SheetSection>

            {oauthState ? (
              <SheetSection title="OAuth status">
                {oauthDone && !oauthError ? (
                  <p className="flex items-center gap-2 text-sm text-success">
                    <CheckCircle2 className="h-4 w-4" />
                    Connected. You may close this sheet.
                  </p>
                ) : oauthError ? (
                  <p className="flex items-center gap-2 text-sm text-destructive">
                    <AlertTriangle className="h-4 w-4" />
                    {oauthError}
                  </p>
                ) : (
                  <p className="text-sm text-muted-foreground">
                    Waiting for consent in the popup window…
                  </p>
                )}
              </SheetSection>
            ) : null}
          </>
        )}
      </form>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

function fmtBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}
