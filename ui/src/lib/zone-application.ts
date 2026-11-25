// SPEC-088 — is the running camera using the zones this editor shows?
//
// The cloud console's rules over the engine's own two reads: the saved-zone
// snapshot (`GET /admin/cameras/{id}/config-snapshot`), shown in the editor
// with the engine's revision of it, and the optional `zones_runtime` receipt
// on `GET /cameras/{id}/stats`. Applied needs all of them to agree with what
// is shown (displayed revision = desired = applied, state `applied`,
// generation = applied generation) in a stats request that BEGAN after the
// displayed snapshot was installed and after the latest save: the
// acceptance epoch. A request begun earlier cannot decide, however late it
// finishes. The UI never computes a revision and a successful PUT is never
// an applied signal.

import { z } from "zod";

import { ApiError } from "@/api/client";
import type { CameraConfigSnapshot } from "@/api/types";

/** A SHA-256 digest the engine computed; the UI only compares these strings. */
const revision = z.string().regex(/^sha256:[0-9a-f]{64}$/);

// Passthrough: a zone field the UI does not model (per-zone minimum area)
// must survive into the next PUT.
const zoneSchema = z
  .object({
    id: z.string(),
    name: z.string(),
    polygon: z.array(z.tuple([z.number(), z.number()])),
    kind: z.enum(["inclusion", "exclusion", "dwell"]),
  })
  .passthrough();

const snapshotSchema = z.object({
  schema_version: z.literal(1),
  camera_id: z.number().int(),
  settings: z.object({ zones: z.array(zoneSchema) }),
  zones_revision: revision,
});

/** Keeps only the allowlisted keys; a body that does not parse throws (a contract error). */
export function parseConfigSnapshot(raw: unknown): CameraConfigSnapshot {
  return snapshotSchema.parse(raw);
}

/** The engine sends all eight keys, nulls explicit; anything else is a contract error. */
const zonesRuntimeSchema = z
  .object({
    schema_version: z.literal(1),
    desired_revision: revision,
    applied_revision: revision.nullable(),
    generation: z.number().int().nonnegative(),
    // Absent from receipts that predate it: valid, but never verified.
    applied_generation: z.number().int().nonnegative().nullable().optional(),
    state: z.enum(["inactive", "applying", "applied", "failed"]),
    applied_at: z.string().datetime({ offset: true }).nullable(),
    error_code: z.string().nullable(),
  })
  .refine((r) =>
    r.state === "applied"
      ? r.applied_revision === r.desired_revision &&
        r.applied_at !== null &&
        r.error_code === null &&
        (r.applied_generation === undefined || r.applied_generation === r.generation)
      : r.state === "failed"
        ? r.error_code !== null
        : r.error_code === null,
  );

let acceptanceSeq = 0;
/**
 * One counter orders save starts and ends, snapshot installations and the
 * start of every snapshot and stats request, so "began after" is exact.
 */
export const nextAcceptanceSeq = () => ++acceptanceSeq;

/** A read tagged with the acceptance seq taken when its request began. */
export type Tagged<T> = { seq: number; body: T };

/** The part of a camera-stats read this module looks at. */
export type StatsBody = { camera_id: number; zones_runtime?: unknown };

/** A stats query's state: a `useQuery` result or `query.state`. */
export type ReceiptRead = {
  data: Tagged<StatsBody> | undefined;
  status: "pending" | "error" | "success";
  dataUpdatedAt: number;
};

/** The snapshot installed in this editor (null before one is) and the snapshot read's error. */
export type SavedSnapshot = { snapshot: CameraConfigSnapshot | null; error: unknown };

/**
 * The zones on screen: the saved snapshot (`clean`), an edit (`dirty`), a
 * save in flight (`saving`), or a save awaiting its post-save snapshot
 * (`saved`). `base` is the snapshot revision a dirty edit began from.
 */
export type ZoneDraftState = {
  phase: "clean" | "dirty" | "saving" | "saved";
  base: string | null;
};

/**
 * `seq` is the later of the latest save start/end and the snapshot
 * installation; `generation` the newest supervisor generation read in
 * this editor, below which a receipt is stale.
 */
export type AcceptanceEpoch = { seq: number; generation: number };

type Shown = { title: string; detail: string; readAt: number | null };
export type ZoneApplication =
  | (Shown & {
      kind:
        | "unread"
        | "unsupported"
        | "inactive"
        | "applying"
        | "failed"
        | "disconnected"
        | "changed_elsewhere";
    })
  | (Shown & { kind: "applied"; appliedAt: string | null })
  /** `receipt` is what the engine reports for the saved zones behind the edit. */
  | (Shown & { kind: "unsaved"; receipt: ZoneApplication });

export const isDenied = (error: unknown) =>
  error instanceof ApiError && (error.status === 401 || error.status === 403);

/** The supervisor generation a stats read reports, when its receipt parses. */
export const receiptGeneration = (body: StatsBody | undefined) =>
  zonesRuntimeSchema.safeParse(body?.zones_runtime).data?.generation;

/**
 * Unresolved application is read every 2 s for 30 s after a save, then
 * every 10 s through applying, an engine failure, a mismatch and failed
 * reads: the engine's 30 s deadline is its verdict, not the end of
 * observation. Inactive and applied are not terminal: a 60 s read finds a
 * remote enable or edit. Unsupported or denied ends it. `errors` are the
 * stats and snapshot read errors.
 */
export function zoneApplicationPollMs(
  application: ZoneApplication,
  errors: unknown[],
  savedAt: number | null,
  now = Date.now(),
): number | false {
  const receipt = application.kind === "unsaved" ? application.receipt : application;
  if (receipt.kind === "unsupported" || errors.some(isDenied)) return false;
  if (receipt.kind === "applied" || receipt.kind === "inactive") return 60_000;
  return savedAt !== null && now - savedAt < 30_000 ? 2_000 : 10_000;
}

export function deriveZoneApplication(
  saved: SavedSnapshot,
  read: ReceiptRead,
  draft: ZoneDraftState,
  epoch: AcceptanceEpoch,
): ZoneApplication {
  const receipt = deriveReceipt(saved, read, draft.phase === "saved", epoch);
  if (draft.phase === "dirty") {
    const elsewhere =
      receipt.kind === "changed_elsewhere" ||
      (draft.base !== null &&
        saved.snapshot !== null &&
        draft.base !== saved.snapshot.zones_revision);
    return elsewhere
      ? {
          kind: "unsaved",
          receipt,
          readAt: receipt.readAt,
          title: "Unsaved changes; zones changed elsewhere",
          detail:
            "Someone saved different zones on this camera since you started editing. Saving replaces them with the zones shown here.",
        }
      : {
          kind: "unsaved",
          receipt,
          readAt: receipt.readAt,
          title: "Unsaved changes",
          detail:
            "These zones are not saved. Save to send them to the engine; application is checked after the save.",
        };
  }
  if (draft.phase === "saving") {
    return {
      kind: "applying",
      readAt: receipt.readAt,
      title: "Saving to camera…",
      detail: "Application is checked once the save completes.",
    };
  }
  return receipt;
}

function deriveReceipt(
  saved: SavedSnapshot,
  read: ReceiptRead,
  awaitingSnapshot: boolean,
  epoch: AcceptanceEpoch,
): ZoneApplication {
  const readAt = read.data ? read.dataUpdatedAt : null;
  if (read.status === "error") {
    return {
      kind: "disconnected",
      readAt,
      title: "Cannot confirm: status read failed",
      detail:
        "The latest status read from the engine failed, so whether the camera is running the saved zones is unknown.",
    };
  }
  const unsupported: ZoneApplication = {
    kind: "unsupported",
    readAt,
    title: "Saved; running application cannot be verified",
    detail:
      "The engine does not report zone application for this camera, so whether the camera runs the saved zones is unknown.",
  };
  if (saved.error) {
    if (isDenied(saved.error)) {
      return {
        kind: "disconnected",
        readAt,
        title: "Cannot verify: no access to the saved zones",
        detail: "Your role cannot read this camera's saved zones, so application is not checked.",
      };
    }
    if (saved.error instanceof z.ZodError) {
      return {
        kind: "failed",
        readAt,
        title: "Cannot verify: unrecognized saved zones from engine",
        detail:
          "The engine's saved-zone snapshot did not match the expected format, so no application verdict is shown.",
      };
    }
    return {
      kind: "disconnected",
      readAt,
      title: "Cannot confirm: saved-zone read failed",
      detail:
        "The latest read of the saved zones failed, so whether the camera is running them is unknown.",
    };
  }
  const unread: ZoneApplication = {
    kind: "unread",
    readAt,
    title: "Reading application status…",
    detail: "Asking the engine which zones the camera is running.",
  };
  const pending: ZoneApplication = {
    kind: "applying",
    readAt,
    title: "Saved; applying to camera",
    detail: "Waiting for the engine to report which zones the camera is running.",
  };
  if (awaitingSnapshot) return pending;
  // Nothing installed yet, or the read began before the installation or the
  // latest save: it cannot answer for what is shown.
  if (!read.data || !saved.snapshot || read.data.seq <= epoch.seq) return unread;
  const contract: ZoneApplication = {
    kind: "failed",
    readAt,
    title: "Cannot verify: unrecognized zone status from engine",
    detail:
      "The engine's zone status did not match the expected format, so no application verdict is shown.",
  };
  const body = read.data.body;
  if (body.camera_id !== saved.snapshot.camera_id) return contract;
  if (body.zones_runtime === undefined || body.zones_runtime === null) return unsupported;
  const parsed = zonesRuntimeSchema.safeParse(body.zones_runtime);
  if (!parsed.success) return contract;
  const zr = parsed.data;
  if (zr.applied_generation === undefined) return unsupported;
  // An answer from a generation older than one already read here.
  if (zr.generation < epoch.generation) return pending;
  if (zr.desired_revision !== saved.snapshot.zones_revision) {
    return {
      kind: "changed_elsewhere",
      readAt,
      title: "Zones changed elsewhere; loading them",
      detail:
        "The engine has a newer zone save than the one shown. It is loaded here before checking which zones the camera is running.",
    };
  }
  if (zr.state === "inactive") {
    return {
      kind: "inactive",
      readAt,
      title: "Saved; applies when enabled",
      detail: "The camera is disabled. The saved zones apply when it is enabled; this is not a failure.",
    };
  }
  if (zr.state === "failed") {
    return zr.error_code === "application_timeout"
      ? {
          kind: "failed",
          readAt,
          title: "Not applied: camera did not process a frame within 30 s",
          detail:
            "The saved zones stay on the engine and apply once the camera processes a frame. Check that the camera is streaming.",
        }
      : {
          kind: "failed",
          readAt,
          title: `Not applied: ${zr.error_code?.slice(0, 64) ?? "no reason reported"}`,
          detail: "The engine reported this error while applying the saved zones.",
        };
  }
  // The schema holds `applied` to equal revisions and generations.
  if (zr.state === "applied") {
    return {
      kind: "applied",
      readAt,
      appliedAt: zr.applied_at,
      title: "Applied to camera",
      detail:
        "The camera processed a frame with the saved zones. Decode and analysis health are reported separately.",
    };
  }
  return {
    kind: "applying",
    readAt,
    title: "Saved; applying to camera",
    detail: "The engine has the saved zones and is waiting for the camera to process a frame with them.",
  };
}
