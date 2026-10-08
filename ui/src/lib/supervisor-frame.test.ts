import { describe, expect, it } from "vitest";

import type { ModelOverride } from "@/api/types";
import { detectorInputFor, supervisorDimsFor } from "@/lib/supervisor-frame";

const override = (input_width?: number): ModelOverride => ({
  kind: "mock",
  ...(input_width === undefined ? {} : { input_width }),
});

// The same table as `supervisor_dims_follow_the_detector_width_and_never_drop_below_it`
// in crates/nexus-engine/src/reconciler.rs, so the camera form's preview
// cannot state a frame the engine does not run.
describe("supervisorDimsFor", () => {
  it.each<
    [string, ModelOverride | null, number | undefined, number, [number, number]]
  >([
    ["the default width", null, undefined, 512, [512, 288]],
    ["another default width", null, undefined, 640, [640, 360]],
    ["a width off the ladder", null, undefined, 600, [600, 338]],
    [
      "a model override, over the default",
      override(640),
      undefined,
      512,
      [640, 360],
    ],
    [
      "a supervisor_width below the detector input",
      null,
      256,
      512,
      [512, 288],
    ],
    [
      "a supervisor_width above the detector input",
      null,
      1024,
      512,
      [1024, 576],
    ],
    // The engine deserialises an override with no input_width at its serde
    // default, 512, whatever the engine's own default model is.
    [
      "an override with no input_width",
      override(),
      undefined,
      1024,
      [512, 288],
    ],
  ])("%s", (_what, modelOverride, supervisorWidth, defaultWidth, want) => {
    const det = detectorInputFor(modelOverride, { w: defaultWidth, h: 0 });
    expect(supervisorDimsFor(det!.w, supervisorWidth)).toEqual(want);
  });
});

describe("detectorInputFor", () => {
  it("is unknown for a camera with no override until the engine's default is read", () => {
    expect(detectorInputFor(null, undefined)).toBeUndefined();
    expect(detectorInputFor(undefined, undefined)).toBeUndefined();
  });

  it("needs no engine default for a camera with an override", () => {
    expect(
      detectorInputFor(
        { kind: "yolo", input_width: 1024, input_height: 576 },
        undefined,
      ),
    ).toEqual({ w: 1024, h: 576 });
  });

  it("is the engine's default model for a camera with no override", () => {
    expect(detectorInputFor(null, { w: 1024, h: 576 })).toEqual({
      w: 1024,
      h: 576,
    });
  });
});
