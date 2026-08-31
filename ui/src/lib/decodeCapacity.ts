// SPEC-069 Phase 1 — presentation for the decode-capacity verdict.
//
// Thresholds live on the engine, never here: it computes `decode_verdict`
// and `decode_summary` so both consoles say the same thing about the same
// camera. This module only maps the string it sent onto an icon and a label.
//
// Every helper takes the *query state*, not a defaulted value, so "still
// loading" and "failed" can never render as an all-clear. A verdict the
// console has not actually read is `unknown`, and says so.

export type DecodeVerdict =
  | "healthy"
  | "losing_frames"
  | "padded"
  | "substream_unavailable"
  | "measuring"
  | "unknown";

export type VerdictTone = "ok" | "warn" | "bad" | "muted";

export interface VerdictPresentation {
  verdict: DecodeVerdict;
  /** Short label for the list chip. Always rendered beside the icon. */
  label: string;
  /** Lucide icon name — the non-colour channel, so colour is never alone. */
  icon: "check" | "alert-triangle" | "copy" | "unplug" | "loader" | "help";
  tone: VerdictTone;
}

const PRESENTATION: Record<DecodeVerdict, Omit<VerdictPresentation, "verdict">> = {
  healthy: { label: "Healthy", icon: "check", tone: "ok" },
  losing_frames: { label: "Losing frames", icon: "alert-triangle", tone: "bad" },
  padded: { label: "Padded", icon: "copy", tone: "warn" },
  substream_unavailable: {
    label: "Analysis on main stream",
    icon: "unplug",
    tone: "warn",
  },
  measuring: { label: "Measuring…", icon: "loader", tone: "muted" },
  unknown: { label: "Not read", icon: "help", tone: "muted" },
};

function asVerdict(raw: string | undefined): DecodeVerdict {
  return raw !== undefined && raw in PRESENTATION
    ? (raw as DecodeVerdict)
    : "unknown";
}

/**
 * Resolve what to render for one camera.
 *
 * `read` is the query's own answer to "did this data arrive?". When it is
 * false the result is `unknown` whatever `raw` holds — an absent verdict on a
 * failed request must not render as Healthy.
 */
export function decodeVerdictPresentation(
  raw: string | undefined,
  read: boolean,
): VerdictPresentation {
  const verdict = read ? asVerdict(raw) : "unknown";
  return { verdict, ...PRESENTATION[verdict] };
}

/** The engine's sentence, or an explicit stand-in naming why there isn't one. */
export function decodeSummaryText(
  summary: string | undefined,
  read: boolean,
): string {
  if (!read) return "Decode status unavailable — the stats request has not completed.";
  return summary && summary.trim().length > 0
    ? summary
    : "This engine reported no decode summary for this camera.";
}

export interface CapacityHeadline {
  /** Icon name, paired with `title` so the state never rides on colour. */
  icon: "check" | "alert-triangle" | "help";
  tone: VerdictTone;
  title: string;
  /** One sentence. Names the binding engine, because "the decoder looks idle"
   *  is the confusion this line exists to prevent. */
  detail: string;
}

/**
 * Host-level capacity line for the System page.
 *
 * Built from the `decode_capacity` block the engine actually ships —
 * `{ binding_engine, binding_engine_pct, oversubscribed }`. The spec's
 * richer sentence ("382 of 1009 fps — 2.6× over capacity") needs
 * `demand_pixels_per_sec` / `ceiling_pixels_per_sec` / `ratio`, which no
 * engine build computes yet, so it is deliberately not fabricated here.
 */
export function capacityHeadline(
  capacity: { binding_engine: string; binding_engine_pct: number; oversubscribed: boolean } | null | undefined,
  camerasLosingFrames: number | null,
): CapacityHeadline {
  if (!capacity) {
    return {
      icon: "help",
      tone: "muted",
      title: "Decode capacity not measured",
      detail:
        "This host reports no per-engine video utilisation, so saturation cannot be judged from here.",
    };
  }
  const engine = engineDisplayName(capacity.binding_engine);
  const pct = `${capacity.binding_engine_pct.toFixed(0)}%`;
  // `null` means the per-camera read has not landed. Say that rather than
  // dropping the clause: a silent omission reads identically to a genuine
  // "no cameras affected", which is the distinction this whole module keeps.
  const affected =
    camerasLosingFrames === null
      ? " Per-camera decode status not read yet."
      : camerasLosingFrames === 1
        ? " 1 camera is losing frames."
        : ` ${camerasLosingFrames} cameras are losing frames.`;

  if (capacity.oversubscribed) {
    return {
      icon: "alert-triangle",
      tone: "bad",
      title: "Over decode capacity",
      detail: `${engine} is at ${pct} and is the binding engine — every camera decoding on it shares that ceiling.${affected}`,
    };
  }
  return {
    icon: "check",
    tone: "ok",
    title: "Decode capacity healthy",
    detail: `${engine} is the busiest video engine at ${pct}.${affected}`,
  };
}

/** Map the engine class the host reports onto something an operator reads. */
export function engineDisplayName(cls: string): string {
  switch (cls) {
    case "video-decode":
      return "Video decode";
    case "video-enhance":
      return "Video post-processing";
    case "video-encode":
      return "Video encode";
    default:
      return cls;
  }
}

/** How many cameras report `losing_frames`, or null when the list wasn't read. */
export function countCamerasLosingFrames(
  stats: Array<{ decode_verdict?: string }> | undefined,
  read: boolean,
): number | null {
  if (!read || stats === undefined) return null;
  return stats.filter((s) => s.decode_verdict === "losing_frames").length;
}

/** `Sub 640×360` / `Main 1920×1080`, or null when there is nothing to show. */
export function analysisStreamLabel(
  a: { mode: string; state: string; width: number; height: number } | null | undefined,
): string | null {
  if (!a) return null;
  const which = a.mode === "substream" ? "Sub" : "Main";
  const dims = a.width > 0 && a.height > 0 ? ` ${a.width}×${a.height}` : "";
  return `${which}${dims}`;
}
