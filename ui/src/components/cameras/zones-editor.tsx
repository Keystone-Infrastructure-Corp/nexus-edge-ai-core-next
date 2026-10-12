import {
  Plus,
  RefreshCw,
  Square,
  Trash2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  latestFrameJpegUrl,
} from "@/api/system";
import type {
  ZoneConfig,
  ZoneKind,
} from "@/api/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  clamp01,
  genZoneId,
  polygonAttr,
  polygonHasSelfIntersection,
  zoneColor,
} from "@/lib/zone-geometry";

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

export function ZonesEditor({
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
