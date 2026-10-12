import type { ZoneConfig, ZoneKind } from "@/api/types";

// Pure helpers --------------------------------------------------------------

export function clamp01(v: number): number {
  if (v < 0) return 0;
  if (v > 1) return 1;
  return v;
}

export function polygonAttr(points: Array<[number, number]>): string {
  return points.map(([x, y]) => `${x},${y}`).join(" ");
}

export function zoneColor(kind: ZoneKind): { fill: string; stroke: string } {
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

export function genZoneId(existing: ZoneConfig[]): string {
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

export function polygonHasSelfIntersection(
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
