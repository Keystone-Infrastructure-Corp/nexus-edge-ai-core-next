import { describe, expect, it } from "vitest";
import { ZodError } from "zod";

import { ApiError } from "@/api/client";
import {
  deriveZoneApplication,
  parseConfigSnapshot,
  receiptGeneration,
  zoneApplicationPollMs,
  type AcceptanceEpoch,
  type ReceiptRead,
  type SavedSnapshot,
  type ZoneDraftState,
} from "@/lib/zone-application";

// SPEC-088: Applied only when the displayed revision, the receipt's desired
// and applied revisions, its state and both generations agree, in a stats
// request begun after the displayed snapshot and the latest save.

const A = `sha256:${"a".repeat(64)}`;
const B = `sha256:${"b".repeat(64)}`;

const shown = (revision = A): SavedSnapshot => ({
  snapshot: {
    schema_version: 1,
    camera_id: 7,
    settings: { zones: [] },
    zones_revision: revision,
  },
  error: null,
});

const receipt = (over: Record<string, unknown> = {}) => ({
  schema_version: 1,
  desired_revision: A,
  applied_revision: A,
  generation: 2,
  applied_generation: 2,
  state: "applied",
  applied_at: "2026-11-24T16:30:00Z",
  error_code: null,
  ...over,
});

const read = (zonesRuntime: unknown, seq = 10, cameraId = 7): ReceiptRead => ({
  data: { seq, body: { camera_id: cameraId, zones_runtime: zonesRuntime } },
  status: "success",
  dataUpdatedAt: 1_000,
  error: null,
});

const clean: ZoneDraftState = { phase: "clean", base: null };
const epoch: AcceptanceEpoch = { seq: 5, generation: -1 };

const derive = (
  r: ReceiptRead,
  saved: SavedSnapshot = shown(),
  draft: ZoneDraftState = clean,
  e: AcceptanceEpoch = epoch,
) => deriveZoneApplication(saved, r, draft, e);

describe("deriveZoneApplication", () => {
  it("is applied only when every revision, state and generation agree", () => {
    const a = derive(read(receipt()));
    expect(a.kind).toBe("applied");
    expect(a.title).toBe("Applied to camera");
    expect(a.readAt).toBe(1_000);
  });

  it("never lets a read begun at or before the acceptance epoch decide", () => {
    expect(derive(read(receipt(), 5)).kind).toBe("unread");
    expect(derive(read(receipt(), 4)).kind).toBe("unread");
  });

  it("is unread with no read or no installed snapshot", () => {
    expect(derive({ data: undefined, status: "pending", dataUpdatedAt: 0, error: null }).kind).toBe("unread");
    expect(derive(read(receipt()), { snapshot: null, error: null }).kind).toBe("unread");
  });

  it("is applying while the running generation has not acknowledged the shown zones", () => {
    const a = derive(
      read(receipt({ desired_revision: B, applied_revision: A, generation: 1, applied_generation: 1, state: "applying", applied_at: null })),
      shown(B),
    );
    expect(a.kind).toBe("applying");
    expect(a.title).toBe("Saved; applying to camera");
  });

  it("never treats equal digests from an older generation as applied", () => {
    expect(derive(read(receipt({ state: "applying", generation: 2, applied_generation: 1 }))).kind).toBe("applying");
    // `applied` with mismatched generations is a contract error, not success.
    expect(derive(read(receipt({ generation: 2, applied_generation: 1 }))).kind).toBe("failed");
  });

  it("cannot verify a receipt without applied_generation or without zones_runtime", () => {
    const older: Record<string, unknown> = receipt();
    delete older.applied_generation;
    expect(derive(read(older)).kind).toBe("unsupported");
    expect(derive(read(undefined)).kind).toBe("unsupported");
    expect(derive(read(undefined)).title).toBe("Saved; running application cannot be verified");
  });

  it("reports a newer save elsewhere instead of a verdict for the shown zones", () => {
    expect(derive(read(receipt({ desired_revision: B, applied_revision: B }))).kind).toBe("changed_elsewhere");
  });

  it("reports a disabled camera as inactive, neither applied nor failed", () => {
    const a = derive(read(receipt({ applied_revision: null, applied_generation: null, state: "inactive", applied_at: null })));
    expect(a.kind).toBe("inactive");
    expect(a.title).toBe("Saved; applies when enabled");
  });

  it("reports the engine's application timeout", () => {
    const a = derive(read(receipt({ state: "failed", applied_revision: null, applied_generation: null, applied_at: null, error_code: "application_timeout" })));
    expect(a.kind).toBe("failed");
    expect(a.title).toBe("Not applied: camera did not process a frame within 30 s");
  });

  it("shows uncertainty, never the cached verdict, when the latest read failed", () => {
    const failed: ReceiptRead = { ...read(receipt()), status: "error" };
    expect(derive(failed).kind).toBe("disconnected");
  });

  it("separates a denied, malformed and failed snapshot read", () => {
    const r = read(receipt());
    expect(derive(r, { snapshot: null, error: new ApiError(403, "forbidden") }).title).toBe(
      "Cannot verify: no access to the saved zones",
    );
    expect(derive(r, { snapshot: null, error: new ZodError([]) }).kind).toBe("failed");
    expect(derive(r, { snapshot: null, error: new ApiError(500, "x") }).kind).toBe("disconnected");
  });

  it("treats another camera's receipt or a malformed receipt as a contract error", () => {
    expect(derive(read(receipt(), 10, 8)).kind).toBe("failed");
    expect(derive(read(receipt({ desired_revision: "A" }))).kind).toBe("failed");
  });

  it("keeps a stale generation from recovering the verdict", () => {
    expect(derive(read(receipt()), shown(), clean, { seq: 5, generation: 3 }).kind).toBe("applying");
  });

  it("never shows Applied on a dirty edit, and says when the zones changed elsewhere", () => {
    const dirty = derive(read(receipt()), shown(), { phase: "dirty", base: A });
    expect(dirty.kind).toBe("unsaved");
    expect(dirty.title).toBe("Unsaved changes");
    const elsewhere = derive(read(receipt({ desired_revision: B, applied_revision: B })), shown(B), {
      phase: "dirty",
      base: A,
    });
    expect(elsewhere.title).toBe("Unsaved changes; zones changed elsewhere");
  });

  it("shows a denied saved-zone read, never an unsaved edit over it", () => {
    const denied = derive(read(receipt()), { snapshot: null, error: new ApiError(403, "forbidden") }, {
      phase: "dirty",
      base: A,
    });
    expect(denied.kind).toBe("disconnected");
    expect(denied.title).toBe("Cannot verify: no access to the saved zones");
  });

  it("shows a denied read, saved zones or status, over an edit or a save in flight", () => {
    const forbidden = new ApiError(403, "forbidden");
    const statsDenied: ReceiptRead = { data: undefined, status: "error", dataUpdatedAt: 0, error: forbidden };
    for (const phase of ["dirty", "saving", "saved"] as const) {
      const draft = { phase, base: A };
      expect(derive(statsDenied, shown(), draft).title).toBe("Cannot confirm: status read failed");
      expect(derive(read(receipt()), { snapshot: null, error: forbidden }, draft).title).toBe(
        "Cannot verify: no access to the saved zones",
      );
    }
  });

  it("never shows Applied while a save is pending or awaiting its snapshot", () => {
    expect(derive(read(receipt()), shown(), { phase: "saving", base: null }).title).toBe("Saving to camera…");
    expect(derive(read(receipt()), shown(), { phase: "saved", base: null }).title).toBe("Saved; applying to camera");
  });
});

describe("zoneApplicationPollMs", () => {
  const at = (r: ReceiptRead, draft: ZoneDraftState = clean) => derive(r, shown(), draft);
  const applying = read(receipt({ state: "applying", applied_revision: null, applied_generation: null, applied_at: null }));

  it("polls unresolved application every 2 s for 30 s after a save, then every 10 s", () => {
    expect(zoneApplicationPollMs(at(applying), [], 1_000, 30_999)).toBe(2_000);
    expect(zoneApplicationPollMs(at(applying), [], 1_000, 31_000)).toBe(10_000);
    expect(zoneApplicationPollMs(at(applying), [], null)).toBe(10_000);
  });

  it("keeps one 60 s read for applied and inactive, including behind an edit", () => {
    expect(zoneApplicationPollMs(at(read(receipt())), [], 1_000, 2_000)).toBe(60_000);
    expect(zoneApplicationPollMs(at(read(receipt()), { phase: "dirty", base: A }), [], null)).toBe(60_000);
  });

  it("stops for an unsupported camera or a denied read", () => {
    expect(zoneApplicationPollMs(at(read(undefined)), [], 1_000, 2_000)).toBe(false);
    expect(zoneApplicationPollMs(at(applying), [new ApiError(403, "no")], 1_000, 2_000)).toBe(false);
  });
});

describe("parseConfigSnapshot", () => {
  it("keeps only the allowlisted keys and every zone field", () => {
    const zone = { id: "z1", name: "Door", polygon: [[0, 0], [1, 0], [0, 1]], kind: "exclusion", min_bbox_area_px_override: 40 };
    const s = parseConfigSnapshot({
      schema_version: 1,
      camera_id: 7,
      settings: { zones: [zone], url: "rtsp://user:secret@cam" },
      zones_revision: A,
      onvif: { password: "secret" },
    });
    expect(s).toEqual({ schema_version: 1, camera_id: 7, settings: { zones: [zone] }, zones_revision: A });
  });

  it("rejects a revision the engine would not send", () => {
    expect(() =>
      parseConfigSnapshot({ schema_version: 1, camera_id: 7, settings: { zones: [] }, zones_revision: "A" }),
    ).toThrow(ZodError);
  });
});

describe("receiptGeneration", () => {
  it("reads the generation of a valid receipt only", () => {
    expect(receiptGeneration({ camera_id: 7, zones_runtime: receipt({ generation: 9, applied_generation: 9 }) })).toBe(9);
    expect(receiptGeneration({ camera_id: 7, zones_runtime: { generation: 9 } })).toBeUndefined();
    expect(receiptGeneration(undefined)).toBeUndefined();
  });
});
