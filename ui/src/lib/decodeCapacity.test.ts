import { describe, expect, it } from "vitest";

import {
  analysisStreamLabel,
  capacityHeadline,
  countCamerasLosingFrames,
  decodeSummaryText,
  decodeVerdictPresentation,
  engineDisplayName,
  type DecodeVerdict,
} from "@/lib/decodeCapacity";

// The invariant these guard is AGENTS.md's "never state what you have not
// read": a pending or failed stats request must not be indistinguishable
// from a real all-clear. That defect has shipped four times in the cloud
// console (BUG-069/072/074/075); these are the edge console's version.
describe("decodeVerdictPresentation", () => {
  it("reports `unknown` when the query has not been read, whatever it holds", () => {
    expect(decodeVerdictPresentation(undefined, false).verdict).toBe("unknown");
    // The dangerous case: data present but the read failed/pending.
    expect(decodeVerdictPresentation("healthy", false).verdict).toBe("unknown");
  });

  it("never renders an unread camera as healthy", () => {
    const p = decodeVerdictPresentation("healthy", false);
    expect(p.verdict).not.toBe("healthy");
    expect(p.label).toBe("Not read");
  });

  it("passes a read verdict through", () => {
    expect(decodeVerdictPresentation("healthy", true).verdict).toBe("healthy");
    expect(decodeVerdictPresentation("losing_frames", true).label).toBe(
      "Losing frames",
    );
  });

  it("degrades an unrecognised verdict to `unknown` rather than showing it raw", () => {
    const p = decodeVerdictPresentation("something_new_from_a_newer_engine", true);
    expect(p.verdict).toBe("unknown");
    expect(p.label).toBe("Not read");
  });

  it("gives every verdict an icon and a label, so colour is never the only channel", () => {
    const all: DecodeVerdict[] = [
      "healthy",
      "losing_frames",
      "padded",
      "substream_unavailable",
      "measuring",
      "unknown",
    ];
    for (const v of all) {
      const p = decodeVerdictPresentation(v, true);
      expect(p.icon.length).toBeGreaterThan(0);
      expect(p.label.length).toBeGreaterThan(0);
    }
  });
});

describe("countCamerasLosingFrames", () => {
  it("returns null — not 0 — when the list was not read", () => {
    // 0 would render as "0 cameras are losing frames", an all-clear the
    // console never actually established.
    expect(countCamerasLosingFrames(undefined, false)).toBeNull();
    expect(countCamerasLosingFrames([{ decode_verdict: "losing_frames" }], false)).toBeNull();
  });

  it("counts only cameras the engine judged to be losing frames", () => {
    const rows = [
      { decode_verdict: "healthy" },
      { decode_verdict: "losing_frames" },
      { decode_verdict: "losing_frames" },
      { decode_verdict: "padded" },
      {},
    ];
    expect(countCamerasLosingFrames(rows, true)).toBe(2);
  });

  it("returns 0 for a genuinely healthy fleet that WAS read", () => {
    expect(countCamerasLosingFrames([{ decode_verdict: "healthy" }], true)).toBe(0);
  });
});

describe("capacityHeadline", () => {
  it("says 'not measured' rather than 'healthy' when the host reports nothing", () => {
    const h = capacityHeadline(null, null);
    expect(h.title).toBe("Decode capacity not measured");
    expect(h.tone).toBe("muted");
  });

  it("names the binding engine when over capacity, so an idle decoder does not confuse", () => {
    const h = capacityHeadline(
      { binding_engine: "video-enhance", binding_engine_pct: 99.1, oversubscribed: true },
      39,
    );
    expect(h.title).toBe("Over decode capacity");
    expect(h.detail).toContain("Video post-processing");
    expect(h.detail).toContain("99%");
    expect(h.detail).toContain("39 cameras are losing frames");
  });

  it("says the per-camera count was not read, rather than dropping the clause", () => {
    // Silently omitting it reads identically to "no cameras affected".
    const h = capacityHeadline(
      { binding_engine: "video-decode", binding_engine_pct: 99, oversubscribed: true },
      null,
    );
    expect(h.detail).toContain("not read yet");
    expect(h.detail).not.toContain("cameras are losing frames");
    expect(h.detail).not.toContain("0 cameras");
  });

  it("singularises one camera", () => {
    const h = capacityHeadline(
      { binding_engine: "video-decode", binding_engine_pct: 99, oversubscribed: true },
      1,
    );
    expect(h.detail).toContain("1 camera is losing frames");
  });

  it("reports healthy when the busiest video engine is not saturated", () => {
    const h = capacityHeadline(
      { binding_engine: "video-decode", binding_engine_pct: 12, oversubscribed: false },
      0,
    );
    expect(h.title).toBe("Decode capacity healthy");
    expect(h.icon).toBe("check");
  });
});

describe("decodeSummaryText", () => {
  it("says the request did not complete rather than going blank", () => {
    expect(decodeSummaryText(undefined, false)).toContain("unavailable");
    expect(decodeSummaryText("Healthy", false)).toContain("unavailable");
  });

  it("prefers the engine's own sentence, since thresholds live only there", () => {
    expect(decodeSummaryText("Losing frames — the appliance cannot keep up", true)).toBe(
      "Losing frames — the appliance cannot keep up",
    );
  });

  it("is explicit when a read succeeded but carried no sentence", () => {
    expect(decodeSummaryText("", true)).toContain("no decode summary");
  });
});

describe("analysisStreamLabel", () => {
  it("is null when there is no analysis-stream status to show", () => {
    expect(analysisStreamLabel(null)).toBeNull();
    expect(analysisStreamLabel(undefined)).toBeNull();
  });

  it("renders the shipped `substream`/`mainstream` values, not the spec's prose", () => {
    expect(
      analysisStreamLabel({ mode: "substream", state: "active", width: 640, height: 360 }),
    ).toBe("Sub 640×360");
    expect(
      analysisStreamLabel({ mode: "mainstream", state: "active", width: 1920, height: 1080 }),
    ).toBe("Main 1920×1080");
  });

  it("omits geometry before the first frame instead of printing 0×0", () => {
    expect(
      analysisStreamLabel({ mode: "substream", state: "probing", width: 0, height: 0 }),
    ).toBe("Sub");
  });
});

describe("engineDisplayName", () => {
  it("names each video engine an operator can be pointed at", () => {
    expect(engineDisplayName("video-decode")).toBe("Video decode");
    expect(engineDisplayName("video-enhance")).toBe("Video post-processing");
    expect(engineDisplayName("video-encode")).toBe("Video encode");
  });

  it("passes an unknown engine class through unchanged", () => {
    expect(engineDisplayName("some-future-engine")).toBe("some-future-engine");
  });
});
