import type { ModelOverride } from "@/api/types";

/** A detector input shape, in pixels. */
export type DetectorInput = { w: number; h: number };

/**
 * The detector input a camera runs at: its `model_override`'s, else the
 * engine's default model's, or `undefined` while that default is unread.
 * An override with no `input_width` / `input_height` runs at 512 × 288,
 * the engine's serde defaults for those fields.
 */
export function detectorInputFor(
  modelOverride: ModelOverride | null | undefined,
  engineDefault: DetectorInput | undefined,
): DetectorInput | undefined {
  if (!modelOverride) return engineDefault;
  return {
    w: modelOverride.input_width ?? 512,
    h: modelOverride.input_height ?? 288,
  };
}

/**
 * The supervisor (analysis) frame a camera runs at: its detector input
 * width, raised to `supervisor_width` when that is larger and never
 * lowered by it, as a 16:9 frame with an even height. With
 * `detectorInputFor`, a copy of the engine's `reconciler::supervisor_dims_for`
 * and `supervisor_frame_for`, pinned to the same table test.
 */
export function supervisorDimsFor(
  detectorWidth: number,
  supervisorWidth: number | undefined,
): [number, number] {
  const supW = Math.max(supervisorWidth ?? detectorWidth, detectorWidth);
  const h = Math.floor((supW * 9) / 16);
  return [supW, h % 2 === 0 ? h : h + 1];
}
