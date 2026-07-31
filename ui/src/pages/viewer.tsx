// Live multi-camera viewer.
//
// Each tile polls the JPEG snapshot + frame metadata every 2s and draws
// bbox overlays + zone polygons on a `<canvas>` layered on top of the
// `<img>`. Click a tile to expand to a fullscreen modal.
//
// Coordinate systems:
//   - Supervisor (analysis) frame is per-camera, 16:9 on the native
//     ladder (512×288 / 1024×576 / 1536×864), optionally a
//     larger rung than the model input (CameraBehavior.supervisor_width).
//     Every bbox in `latest.json` is in those units.
//   - `FrameMetadata.width` / `.height` carry the actual source dims for
//     a given snapshot — always trust these, never hardcode 960×540.
//   - Zone polygons are normalised [0..1] — multiply by canvas dims.
//
// Network: snapshot URL is cache-busted via `?t=` so the browser doesn't
// pin the first JPEG. Metadata poll uses TanStack Query so multiple
// tiles share a single in-flight request per camera.

import { useQuery } from "@tanstack/react-query";
import { Camera, Eraser, Maximize2, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import {
  clearStaticAnchors,
  getLatestFrameMeta,
  getStaticAnchors,
  latestFrameJpegUrl,
  listCameras,
} from "@/api/system";
import type {
  CameraConfig,
  FrameMetadata,
  StaticAnchor,
  ZoneConfig,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { ageMs, formatAgo } from "@/lib/format";
import { useQueryClient } from "@tanstack/react-query";

const POLL_MS = 2000;
const STALE_MS = 5000;
// Anchors mutate at a slower cadence (frames-of-dwell, not
// per-frame), so polling every snapshot tick is wasteful. 5s is
// a comfortable middle ground.
const ANCHOR_POLL_MS = 5000;

export function ViewerPage() {
  const camerasQuery = useQuery({
    queryKey: ["cameras", "list"],
    queryFn: listCameras,
    staleTime: 30_000,
  });

  const [expandedId, setExpandedId] = useState<string | null>(null);

  // Depend on the query's `.data` directly (stable reference between
  // refetches) rather than the `cameras` fallback (a new `[]` every
  // render would trip react-hooks/exhaustive-deps and break memoization).
  const cameraList = camerasQuery.data;
  const cameras = cameraList ?? [];
  const expanded = useMemo(
    () => cameraList?.find((c) => String(c.id) === expandedId) ?? null,
    [cameraList, expandedId],
  );

  return (
    <div className="space-y-6">
      <header className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Viewer</h1>
          <p className="text-sm text-muted-foreground">
            Live camera tiles with detection and zone overlays. Click a tile
            to expand.
          </p>
        </div>
        <Badge variant="outline">
          {cameras.length} {cameras.length === 1 ? "camera" : "cameras"}
        </Badge>
      </header>

      {camerasQuery.isLoading ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[0, 1, 2, 3, 4, 5].map((i) => (
            <Skeleton key={i} className="aspect-video w-full" />
          ))}
        </div>
      ) : cameras.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
            <Camera className="h-8 w-8 opacity-50" />
            <p>No cameras configured.</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {cameras.map((cam) => (
            <ViewerTile
              key={String(cam.id)}
              camera={cam}
              onExpand={() => setExpandedId(String(cam.id))}
            />
          ))}
        </div>
      )}

      {expanded ? (
        <FullscreenViewer
          camera={expanded}
          onClose={() => setExpandedId(null)}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Grid tile.
// ---------------------------------------------------------------------------

function ViewerTile({
  camera,
  onExpand,
}: {
  camera: CameraConfig;
  onExpand: () => void;
}) {
  const cameraId = String(camera.id);
  const [bust, setBust] = useState(() => Date.now());

  useEffect(() => {
    const t = setInterval(() => setBust(Date.now()), POLL_MS);
    return () => clearInterval(t);
  }, []);

  const metaQuery = useQuery({
    queryKey: ["cameras", cameraId, "frames", "latest.json"],
    queryFn: () => getLatestFrameMeta(cameraId),
    refetchInterval: POLL_MS,
    refetchIntervalInBackground: false,
    retry: false,
  });

  const meta = metaQuery.data ?? null;
  const stale = meta ? ageMs(meta.captured_at) > STALE_MS : false;
  const zones = extractZones(camera);

  const anchorsQuery = useQuery({
    queryKey: ["cameras", cameraId, "static-anchors"],
    queryFn: () => getStaticAnchors(cameraId),
    refetchInterval: ANCHOR_POLL_MS,
    refetchIntervalInBackground: false,
    retry: false,
  });
  const anchors = anchorsQuery.data?.anchors ?? [];

  return (
    <Card
      className="group cursor-pointer overflow-hidden transition hover:border-primary/60"
      onClick={onExpand}
    >
      <div className="relative aspect-video w-full bg-muted/40">
        <img
          src={`${latestFrameJpegUrl(cameraId)}?t=${bust}`}
          alt={camera.name ?? cameraId}
          className="h-full w-full object-cover"
          onError={(e) => {
            (e.target as HTMLImageElement).style.opacity = "0.2";
          }}
        />
        <FrameOverlay meta={meta} zones={zones} anchors={anchors} />
        <div className="absolute right-2 top-2 flex gap-2">
          {stale ? <Badge variant="destructive">STALLED</Badge> : null}
          <button
            type="button"
            className="rounded-md bg-background/70 p-1.5 text-foreground opacity-0 transition group-hover:opacity-100"
            onClick={(e) => {
              e.stopPropagation();
              onExpand();
            }}
            aria-label="Expand"
          >
            <Maximize2 className="h-4 w-4" />
          </button>
        </div>
        <div className="absolute inset-x-0 bottom-0 flex items-center justify-between bg-gradient-to-t from-background/90 to-transparent px-3 py-2 text-xs">
          <span className="font-medium">{camera.name ?? cameraId}</span>
          <span className="text-muted-foreground">
            {meta ? formatAgo(meta.captured_at) : "—"}
          </span>
        </div>
      </div>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Fullscreen modal.
// ---------------------------------------------------------------------------

function FullscreenViewer({
  camera,
  onClose,
}: {
  camera: CameraConfig;
  onClose: () => void;
}) {
  const cameraId = String(camera.id);
  const [bust, setBust] = useState(() => Date.now());
  const [clearing, setClearing] = useState(false);
  const [clearError, setClearError] = useState<string | null>(null);
  const queryClient = useQueryClient();

  useEffect(() => {
    const t = setInterval(() => setBust(Date.now()), POLL_MS);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const metaQuery = useQuery({
    queryKey: ["cameras", cameraId, "frames", "latest.json"],
    queryFn: () => getLatestFrameMeta(cameraId),
    refetchInterval: POLL_MS,
    retry: false,
  });

  const meta = metaQuery.data ?? null;
  const zones = extractZones(camera);

  const anchorsQuery = useQuery({
    queryKey: ["cameras", cameraId, "static-anchors"],
    queryFn: () => getStaticAnchors(cameraId),
    refetchInterval: ANCHOR_POLL_MS,
    retry: false,
  });
  const anchors = anchorsQuery.data?.anchors ?? [];

  const onClearAnchors = async () => {
    if (clearing) return;
    if (
      !confirm(
        `Clear all ${anchors.length} persisted static anchor(s) for "${camera.name ?? cameraId}"?\n\n` +
          "Vehicles still in view will re-promote naturally after the dwell window.",
      )
    ) {
      return;
    }
    setClearError(null);
    setClearing(true);
    try {
      await clearStaticAnchors(cameraId);
      // The supervisor wipes on its next frame; refetch shortly
      // after so the overlay catches up.
      setTimeout(() => {
        queryClient.invalidateQueries({
          queryKey: ["cameras", cameraId, "static-anchors"],
        });
      }, 500);
    } catch (e) {
      setClearError(e instanceof Error ? e.message : String(e));
    } finally {
      setClearing(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex flex-col bg-background/95 backdrop-blur"
      role="dialog"
      aria-modal="true"
    >
      <header className="flex items-center justify-between border-b border-border px-4 py-3">
        <div>
          <h2 className="text-lg font-semibold">{camera.name ?? cameraId}</h2>
          <p className="text-xs text-muted-foreground">
            {meta
              ? `${meta.width}x${meta.height} · ${meta.objects.length} object(s) · ${anchors.length} static · ${formatAgo(meta.captured_at)}`
              : "Loading…"}
            {clearError ? (
              <span className="ml-2 text-destructive">
                clear failed: {clearError}
              </span>
            ) : null}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={onClearAnchors}
            disabled={clearing || anchors.length === 0}
            title={
              anchors.length === 0
                ? "No anchors to clear"
                : "Wipe persisted static-object anchors for this camera"
            }
          >
            <Eraser className="mr-2 h-4 w-4" />
            {clearing ? "Clearing…" : "Clear anchors"}
          </Button>
          <button
            type="button"
            className="rounded-md p-2 hover:bg-muted"
            onClick={onClose}
            aria-label="Close"
          >
            <X className="h-5 w-5" />
          </button>
        </div>
      </header>
      <div className="relative min-h-0 flex-1 overflow-hidden">
        <img
          src={`${latestFrameJpegUrl(cameraId)}?t=${bust}`}
          alt={camera.name ?? cameraId}
          className="h-full w-full object-contain"
        />
        <FrameOverlay meta={meta} zones={zones} anchors={anchors} fit="contain" />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Canvas overlay — draws bboxes + zone polygons on top of the JPEG.
// ---------------------------------------------------------------------------

function FrameOverlay({
  meta,
  zones,
  anchors = [],
  fit = "cover",
}: {
  meta: FrameMetadata | null;
  zones: ZoneConfig[];
  anchors?: StaticAnchor[];
  fit?: "cover" | "contain";
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container) return;

    const draw = () => {
      const rect = container.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      canvas.width = rect.width * dpr;
      canvas.height = rect.height * dpr;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, rect.width, rect.height);

      // Source frame dims come from FrameMetadata (per-camera supervisor
      // frame); 960×540 is just the legacy default for cameras whose
      // detector input width is unset or hasn't been resolved yet.
      const sw = meta?.width ?? 960;
      const sh = meta?.height ?? 540;

      // Map source-frame coords -> visible canvas coords accounting for
      // object-fit. For object-cover the image fills the box and crops
      // the off-axis edges. For object-contain the image is letterboxed.
      const scale =
        fit === "cover"
          ? Math.max(rect.width / sw, rect.height / sh)
          : Math.min(rect.width / sw, rect.height / sh);
      const ox = (rect.width - sw * scale) / 2;
      const oy = (rect.height - sh * scale) / 2;
      const toX = (x: number) => ox + x * scale;
      const toY = (y: number) => oy + y * scale;

      // Zones first (under bboxes).
      for (const zone of zones) {
        const color =
          zone.kind === "exclusion"
            ? "rgba(239, 71, 111, 0.7)"
            : zone.kind === "dwell"
              ? "rgba(255, 209, 102, 0.8)"
              : "rgba(56, 225, 255, 0.7)";
        ctx.strokeStyle = color;
        ctx.fillStyle = color.replace(/[\d.]+\)/, "0.12)");
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        zone.polygon.forEach(([nx, ny], i) => {
          const x = toX(nx * sw);
          const y = toY(ny * sh);
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        });
        ctx.closePath();
        ctx.fill();
        ctx.stroke();
      }

      // Static-object anchors (under bboxes, over zones). Each anchor
      // is a ghost crosshair + label drawn at the persisted centroid.
      // We don't have width/height on disk (the registry only stores
      // (label, center_x, center_y)), so the marker is a fixed-radius
      // ring in screen-space — it stays readable at any zoom level.
      if (anchors.length > 0) {
        ctx.save();
        ctx.font = "11px ui-sans-serif, system-ui";
        ctx.lineWidth = 1.5;
        const RADIUS = 10;
        for (const a of anchors) {
          const cx = toX(a.center_x);
          const cy = toY(a.center_y);
          ctx.strokeStyle = "rgba(180, 180, 220, 0.85)";
          ctx.fillStyle = "rgba(180, 180, 220, 0.18)";
          // Ring.
          ctx.beginPath();
          ctx.arc(cx, cy, RADIUS, 0, Math.PI * 2);
          ctx.fill();
          ctx.stroke();
          // Crosshair.
          ctx.beginPath();
          ctx.moveTo(cx - RADIUS - 3, cy);
          ctx.lineTo(cx + RADIUS + 3, cy);
          ctx.moveTo(cx, cy - RADIUS - 3);
          ctx.lineTo(cx, cy + RADIUS + 3);
          ctx.stroke();
          // Label pill above the ring.
          const label = a.label;
          const pad = 3;
          const tw = ctx.measureText(label).width + pad * 2;
          const lx = cx - tw / 2;
          const ly = cy - RADIUS - 16;
          ctx.fillStyle = "rgba(40, 40, 60, 0.85)";
          ctx.fillRect(lx, ly, tw, 14);
          ctx.fillStyle = "rgba(220, 220, 240, 0.95)";
          ctx.fillText(label, lx + pad, ly + 10);
        }
        ctx.restore();
      }

      // Bboxes.
      if (meta) {
        ctx.font = "12px ui-sans-serif, system-ui";
        ctx.lineWidth = 2;
        for (const obj of meta.objects) {
          const x = toX(obj.bbox.x1);
          const y = toY(obj.bbox.y1);
          const w = (obj.bbox.x2 - obj.bbox.x1) * scale;
          const h = (obj.bbox.y2 - obj.bbox.y1) * scale;
          // Suppressed-as-static tracks draw with a dashed grey box
          // so the operator can see "the engine recognised this as
          // a known-static object" without it shouting like an alert.
          const isStatic = obj.attributes?.["tracker.is_static"] === true;
          if (isStatic) {
            ctx.save();
            ctx.setLineDash([4, 3]);
            ctx.strokeStyle = "rgba(180, 180, 220, 0.85)";
            ctx.strokeRect(x, y, w, h);
            ctx.restore();
            const label = `${obj.label} · STATIC`;
            const padding = 4;
            const textWidth = ctx.measureText(label).width + padding * 2;
            ctx.fillStyle = "rgba(180, 180, 220, 0.85)";
            ctx.fillRect(x, y - 16, textWidth, 16);
            ctx.fillStyle = "#0b0d10";
            ctx.fillText(label, x + padding, y - 4);
          } else {
            ctx.strokeStyle = "rgba(6, 214, 160, 0.95)";
            ctx.strokeRect(x, y, w, h);
            const label = `${obj.label} ${(obj.confidence * 100).toFixed(0)}%`;
            const padding = 4;
            const textWidth = ctx.measureText(label).width + padding * 2;
            ctx.fillStyle = "rgba(6, 214, 160, 0.85)";
            ctx.fillRect(x, y - 16, textWidth, 16);
            ctx.fillStyle = "#0b0d10";
            ctx.fillText(label, x + padding, y - 4);
          }
        }
      }
    };

    draw();
    const ro = new ResizeObserver(draw);
    ro.observe(container);
    return () => ro.disconnect();
  }, [meta, zones, anchors, fit]);

  return (
    <div
      ref={containerRef}
      className="pointer-events-none absolute inset-0"
      aria-hidden
    >
      <canvas
        ref={canvasRef}
        className="absolute inset-0 h-full w-full"
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

function extractZones(camera: CameraConfig): ZoneConfig[] {
  const raw = camera.zones;
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (z) =>
      !!z &&
      typeof z === "object" &&
      typeof (z as ZoneConfig).id === "string" &&
      typeof (z as ZoneConfig).name === "string" &&
      Array.isArray((z as ZoneConfig).polygon),
  );
}
