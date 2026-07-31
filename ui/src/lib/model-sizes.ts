// Per-kind ONNX input-shape table (M_NATIVE_ASPECT).
//
// The engine ships detectors on the native 16:9 ladder (exact 16:9 ∩
// stride-32, W=512k / H=288k) and the per-detector resolver is strict —
// it picks a `<model>_<W>x<H>.onnx` file out of the pack by exact shape
// match and hard-fails when missing (see `resolve_yolo26n_path` in
// `yolo.rs`, `resolve_yolo_world_path` in `yolo_world.rs`). The Intel
// NPU plugin silently falls back to CPU on dynamic or wrong-shape static
// inputs, so the operator MUST pick a shape the kind actually ships at —
// letting them free-type pixels invites a silent CPU downgrade in prod
// that no metric surfaces (hence named tiers only, no free-text entry).
//
// This table mirrors what `tools/models/gen_*.py --all-static` writes to
// `models/models-manifest.json`. Keep it in lockstep with the pack.

/** A native-16:9 detector input shape and its human-facing tier name. */
export type ModelShape = {
  readonly w: number;
  readonly h: number;
  readonly tier: string; // "Standard" | "Long range" | "High detail"
};

/** The full native-16:9 ladder (exact 16:9 ∩ stride-32). Detector kinds
 *  that ship every rung reference this directly. */
export const SHAPE_LADDER: readonly ModelShape[] = [
  { w: 512, h: 288, tier: "Standard" },
  { w: 1024, h: 576, tier: "Long range" },
  { w: 1536, h: 864, tier: "High detail" },
];

/** Shapes the kind's pack actually ships. Empty means "no shape choice —
 *  the kind ships a single fixed input and the engine defaults apply".
 *  Every detector ships the full ladder as of M_NATIVE_ASPECT. */
export const MODEL_KIND_SHAPES: Record<string, readonly ModelShape[]> = {
  yolo: SHAPE_LADDER,
  yolo_world: SHAPE_LADDER,
  yoloe: SHAPE_LADDER,
  yoloe_promptfree: SHAPE_LADDER,
  yoloe_visual: SHAPE_LADDER,
  // mock / classifier_ensemble: omit on purpose — UI hides the shape
  // section entirely for kinds not in this map.
};

/** Shapes available for a given kind; empty array if the kind isn't
 *  recognised or doesn't take a detector-input choice. */
export function shapesForKind(kind: string | undefined | null): readonly ModelShape[] {
  if (!kind) return [];
  return MODEL_KIND_SHAPES[kind] ?? [];
}

/** First sensible shape for a kind (Standard tier — used to auto-snap
 *  when the operator switches kinds and the previously-selected shape
 *  isn't supported). */
export function defaultShapeForKind(
  kind: string | undefined | null,
): ModelShape | undefined {
  return shapesForKind(kind)[0];
}

/** The canonical `"<W>x<H>"` preset string for a shape — matches the
 *  engine's `preset` label and the `<model>_<W>x<H>.onnx` filename. */
export function shapeKey(s: ModelShape): string {
  return `${s.w}x${s.h}`;
}

/** Resolve a `"<W>x<H>"` preset string to a shape from a kind's set, or
 *  `undefined` if it isn't one the kind ships. */
export function shapeFromKey(
  kind: string | undefined | null,
  key: string,
): ModelShape | undefined {
  return shapesForKind(kind).find((s) => shapeKey(s) === key);
}

/** Human-readable hint for the shape dropdown: tier + dims + a relative
 *  cost cue vs. the pre-migration square tier it replaces. */
export function describeShape(s: ModelShape): string {
  switch (`${s.w}x${s.h}`) {
    case "512x288":
      return "Standard — 512 × 288 · fastest, fits every box (36% the cost of the old 640 tier)";
    case "1024x576":
      return "Long range — 1024 × 576 · balanced (64% the cost of the old 960 tier)";
    case "1536x864":
      return "High detail — 1536 × 864 · plate / face detail (81% the cost of the old 1280 tier)";
    default:
      return `${s.tier} — ${s.w} × ${s.h}`;
  }
}
