import { describe, expect, it } from "vitest";

import type { HealthResponse } from "@/api/types";
import { engineHealth, issueCodes } from "@/lib/engineHealth";

const OK: HealthResponse = { status: "ok", version: "0.1.99", issues: [] };
const DEGRADED: HealthResponse = {
  status: "degraded",
  version: "0.1.99",
  issues: [{ component: "recorder", code: "recorder_stub" }],
};

// BUG-225, and AGENTS.md's "never state what you have not read": TanStack
// Query keeps the last answer in `data` when a refetch fails, so the verdict
// must come from the query's state, not from whatever `data` still holds.
describe("engineHealth", () => {
  it("reports a failed request as unreachable whatever the last answer was", () => {
    expect(engineHealth({ isError: true, isSuccess: false, data: OK }).verdict).toBe(
      "unreachable",
    );
    expect(
      engineHealth({ isError: true, isSuccess: false, data: DEGRADED }).verdict,
    ).toBe("unreachable");
  });

  it("reports an unanswered request as unread", () => {
    expect(
      engineHealth({ isError: false, isSuccess: false, data: undefined }).verdict,
    ).toBe("unread");
  });

  it("carries the issues of a degraded answer", () => {
    expect(engineHealth({ isError: false, isSuccess: true, data: DEGRADED })).toEqual({
      verdict: "degraded",
      read: DEGRADED,
    });
  });
});

describe("issueCodes", () => {
  it("names each distinct code once, verbatim", () => {
    expect(
      issueCodes([
        { component: "detector", code: "detector_unavailable", kind: "yolo" },
        { component: "detector", code: "detector_unavailable", kind: "yoloe" },
        { component: "x", code: "a_code_this_ui_has_never_seen" },
      ]),
    ).toBe("detector_unavailable, a_code_this_ui_has_never_seen");
  });
});
