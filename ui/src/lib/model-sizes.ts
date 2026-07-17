// Per-kind ONNX input-size table.
//
// As of engine v0.1.22 the per-detector resolver is strict — it picks a
// `<model>_<W>.onnx` file out of the pack by exact size match and hard
// fails when missing (see `resolve_yolo26n_path` in `yolo.rs` and
// `resolve_yolo_world_path` in `yolo_world.rs`). The Intel NPU plugin
// silently falls back to CPU on dynamic shapes or wrong-size static
// shapes, so the operator MUST pick a size that the kind actually
// ships at — letting them free-type pixels invites a silent CPU
// downgrade in prod that no metric surfaces.
//
// This table mirrors what `tools/models/gen_*.py --all-static` writes
// to `models/models-manifest.json`. Keep it in lockstep when a new
// per-size variant ships.
//
//   yolo            (yolo26n)        — 640 / 960 / 1280
//   yolo_world      (yolo_world_v2_s)— 640 / 960   (1280 intentionally not shipped)
//   yoloe           (yoloe26_s)      — single fixed size (multi-size deferred to M3.4)
//   yoloe_promptfree                  — single fixed size
//   yoloe_visual                      — single fixed size
//   classifier_ensemble / mock        — no detector input (classifier or stub)

/** Sizes (square — w == h) the kind's pack actually ships. Empty
 *  means "no size choice — the kind ships exactly one ONNX and the
 *  engine's defaults apply". */
export const MODEL_KIND_SIZES: Record<string, readonly number[]> = {
  yolo: [640, 960, 1280],
  yolo_world: [640, 960],
  yoloe: [640],
  yoloe_promptfree: [640],
  yoloe_visual: [640],
  // mock / classifier_ensemble: omit on purpose — UI hides the size
  // section entirely for kinds not in this map.
};

/** Sizes available for a given kind; empty array if the kind isn't
 *  recognised or doesn't take a detector-input choice. */
export function sizesForKind(kind: string | undefined | null): readonly number[] {
  if (!kind) return [];
  return MODEL_KIND_SIZES[kind] ?? [];
}

/** First sensible size for a kind (used to auto-snap when the operator
 *  switches kinds and the previously-selected size isn't supported). */
export function defaultSizeForKind(kind: string): number | undefined {
  const opts = sizesForKind(kind);
  return opts[0];
}

/** Human-readable hint for the size dropdown. */
export function describeSize(size: number): string {
  switch (size) {
    case 640:
      return "640 × 640 — fastest, fits every box";
    case 960:
      return "960 × 960 — balanced; default on dGPU / NPU boxes";
    case 1280:
      return "1280 × 1280 — highest detail; opt-in for plate/face";
    default:
      return `${size} × ${size}`;
  }
}
