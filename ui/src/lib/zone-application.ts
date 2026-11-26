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
//
// SPEC-077 — an engine with parking adds the parking-lot mode and v2 policy
// to the snapshot, with `parking_revision` over them and the zones, and a
// `parking_runtime` receipt of the same shape. While it does, Applied also
// needs that receipt to apply the shown parking revision under the same
// rules: a zone success never certifies parking settings.

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
    kind: z.enum(["inclusion", "exclusion", "dwell", "parking"]),
  })
  .passthrough();

const positive = z.number().positive();
const parkingSchema = z.object({
  parking_lot_mode: z.boolean(),
  parking_policy_v2: z
    .object({
      dwell_secs: positive,
      max_speed_box_diagonals_per_sec: positive,
      max_displacement_box_diagonals: positive,
      max_observation_gap_secs: positive,
    })
    .strict()
    .nullable(),
});

// `parking` and `parking_revision` come together or not at all (an engine
// without parking).
const snapshotSchema = z
  .object({
    schema_version: z.literal(1),
    camera_id: z.number().int(),
    settings: z.object({ zones: z.array(zoneSchema), parking: parkingSchema.optional() }),
    zones_revision: revision,
    parking_revision: revision.optional(),
  })
  .refine((s) => (s.settings.parking === undefined) === (s.parking_revision === undefined));

/** Keeps only the allowlisted keys; a body that does not parse throws (a contract error). */
export function parseConfigSnapshot(raw: unknown): CameraConfigSnapshot {
  return snapshotSchema.parse(raw);
}

/**
 * The engine sends all eight keys, nulls explicit; anything else is a
 * contract error. `parking_runtime` has the same shape.
 */
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

/** The engine could not read the saved settings: every key but the state null. */
const readErrorSchema = z.object({
  schema_version: z.literal(1),
  desired_revision: z.null(),
  applied_revision: z.null(),
  generation: z.null(),
  applied_generation: z.null(),
  state: z.literal("read_error"),
  applied_at: z.null(),
  error_code: z.null(),
});

let acceptanceSeq = 0;
/**
 * One counter orders save starts and ends, snapshot installations and the
 * start of every snapshot and stats request, so "began after" is exact.
 */
export const nextAcceptanceSeq = () => ++acceptanceSeq;

const zoneSaveSeqs = new Map<number, number>();
/**
 * A save of the camera started or ended, or the editor's access changed.
 * Kept per camera outside any editor, so one closed and reopened mid-save
 * still fences out every read begun before the save ended.
 */
export const markZoneSave = (cameraId: number) => {
  zoneSaveSeqs.set(cameraId, nextAcceptanceSeq());
};
/** The acceptance seq of the camera's latest {@link markZoneSave} in this tab. */
export const zoneSaveSeq = (cameraId: number) => zoneSaveSeqs.get(cameraId) ?? 0;

/** A read tagged with the acceptance seq taken when its request began. */
export type Tagged<T> = { seq: number; body: T };

/** The part of a camera-stats read this module looks at. */
export type StatsBody = { camera_id: number; zones_runtime?: unknown; parking_runtime?: unknown };

/** A stats query's state: a `useQuery` result or `query.state`. */
export type ReceiptRead = {
  data: Tagged<StatsBody> | undefined;
  status: "pending" | "error" | "success";
  dataUpdatedAt: number;
  error: unknown;
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
  /** The snapshot's `parking_revision` a dirty edit began from, where it has one. */
  parkingBase?: string | null;
  /**
   * A save went through and its snapshot is not installed yet, as `saved`
   * says; a dirty edit (a sketch holds the snapshot) can be over one too.
   */
  awaiting?: boolean;
};

/**
 * `seq` is the later of the latest save start/end and the snapshot
 * installation; `generation` the newest supervisor generation read in
 * this editor, below which a receipt is stale; `parkingGeneration` the
 * same for the parking receipt, whose generations are its own.
 */
export type AcceptanceEpoch = { seq: number; generation: number; parkingGeneration?: number };

/** What a receipt answers for. */
type Subject = "zones" | "parking settings";

type Shown = { title: string; detail: string; readAt: number | null };
export type ZoneApplication =
  | (Shown & {
      kind:
        | "unread"
        | "unsupported"
        | "inactive"
        | "applying"
        | "failed"
        | "disconnected";
    })
  | (Shown & { kind: "changed_elsewhere"; what: Subject })
  | (Shown & { kind: "applied"; appliedAt: string | null })
  /** `receipt` is what the engine reports for the saved zones behind the edit. */
  | (Shown & { kind: "unsaved"; receipt: ZoneApplication });

export const isDenied = (error: unknown) =>
  error instanceof ApiError && (error.status === 401 || error.status === 403);

/**
 * What the engine refused a save over: its stored zones, or (SPEC-077) its
 * parking settings, were no longer the revision the save expected. Nothing
 * was written. Undefined for any other error.
 */
export function revisionConflict(error: unknown): Subject | undefined {
  if (!(error instanceof ApiError) || error.status !== 409) return undefined;
  const code = (error.body as { error?: unknown } | undefined)?.error;
  if (code === "zones_revision_conflict") return "zones";
  if (code === "parking_revision_conflict") return "parking settings";
  return undefined;
}

/** The supervisor generation a stats read's receipt reports, when it parses. */
export const receiptGeneration = (
  body: StatsBody | undefined,
  receipt: "zones_runtime" | "parking_runtime" = "zones_runtime",
) => zonesRuntimeSchema.safeParse(body?.[receipt]).data?.generation;

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
  const receipt = deriveReceipt(saved, read, draft.phase === "saved" || draft.awaiting === true, epoch);
  // A denied read, of the saved zones or of the status, shows as such: never
  // as an edit to save or a save in flight, which the denial dropped.
  if (isDenied(saved.error) || isDenied(read.error)) return receipt;
  if (draft.phase === "dirty") {
    const s = saved.snapshot;
    const moved = (base: string | null | undefined, now: string | undefined) =>
      base != null && now !== undefined && base !== now;
    const elsewhere: Subject | null =
      s && moved(draft.base, s.zones_revision)
        ? "zones"
        : s && moved(draft.parkingBase, s.parking_revision)
          ? "parking settings"
          : receipt.kind === "changed_elsewhere"
            ? receipt.what
            : null;
    return elsewhere
      ? {
          kind: "unsaved",
          receipt,
          readAt: receipt.readAt,
          title: `Unsaved changes; ${elsewhere} changed elsewhere`,
          detail: `Someone saved different ${elsewhere} on this camera since you started editing. Saving replaces them with the ${elsewhere} shown here.`,
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
  const contract = (what: Subject): ZoneApplication => ({
    kind: "failed",
    readAt,
    title: `Cannot verify: unrecognized ${what === "zones" ? "zone" : "parking"} status from engine`,
    detail: `The engine's ${what === "zones" ? "zone" : "parking"} status did not match the expected format, so no application verdict is shown.`,
  });
  const body = read.data.body;
  const snapshot = saved.snapshot;
  if (body.camera_id !== snapshot.camera_id) return contract("zones");

  /** One receipt's verdict on the shown `revision` of `what`, from generation `floor` up. */
  const verdict = (raw: unknown, revision: string, floor: number, what: Subject): ZoneApplication => {
    if (raw === undefined || raw === null) {
      // An engine that reports parking settings owes their receipt: its
      // absence is an unknown, never a zone-only Applied.
      return what === "zones"
        ? unsupported
        : {
            kind: "disconnected",
            readAt,
            title: "Cannot confirm: engine did not report parking status",
            detail:
              "The engine reports parking settings but not whether the camera runs them, so that is unknown. It is asked again.",
          };
    }
    if (readErrorSchema.safeParse(raw).success) {
      return {
        kind: "disconnected",
        readAt,
        title: `Cannot confirm: engine could not read the saved ${what}`,
        detail: `The engine failed to read this camera's saved ${what}, so whether the camera is running them is unknown. It is asked again.`,
      };
    }
    const parsed = zonesRuntimeSchema.safeParse(raw);
    if (!parsed.success) return contract(what);
    const zr = parsed.data;
    if (zr.applied_generation === undefined) return what === "zones" ? unsupported : contract(what);
    // An answer from a generation older than one already read.
    if (zr.generation < floor) return pending;
    if (zr.desired_revision !== revision) {
      return what === "zones"
        ? {
            kind: "changed_elsewhere",
            what,
            readAt,
            title: "Zones changed elsewhere; loading them",
            detail:
              "The engine has a newer zone save than the one shown. It is loaded here before checking which zones the camera is running.",
          }
        : {
            kind: "changed_elsewhere",
            what,
            readAt,
            title: "Parking settings changed elsewhere; loading them",
            detail:
              "The engine has newer parking settings than the ones shown. They are loaded here before checking which settings the camera is running.",
          };
    }
    if (zr.state === "inactive") {
      return {
        kind: "inactive",
        readAt,
        title: "Saved; applies when enabled",
        detail: `The camera is disabled. The saved ${what} apply when it is enabled; this is not a failure.`,
      };
    }
    if (zr.state === "failed") {
      return zr.error_code === "application_timeout"
        ? {
            kind: "failed",
            readAt,
            title: "Not applied: camera did not process a frame within 30 s",
            detail: `The saved ${what} stay on the engine and apply once the camera processes a frame. Check that the camera is streaming.`,
          }
        : {
            kind: "failed",
            readAt,
            title: `Not applied: ${zr.error_code?.slice(0, 64) ?? "no reason reported"}`,
            detail: `The engine reported this error while applying the saved ${what}.`,
          };
    }
    // The schema holds `applied` to equal revisions and generations.
    if (zr.state === "applied") {
      return {
        kind: "applied",
        readAt,
        appliedAt: zr.applied_at,
        title: "Applied to camera",
        detail: `The camera processed a frame with the saved ${what}. Decode and analysis health are reported separately.`,
      };
    }
    return {
      kind: "applying",
      readAt,
      title: "Saved; applying to camera",
      detail: `The engine has the saved ${what} and is waiting for the camera to process a frame with them.`,
    };
  };

  const zones = verdict(body.zones_runtime, snapshot.zones_revision, epoch.generation, "zones");
  if (zones.kind !== "applied" || snapshot.parking_revision === undefined) return zones;
  return verdict(body.parking_runtime, snapshot.parking_revision, epoch.parkingGeneration ?? -1, "parking settings");
}
