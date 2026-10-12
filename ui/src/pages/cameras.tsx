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
  Camera as CameraIcon,
  Pencil,
  Plus,
  Radar,
  Trash2,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import {
  deleteCamera,
} from "@/api/config";
import {
  listCameraStats,
  listCameras,
  type CameraFrameStats,
} from "@/api/system";
import type {
  CameraConfig,
  DiscoveredDevice,
} from "@/api/types";
import { DecodeVerdictChip } from "@/components/DecodeVerdictChip";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { FleetManagedBadge } from "@/components/fleet-managed-badge";
import {
  analysisStreamLabel,
  decodeSummaryText,
} from "@/lib/decodeCapacity";
import { CameraEditor } from "@/components/cameras/camera-editor";
import { DiscoverySheet } from "@/components/cameras/discovery-sheet";

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
  // One batch call for the whole table (SPEC-069 Phase 1) rather than one
  // per row. `isSuccess` is passed down so a pending or failed read renders
  // "Not read" instead of an all-clear.
  const statsQuery = useQuery({
    queryKey: ["cameras", "stats", "all"],
    queryFn: listCameraStats,
    refetchInterval: 5_000,
  });
  const statsById = useMemo(() => {
    const m = new Map<number, CameraFrameStats>();
    for (const s of statsQuery.data ?? []) m.set(s.camera_id, s);
    return m;
  }, [statsQuery.data]);

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
    nameOverride?: string,
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
      name:
        nameOverride?.trim() ||
        (device.vendor
          ? `${device.vendor} ${device.model ?? ""}`.trim()
          : device.ip),
      url: rtsp,
      // Seed the codec from discovery so the operator doesn't
      // have to re-pick. `null` here means "let the admin API
      // autodetect" — if the probe also failed during discovery
      // we leave it unset rather than guessing H.264.
      codec: device.codec ?? null,
      // Persist the camera's ONVIF device-service URL + the same
      // credentials so the PTZ / imaging / device-control surface
      // works without re-entry (the engine resolves `cam.onvif`
      // per call; an unset endpoint makes every device-control
      // action 400 with "camera has no ONVIF endpoint configured").
      // Only ONVIF-discovered devices advertise an XAddrs URL;
      // CIDR-scanned ones leave `onvif` unset.
      onvif: device.onvif_xaddrs
        ? {
            endpoint: device.onvif_xaddrs,
            username: username || undefined,
            password: password || undefined,
          }
        : undefined,
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
        <div className="flex flex-wrap items-center gap-2">
          <FleetManagedBadge category="text_prompts" />
          <FleetManagedBadge category="detector_config" />
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
                    <th className="px-3 py-2 text-left">Decode</th>
                    <th className="px-3 py-2 text-left">Status</th>
                    <th className="px-3 py-2 text-right">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {cameras.map((cam) => (
                    <CameraRow
                      key={String(cam.id)}
                      camera={cam}
                      stats={statsById.get(Number(cam.id))}
                      statsRead={statsQuery.isSuccess}
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
  stats,
  statsRead,
  onEdit,
}: {
  camera: CameraConfig;
  stats: CameraFrameStats | undefined;
  statsRead: boolean;
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
  // A row the batch read covers but has no entry for is a camera the
  // supervisor has not produced a frame for yet — distinct from an
  // unread list, and still not an all-clear.
  const analysis = analysisStreamLabel(stats?.analysis_stream);

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
        <div className="flex flex-col items-start gap-1">
          <DecodeVerdictChip
            verdict={stats?.decode_verdict}
            read={statsRead && stats !== undefined}
            title={decodeSummaryText(stats?.decode_summary, statsRead && stats !== undefined)}
          />
          {analysis ? (
            <span className="text-[11px] text-muted-foreground">
              Analysis: {analysis}
            </span>
          ) : null}
        </div>
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

