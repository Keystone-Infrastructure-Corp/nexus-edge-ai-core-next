// Cameras page — list + add/edit slide-over + discovery sheet.
//
// The slide-over editor is the primary UX. We also support deep-links
// at `/cameras/$id` (TanStack Router): when the URL carries an id, the
// component pulls it via `useParams({ strict: false })`, opens the
// editor for the matching row, and on close pushes back to `/cameras`.
// The list view at `/cameras` behaves identically to before.
//
// Warning banner is rendered when `model_override.kind` differs from the
// existing camera — that change requires an engine restart (router
// rebuilds layers at boot only).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import {
  AlertTriangle,
  Camera as CameraIcon,
  Pencil,
  Plus,
  Radar,
  RefreshCw,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  attachVisualPrompt,
  deleteCamera,
  detachVisualPrompt,
  getDiscoverySession,
  getModelPromptsCatalog,
  listCameraVisualPrompts,
  listVisualPrompts,
  probeOnvifStreams,
  probeRtsp,
  startCidrScan,
  startOnvifDiscovery,
  upsertCamera,
} from "@/api/config";
import {
  getStaticObjectDefaults,
  latestFrameJpegUrl,
  listCameras,
} from "@/api/system";
import type {
  CameraConfig,
  CodecKind,
  DiscoveredDevice,
  DiscoverySessionView,
  ModelOverride,
  ModelPromptsCatalog,
  ModelPromptsEntry,
  ProbeRtspStream,
  ZoneConfig,
  ZoneKind,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import {
  defaultSizeForKind,
  describeSize,
  sizesForKind,
} from "@/lib/model-sizes";

const EMPTY_CAMERA: CameraConfig = {
  // 0 is the sentinel for "no server-assigned id yet". The
  // smart `upsertCamera` helper routes to POST when it sees
  // `id <= 0` and the engine assigns the real i64 from
  // SQLite's rowid alias. Any string here would have been
  // rejected by `Path<i64>` extractor with HTTP 400.
  id: 0,
  name: "",
  url: "",
  enabled: true,
  max_fps: 0,
  prompts: [],
  visual_prompts: [],
  model_override: null,
  parking_lot_mode: false,
  zones: [],
};

export function CamerasPage() {
  const qc = useQueryClient();
  const navigate = useNavigate();
  // `strict: false` returns `{}` when mounted on `/cameras` (no
  // params) and `{ id: "<cam>" }` when mounted on `/cameras/$id`.
  const params = useParams({ strict: false }) as { id?: string };
  const deepLinkId = params.id;
  const camerasQuery = useQuery({
    queryKey: ["cameras", "list"],
    queryFn: listCameras,
    staleTime: 10_000,
  });

  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<CameraConfig | null>(null);
  const [discoveryOpen, setDiscoveryOpen] = useState(false);

  const cameras = camerasQuery.data ?? [];

  const openNew = () => {
    setEditing({ ...EMPTY_CAMERA });
    setEditorOpen(true);
  };
  const openExisting = (cam: CameraConfig) => {
    // Shallow clone with sensible fallbacks so the form fields always
    // have defined values to bind to.
    setEditing({
      ...EMPTY_CAMERA,
      ...cam,
      prompts: cam.prompts ?? [],
      visual_prompts: cam.visual_prompts ?? [],
      zones: cam.zones ?? [],
    });
    setEditorOpen(true);
  };

  const openFromDiscovered = (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
  ) => {
    // Embed credentials in the URL when supplied so the engine's RTSP
    // client picks them up without an extra `[cameras.*]` field. Both
    // halves are URL-encoded so passwords containing `:` / `@` / `/`
    // / `?` survive the parser round-trip.
    const creds = username
      ? `${encodeURIComponent(username)}${
          password ? `:${encodeURIComponent(password)}` : ""
        }@`
      : "";
    // device.port is the port the discovery scan answered on —
    // typically :80 (ONVIF/web) for cameras discovered via
    // WS-Discovery, never :554. The actual RTSP socket lives at
    // device.rtsp_port (set by the scan merge) or, when discovery
    // never confirmed it, the RFC-standard 554.
    const rtspPort = device.rtsp_port ?? 554;
    const rtsp = `rtsp://${creds}${device.ip}:${rtspPort}${path}`;
    setEditing({
      ...EMPTY_CAMERA,
      // `id` stays at 0 (the EMPTY_CAMERA sentinel) — the engine
      // assigns the real i64 on save via POST /cameras. A
      // derived string id like `cam-<ip>` would fail the
      // server's `Path<i64>` extractor with HTTP 400.
      name: device.vendor
        ? `${device.vendor} ${device.model ?? ""}`.trim()
        : device.ip,
      url: rtsp,
      // Seed the codec from discovery so the operator doesn't
      // have to re-pick. `null` here means "let the admin API
      // autodetect" — if the probe also failed during discovery
      // we leave it unset rather than guessing H.264.
      codec: device.codec ?? null,
    });
    setDiscoveryOpen(false);
    setEditorOpen(true);
  };

  const closeEditor = () => {
    setEditorOpen(false);
    setEditing(null);
    // If we landed via deep-link, drop the param so the URL goes
    // back to the list view. No-op when already on `/cameras`.
    if (deepLinkId !== undefined) {
      navigate({ to: "/cameras" });
    }
  };

  const handleSaved = () => {
    closeEditor();
    qc.invalidateQueries({ queryKey: ["cameras", "list"] });
  };

  // Honor the deep-link: once the list resolves, open the editor for
  // the matching camera. Only fires when the URL carries an id and
  // the editor isn't already open for that exact row.
  useEffect(() => {
    if (!deepLinkId || editorOpen) return;
    const cam = cameras.find((c) => String(c.id) === deepLinkId);
    if (cam) openExisting(cam);
    // We intentionally depend only on the id + cameras list; the
    // editor open flag is read fresh each render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deepLinkId, cameras.length]);

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Cameras</h1>
          <p className="text-sm text-muted-foreground">
            Configure RTSP sources, detector overrides, prompts, and zones.
          </p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => setDiscoveryOpen(true)}>
            <Radar className="mr-2 h-4 w-4" />
            Discover
          </Button>
          <Button onClick={openNew}>
            <Plus className="mr-2 h-4 w-4" />
            Add camera
          </Button>
        </div>
      </header>

      <Card>
        <CardContent className="p-0">
          {camerasQuery.isLoading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-10 w-full" />
              ))}
            </div>
          ) : cameras.length === 0 ? (
            <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
              <CameraIcon className="h-8 w-8 opacity-50" />
              <p>No cameras configured.</p>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="bg-muted/30 text-xs uppercase text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left">ID</th>
                    <th className="px-3 py-2 text-left">Name</th>
                    <th className="px-3 py-2 text-left">URL</th>
                    <th className="px-3 py-2 text-left">Model</th>
                    <th className="px-3 py-2 text-left">Status</th>
                    <th className="px-3 py-2 text-right">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {cameras.map((cam) => (
                    <CameraRow
                      key={String(cam.id)}
                      camera={cam}
                      onEdit={() => openExisting(cam)}
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {editorOpen && editing ? (
        <CameraEditor
          camera={editing}
          existing={cameras}
          onClose={closeEditor}
          onSaved={handleSaved}
        />
      ) : null}

      {discoveryOpen ? (
        <DiscoverySheet
          onClose={() => setDiscoveryOpen(false)}
          onAdd={openFromDiscovered}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// List row.
// ---------------------------------------------------------------------------

function CameraRow({
  camera,
  onEdit,
}: {
  camera: CameraConfig;
  onEdit: () => void;
}) {
  const qc = useQueryClient();
  const delMutation = useMutation({
    mutationFn: (id: number) => deleteCamera(id),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["cameras", "list"] }),
  });

  const id = String(camera.id);
  const enabled = camera.enabled !== false;
  const modelKind =
    camera.model_override && typeof camera.model_override === "object"
      ? ((camera.model_override as { kind?: unknown }).kind as string | undefined) ?? "—"
      : "—";

  return (
    <tr className="border-t border-border/40">
      <td className="px-3 py-2 font-mono text-xs text-muted-foreground">
        {id}
      </td>
      <td className="px-3 py-2 font-medium">{camera.name || "—"}</td>
      <td className="max-w-md truncate px-3 py-2 font-mono text-xs text-muted-foreground">
        {camera.url}
      </td>
      <td className="px-3 py-2">
        <Badge variant="outline">{modelKind}</Badge>
      </td>
      <td className="px-3 py-2">
        {enabled ? (
          <Badge variant="success">enabled</Badge>
        ) : (
          <Badge variant="secondary">disabled</Badge>
        )}
      </td>
      <td className="px-3 py-2 text-right">
        <div className="flex justify-end gap-1">
          <Button size="sm" variant="ghost" onClick={onEdit}>
            <Pencil className="h-4 w-4" />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              if (
                confirm(`Delete camera ${id}? This cannot be undone.`)
              ) {
                delMutation.mutate(camera.id);
              }
            }}
            disabled={delMutation.isPending}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </td>
    </tr>
  );
}

// ---------------------------------------------------------------------------
// Camera editor sheet.
// ---------------------------------------------------------------------------

function CameraEditor({
  camera,
  existing,
  onClose,
  onSaved,
}: {
  camera: CameraConfig;
  existing: CameraConfig[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const [draft, setDraft] = useState<CameraConfig>(camera);
  const [error, setError] = useState<string | null>(null);

  // Engine-wide defaults for tracker.static_object — used to label
  // the per-camera anchor-TTL override input with the fallback the
  // engine will apply when the override is blank.
  const staticDefaults = useQuery({
    queryKey: ["system", "static-object-defaults"],
    queryFn: getStaticObjectDefaults,
    staleTime: 5 * 60 * 1000,
  });
  const defaultAnchorTtlSecs = staticDefaults.data?.anchor_ttl_secs;

  const original = useMemo(
    () => existing.find((c) => String(c.id) === String(camera.id)) ?? null,
    [existing, camera.id],
  );
  const isNew = original === null;

  const draftKind = (() => {
    const mo = draft.model_override;
    return mo && typeof mo === "object"
      ? ((mo as { kind?: unknown }).kind as string | undefined)
      : undefined;
  })();
  // The router instantiates one InferenceLayer per `model_override.kind` at
  // boot and dedups by kind alone (see `crates/nexus-inference/src/router.rs`).
  // Structural changes (kind, preset, pack_path, input_width, input_height)
  // therefore only take effect after an engine restart; per-camera
  // `score_threshold` / `top_k` are honored live by the rule layer.
  const STRUCTURAL_KEYS = [
    "kind",
    "pack_path",
    "preset",
    "input_width",
    "input_height",
  ] as const;
  const restartRequired = (() => {
    if (isNew) return false;
    const a = (original?.model_override ?? null) as ModelOverride | null;
    const b = (draft.model_override ?? null) as ModelOverride | null;
    const modelChanged =
      !(a === null && b === null) &&
      STRUCTURAL_KEYS.some((k) => {
        const av = a?.[k] ?? null;
        const bv = b?.[k] ?? null;
        return av !== bv;
      });
    // `anchor_ttl_secs` is consumed by `StaticObjectFilter::new` at
    // supervisor boot; the reconciler only respawns on URL change
    // today, so editing it on a live camera demands a restart for
    // the new value to take effect.
    const ttlChanged =
      (original?.anchor_ttl_secs ?? null) !==
      (draft.anchor_ttl_secs ?? null);
    return modelChanged || ttlChanged;
  })();

  const mutation = useMutation({
    mutationFn: (cfg: CameraConfig) => upsertCamera(cfg),
    onSuccess: onSaved,
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
  });

  const set = <K extends keyof CameraConfig>(
    k: K,
    v: CameraConfig[K],
  ) => {
    setDraft((d) => ({ ...d, [k]: v }));
  };

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    // ID is server-assigned on create (POST /cameras returns
    // the populated row). For updates we already have it from
    // the existing row. Only URL is operator-required up front.
    if (!draft.url.trim()) {
      setError("URL is required.");
      return;
    }
    mutation.mutate(draft);
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title={isNew ? "New camera" : `Edit ${draft.name || `cam-${draft.id}`}`}
      description="Changes apply on save. Some changes (model kind) require an engine restart."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={mutation.isPending}>
            {mutation.isPending ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      <form onSubmit={onSubmit} className="space-y-0">
        {error ? (
          <div className="border-b border-destructive/50 bg-destructive/10 px-5 py-3 text-sm text-destructive">
            {error}
          </div>
        ) : null}

        {restartRequired ? (
          <div className="flex items-start gap-2 border-b border-warning/40 bg-warning/10 px-5 py-3 text-xs">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            <div>
              <p className="font-semibold text-warning">
                Engine restart required
              </p>
              <p className="text-muted-foreground">
                The inference router instantiates model layers at boot,
                keyed by{" "}
                <code className="font-mono">model_override.kind</code>.
                Structural fields (kind, preset, pack path, input
                width/height) only take effect after a restart;{" "}
                <code className="font-mono">score_threshold</code> and{" "}
                <code className="font-mono">top_k</code> apply live.
              </p>
            </div>
          </div>
        ) : null}

        <SheetSection title="Identity">
          {isNew ? (
            <p className="text-xs text-muted-foreground">
              ID will be assigned by the engine on save.
            </p>
          ) : (
            <div className="space-y-2">
              <Label htmlFor="cam-id">ID</Label>
              <Input
                id="cam-id"
                value={String(draft.id)}
                disabled
                className="font-mono"
              />
            </div>
          )}
          <div className="space-y-2">
            <Label htmlFor="cam-name">Name</Label>
            <Input
              id="cam-name"
              value={draft.name ?? ""}
              onChange={(e) => set("name", e.target.value)}
              placeholder="Front door"
            />
          </div>
        </SheetSection>

        <SheetSection title="Source">
          <div className="space-y-2">
            <Label htmlFor="cam-url">RTSP URL</Label>
            <Input
              id="cam-url"
              value={draft.url}
              onChange={(e) => set("url", e.target.value)}
              placeholder="rtsp://user:pass@host:554/stream"
              className="font-mono"
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="cam-fps">Max FPS (0 = unbounded)</Label>
              <Input
                id="cam-fps"
                type="number"
                min={0}
                value={draft.max_fps ?? 0}
                onChange={(e) =>
                  set("max_fps", Number.parseInt(e.target.value, 10) || 0)
                }
              />
            </div>
            <div className="flex items-end">
              <label className="inline-flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={draft.enabled !== false}
                  onChange={(e) => set("enabled", e.target.checked)}
                  className="h-4 w-4 rounded border-border"
                />
                Enabled
              </label>
            </div>
          </div>
          <div className="space-y-2">
            <Label htmlFor="cam-codec">Codec</Label>
            <select
              id="cam-codec"
              className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm"
              value={draft.codec ?? "auto"}
              onChange={(e) => {
                const v = e.target.value;
                // "auto" maps to undefined / null so the engine's
                // admin-API autodetect (RTSP DESCRIBE) decides. The
                // explicit kinds bypass autodetect and pin the
                // pipeline's decoder branch (h264 ↔ h265). The `_plus`
                // variants are vendor SVC labels for Hikvision /
                // Dahua / Uniview — picking them tells the engine the
                // wire bitstream is still standard H.26x but the
                // camera advertises an SVC tag.
                set("codec", v === "auto" ? null : (v as CodecKind));
              }}
            >
              <option value="auto">Auto-detect (recommended)</option>
              <option value="h264">H.264</option>
              <option value="h264_plus">H.264+ (Hikvision/Dahua SVC)</option>
              <option value="h265">H.265 / HEVC</option>
              <option value="h265_plus">H.265+ (Hikvision/Dahua SVC)</option>
            </select>
            <p className="text-xs text-muted-foreground">
              Auto-detect runs a one-shot RTSP DESCRIBE when you save. Pick a
              specific codec only if autodetect fails (camera requires Digest
              auth or speaks a non-H.26x codec).
            </p>
          </div>
        </SheetSection>

        <SheetSection
          title="Detector"
          description="Open-vocab prompts narrow detector output to the labels you care about."
        >
          <ModelOverrideEditor
            value={(draft.model_override ?? null) as ModelOverride | null}
            onChange={(mo) => set("model_override", mo)}
          />
          <PromptsField
            prompts={draft.prompts ?? []}
            onChange={(p) => set("prompts", p)}
            modelKind={draftKind}
          />
        </SheetSection>

        <SheetSection
          title="Behaviour"
          description="Parking-lot mode suppresses born/died churn on long-dwelling tracks."
        >
          <label className="inline-flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.parking_lot_mode === true}
              onChange={(e) => set("parking_lot_mode", e.target.checked)}
              className="h-4 w-4 rounded border-border"
            />
            Enable parking-lot mode
          </label>

          <div className="mt-3 flex flex-col gap-1 text-sm">
            <label
              htmlFor="anchor-ttl-secs"
              className="text-xs font-medium text-muted-foreground"
            >
              Static-anchor TTL (seconds)
              {typeof defaultAnchorTtlSecs === "number" && (
                <span className="ml-2 font-normal text-muted-foreground/80">
                  (engine default: {defaultAnchorTtlSecs}s)
                </span>
              )}
            </label>
            <input
              id="anchor-ttl-secs"
              type="number"
              min={0}
              step={1}
              inputMode="numeric"
              placeholder={
                typeof defaultAnchorTtlSecs === "number"
                  ? `inherit engine default (${defaultAnchorTtlSecs})`
                  : "inherit engine default"
              }
              value={
                typeof draft.anchor_ttl_secs === "number"
                  ? String(draft.anchor_ttl_secs)
                  : ""
              }
              onChange={(e) => {
                const raw = e.target.value.trim();
                if (raw === "") {
                  set("anchor_ttl_secs", null);
                  return;
                }
                const n = Number(raw);
                if (Number.isFinite(n) && n >= 0) {
                  set("anchor_ttl_secs", Math.floor(n));
                }
              }}
              disabled={draft.parking_lot_mode !== true}
              className="w-48 rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
            />
            <span className="text-xs text-muted-foreground">
              Persisted static anchors with no matching observation older than
              this many seconds are swept on the next classify tick. Blank =
              inherit the engine-wide setting (tracker.static_object.anchor_ttl_secs).
              Restart required.
            </span>
          </div>
        </SheetSection>

        <SheetSection
          title="Zones"
          description="Draw polygon zones on the live snapshot. Click to add vertices, double-click to close, drag a vertex to move it, right-click a vertex to delete it. Coordinates are stored normalized 0..1 of the source frame."
        >
          <ZonesEditor
            cameraId={draft.id}
            isNew={isNew}
            zones={draft.zones ?? []}
            onChange={(z) => set("zones", z)}
          />
        </SheetSection>

        {!isNew ? (
          <SheetSection
            title="Visual prompts"
            description="Attach reference images to drive open-vocab detection."
          >
            <CameraVisualPromptsEditor cameraId={String(draft.id)} />
          </SheetSection>
        ) : null}
      </form>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Model override editor — kind dropdown + structural / threshold fields.
//
// Every field is optional: anything left blank is omitted from the wire
// payload, and the engine applies the `nexus_config::ModelConfig` default
// (see `crates/nexus-config/src/lib.rs`). Clearing the kind nulls the
// whole override (camera then runs the engine-wide default kind).
// ---------------------------------------------------------------------------

function ModelOverrideEditor({
  value,
  onChange,
}: {
  value: ModelOverride | null;
  onChange: (v: ModelOverride | null) => void;
}) {
  const catalog = useQuery({
    queryKey: ["models", "prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 5 * 60_000,
  });
  const entries = catalog.data?.kinds ?? [];
  const kind = value?.kind ?? "";
  const selected = entries.find((e) => e.kind === kind);
  const availableSizes = sizesForKind(kind);
  const showSizePicker = availableSizes.length > 1;
  const fixedSize = availableSizes.length === 1 ? availableSizes[0] : undefined;

  // Patch helper — merges a partial update, then strips any field that
  // collapsed to `""` / `null` / `undefined` so the wire payload only
  // carries values the operator actually set. When the resulting object
  // has no `kind`, the whole override is nulled.
  const patch = (delta: Partial<ModelOverride>) => {
    const merged: ModelOverride = { ...(value ?? {}), ...delta };
    const cleaned: ModelOverride = {};
    for (const [k, v] of Object.entries(merged)) {
      if (v === "" || v === null || v === undefined) continue;
      cleaned[k] = v;
    }
    if (!cleaned.kind) {
      onChange(null);
    } else {
      onChange(cleaned);
    }
  };

  // Snap size fields when the kind changes so we never persist an
  // (kind, size) combo the engine doesn't ship a file for. Per-kind
  // size tables live in `lib/model-sizes.ts`.
  const switchKind = (nextKind: string) => {
    if (nextKind === kind) return;
    if (!nextKind) {
      // Clearing kind drops the whole override (see patch()).
      patch({
        kind: "",
        preset: undefined,
        input_width: undefined,
        input_height: undefined,
      });
      return;
    }
    const opts = sizesForKind(nextKind);
    if (opts.length === 0) {
      // Single-size or no-size kind: clear size fields and let the engine apply its default.
      patch({
        kind: nextKind,
        preset: undefined,
        input_width: undefined,
        input_height: undefined,
      });
      return;
    }
    const current = value?.input_width;
    const keep = current && opts.includes(current) ? current : defaultSizeForKind(nextKind);
    if (!keep) {
      patch({ kind: nextKind });
      return;
    }
    patch({
      kind: nextKind,
      preset: String(keep),
      input_width: keep,
      input_height: keep,
    });
  };

  // Parses a number input that may be empty. Returns undefined for empty
  // strings so the field is dropped from the payload, and NaN-guards bad
  // input (treats it as "unset" rather than poisoning the override).
  const num = (raw: string): number | undefined => {
    if (raw.trim() === "") return undefined;
    const n = Number(raw);
    return Number.isFinite(n) ? n : undefined;
  };

  return (
    <div className="space-y-3">
      <div className="space-y-2">
        <Label htmlFor="cam-model-kind">Model override kind</Label>
        <select
          id="cam-model-kind"
          className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
          value={kind}
          onChange={(e) => switchKind(e.target.value)}
        >
          <option value="">(use engine default)</option>
          {entries.map((e) => (
            <option key={e.kind} value={e.kind}>
              {e.kind}
              {e.loaded ? "" : " — restart engine to activate"}
            </option>
          ))}
        </select>
        {selected?.note ? (
          <p className="text-xs text-muted-foreground">{selected.note}</p>
        ) : null}
        {selected && !selected.loaded ? (
          <p className="text-xs text-amber-600">
            This kind is not currently loaded by the engine. Save the
            camera and restart the engine to materialise its inference
            layer; otherwise the camera will fall back to the default.
          </p>
        ) : null}
      </div>

      {kind ? (
        <div className="rounded-md border border-border/60 bg-muted/20 p-3 space-y-3">
          <p className="text-xs text-muted-foreground">
            Leave any field blank to inherit the engine default. The
            router dedups inference layers by <em>(kind, input size)</em>,
            so structural fields (size, pack path) only take effect when
            the engine restarts — pick a size that the kind's model
            registry actually ships.
          </p>

          <div className="space-y-2">
            <Label htmlFor="cam-model-size">Input size</Label>
            {showSizePicker ? (
              <>
                <select
                  id="cam-model-size"
                  className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
                  value={value?.input_width ? String(value.input_width) : ""}
                  onChange={(e) => {
                    const raw = e.target.value;
                    if (raw === "") {
                      // Clear all three coupled fields atomically.
                      patch({
                        preset: undefined,
                        input_width: undefined,
                        input_height: undefined,
                      });
                      return;
                    }
                    const n = Number(raw);
                    patch({
                      preset: raw,
                      input_width: n,
                      input_height: n,
                    });
                  }}
                >
                  <option value="">(use tier default)</option>
                  {availableSizes.map((sz) => (
                    <option key={sz} value={String(sz)}>
                      {describeSize(sz)}
                    </option>
                  ))}
                </select>
                <p className="text-xs text-muted-foreground">
                  The router picks the matching
                  <code className="mx-1 font-mono">{kind}_&lt;size&gt;.onnx</code>
                  from the pack at boot. Only sizes the pack actually ships
                  for this kind are listed — a mismatched size would silently
                  CPU-fall-back on Intel NPU hardware (engine v0.1.22 hard
                  fails on missing per-size files instead).
                </p>
              </>
            ) : fixedSize !== undefined ? (
              <p className="text-xs text-muted-foreground">
                Fixed at <code className="font-mono">{fixedSize} × {fixedSize}</code>
                {" — "}
                <code className="font-mono">{kind}</code> ships exactly one
                per-size variant today. Multi-size export is on the M3.4
                roadmap (prompt-set rework).
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                <code className="font-mono">{kind}</code> takes no detector
                input size — engine defaults apply.
              </p>
            )}
          </div>

          <div className="space-y-2">
            <Label htmlFor="cam-model-pack">Pack path</Label>
            <Input
              id="cam-model-pack"
              value={value?.pack_path ?? ""}
              onChange={(e) => patch({ pack_path: e.target.value })}
              placeholder="/abs/path/to/model-pack/"
              className="font-mono"
            />
            <p className="text-xs text-muted-foreground">
              Directory containing <code className="font-mono">models-manifest.json</code>
              and the <code className="font-mono">yolo26n_&lt;size&gt;.onnx</code>
              files. Leave blank to use the engine's default pack.
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="cam-model-score">Score threshold</Label>
            <Input
              id="cam-model-score"
              type="number"
              min={0}
              max={1}
              step={0.05}
              value={value?.score_threshold ?? ""}
              onChange={(e) =>
                patch({ score_threshold: num(e.target.value) })
              }
              placeholder="0.25"
            />
            <p className="text-xs text-muted-foreground">
              Applied live by the rule layer — no engine restart needed.
            </p>
          </div>

          {kind === "yoloe_promptfree" ? (
            <div className="space-y-2">
              <Label htmlFor="cam-model-topk">Top-K (prompt-free only)</Label>
              <Input
                id="cam-model-topk"
                type="number"
                min={0}
                step={1}
                value={value?.top_k ?? ""}
                onChange={(e) => patch({ top_k: num(e.target.value) })}
                placeholder="unbounded"
              />
              <p className="text-xs text-muted-foreground">
                Per-frame cap on detections after NMS. Blank keeps every
                detection.
              </p>
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Prompts chip input + datalist autocomplete.
// ---------------------------------------------------------------------------

function PromptsField({
  prompts,
  modelKind,
  onChange,
}: {
  prompts: string[];
  modelKind: string | undefined;
  onChange: (p: string[]) => void;
}) {
  const [value, setValue] = useState("");
  const catalog = useQuery({
    queryKey: ["models", "prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 5 * 60_000,
  });
  const suggestions = useMemo(
    () => buildPromptSuggestions(catalog.data, modelKind),
    [catalog.data, modelKind],
  );

  const add = (raw: string) => {
    const v = raw.trim();
    if (!v) return;
    if (prompts.includes(v)) return;
    onChange([...prompts, v]);
    setValue("");
  };

  return (
    <div className="space-y-2">
      <Label htmlFor="cam-prompt-input">Prompts</Label>
      <div className="flex flex-wrap gap-1">
        {prompts.length === 0 ? (
          <span className="text-xs text-muted-foreground">
            No prompts — detector emits all labels.
          </span>
        ) : (
          prompts.map((p) => (
            <span
              key={p}
              className="inline-flex items-center gap-1 rounded-md border border-border bg-muted/30 px-2 py-0.5 text-xs"
            >
              {p}
              <button
                type="button"
                onClick={() =>
                  onChange(prompts.filter((x) => x !== p))
                }
                className="rounded-sm hover:bg-muted"
                aria-label={`Remove ${p}`}
              >
                <X className="h-3 w-3" />
              </button>
            </span>
          ))
        )}
      </div>
      <div className="flex gap-2">
        <Input
          id="cam-prompt-input"
          list="cam-prompt-suggestions"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              add(value);
            }
          }}
          placeholder="excavator"
        />
        <Button type="button" variant="outline" onClick={() => add(value)}>
          Add
        </Button>
      </div>
      <datalist id="cam-prompt-suggestions">
        {suggestions.map((s) => (
          <option key={s} value={s} />
        ))}
      </datalist>
    </div>
  );
}

function buildPromptSuggestions(
  catalog: ModelPromptsCatalog | undefined,
  modelKind: string | undefined,
): string[] {
  if (!catalog) return [];
  const set = new Set<string>();
  const entries: ModelPromptsEntry[] = catalog.kinds ?? [];
  for (const entry of entries) {
    if (modelKind && entry.kind !== modelKind) continue;
    for (const label of entry.prompts ?? []) set.add(label);
  }
  // Fallback: if no model-kind filter matched anything, show everything
  // we know about so the operator still gets autocomplete hints.
  if (set.size === 0 && modelKind) {
    for (const entry of entries) {
      for (const label of entry.prompts ?? []) set.add(label);
    }
  }
  return Array.from(set).sort();
}

// ---------------------------------------------------------------------------
// Camera visual prompts editor (attach / detach).
// ---------------------------------------------------------------------------

function CameraVisualPromptsEditor({ cameraId }: { cameraId: string }) {
  const qc = useQueryClient();
  const attachmentsQuery = useQuery({
    queryKey: ["cameras", cameraId, "visual-prompts"],
    queryFn: () => listCameraVisualPrompts(cameraId),
  });
  const promptsQuery = useQuery({
    queryKey: ["visual-prompts", "list"],
    queryFn: listVisualPrompts,
  });

  const attachMutation = useMutation({
    mutationFn: (vpId: string) => attachVisualPrompt(cameraId, vpId),
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: ["cameras", cameraId, "visual-prompts"],
      }),
  });
  const detachMutation = useMutation({
    mutationFn: (vpId: string) => detachVisualPrompt(cameraId, vpId),
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: ["cameras", cameraId, "visual-prompts"],
      }),
  });

  const attached = attachmentsQuery.data ?? [];
  const attachedIds = new Set(attached.map((a) => a.visual_prompt_id));
  const available = (promptsQuery.data ?? []).filter(
    (vp) => !attachedIds.has(vp.id),
  );

  return (
    <div className="space-y-3">
      <div>
        <Label className="text-xs text-muted-foreground">Attached</Label>
        {attached.length === 0 ? (
          <p className="text-xs text-muted-foreground">None attached.</p>
        ) : (
          <ul className="space-y-1">
            {attached.map((a) => (
              <li
                key={a.visual_prompt_id}
                className="flex items-center justify-between rounded-md border border-border bg-muted/20 px-2 py-1 text-xs"
              >
                <span>
                  <span className="font-mono">{a.visual_prompt_id}</span> ·{" "}
                  <span className="text-muted-foreground">{a.label}</span>
                </span>
                <button
                  type="button"
                  onClick={() => detachMutation.mutate(a.visual_prompt_id)}
                  className="rounded-sm p-1 hover:bg-muted"
                  aria-label="Detach"
                >
                  <X className="h-3 w-3" />
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div>
        <Label className="text-xs text-muted-foreground">Available</Label>
        {available.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            No more visual prompts to attach.
          </p>
        ) : (
          <ul className="space-y-1">
            {available.map((vp) => (
              <li
                key={vp.id}
                className="flex items-center justify-between rounded-md border border-border px-2 py-1 text-xs"
              >
                <span>
                  <span className="font-mono">{vp.id}</span> ·{" "}
                  <span className="text-muted-foreground">{vp.name}</span>
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => attachMutation.mutate(vp.id)}
                >
                  Attach
                </Button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Discovery sheet — ONVIF + CIDR scan.
// ---------------------------------------------------------------------------

function DiscoverySheet({
  onClose,
  onAdd,
}: {
  onClose: () => void;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
  ) => void;
}) {
  const [mode, setMode] = useState<"onvif" | "scan">("onvif");
  // Credentials are global to the discovery session: nearly every
  // operator deploys cameras with one shared admin account, and
  // typing creds per-device after a /24 scan finds 20 IPs is
  // miserable. The same (username, password) is sent with every
  // Probe and embedded in every camera URL on Add.
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");

  return (
    <Sheet
      open
      onClose={onClose}
      title="Discover cameras"
      description="Find devices via ONVIF multicast or by scanning a CIDR range."
      width="max-w-3xl"
    >
      <div className="border-b border-border px-5 py-4">
        <Label className="text-xs font-semibold uppercase text-muted-foreground">
          Camera credentials
        </Label>
        <p className="mt-1 text-xs text-muted-foreground">
          Used for every probe and embedded in the camera URL on Add.
          Leave blank for cameras without authentication.
        </p>
        <div className="mt-2 grid grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="discovery-username" className="text-xs">
              Username
            </Label>
            <Input
              id="discovery-username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="admin"
              autoComplete="username"
              className="h-8 font-mono text-xs"
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="discovery-password" className="text-xs">
              Password
            </Label>
            <Input
              id="discovery-password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
              className="h-8 font-mono text-xs"
            />
          </div>
        </div>
      </div>
      <div className="border-b border-border px-5">
        <div className="flex gap-1 py-3">
          <button
            type="button"
            onClick={() => setMode("onvif")}
            className={`rounded-md px-3 py-1 text-sm ${
              mode === "onvif"
                ? "bg-primary/10 text-primary"
                : "text-muted-foreground hover:bg-muted"
            }`}
          >
            ONVIF multicast
          </button>
          <button
            type="button"
            onClick={() => setMode("scan")}
            className={`rounded-md px-3 py-1 text-sm ${
              mode === "scan"
                ? "bg-primary/10 text-primary"
                : "text-muted-foreground hover:bg-muted"
            }`}
          >
            CIDR scan
          </button>
        </div>
      </div>
      {mode === "onvif" ? (
        <DiscoveryRunner
          kind="onvif"
          onAdd={onAdd}
          username={username}
          password={password}
        />
      ) : (
        <DiscoveryRunner
          kind="scan"
          onAdd={onAdd}
          username={username}
          password={password}
        />
      )}
    </Sheet>
  );
}

function DiscoveryRunner({
  kind,
  onAdd,
  username,
  password,
}: {
  kind: "onvif" | "scan";
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
  ) => void;
  username: string;
  password: string;
}) {
  const [cidr, setCidr] = useState("");
  const [ports, setPorts] = useState("554,80,8080");
  const [confirm, setConfirm] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [startError, setStartError] = useState<string | null>(null);

  const startMutation = useMutation({
    mutationFn: async () => {
      if (kind === "onvif") {
        return startOnvifDiscovery();
      }
      const portsList = ports
        .split(",")
        .map((p) => Number.parseInt(p.trim(), 10))
        .filter((n) => Number.isFinite(n) && n > 0);
      return startCidrScan({
        cidr: cidr.trim(),
        ports: portsList.length ? portsList : undefined,
        confirm: confirm || undefined,
      });
    },
    onSuccess: (s) => {
      setSessionId(s.session_id);
      setStartError(null);
    },
    onError: (e: unknown) =>
      setStartError(e instanceof Error ? e.message : String(e)),
  });

  const sessionQuery = useQuery<DiscoverySessionView>({
    queryKey: ["discovery", "session", sessionId],
    queryFn: () => getDiscoverySession(sessionId!),
    enabled: !!sessionId,
    refetchInterval: (q) =>
      q.state.data?.state === "running" ? 1_000 : false,
  });

  const session = sessionQuery.data;

  return (
    <div className="space-y-5 p-5">
      {kind === "scan" ? (
        <div className="space-y-3 rounded-md border border-border p-4">
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="scan-cidr">CIDR</Label>
              <Input
                id="scan-cidr"
                value={cidr}
                onChange={(e) => setCidr(e.target.value)}
                placeholder="192.168.1.0/24"
                className="font-mono"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="scan-ports">Ports (comma-separated)</Label>
              <Input
                id="scan-ports"
                value={ports}
                onChange={(e) => setPorts(e.target.value)}
                className="font-mono"
              />
            </div>
          </div>
          <label className="inline-flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={confirm}
              onChange={(e) => setConfirm(e.target.checked)}
              className="h-3 w-3 rounded border-border"
            />
            Confirm scanning a network larger than /22
          </label>
        </div>
      ) : null}

      <div className="flex items-center gap-2">
        <Button
          onClick={() => startMutation.mutate()}
          disabled={
            startMutation.isPending ||
            (kind === "scan" && !cidr.trim())
          }
        >
          {startMutation.isPending
            ? "Starting…"
            : sessionId
              ? "Restart"
              : "Start"}
        </Button>
        {session ? (
          <Badge
            variant={
              session.state === "done"
                ? "success"
                : session.state === "error"
                  ? "destructive"
                  : "secondary"
            }
          >
            {session.state} · {session.scanned}
            {session.total_targets ? `/${session.total_targets}` : ""}
          </Badge>
        ) : null}
      </div>

      {startError ? (
        <p className="text-sm text-destructive">{startError}</p>
      ) : null}
      {session?.error ? (
        <p className="text-sm text-destructive">{session.error}</p>
      ) : null}

      {session ? (
        <DiscoveredList
          session={session}
          onAdd={onAdd}
          username={username}
          password={password}
        />
      ) : null}
    </div>
  );
}

function DiscoveredList({
  session,
  onAdd,
  username,
  password,
}: {
  session: DiscoverySessionView;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
  ) => void;
  username: string;
  password: string;
}) {
  if (session.found.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        {session.state === "running" ? "Scanning…" : "No devices found."}
      </p>
    );
  }

  return (
    <div className="space-y-2">
      <h4 className="text-xs font-semibold uppercase text-muted-foreground">
        Found {session.found.length}
      </h4>
      {session.found.map((d, i) => (
        <DiscoveredItem
          key={`${d.ip}:${d.port}:${i}`}
          sessionId={session.session_id}
          device={d}
          onAdd={onAdd}
          username={username}
          password={password}
        />
      ))}
    </div>
  );
}

function DiscoveredItem({
  sessionId,
  device,
  onAdd,
  username,
  password,
}: {
  sessionId: string;
  device: DiscoveredDevice;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
  ) => void;
  username: string;
  password: string;
}) {
  const [probing, setProbing] = useState(false);
  const [probeError, setProbeError] = useState<string | null>(null);
  // CIDR-scanned devices typically arrive with `rtsp_paths` empty
  // (port open, no enumeration). Default the picker to "/" so the
  // backend's parallel vendor-default path walker kicks in on
  // Probe; the operator can override at any time.
  const [picked, setPicked] = useState<string>(
    device.rtsp_paths[0] ?? "/",
  );
  // Streams returned by the most recent successful Probe. Each
  // entry is a path that DESCRIBE-200'd plus its primary video
  // codec / resolution. Populating this turns the path input
  // into a labeled dropdown ("/Streaming/Channels/101 — H264
  // 1920x1080" vs "/Streaming/Channels/102 — H264 640x360").
  const [streams, setStreams] = useState<ProbeRtspStream[]>([]);
  const [probedOk, setProbedOk] = useState(false);
  /// `true` IFF the populated `streams` came from ONVIF Media
  /// (`GetProfiles` + `GetStreamUri`) rather than the RTSP path
  /// sweep. Drives the "verified via ONVIF" badge so the
  /// operator can tell at a glance that the displayed paths are
  /// authoritative (vendor-reported) vs. heuristic.
  const [probedViaOnvif, setProbedViaOnvif] = useState(false);
  const effectivePath = picked.trim() || "/";

  const onProbe = async () => {
    setProbing(true);
    setProbeError(null);
    setProbedOk(false);
    setProbedViaOnvif(false);
    setStreams([]);

    // ONVIF Media path: when the device carries an XAddrs URL
    // AND the operator supplied credentials, ask the camera for
    // its own profile list. This is authoritative (vendor-built)
    // and replaces the brute-force path sweep entirely. We fall
    // back silently on any failure so a misconfigured ONVIF
    // service can't block the RTSP path that's worked for years.
    if (device.onvif_xaddrs && username && password) {
      try {
        const onvif = await probeOnvifStreams(sessionId, {
          xaddr: device.onvif_xaddrs,
          username,
          password,
        });
        if (onvif.ok && onvif.streams.length > 0) {
          // Map OnvifMediaStream \u2192 ProbeRtspStream so the
          // existing dropdown rendering keeps working. We strip
          // the scheme/host/port off the camera-reported URI
          // because openFromDiscovered() rebuilds the URL with
          // creds + the device's IP / rtsp_port. Keeping just
          // the path+query preserves vendor-specific query
          // params (e.g. Hikvision's ?transportmode=unicast)
          // while letting us inject the operator-supplied creds
          // and the locally-routable IP.
          const mapped: ProbeRtspStream[] = onvif.streams.map((s) => ({
            path: extractPathAndQuery(s.uri),
            codec: s.codec ?? null,
            resolution: s.resolution ?? null,
          }));
          setStreams(mapped);
          setPicked(mapped[0]!.path);
          setProbedOk(true);
          setProbedViaOnvif(true);
          setProbing(false);
          return;
        }
        // Empty / error \u2014 fall through to RTSP probe below.
        // Don't surface ONVIF errors yet: the RTSP fallback may
        // still succeed (and if it doesn't, its own error is
        // more actionable for the operator).
      } catch {
        // Network/parse error on the ONVIF endpoint itself \u2014
        // silently fall back. The engine handler returns 200
        // on every soft failure, so reaching this `catch`
        // means something is genuinely wrong with the request
        // path (auth header, missing route, etc.) and the
        // operator needs to know via the RTSP fallback path.
      }
    }

    try {
      const res = await probeRtsp(sessionId, {
        host: device.ip,
        // RTSP probes MUST hit the RTSP port, not the discovery
        // port. ONVIF-discovered devices report :80 in `port`
        // (the web service); sending RTSP DESCRIBE there gets
        // "405 Method Not Allowed" from the HTTP server for every
        // candidate path. Fall back to :554 when discovery didn't
        // confirm an RTSP port explicitly.
        port: device.rtsp_port ?? 554,
        // Send "/" to ask the backend to walk DEFAULT_PATHS in
        // parallel. When the user already picked a specific path
        // we send that verbatim so an explicit choice is honoured.
        path: effectivePath,
        username: username || undefined,
        password: password || undefined,
      });
      if (!res.ok) {
        if (res.status === 401 || res.status === 403) {
          setProbeError(
            username
              ? `auth rejected (status ${res.status}) — check credentials above`
              : `authentication required (status ${res.status}) — enter credentials above`,
          );
        } else if (res.status === 0) {
          setProbeError("no RTSP server answered (connect/timeout)");
        } else {
          setProbeError(`status ${res.status}`);
        }
        if (res.path) setPicked(res.path);
      } else {
        const probed = res.streams ?? [];
        setStreams(probed);
        if (probed[0]) {
          setPicked(probed[0].path);
        } else if (res.path) {
          setPicked(res.path);
        }
        setProbedOk(true);
      }
    } catch (e) {
      setProbeError(e instanceof Error ? e.message : String(e));
    } finally {
      setProbing(false);
    }
  };

  // Path options the dropdown shows. After a successful Probe
  // we prefer the probed streams (they carry codec/resolution
  // labels). Before that, fall back to whatever ONVIF reported
  // in `rtsp_paths`. A bare `<Input>` is only used as a last
  // resort for raw CIDR finds.
  const pathOptions: ProbeRtspStream[] =
    streams.length > 0
      ? streams
      : device.rtsp_paths.map((p) => ({ path: p }));

  return (
    <div className="rounded-md border border-border p-3 text-sm">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-mono">{device.ip}:{device.port}</span>
            <Badge variant="outline">{device.kind}</Badge>
            {probedOk ? (
              <Badge variant="success">
                {probedViaOnvif ? "verified · ONVIF" : "verified"}
              </Badge>
            ) : null}
            {device.codec ? (
              <Badge variant="outline" className="font-mono uppercase">
                {device.codec.replace("_plus", "+")}
              </Badge>
            ) : null}
          </div>
          <p className="mt-0.5 text-xs text-muted-foreground">
            {[device.vendor, device.model, device.firmware]
              .filter(Boolean)
              .join(" · ") || "—"}
          </p>
          <p className="mt-0.5 text-xs text-muted-foreground">
            RTSP on port {device.rtsp_port ?? 554}
            {device.rtsp_port == null ? " (default)" : ""}
          </p>
        </div>
        <Button
          size="sm"
          onClick={() => onAdd(device, effectivePath, username, password)}
          disabled={probing}
        >
          Add as camera
        </Button>
      </div>

      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Label className="text-xs text-muted-foreground">Path</Label>
        {pathOptions.length > 0 ? (
          <select
            className="h-7 max-w-md rounded-md border border-input bg-transparent px-2 text-xs font-mono"
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
          >
            {pathOptions.map((s) => (
              <option key={s.path} value={s.path}>
                {streamLabel(s)}
              </option>
            ))}
          </select>
        ) : (
          <Input
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
            placeholder="/"
            className="h-7 w-64 font-mono text-xs"
          />
        )}
        <Button
          size="sm"
          variant="outline"
          onClick={onProbe}
          disabled={probing}
        >
          {probing ? "Probing…" : streams.length > 0 ? "Re-probe" : "Probe"}
        </Button>
        {probeError ? (
          <span className="text-xs text-destructive">{probeError}</span>
        ) : null}
      </div>

      {streams.length > 1 ? (
        <p className="mt-1 text-xs text-muted-foreground">
          {streams.length} streams available — pick one before Add.
        </p>
      ) : null}
    </div>
  );
}

function streamLabel(s: ProbeRtspStream): string {
  const tail = [s.codec, s.resolution].filter(Boolean).join(" ");
  return tail ? `${s.path} — ${tail}` : s.path;
}

/// Pull just the `pathname + search` out of an ONVIF-reported
/// stream URI (e.g.
/// `"rtsp://192.168.1.66:554/Streaming/Channels/101?transportmode=unicast"`
/// \u2192 `"/Streaming/Channels/101?transportmode=unicast"`).
///
/// Used to translate `OnvifMediaStream.uri` into the
/// path-only shape `openFromDiscovered()` expects \u2014 it
/// rebuilds the URL with operator-supplied creds and the
/// locally-routable IP/port from `DiscoveredDevice`, which is
/// the right thing to do because the camera's reported URI may
/// embed a hostname or NAT-internal IP that the engine can't
/// reach.
///
/// Falls back to the verbatim input on parse failure (e.g.
/// vendors that return a non-URL path fragment). The downstream
/// composer will produce a malformed URL in that pathological
/// case, but it's no worse than what an operator would type
/// manually \u2014 they'll see the error on first stream-test.
// ---------------------------------------------------------------------------
// Zones editor — SVG overlay on the latest-snapshot JPEG.
//
// Storage shape matches `nexus-config::ZoneConfig`: polygon vertices live
// in normalised [0..1] coordinates against the source frame, NOT pixels.
// We render the SVG with `viewBox="0 0 1 1"` and `preserveAspectRatio="none"`
// so the polygon track the snapshot exactly regardless of container width.
// Vertex handles are positioned with CSS percents on top of the same
// container so they stay a constant pixel size (the SVG vector-effect
// keeps stroke width sane too).
//
// Editor states:
//   - idle: zones rendered as filled polygons; clicking a zone selects it.
//           Selected zone shows draggable vertex handles.
//   - drawing: cursor adds vertices on click; double-click closes the
//           polygon (>= 3 vertices required, self-intersection rejected);
//           ESC cancels.
//
// For brand-new cameras we don't have a snapshot yet (the engine hasn't
// produced a frame), so we render a grey placeholder with guidance to
// save first. The editor still works on an existing camera that hasn't
// produced a frame yet by falling back to the same placeholder on the
// img's `onError`.
// ---------------------------------------------------------------------------

type ZonesEditorMode =
  | { type: "idle" }
  | { type: "drawing"; points: Array<[number, number]> };

function ZonesEditor({
  cameraId,
  isNew,
  zones,
  onChange,
}: {
  cameraId: number;
  isNew: boolean;
  zones: ZoneConfig[];
  onChange: (zones: ZoneConfig[]) => void;
}) {
  const [mode, setMode] = useState<ZonesEditorMode>({ type: "idle" });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [drawError, setDrawError] = useState<string | null>(null);
  const [snapshotKey, setSnapshotKey] = useState(() => Date.now());
  const [snapshotFailed, setSnapshotFailed] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);
  // Drag state for an in-progress vertex move.
  const dragRef = useRef<{
    zoneId: string;
    vertex: number;
  } | null>(null);

  const snapshotUrl = !isNew
    ? `${latestFrameJpegUrl(String(cameraId))}?t=${snapshotKey}`
    : null;

  const selectedZone = useMemo(
    () => zones.find((z) => z.id === selectedId) ?? null,
    [zones, selectedId],
  );

  // Project a pointer event to a normalised [0..1] coordinate inside
  // the snapshot container. Returns null if the container isn't
  // mounted yet (defensive — should never happen).
  const projectEvent = useCallback(
    (clientX: number, clientY: number): [number, number] | null => {
      const el = containerRef.current;
      if (!el) return null;
      const rect = el.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return null;
      const x = clamp01((clientX - rect.left) / rect.width);
      const y = clamp01((clientY - rect.top) / rect.height);
      return [x, y];
    },
    [],
  );

  // Replace one vertex of a zone with a new normalised coord.
  const setVertex = useCallback(
    (zoneId: string, vertex: number, point: [number, number]) => {
      const next = zones.map((z) => {
        if (z.id !== zoneId) return z;
        const polygon = z.polygon.slice();
        polygon[vertex] = point;
        return { ...z, polygon };
      });
      onChange(next);
    },
    [zones, onChange],
  );

  const beginDrawing = () => {
    setMode({ type: "drawing", points: [] });
    setSelectedId(null);
    setDrawError(null);
  };

  const cancelDrawing = useCallback(() => {
    setMode({ type: "idle" });
    setDrawError(null);
  }, []);

  // Commit the in-progress polygon as a new zone. Returns false +
  // sets drawError on validation failure.
  const commitDrawing = useCallback(
    (points: Array<[number, number]>): boolean => {
      if (points.length < 3) {
        setDrawError("Need at least 3 vertices.");
        return false;
      }
      if (polygonHasSelfIntersection(points)) {
        setDrawError("Polygon edges must not cross.");
        return false;
      }
      const newZone: ZoneConfig = {
        id: genZoneId(zones),
        name: `Zone ${zones.length + 1}`,
        polygon: points,
        kind: "inclusion",
      };
      onChange([...zones, newZone]);
      setMode({ type: "idle" });
      setDrawError(null);
      setSelectedId(newZone.id);
      return true;
    },
    [zones, onChange],
  );

  // ESC cancels an in-progress draw.
  useEffect(() => {
    if (mode.type !== "drawing") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") cancelDrawing();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mode.type, cancelDrawing]);

  // Drag handlers (window-level so the user can drag past the
  // container edge without losing the move).
  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      const d = dragRef.current;
      if (!d) return;
      const p = projectEvent(e.clientX, e.clientY);
      if (!p) return;
      setVertex(d.zoneId, d.vertex, p);
    };
    const onUp = () => {
      dragRef.current = null;
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [projectEvent, setVertex]);

  // Snap distance (normalised [0..1] coords) for "is this click on top
  // of the starting vertex?". 3% ≈ ~30 px on a 1080-wide canvas, which
  // matches the visual radius of the vertex handle plus a forgiveness
  // halo. Keep in sync with the visual handle size in `VertexHandle`.
  const VERTEX_SNAP = 0.03;

  // Container click handler. Behaviour by mode:
  //   * `drawing`     → append a vertex; if the click is on the
  //                     starting vertex AND we already have ≥3 points,
  //                     close the polygon instead (standard polygon
  //                     editor UX).
  //   * `idle`        → NO-OP. Selection is sticky: clicking the
  //                     background does NOT deselect, because that
  //                     felt like "the editor closed on me" — vertex
  //                     handles vanished after a single stray click.
  //                     Operators deselect by clicking another zone,
  //                     or via the row picker below.
  const onContainerClick = (e: React.MouseEvent) => {
    if (mode.type !== "drawing") return;
    const p = projectEvent(e.clientX, e.clientY);
    if (!p) return;
    // Snap-to-close when the click lands on the starting vertex.
    if (mode.points.length >= 3) {
      const first = mode.points[0]!;
      if (Math.hypot(p[0] - first[0], p[1] - first[1]) < VERTEX_SNAP) {
        commitDrawing(mode.points);
        return;
      }
    }
    setMode({ ...mode, points: [...mode.points, p] });
    setDrawError(null);
  };

  // Double-click closes the polygon. We don't use the final mousedown
  // from the dblclick as a vertex — by the time React fires it the
  // single-click handler has already appended it, so we trim the last
  // point off if it's within ~2% of the previous one (anti-double-add).
  const onContainerDoubleClick = (e: React.MouseEvent) => {
    if (mode.type !== "drawing") return;
    e.preventDefault();
    let points = mode.points;
    if (points.length >= 2) {
      const last = points[points.length - 1]!;
      const prev = points[points.length - 2]!;
      if (Math.hypot(last[0] - prev[0], last[1] - prev[1]) < 0.02) {
        points = points.slice(0, -1);
      }
    }
    commitDrawing(points);
  };

  // Insert a new vertex into an existing zone at index `before`
  // (i.e. between vertex `before - 1` and `before`). Wired into the
  // midpoint handles rendered between consecutive vertices of the
  // selected zone — addresses "can't add a vertex to an existing
  // line".
  const insertVertex = useCallback(
    (zoneId: string, before: number, point: [number, number]) => {
      const next = zones.map((z) => {
        if (z.id !== zoneId) return z;
        const polygon = z.polygon.slice();
        polygon.splice(before, 0, point);
        return { ...z, polygon };
      });
      onChange(next);
      // Stage the just-inserted vertex for immediate drag — the
      // mousedown event that created the midpoint click is gone, so
      // the user has to grab the new handle explicitly, but at least
      // selection stays put.
    },
    [zones, onChange],
  );

  const deleteVertex = (zoneId: string, vertex: number) => {
    const target = zones.find((z) => z.id === zoneId);
    if (!target) return;
    if (target.polygon.length <= 3) {
      setDrawError("A polygon must keep at least 3 vertices.");
      return;
    }
    const next = zones.map((z) =>
      z.id === zoneId
        ? { ...z, polygon: z.polygon.filter((_, i) => i !== vertex) }
        : z,
    );
    onChange(next);
  };

  const deleteZone = (zoneId: string) => {
    onChange(zones.filter((z) => z.id !== zoneId));
    if (selectedId === zoneId) setSelectedId(null);
  };

  const updateZone = (zoneId: string, patch: Partial<ZoneConfig>) => {
    onChange(zones.map((z) => (z.id === zoneId ? { ...z, ...patch } : z)));
  };

  // Toolbar buttons differ between idle and drawing modes.
  const toolbar = (
    <div className="flex flex-wrap items-center gap-2">
      {mode.type === "drawing" ? (
        <>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={() => commitDrawing(mode.points)}
            disabled={mode.points.length < 3}
          >
            <Square className="mr-2 h-4 w-4" />
            Finish ({mode.points.length} pt)
          </Button>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            onClick={cancelDrawing}
          >
            Cancel
          </Button>
          <span className="text-xs text-muted-foreground">
            Click to add a vertex · click the first vertex (or double-click) to close · ESC to cancel
          </span>
        </>
      ) : (
        <>
          <Button
            type="button"
            size="sm"
            variant="outline"
            onClick={beginDrawing}
            disabled={isNew}
          >
            <Plus className="mr-2 h-4 w-4" />
            New zone
          </Button>
          {!isNew ? (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => {
                setSnapshotKey(Date.now());
                setSnapshotFailed(false);
              }}
              title="Reload snapshot"
            >
              <RefreshCw className="h-4 w-4" />
            </Button>
          ) : null}
        </>
      )}
    </div>
  );

  return (
    <div className="space-y-3">
      {toolbar}

      {drawError ? (
        <p className="rounded-md border border-destructive/50 bg-destructive/10 px-2 py-1 text-xs text-destructive">
          {drawError}
        </p>
      ) : null}

      {isNew ? (
        <div className="flex h-40 flex-col items-center justify-center gap-1 rounded-md border border-dashed border-border bg-muted/20 text-center text-xs text-muted-foreground">
          <p>Save the camera first.</p>
          <p>
            The polygon editor draws on the live snapshot — only
            available once the engine has produced a frame.
          </p>
        </div>
      ) : (
        <div
          ref={containerRef}
          className={`relative w-full overflow-hidden rounded-md border border-border bg-muted/30 ${
            mode.type === "drawing"
              ? "cursor-crosshair"
              : "cursor-default"
          }`}
          style={{ aspectRatio: "16 / 9" }}
          onClick={onContainerClick}
          onDoubleClick={onContainerDoubleClick}
          onContextMenu={(e) => {
            // Suppress the browser context menu on the canvas so
            // right-click-to-delete-vertex feels natural.
            e.preventDefault();
          }}
        >
          {snapshotUrl && !snapshotFailed ? (
            <img
              src={snapshotUrl}
              alt={`Snapshot for camera ${cameraId}`}
              className="pointer-events-none absolute inset-0 h-full w-full object-contain"
              draggable={false}
              onError={() => setSnapshotFailed(true)}
            />
          ) : (
            <div className="absolute inset-0 flex items-center justify-center text-xs text-muted-foreground">
              No snapshot yet — zones can still be drawn against an
              unscaled canvas.
            </div>
          )}

          <svg
            viewBox="0 0 1 1"
            preserveAspectRatio="none"
            className="absolute inset-0 h-full w-full"
          >
            {/* Existing zones, filled + stroked by kind. */}
            {zones.map((z) => {
              const isSel = z.id === selectedId;
              const c = zoneColor(z.kind);
              return (
                <polygon
                  key={z.id}
                  points={polygonAttr(z.polygon)}
                  fill={c.fill}
                  fillOpacity={isSel ? 0.35 : 0.2}
                  stroke={c.stroke}
                  strokeWidth={isSel ? 2.5 : 1.5}
                  vectorEffect="non-scaling-stroke"
                  style={{
                    cursor: mode.type === "drawing" ? "crosshair" : "pointer",
                  }}
                  onClick={(e) => {
                    if (mode.type === "drawing") return;
                    e.stopPropagation();
                    setSelectedId(z.id);
                  }}
                />
              );
            })}

            {/* Clickable edge overlays for the selected zone — clicking
                anywhere along a polygon edge inserts a vertex at the
                click point (not just at the midpoint). Stroke is
                transparent but ~12px wide so the hit-test is generous;
                vectorEffect keeps the width in screen px regardless
                of container size. Only rendered when a zone is
                selected and we're not currently drawing a new one. */}
            {selectedZone && mode.type !== "drawing"
              ? selectedZone.polygon.map(([x, y], i) => {
                  const next =
                    selectedZone.polygon[
                      (i + 1) % selectedZone.polygon.length
                    ]!;
                  return (
                    <line
                      key={`edge-${selectedZone.id}-${i}`}
                      x1={x}
                      y1={y}
                      x2={next[0]}
                      y2={next[1]}
                      stroke="rgba(0,0,0,0)"
                      strokeWidth={12}
                      strokeLinecap="round"
                      vectorEffect="non-scaling-stroke"
                      style={{ cursor: "copy" }}
                      onClick={(e) => {
                        e.stopPropagation();
                        const p = projectEvent(e.clientX, e.clientY);
                        if (!p) return;
                        insertVertex(selectedZone.id, i + 1, p);
                      }}
                    />
                  );
                })
              : null}

            {/* In-progress polyline while drawing. */}
            {mode.type === "drawing" && mode.points.length > 0 ? (
              <polyline
                points={polygonAttr(mode.points)}
                fill="none"
                stroke="#38bdf8"
                strokeWidth={1.5}
                strokeDasharray="4 3"
                vectorEffect="non-scaling-stroke"
              />
            ) : null}

            {/* Preview the closing edge once we have a valid polygon
                (≥3 vertices). Faint dashed line back to the first
                vertex — makes "this will be a triangle/quad if you
                close now" obvious without committing. Addresses
                "the polygon doesn't auto-complete after three
                vertices". */}
            {mode.type === "drawing" && mode.points.length >= 3 ? (
              <line
                x1={mode.points[mode.points.length - 1]![0]}
                y1={mode.points[mode.points.length - 1]![1]}
                x2={mode.points[0]![0]}
                y2={mode.points[0]![1]}
                stroke="#38bdf8"
                strokeWidth={1}
                strokeDasharray="2 3"
                strokeOpacity={0.6}
                vectorEffect="non-scaling-stroke"
              />
            ) : null}
          </svg>

          {/* Vertex handles. Rendered as HTML so they stay a constant
              pixel size and accept right-click for delete. */}
          {selectedZone && mode.type !== "drawing"
            ? selectedZone.polygon.map(([x, y], i) => (
                <VertexHandle
                  key={`${selectedZone.id}-${i}`}
                  x={x}
                  y={y}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    dragRef.current = {
                      zoneId: selectedZone.id,
                      vertex: i,
                    };
                  }}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    deleteVertex(selectedZone.id, i);
                  }}
                />
              ))
            : null}

          {/* Midpoint handles — click between two vertices to insert
              a new one. Smaller + ghosted so they don't compete with
              the vertex handles visually. Only rendered when a zone
              is selected and we're not currently drawing a new one. */}
          {selectedZone && mode.type !== "drawing"
            ? selectedZone.polygon.map(([x, y], i) => {
                const next =
                  selectedZone.polygon[
                    (i + 1) % selectedZone.polygon.length
                  ]!;
                const mx = (x + next[0]) / 2;
                const my = (y + next[1]) / 2;
                return (
                  <MidpointHandle
                    key={`mp-${selectedZone.id}-${i}`}
                    x={mx}
                    y={my}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      insertVertex(selectedZone.id, i + 1, [mx, my]);
                    }}
                  />
                );
              })
            : null}

          {/* Drawing-mode vertex previews (not draggable, not deletable
              individually — backspace would be nice but ESC cancels
              the whole shape which is what most users want). */}
          {mode.type === "drawing"
            ? mode.points.map(([x, y], i) => (
                <VertexHandle
                  key={`draw-${i}`}
                  x={x}
                  y={y}
                  variant="ghost"
                />
              ))
            : null}
        </div>
      )}

      {zones.length === 0 ? (
        <p className="text-xs text-muted-foreground">No zones defined.</p>
      ) : (
        <ul className="space-y-2">
          {zones.map((z) => (
            <li
              key={z.id}
              className={`flex items-center gap-2 rounded-md border px-2 py-2 ${
                z.id === selectedId
                  ? "border-primary/60 bg-primary/5"
                  : "border-border bg-muted/20"
              }`}
            >
              <button
                type="button"
                className="h-3 w-3 shrink-0 rounded-sm border border-border"
                style={{ background: zoneColor(z.kind).stroke }}
                onClick={() => setSelectedId(z.id)}
                title="Select zone"
                aria-label={`Select ${z.name}`}
              />
              <Input
                value={z.name}
                onChange={(e) => updateZone(z.id, { name: e.target.value })}
                className="h-8"
              />
              <select
                value={z.kind}
                onChange={(e) =>
                  updateZone(z.id, { kind: e.target.value as ZoneKind })
                }
                className="h-8 rounded-md border border-input bg-transparent px-2 text-sm"
              >
                <option value="inclusion">inclusion</option>
                <option value="exclusion">exclusion</option>
                <option value="dwell">dwell</option>
              </select>
              <span className="font-mono text-[10px] text-muted-foreground">
                {z.polygon.length} pt
              </span>
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={() => deleteZone(z.id)}
                title="Delete zone"
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function VertexHandle({
  x,
  y,
  variant = "solid",
  onMouseDown,
  onContextMenu,
}: {
  x: number;
  y: number;
  variant?: "solid" | "ghost";
  onMouseDown?: (e: React.MouseEvent) => void;
  onContextMenu?: (e: React.MouseEvent) => void;
}) {
  return (
    <div
      role="button"
      aria-label="Polygon vertex"
      className={`absolute h-3 w-3 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 ${
        variant === "ghost"
          ? "border-sky-400 bg-sky-400/40"
          : "border-primary bg-background"
      } cursor-grab active:cursor-grabbing`}
      style={{ left: `${x * 100}%`, top: `${y * 100}%` }}
      onMouseDown={onMouseDown}
      onContextMenu={onContextMenu}
    />
  );
}

// Edge-midpoint handle. Smaller, ghosted, hollow centre so it reads
// as "insert here" rather than "drag this". MouseDown inserts a new
// vertex at the midpoint of the parent edge — the user can then grab
// the freshly-promoted real vertex and drag it where they want.
function MidpointHandle({
  x,
  y,
  onMouseDown,
}: {
  x: number;
  y: number;
  onMouseDown?: (e: React.MouseEvent) => void;
}) {
  return (
    <div
      role="button"
      aria-label="Insert vertex"
      title="Click to insert a vertex on this edge"
      className="absolute h-2 w-2 -translate-x-1/2 -translate-y-1/2 cursor-copy rounded-full border border-primary/60 bg-background/50 opacity-60 transition-opacity hover:opacity-100"
      style={{ left: `${x * 100}%`, top: `${y * 100}%` }}
      onMouseDown={onMouseDown}
    />
  );
}

// Pure helpers --------------------------------------------------------------

function clamp01(v: number): number {
  if (v < 0) return 0;
  if (v > 1) return 1;
  return v;
}

function polygonAttr(points: Array<[number, number]>): string {
  return points.map(([x, y]) => `${x},${y}`).join(" ");
}

function zoneColor(kind: ZoneKind): { fill: string; stroke: string } {
  // Tailwind-ish but inline so we don't fight the JIT.
  switch (kind) {
    case "exclusion":
      return { fill: "#ef4444", stroke: "#ef4444" }; // red-500
    case "dwell":
      return { fill: "#f59e0b", stroke: "#f59e0b" }; // amber-500
    case "inclusion":
    default:
      return { fill: "#22c55e", stroke: "#22c55e" }; // green-500
  }
}

function genZoneId(existing: ZoneConfig[]): string {
  const used = new Set(existing.map((z) => z.id));
  for (let i = 1; i < 10_000; i += 1) {
    const candidate = `zone-${i}`;
    if (!used.has(candidate)) return candidate;
  }
  // Pathological — fall back to a timestamp tag.
  return `zone-${Date.now()}`;
}

// Segment / polygon geometry. The engine accepts self-intersecting
// polygons silently but the tracker treats them as undefined behaviour
// (per M_ADMIN Phase 2 spec), so we reject them up front.

function polygonHasSelfIntersection(
  points: Array<[number, number]>,
): boolean {
  const n = points.length;
  if (n < 4) return false;
  for (let i = 0; i < n; i += 1) {
    const a1 = points[i]!;
    const a2 = points[(i + 1) % n]!;
    for (let j = i + 1; j < n; j += 1) {
      // Skip adjacent edges (they share an endpoint by construction).
      if (j === i) continue;
      if ((j + 1) % n === i) continue;
      if (j === (i + 1) % n) continue;
      const b1 = points[j]!;
      const b2 = points[(j + 1) % n]!;
      if (segmentsIntersect(a1, a2, b1, b2)) return true;
    }
  }
  return false;
}

function segmentsIntersect(
  p1: [number, number],
  p2: [number, number],
  p3: [number, number],
  p4: [number, number],
): boolean {
  const d1 = cross(p4[0] - p3[0], p4[1] - p3[1], p1[0] - p3[0], p1[1] - p3[1]);
  const d2 = cross(p4[0] - p3[0], p4[1] - p3[1], p2[0] - p3[0], p2[1] - p3[1]);
  const d3 = cross(p2[0] - p1[0], p2[1] - p1[1], p3[0] - p1[0], p3[1] - p1[1]);
  const d4 = cross(p2[0] - p1[0], p2[1] - p1[1], p4[0] - p1[0], p4[1] - p1[1]);
  if (((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) &&
      ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0))) {
    return true;
  }
  // Collinear-on-segment cases are vanishingly rare in operator input;
  // treat them as non-intersecting and let the engine deal with them.
  return false;
}

function cross(ax: number, ay: number, bx: number, by: number): number {
  return ax * by - ay * bx;
}

// ---------------------------------------------------------------------------

function extractPathAndQuery(uri: string): string {
  try {
    const u = new URL(uri);
    return `${u.pathname}${u.search}`;
  } catch {
    return uri;
  }
}
