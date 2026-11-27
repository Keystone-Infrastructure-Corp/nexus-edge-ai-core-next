import {
  focusManager,
  useIsMutating,
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import {
  AlertTriangle,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { toast } from "sonner";

import { getCameraConfigSnapshot } from "@/api/admin";
import {
  attachVisualPrompt,
  detachVisualPrompt,
  getModelPromptsCatalog,
  listCameraVisualPrompts,
  listVisualPrompts,
  upsertCamera,
} from "@/api/config";
import {
  getCameraStats,
  getStaticObjectDefaults,
  listCameras,
  type CameraFrameStats,
} from "@/api/system";
import type {
  CameraConfig,
  CameraConfigSnapshot,
  CameraOnvif,
  CodecKind,
  ModelOverride,
  ModelPromptsCatalog,
  ModelPromptsEntry,
  ZoneConfig,
} from "@/api/types";
import { DecodeVerdictChip } from "@/components/DecodeVerdictChip";
import { Badge, type BadgeProps } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import {
  defaultShapeForKind,
  describeShape,
  SHAPE_LADDER,
  shapeFromKey,
  shapeKey,
  shapesForKind,
} from "@/lib/model-sizes";
import {
  analysisStreamLabel,
  decodeSummaryText,
  decodeVerdictPresentation,
} from "@/lib/decodeCapacity";
import { useSession } from "@/lib/auth";
import { detectorInputFor, supervisorDimsFor } from "@/lib/supervisor-frame";
import {
  deriveZoneApplication,
  isDenied,
  isZonesRevisionConflict,
  markZoneSave,
  nextAcceptanceSeq,
  parseConfigSnapshot,
  receiptGeneration,
  zoneApplicationPollMs,
  zoneSaveSeq,
  type AcceptanceEpoch,
  type ReceiptRead,
  type SavedSnapshot,
  type Tagged,
  type ZoneApplication,
  type ZoneDraftState,
} from "@/lib/zone-application";
import { ZonesEditor } from "@/components/cameras/zones-editor";

// ---------------------------------------------------------------------------
// Camera editor sheet.
// ---------------------------------------------------------------------------

/**
 * SPEC-069 Phase 1 — decode verdict for one camera, with the raw counters
 * behind a disclosure so the sheet stays readable for the operators who do
 * not need them.
 *
 * Every branch below distinguishes "not read yet" from a real answer: a
 * pending or failed stats request says so rather than rendering Healthy.
 * The editor owns the stats read, which zone application shares.
 */
function CameraDecodeSection({
  stats: s,
  read,
}: {
  stats: CameraFrameStats | undefined;
  read: boolean;
}) {
  const [showCounters, setShowCounters] = useState(false);
  const p = decodeVerdictPresentation(s?.decode_verdict, read);
  const analysis = analysisStreamLabel(s?.analysis_stream);

  return (
    <div className="border-b border-border/50 px-5 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Decode
        </span>
        <DecodeVerdictChip verdict={s?.decode_verdict} read={read} />
        {analysis ? (
          <Badge variant="outline" className="font-normal">
            Analysis: {analysis}
          </Badge>
        ) : null}
        <button
          type="button"
          className="ml-auto text-xs text-muted-foreground underline-offset-2 hover:underline"
          onClick={() => setShowCounters((v) => !v)}
        >
          {showCounters ? "Hide decode counters" : "Show decode counters"}
        </button>
      </div>
      <p className="mt-1 text-xs text-muted-foreground" data-testid="decode-summary">
        {decodeSummaryText(s?.decode_summary, read)}
      </p>
      {p.verdict === "substream_unavailable" && s?.analysis_stream?.reason ? (
        <p className="mt-1 text-xs text-muted-foreground">
          Substream rejected: {s.analysis_stream.reason}.
        </p>
      ) : null}
      {showCounters ? (
        <dl className="mt-2 grid grid-cols-2 gap-x-6 gap-y-1 text-xs sm:grid-cols-3">
          <Counter label="Decoder input drops" value={s?.decoder_input_drops} read={read} />
          <Counter label="Decoder output frames" value={s?.decoder_output_frames} read={read} />
          <Counter label="Sampled frames" value={s?.sampled_frames} read={read} />
          <Counter label="Duplicate frames" value={s?.duplicate_frames} read={read} />
          <Counter label="Decoder output fps" value={s?.decoder_output_fps} read={read} />
          <Counter label="Sampled fps" value={s?.sampled_fps} read={read} />
          <Counter
            label="Decoded geometry"
            value={
              s?.decoder_width && s?.decoder_height
                ? `${s.decoder_width}×${s.decoder_height}`
                : undefined
            }
            read={read}
          />
        </dl>
      ) : null}
    </div>
  );
}

function Counter({
  label,
  value,
  read,
}: {
  label: string;
  value: number | string | undefined;
  read: boolean;
}) {
  return (
    <div>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="tabular-nums">
        {!read ? "not read" : value === undefined ? "—" : typeof value === "number" ? round2(value) : value}
      </dd>
    </div>
  );
}

function round2(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(2);
}

// ---------------------------------------------------------------------------
// SPEC-088 zone application status.
// ---------------------------------------------------------------------------

const statsKey = (id: number) => ["cameras", "stats", id] as const;
const snapshotKey = (id: number) => ["cameras", "config-snapshot", id] as const;
const saveKey = (id: number) => ["cameras", "save", id] as const;

/**
 * Drop any request in flight and start a new one, rather than joining it.
 * `refetchType: "none"` only marks the query stale, for its revalidation.
 */
function restartQuery(
  qc: QueryClient,
  queryKey: readonly unknown[],
  refetchType: "active" | "none" = "active",
) {
  void qc
    .cancelQueries({ queryKey, exact: true })
    .then(() => qc.invalidateQueries({ queryKey, exact: true, refetchType }));
}

/**
 * The zone side of the editor. `synced` is the zones array the draft last
 * matched, installed from a snapshot (`base` = its revision) or sent in a
 * save (`base` = null): the draft's zones are dirty when they are another
 * array. `installed` is the snapshot shown, with the acceptance seq of its
 * installation; `awaiting` a settled save whose snapshot is not installed
 * yet; `generation` the newest supervisor generation read here. The latest
 * save start or end is per camera, outside the editor (`zoneSaveSeq`).
 */
type ZoneView = {
  synced: ZoneConfig[] | undefined;
  base: string | null;
  installed: { read: Tagged<CameraConfigSnapshot>; seq: number } | null;
  savedAt: number | null;
  awaiting: boolean;
  generation: number;
};

type Saved = { camera: CameraConfig; zones: ZoneConfig[] | undefined; revision: string | null };

/**
 * The camera as saved, read again just before a save: whole from the camera
 * list and, with `zones`, its zones with their revision from the config
 * snapshot, read first. When that read fails (a role that may save but not
 * read it, say) the zones are the listed ones, with no revision. Throws when
 * the list cannot be read. The save expects the snapshot's revision, so
 * another save landing after this read is refused (409), never overwritten.
 * The list gives no revision to expect: that save is unconditional, as
 * before, and still overwrites one landing after it, as does a save landing
 * after the list read in any field outside the zones.
 */
async function rereadSaved(cameraId: number, zones: boolean): Promise<Saved> {
  let snap: CameraConfigSnapshot | null = null;
  if (zones) {
    try {
      snap = parseConfigSnapshot(await getCameraConfigSnapshot(cameraId));
    } catch {
      // The listed zones stand in, with no revision.
    }
  }
  const camera = (await listCameras()).find((c) => c.id === cameraId);
  if (!camera) throw new Error(`camera ${cameraId} is not listed`);
  return snap
    ? { camera, zones: snap.settings.zones, revision: snap.zones_revision }
    : { camera, zones: camera.zones, revision: null };
}

/**
 * `theirs` with every field `mine` changed from `base` (by value), and
 * `mine`'s zones, which the zone view tracks by identity.
 */
function rebase(mine: CameraConfig, base: CameraConfig, theirs: CameraConfig): CameraConfig {
  const edited = Object.entries(mine).filter(
    ([k, v]) => k === "zones" || JSON.stringify(v) !== JSON.stringify(base[k as keyof CameraConfig]),
  );
  return { ...theirs, ...Object.fromEntries(edited) };
}

/** A zone view with nothing read, showing `zones`. */
const unreadZoneView = (zones: ZoneConfig[] | undefined): ZoneView => ({
  synced: zones,
  base: null,
  installed: null,
  savedAt: null,
  awaiting: false,
  generation: -1,
});

const APPLICATION_BADGE: Record<
  ZoneApplication["kind"],
  { label: string; variant: BadgeProps["variant"] }
> = {
  unread: { label: "Reading", variant: "secondary" },
  unsupported: { label: "Unverified", variant: "secondary" },
  inactive: { label: "Inactive", variant: "secondary" },
  applying: { label: "Applying", variant: "default" },
  applied: { label: "Applied", variant: "success" },
  failed: { label: "Failed", variant: "destructive" },
  disconnected: { label: "Unconfirmed", variant: "warning" },
  changed_elsewhere: { label: "Changed", variant: "warning" },
  unsaved: { label: "Unsaved", variant: "secondary" },
};

const clock = (at: number | string) => new Date(at).toLocaleTimeString();

/** Only the verdict is a live region: the read time changes with every poll. */
function ZoneApplicationStatus({ application: a }: { application: ZoneApplication }) {
  const badge = APPLICATION_BADGE[a.kind];
  return (
    <div
      data-testid="zone-application"
      data-state={a.kind}
      className="space-y-1 rounded-md border border-border bg-muted/20 p-2.5 text-xs"
    >
      <div role="status" aria-live="polite">
        <div className="flex items-start gap-2">
          <Badge variant={badge.variant} className="shrink-0">
            {badge.label}
          </Badge>
          <span className="font-semibold">{a.title}</span>
        </div>
        <p className="mt-1 text-muted-foreground">{a.detail}</p>
      </div>
      <p className="tabular-nums text-muted-foreground">
        {a.kind === "applied" && a.appliedAt ? `Applied ${clock(a.appliedAt)} · ` : ""}
        {a.readAt !== null ? `Last read ${clock(a.readAt)}` : "No successful read yet"}
      </p>
    </div>
  );
}

/**
 * Derive the conventional ONVIF device-service URL from an RTSP URL's host —
 * `rtsp://…@192.168.1.64:554/s1` → `http://192.168.1.64/onvif/device_service`.
 * ONVIF almost always rides port 80 (not RTSP's 554) and `/onvif/device_service`
 * is the ONVIF-mandated default path most cameras honour. Returns `""` when the
 * URL has no parseable host, so the field simply stays blank. Only a starting
 * point — the operator edits it for vendors on a non-standard host/port/path.
 */
function deriveOnvifEndpoint(rtspUrl: string | null | undefined): string {
  if (!rtspUrl) return "";
  try {
    const host = new URL(rtspUrl).hostname;
    return host ? `http://${host}/onvif/device_service` : "";
  } catch {
    return "";
  }
}

export function CameraEditor({
  camera,
  existing,
  onClose,
  onSaved,
}: {
  camera: CameraConfig;
  existing: CameraConfig[];
  onClose: () => void;
  /** `created`: the save added a new camera rather than updating one. */
  onSaved: (created: boolean) => void;
}) {
  const qc = useQueryClient();
  const [draft, setDraft] = useState<CameraConfig>(camera);
  // The full-config write base: the camera as last saved here, or as opened.
  // A save sends every field the draft has not changed from it as saved by
  // then; an access reset returns the draft to it.
  const writeBase = useRef(camera);
  const [error, setError] = useState<string | null>(null);
  // Tracks whether the operator has hand-edited the ONVIF endpoint. While
  // false, the endpoint auto-tracks the RTSP URL host (see the URL input's
  // onChange). Seeded true when the camera already carries an endpoint — an
  // existing save or a discovery-supplied XAddrs — so we never clobber it.
  const onvifEndpointTouched = useRef(Boolean(camera.onvif?.endpoint));

  // Engine-wide defaults for tracker.static_object — used to label
  // the per-camera anchor-TTL override input with the fallback the
  // engine will apply when the override is blank.
  const staticDefaults = useQuery({
    queryKey: ["system", "static-object-defaults"],
    queryFn: getStaticObjectDefaults,
    staleTime: 5 * 60 * 1000,
  });
  const defaultAnchorTtlSecs = staticDefaults.data?.anchor_ttl_secs;

  const original = useMemo(
    () => existing.find((c) => String(c.id) === String(camera.id)) ?? null,
    [existing, camera.id],
  );
  const isNew = original === null;

  const draftKind = (() => {
    const mo = draft.model_override;
    return mo && typeof mo === "object"
      ? ((mo as { kind?: unknown }).kind as string | undefined)
      : undefined;
  })();
  // The router instantiates one InferenceLayer per `model_override.kind` at
  // boot and dedups by kind alone (see `crates/nexus-inference/src/router.rs`).
  // Structural changes (kind, preset, pack_path, input_width, input_height)
  // therefore only take effect after an engine restart; per-camera
  // `score_threshold` / `top_k` are honored live by the rule layer.
  const STRUCTURAL_KEYS = [
    "kind",
    "pack_path",
    "preset",
    "input_width",
    "input_height",
  ] as const;
  const restartRequired = (() => {
    if (isNew) return false;
    const a = (original?.model_override ?? null) as ModelOverride | null;
    const b = (draft.model_override ?? null) as ModelOverride | null;
    const modelChanged =
      !(a === null && b === null) &&
      STRUCTURAL_KEYS.some((k) => {
        const av = a?.[k] ?? null;
        const bv = b?.[k] ?? null;
        return av !== bv;
      });
    // `anchor_ttl_secs` is consumed by `StaticObjectFilter::new` at
    // supervisor boot; the reconciler only respawns on URL change
    // today, so editing it on a live camera demands a restart for
    // the new value to take effect.
    const ttlChanged =
      (original?.anchor_ttl_secs ?? null) !==
      (draft.anchor_ttl_secs ?? null);
    // `supervisor_width` respawns the camera (supervisor dims change) —
    // see reconciler `start_camera`.
    const supervisorChanged =
      (original?.supervisor_width ?? null) !== (draft.supervisor_width ?? null);
    return modelChanged || ttlChanged || supervisorChanged;
  })();

  // M_NATIVE_ASPECT — live tiling-geometry preview. The supervisor
  // (analysis) frame can be a larger native-16:9 ladder rung than the
  // model input; when it divides evenly by the tile grid, each tile is
  // pixel-identical to the model input (zero resampling). A camera with
  // no model override runs at the engine's default model, so until the
  // catalog is read the preview states no frame.
  const promptsCatalog = useQuery({
    queryKey: ["models", "prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 5 * 60_000,
  });
  const modelInput = detectorInputFor(
    draft.model_override as ModelOverride | null,
    promptsCatalog.data && {
      w: promptsCatalog.data.default_input_width,
      h: promptsCatalog.data.default_input_height,
    },
  );
  const tilePreview = (() => {
    if (!modelInput) {
      return {
        text: promptsCatalog.isError
          ? "Analysis frame unknown: the engine's default model could not be read."
          : "Reading the engine's default model…",
        cost: "",
        exact: undefined,
      };
    }
    const { w: modelInputW, h: modelInputH } = modelInput;
    const [supW, supH] = supervisorDimsFor(
      modelInputW,
      draft.supervisor_width,
    );
    const cols = draft.tile_grid === "g3x3" ? 3 : 2;
    const rows = cols;
    const tileW = Math.floor(supW / cols);
    const tileH = Math.floor(supH / rows);
    const exact = tileW === modelInputW && tileH === modelInputH;
    const maxTiles =
      typeof draft.tile_max_per_frame === "number"
        ? draft.tile_max_per_frame
        : 3;
    const text = exact
      ? `Analysis ${supW} × ${supH} · grid ${cols} × ${rows} → tiles ${tileW} × ${tileH} · native 1:1 ✓`
      : `Analysis ${supW} × ${supH} · grid ${cols} × ${rows} → tiles ${tileW} × ${tileH} → resampled to ${modelInputW} × ${modelInputH} ⚠`;
    const cost = `Worst case ≈ ${(maxTiles * tileW * tileH).toLocaleString()} px/frame at ${maxTiles} tile(s).`;
    return { text, cost, exact };
  })();

  // SPEC-088 — does the running camera use the zones shown? Installing a
  // saved-zone snapshot, or a save starting or ending, advances the
  // acceptance epoch; only a stats request begun after it can certify them.
  // The draft stays the full-config write base: a save sends it whole, with
  // the zones on screen, and nothing from the snapshot but its zones.
  const [zoneView, setZoneView] = useState<ZoneView>(() => unreadZoneView(camera.zones));

  // The save, its fence and its post-save snapshot read outlive this editor:
  // a reopened one sees the save pending, and its settling refreshes it.
  const mutation = useMutation({
    mutationKey: saveKey(camera.id),
    mutationFn: ({ cfg, expected }: { cfg: CameraConfig; expected?: string }) =>
      upsertCamera(cfg, expected),
    onMutate: () => markZoneSave(camera.id),
    onSuccess: () => {
      if (!isNew) toast.success("Camera saved");
      onSaved(isNew);
    },
    // A refused save is never retried: the saved zones it raced are read
    // again on settling, rebasing a clean view or showing an edit as
    // changed elsewhere, and the operator saves again.
    onError: (e: unknown) =>
      setError(
        isZonesRevisionConflict(e)
          ? "Not saved: someone else saved this camera's zones since they were last read here, so nothing was changed. Check the zones and save again."
          : e instanceof Error
            ? e.message
            : String(e),
      ),
    // A failed PUT may still have committed: read what is saved either way.
    onSettled: () => {
      markZoneSave(camera.id);
      restartQuery(qc, snapshotKey(camera.id));
    },
  });
  const saving = useIsMutating({ mutationKey: saveKey(camera.id) }) > 0;
  const resetSave = mutation.reset;

  const snapQ = useQuery({
    queryKey: snapshotKey(camera.id),
    enabled: !isNew,
    queryFn: async ({ signal }) => {
      const seq = nextAcceptanceSeq();
      return { seq, body: parseConfigSnapshot(await getCameraConfigSnapshot(camera.id, signal)) };
    },
    // Read on open, after a save and to reconcile a mismatch; never on a timer.
    staleTime: Infinity,
    gcTime: 0,
    retry: false,
    refetchOnWindowFocus: false,
  });
  // A polygon being drawn is an edit too, though not yet in the draft.
  const [sketching, setSketching] = useState(false);
  // Install a snapshot read begun after the latest save; a clean view shows
  // its zones, a dirty edit keeps the operator's. A sketch holds it until
  // finished or cancelled: it joins the zones it was drawn over, never newer.
  const snap = snapQ.data;
  if (
    snap &&
    snap !== zoneView.installed?.read &&
    !saving &&
    !sketching &&
    snap.seq > zoneSaveSeq(camera.id)
  ) {
    const clean = draft.zones === zoneView.synced;
    setZoneView((v) => ({
      ...v,
      installed: { read: snap, seq: nextAcceptanceSeq() },
      awaiting: false,
      ...(clean ? { synced: snap.body.settings.zones, base: snap.body.zones_revision } : {}),
    }));
    if (clean) setDraft((d) => ({ ...d, zones: snap.body.settings.zones }));
  }

  const sentZones = mutation.isPending ? mutation.variables?.cfg.zones : zoneView.synced;
  const zoneDraft: ZoneDraftState = {
    phase:
      sketching || draft.zones !== sentZones ? "dirty" : saving ? "saving" : zoneView.awaiting ? "saved" : "clean",
    base: zoneView.base,
    awaiting: zoneView.awaiting,
  };
  const saved: SavedSnapshot = {
    snapshot: zoneView.installed?.read.body ?? null,
    error: snapQ.error,
  };
  const epoch: AcceptanceEpoch = {
    seq: Math.max(zoneSaveSeq(camera.id), zoneView.installed?.seq ?? 0),
    generation: zoneView.generation,
  };
  const zonePollMs = (read: ReceiptRead & { error: unknown }) =>
    zoneApplicationPollMs(
      deriveZoneApplication(saved, read, zoneDraft, epoch),
      [read.error, snapQ.error],
      zoneView.savedAt,
    );
  const statsQ = useQuery({
    queryKey: statsKey(camera.id),
    enabled: !isNew,
    queryFn: async () => {
      const seq = nextAcceptanceSeq();
      return { seq, body: await getCameraStats(camera.id) };
    },
    // One read for decode health and zone application, at the receipt's
    // cadence: once a minute when Applied or Inactive, none when denied or
    // unsupported. Paused while the page is hidden, re-read at once when it
    // is visible again.
    refetchInterval: (q) => zonePollMs(q.state),
    // A failed read shows at once: an internal retry would keep the last
    // answer (an Applied, say) on screen meanwhile. The cadence reads again.
    retry: false,
    refetchOnWindowFocus: (q) => (zonePollMs(q.state) === false ? false : "always"),
  });
  const readGeneration =
    statsQ.data?.body.camera_id === camera.id ? receiptGeneration(statsQ.data.body) : undefined;
  if (readGeneration !== undefined && readGeneration > zoneView.generation) {
    setZoneView((v) => (readGeneration > v.generation ? { ...v, generation: readGeneration } : v));
  }
  const application = deriveZoneApplication(saved, statsQ, zoneDraft, epoch);
  const pollMs = zoneApplicationPollMs(application, [statsQ.error, snapQ.error], zoneView.savedAt);

  // A newly installed snapshot needs a stats read begun after it. A hidden
  // page leaves that read to its revalidation once visible again.
  const installedSeq = zoneView.installed?.seq;
  useEffect(() => {
    if (installedSeq === undefined) return;
    restartQuery(qc, statsKey(camera.id), focusManager.isFocused() ? "active" : "none");
  }, [qc, camera.id, installedSeq]);
  // A mismatch or a failed snapshot read reloads the saved zones at once,
  // then at most once per receipt interval, never while a save is pending
  // or while the page is hidden (it reconciles once visible again).
  const receiptKind = application.kind === "unsaved" ? application.receipt.kind : application.kind;
  const visible = useSyncExternalStore(focusManager.subscribe, () => focusManager.isFocused());
  const reconcile =
    visible &&
    !isNew &&
    pollMs !== false &&
    !saving &&
    !snapQ.isFetching &&
    (receiptKind === "changed_elsewhere" || snapQ.isError);
  const reconciledAt = useRef(-Infinity);
  useEffect(() => {
    if (!reconcile || Date.now() - reconciledAt.current < pollMs) return;
    reconciledAt.current = Date.now();
    void qc.refetchQueries({ queryKey: snapshotKey(camera.id), exact: true });
  }, [qc, camera.id, reconcile, pollMs, statsQ.dataUpdatedAt, statsQ.errorUpdatedAt]);

  // What this editor shows and certifies was read with the signed-in role.
  // A token refresh can carry another role (or user) without unmounting the
  // editor, and a denied read means the access is gone: either drops the
  // edit and the acceptance state, fencing out every read begun before, and
  // a role change reads the saved zones again as the new role.
  const user = useSession()?.user;
  const authority = user ? `${user.id}:${user.role}` : "";
  // A re-read of a query never answered clears its error until it settles:
  // that is no access regained, so a denial holds through it.
  const readDenied = [snapQ.error, statsQ.error].some(isDenied);
  const [denied, setDenied] = useState(readDenied);
  const rereading = snapQ.fetchStatus !== "idle" || statsQ.fetchStatus !== "idle";
  if (readDenied !== denied && !(denied && rereading)) setDenied(readDenied);
  const heldAs = useRef({ authority, denied });
  useEffect(() => {
    const was = heldAs.current;
    heldAs.current = { authority, denied };
    const changed = authority !== was.authority;
    if (!changed && (!denied || was.denied)) return;
    markZoneSave(camera.id);
    // A save begun before stays on the engine but no longer sets what this
    // editor sent or synced: its zones would read as an edit or a verdict.
    resetSave();
    const base = writeBase.current;
    onvifEndpointTouched.current = Boolean(base.onvif?.endpoint);
    setDraft(base);
    setError(null);
    setZoneView(unreadZoneView(base.zones));
    if (changed) {
      restartQuery(qc, snapshotKey(camera.id));
      restartQuery(qc, statsKey(camera.id));
    }
  }, [qc, camera.id, authority, denied, resetSave]);

  const set = <K extends keyof CameraConfig>(
    k: K,
    v: CameraConfig[K],
  ) => {
    setDraft((d) => ({ ...d, [k]: v }));
  };

  // Nested updater for the ONVIF sub-object. Collapses the whole
  // `onvif` blob back to undefined once every field is blank so the
  // engine keeps `CameraOnvif::is_empty()` true (skips it on
  // serialize) and the stored config_json shape stays unchanged for
  // the non-ONVIF majority.
  const setOnvif = (k: keyof CameraOnvif, v: string) => {
    setDraft((d) => {
      const next: CameraOnvif = {
        ...(d.onvif ?? {}),
        [k]: v.trim() === "" ? undefined : v,
      };
      const empty = !next.endpoint && !next.username && !next.password;
      return { ...d, onvif: empty ? undefined : next };
    });
  };

  // The saved zones are being read again before a save is sent.
  const [checking, setChecking] = useState(false);

  const onSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    // ID is server-assigned on create (POST /cameras returns
    // the populated row). For updates we already have it from
    // the existing row. Only URL is operator-required up front.
    if (!draft.url.trim()) {
      setError("URL is required.");
      return;
    }
    // The save replaces the camera's zones whole, expecting the revision of
    // the saved zones it replaces, so a save from elsewhere it has not read
    // is refused (409), never overwritten. Unedited zones go out as saved
    // now, not as last read here, so an unrelated edit never reverts another
    // operator's newer zones; they expect their re-read revision (see
    // `rereadSaved`). Edited zones, a drawing in progress included (it
    // freezes the zones it was drawn over), are sent as shown, expecting the
    // latest snapshot read here since the latest save, installed or held by
    // the drawing: once one has read another operator's save, the status
    // says the zones changed elsewhere and saving replaces them knowingly.
    // With no snapshot read, nothing is expected. Every other field the
    // draft has not changed from the write base goes out as saved now too,
    // so a save never reverts another operator's change to a field not
    // edited here.
    let cfg = draft;
    const read = snap && snap.seq > zoneSaveSeq(camera.id) ? snap : zoneView.installed?.read;
    let expected = read?.body.zones_revision;
    if (!isNew) {
      const zonesClean = !sketching && draft.zones === zoneView.synced;
      setChecking(true);
      let saved: Saved;
      try {
        saved = await rereadSaved(camera.id, zonesClean);
      } catch (err) {
        setError(
          `Not saved: this camera could not be re-read, so saving could undo ` +
            `another operator's changes (${err instanceof Error ? err.message : String(err)}). Try again.`,
        );
        return;
      } finally {
        setChecking(false);
      }
      cfg = rebase(draft, writeBase.current, saved.camera);
      if (zonesClean) {
        const { zones, revision } = saved;
        expected = revision ?? undefined;
        const changed =
          revision !== null
            ? revision !== zoneView.base
            : JSON.stringify(zones) !== JSON.stringify(draft.zones);
        if (changed) {
          cfg = { ...cfg, zones };
          setZoneView((v) => ({ ...v, synced: zones, base: revision }));
          setDraft((d) => (d.zones === draft.zones ? { ...d, zones } : d));
        }
      }
    }
    mutation.mutate({ cfg, expected }, {
      // Not called once `resetSave` fenced the save. The PUT answer carries
      // no revision and its zones are not applied: the post-save snapshot
      // and a fresh receipt decide. Outside the zones it is the write base
      // now, and every field not typed in since the save began shows it;
      // typing since then stays.
      onSuccess: (saved, { cfg }) => {
        writeBase.current = saved;
        setDraft((d) => rebase(d, draft, saved));
        setZoneView((v) => ({
          ...v,
          savedAt: Date.now(),
          awaiting: true,
          synced: cfg.zones,
          base: null,
        }));
      },
    });
  };

  return (
    <Sheet
      open
      onClose={onClose}
      title={isNew ? "New camera" : `Edit ${draft.name || `cam-${draft.id}`}`}
      description="Changes apply on save. Some changes (model kind) require an engine restart."
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={saving || checking}>
            {saving || checking ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      <form onSubmit={onSubmit} className="space-y-0">
        {error ? (
          <div className="border-b border-destructive/50 bg-destructive/10 px-5 py-3 text-sm text-destructive">
            {error}
          </div>
        ) : null}

        {isNew ? null : (
          <CameraDecodeSection stats={statsQ.data?.body} read={statsQ.isSuccess} />
        )}

        {restartRequired ? (
          <div className="flex items-start gap-2 border-b border-warning/40 bg-warning/10 px-5 py-3 text-xs">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            <div>
              <p className="font-semibold text-warning">
                Engine restart required
              </p>
              <p className="text-muted-foreground">
                The inference router instantiates model layers at boot,
                keyed by{" "}
                <code className="font-mono">model_override.kind</code>.
                Structural fields (kind, preset, pack path, input
                width/height) only take effect after a restart;{" "}
                <code className="font-mono">score_threshold</code> and{" "}
                <code className="font-mono">top_k</code> apply live.
              </p>
            </div>
          </div>
        ) : null}

        <SheetSection title="Identity">
          {isNew ? (
            <p className="text-xs text-muted-foreground">
              ID will be assigned by the engine on save.
            </p>
          ) : (
            <div className="space-y-2">
              <Label htmlFor="cam-id">ID</Label>
              <Input
                id="cam-id"
                value={String(draft.id)}
                disabled
                className="font-mono"
              />
            </div>
          )}
          <div className="space-y-2">
            <Label htmlFor="cam-name">Name</Label>
            <Input
              id="cam-name"
              value={draft.name ?? ""}
              onChange={(e) => set("name", e.target.value)}
              placeholder="Front door"
            />
          </div>
        </SheetSection>

        <SheetSection title="Source">
          <div className="space-y-2">
            <Label htmlFor="cam-url">RTSP URL</Label>
            <Input
              id="cam-url"
              value={draft.url}
              onChange={(e) => {
                const v = e.target.value;
                set("url", v);
                // Keep the ONVIF endpoint tracking the URL host until the
                // operator edits it by hand (see `onvifEndpointTouched`).
                if (!onvifEndpointTouched.current) {
                  setOnvif("endpoint", deriveOnvifEndpoint(v));
                }
              }}
              placeholder="rtsp://user:pass@host:554/stream"
              className="font-mono"
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="cam-fps">Max FPS (0 = unbounded)</Label>
              <Input
                id="cam-fps"
                type="number"
                min={0}
                value={draft.max_fps ?? 0}
                onChange={(e) =>
                  set("max_fps", Number.parseInt(e.target.value, 10) || 0)
                }
              />
            </div>
            <div className="flex items-end">
              <label className="inline-flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={draft.enabled !== false}
                  onChange={(e) => set("enabled", e.target.checked)}
                  className="h-4 w-4 rounded border-border"
                />
                Enabled
              </label>
            </div>
          </div>
          <div className="space-y-2">
            <Label htmlFor="cam-codec">Codec</Label>
            <select
              id="cam-codec"
              className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm"
              value={draft.codec ?? "auto"}
              onChange={(e) => {
                const v = e.target.value;
                // "auto" maps to undefined / null so the engine's
                // admin-API autodetect (RTSP DESCRIBE) decides. The
                // explicit kinds bypass autodetect and pin the
                // pipeline's decoder branch (h264 ↔ h265). The `_plus`
                // variants are vendor SVC labels for Hikvision /
                // Dahua / Uniview — picking them tells the engine the
                // wire bitstream is still standard H.26x but the
                // camera advertises an SVC tag.
                set("codec", v === "auto" ? null : (v as CodecKind));
              }}
            >
              <option value="auto">Auto-detect (recommended)</option>
              <option value="h264">H.264</option>
              <option value="h264_plus">H.264+ (Hikvision/Dahua SVC)</option>
              <option value="h265">H.265 / HEVC</option>
              <option value="h265_plus">H.265+ (Hikvision/Dahua SVC)</option>
            </select>
            <p className="text-xs text-muted-foreground">
              Auto-detect runs a one-shot RTSP DESCRIBE when you save. Pick a
              specific codec only if autodetect fails (camera requires Digest
              auth or speaks a non-H.26x codec).
            </p>
          </div>
        </SheetSection>

        <SheetSection
          title="Device control (ONVIF)"
          description="Endpoint + credentials for PTZ, imaging, and other ONVIF device-control actions. Auto-filled from ONVIF discovery, or derived from the RTSP URL host (port 80) as you type — edit it for cameras that expose the ONVIF service on a non-standard host, port, or path. Without an endpoint, every device-control action fails with “camera has no ONVIF endpoint configured”."
        >
          <div className="space-y-2">
            <Label htmlFor="cam-onvif-endpoint">Device-service URL</Label>
            <Input
              id="cam-onvif-endpoint"
              value={draft.onvif?.endpoint ?? ""}
              onChange={(e) => {
                // Any manual edit stops the auto-derive-from-URL tracking.
                onvifEndpointTouched.current = true;
                setOnvif("endpoint", e.target.value);
              }}
              placeholder="http://192.168.1.64/onvif/device_service"
              className="font-mono"
            />
            <p className="text-xs text-muted-foreground">
              The verbatim ONVIF{" "}
              <code className="font-mono">XAddrs</code> device-service URL the
              camera advertises. The host usually matches the RTSP URL, but the
              port is typically 80 (ONVIF/web), not 554.
            </p>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="cam-onvif-user">Username</Label>
              <Input
                id="cam-onvif-user"
                value={draft.onvif?.username ?? ""}
                onChange={(e) => setOnvif("username", e.target.value)}
                autoComplete="off"
                placeholder="admin"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="cam-onvif-pass">Password</Label>
              <Input
                id="cam-onvif-pass"
                type="password"
                value={draft.onvif?.password ?? ""}
                onChange={(e) => setOnvif("password", e.target.value)}
                autoComplete="new-password"
                placeholder="••••••••"
              />
            </div>
          </div>
        </SheetSection>

        <SheetSection
          title="Detector"
          description="Open-vocab prompts narrow detector output to the labels you care about."
        >
          <ModelOverrideEditor
            value={(draft.model_override ?? null) as ModelOverride | null}
            onChange={(mo) => set("model_override", mo)}
          />
          <PromptsField
            prompts={draft.prompts ?? []}
            onChange={(p) => set("prompts", p)}
            modelKind={draftKind}
          />
        </SheetSection>

        <SheetSection
          title="Behaviour"
          description="Parking-lot mode suppresses born/died churn on long-dwelling tracks."
        >
          <label className="inline-flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.parking_lot_mode === true}
              onChange={(e) => set("parking_lot_mode", e.target.checked)}
              className="h-4 w-4 rounded border-border"
            />
            Enable parking-lot mode
          </label>

          <div className="mt-3 flex flex-col gap-1 text-sm">
            <label
              htmlFor="anchor-ttl-secs"
              className="text-xs font-medium text-muted-foreground"
            >
              Static-anchor TTL (seconds)
              {typeof defaultAnchorTtlSecs === "number" && (
                <span className="ml-2 font-normal text-muted-foreground/80">
                  (engine default: {defaultAnchorTtlSecs}s)
                </span>
              )}
            </label>
            <input
              id="anchor-ttl-secs"
              type="number"
              min={0}
              step={1}
              inputMode="numeric"
              placeholder={
                typeof defaultAnchorTtlSecs === "number"
                  ? `inherit engine default (${defaultAnchorTtlSecs})`
                  : "inherit engine default"
              }
              value={
                typeof draft.anchor_ttl_secs === "number"
                  ? String(draft.anchor_ttl_secs)
                  : ""
              }
              onChange={(e) => {
                const raw = e.target.value.trim();
                if (raw === "") {
                  set("anchor_ttl_secs", null);
                  return;
                }
                const n = Number(raw);
                if (Number.isFinite(n) && n >= 0) {
                  set("anchor_ttl_secs", Math.floor(n));
                }
              }}
              disabled={draft.parking_lot_mode !== true}
              className="w-48 rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
            />
            <span className="text-xs text-muted-foreground">
              Persisted static anchors with no matching observation older than
              this many seconds are swept on the next classify tick. Blank =
              inherit the engine-wide setting (tracker.static_object.anchor_ttl_secs).
              Restart required.
            </span>
          </div>
        </SheetSection>

        <SheetSection
          title="Crowded-scene tiling"
          description="Re-run the detector on N cropped sub-regions when stage-1 finds enough objects. Recovers small/distant targets on crowded frames with no second model. Disabled when the camera's model.kind is ensemble."
        >
          {draftKind === "ensemble" ? (
            <div
              role="alert"
              className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-900 dark:text-amber-200"
            >
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>
                Tiling is incompatible with ensemble routers. Switch the camera
                off the ensemble model to enable.
              </span>
            </div>
          ) : null}
          <label className="mt-2 inline-flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.tile_enabled === true}
              onChange={(e) =>
                set("tile_enabled", e.target.checked ? true : undefined)
              }
              disabled={draftKind === "ensemble"}
              className="h-4 w-4 rounded border-border disabled:opacity-60"
            />
            Enable crowded-scene tiling
          </label>

          <div className="mt-3 grid gap-3 sm:grid-cols-3">
            <div className="flex flex-col gap-1 text-sm">
              <label
                htmlFor="tile-trigger"
                className="text-xs font-medium text-muted-foreground"
              >
                Stage-1 crowd threshold
              </label>
              <input
                id="tile-trigger"
                type="number"
                min={1}
                step={1}
                inputMode="numeric"
                placeholder="e.g. 12"
                value={
                  typeof draft.tile_trigger === "number"
                    ? String(draft.tile_trigger)
                    : ""
                }
                onChange={(e) => {
                  const raw = e.target.value.trim();
                  if (raw === "") {
                    set("tile_trigger", undefined);
                    return;
                  }
                  const n = Number(raw);
                  if (Number.isFinite(n) && n >= 1) {
                    set("tile_trigger", Math.floor(n));
                  }
                }}
                disabled={draft.tile_enabled !== true}
                className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
              />
              <span className="text-[11px] text-muted-foreground">
                Fire the cascade when stage-1 produces at least this many
                detections.
              </span>
            </div>

            <div className="flex flex-col gap-1 text-sm">
              <label
                htmlFor="tile-max"
                className="text-xs font-medium text-muted-foreground"
              >
                Max tiles per frame
              </label>
              <input
                id="tile-max"
                type="number"
                min={1}
                step={1}
                inputMode="numeric"
                placeholder="3"
                value={
                  typeof draft.tile_max_per_frame === "number"
                    ? String(draft.tile_max_per_frame)
                    : ""
                }
                onChange={(e) => {
                  const raw = e.target.value.trim();
                  if (raw === "") {
                    set("tile_max_per_frame", undefined);
                    return;
                  }
                  const n = Number(raw);
                  if (Number.isFinite(n) && n >= 1) {
                    set("tile_max_per_frame", Math.floor(n));
                  }
                }}
                disabled={draft.tile_enabled !== true}
                className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
              />
              <span className="text-[11px] text-muted-foreground">
                Cap on per-frame stage-2 inferences. Blank = engine default
                (3).
              </span>
            </div>

            <div className="flex flex-col gap-1 text-sm">
              <label
                htmlFor="tile-grid"
                className="text-xs font-medium text-muted-foreground"
              >
                Tile grid
              </label>
              <select
                id="tile-grid"
                value={draft.tile_grid ?? ""}
                onChange={(e) => {
                  const v = e.target.value;
                  if (v === "g2x2" || v === "g3x3") {
                    set("tile_grid", v);
                  } else {
                    set("tile_grid", undefined);
                  }
                }}
                disabled={draft.tile_enabled !== true}
                className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
              >
                <option value="">default (2×2)</option>
                <option value="g2x2">2×2 (four slots)</option>
                <option value="g3x3">3×3 (nine slots, more zoom)</option>
              </select>
              <span className="text-[11px] text-muted-foreground">
                Square grids preserve 16:9 framing per cell.
              </span>
            </div>
          </div>

          {/* M_NATIVE_ASPECT — analysis (supervisor) frame width, decoupled
              from the model input so tiles divide the frame 1:1. */}
          <div className="mt-3 flex flex-col gap-1 text-sm">
            <label
              htmlFor="supervisor-width"
              className="text-xs font-medium text-muted-foreground"
            >
              Analysis frame (native 16:9)
            </label>
            <select
              id="supervisor-width"
              value={
                typeof draft.supervisor_width === "number"
                  ? String(draft.supervisor_width)
                  : ""
              }
              onChange={(e) => {
                const v = e.target.value;
                set("supervisor_width", v === "" ? undefined : Number(v));
              }}
              disabled={draft.tile_enabled !== true}
              className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
            >
              <option value="">Match model input (default)</option>
              {SHAPE_LADDER.filter(
                (s) => !modelInput || s.w >= modelInput.w,
              ).map((s) => (
                <option key={s.w} value={String(s.w)}>
                  {s.tier} — {s.w} × {s.h}
                </option>
              ))}
            </select>
            <span
              className={
                tilePreview.exact === undefined
                  ? "text-[11px] text-muted-foreground"
                  : tilePreview.exact
                    ? "text-[11px] text-emerald-600 dark:text-emerald-400"
                    : "text-[11px] text-amber-600 dark:text-amber-400"
              }
            >
              {tilePreview.text}
            </span>
            <span className="text-[11px] text-muted-foreground">
              {tilePreview.cost}
            </span>
          </div>

          {/* M_PERF_CROWD E3 — adaptive detector-input downscale under
              sustained crowd. Same native-tier vocabulary as the analysis
              frame above (D6: named tiers, no free-text pixels). */}
          <div className="mt-3 flex flex-col gap-1 text-sm">
            <label
              htmlFor="downscale-target"
              className="text-xs font-medium text-muted-foreground"
            >
              Crowd downscale detector (native 16:9)
            </label>
            <select
              id="downscale-target"
              value={
                typeof draft.detector_downscale_to_width === "number"
                  ? String(draft.detector_downscale_to_width)
                  : ""
              }
              onChange={(e) => {
                const rung = SHAPE_LADDER.find(
                  (s) => String(s.w) === e.target.value,
                );
                set("detector_downscale_to_width", rung ? rung.w : undefined);
                set("detector_downscale_to_height", rung ? rung.h : undefined);
              }}
              className="rounded-md border border-border bg-background px-2 py-1 text-sm"
            >
              <option value="">Off — always full-res</option>
              {/* Until the detector input is read, offer only the saved
                  rung, so a saved value is not shown as "Off". */}
              {SHAPE_LADDER.filter((s) =>
                modelInput
                  ? s.w < modelInput.w
                  : s.w === draft.detector_downscale_to_width,
              ).map((s) => (
                <option key={s.w} value={String(s.w)}>
                  {s.tier} — {s.w} × {s.h}
                </option>
              ))}
            </select>
            <span className="text-[11px] text-muted-foreground">
              Under sustained crowd the detector swaps to this lower-res layer
              to keep cadence, then swaps back when the scene clears.
            </span>
            <div className="mt-1 flex gap-2">
              <div className="flex flex-1 flex-col gap-1">
                <label
                  htmlFor="downscale-threshold"
                  className="text-xs font-medium text-muted-foreground"
                >
                  Crowd threshold (objects)
                </label>
                <input
                  id="downscale-threshold"
                  type="number"
                  min={0}
                  inputMode="numeric"
                  value={
                    typeof draft.detector_downscale_crowded_threshold ===
                    "number"
                      ? String(draft.detector_downscale_crowded_threshold)
                      : ""
                  }
                  onChange={(e) =>
                    set(
                      "detector_downscale_crowded_threshold",
                      e.target.value === ""
                        ? undefined
                        : Number(e.target.value),
                    )
                  }
                  disabled={
                    typeof draft.detector_downscale_to_width !== "number"
                  }
                  className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
                />
              </div>
              <div className="flex flex-1 flex-col gap-1">
                <label
                  htmlFor="downscale-secs"
                  className="text-xs font-medium text-muted-foreground"
                >
                  Sustained window (s)
                </label>
                <input
                  id="downscale-secs"
                  type="number"
                  min={0}
                  inputMode="numeric"
                  value={
                    typeof draft.detector_downscale_sustained_secs === "number"
                      ? String(draft.detector_downscale_sustained_secs)
                      : ""
                  }
                  onChange={(e) =>
                    set(
                      "detector_downscale_sustained_secs",
                      e.target.value === ""
                        ? undefined
                        : Number(e.target.value),
                    )
                  }
                  disabled={
                    typeof draft.detector_downscale_to_width !== "number"
                  }
                  className="rounded-md border border-border bg-background px-2 py-1 text-sm disabled:opacity-60"
                />
              </div>
            </div>
          </div>
        </SheetSection>

        <SheetSection
          title="Zones"
          description="Draw polygon zones on the live snapshot. Click to add vertices, double-click to close, drag a vertex to move it, right-click a vertex to delete it. Coordinates are stored normalized 0..1 of the source frame."
        >
          {isNew ? null : <ZoneApplicationStatus application={application} />}
          <ZonesEditor
            // The access reset above drops the draft; this drops the
            // unfinished drawing the editor holds of its own.
            key={`${authority}:${denied}`}
            cameraId={draft.id}
            isNew={isNew}
            zones={draft.zones ?? []}
            onChange={(z) => set("zones", z)}
            onSketchingChange={setSketching}
          />
        </SheetSection>

        {!isNew ? (
          <SheetSection
            title="Visual prompts"
            description="Attach reference images to drive open-vocab detection."
          >
            <CameraVisualPromptsEditor cameraId={String(draft.id)} />
          </SheetSection>
        ) : null}
      </form>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Model override editor — kind dropdown + structural / threshold fields.
//
// Every field is optional: anything left blank is omitted from the wire
// payload, and the engine applies the `nexus_config::ModelConfig` default
// (see `crates/nexus-config/src/lib.rs`). Clearing the kind nulls the
// whole override (camera then runs the engine-wide default kind).
// ---------------------------------------------------------------------------

function ModelOverrideEditor({
  value,
  onChange,
}: {
  value: ModelOverride | null;
  onChange: (v: ModelOverride | null) => void;
}) {
  const catalog = useQuery({
    queryKey: ["models", "prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 5 * 60_000,
  });
  const entries = catalog.data?.kinds ?? [];
  const kind = value?.kind ?? "";
  const selected = entries.find((e) => e.kind === kind);
  const availableShapes = shapesForKind(kind);
  const showSizePicker = availableShapes.length > 1;
  const fixedShape = availableShapes.length === 1 ? availableShapes[0] : undefined;

  // Patch helper — merges a partial update, then strips any field that
  // collapsed to `""` / `null` / `undefined` so the wire payload only
  // carries values the operator actually set. When the resulting object
  // has no `kind`, the whole override is nulled.
  const patch = (delta: Partial<ModelOverride>) => {
    const merged: ModelOverride = { ...(value ?? {}), ...delta };
    const cleaned: ModelOverride = {};
    for (const [k, v] of Object.entries(merged)) {
      if (v === "" || v === null || v === undefined) continue;
      cleaned[k] = v;
    }
    if (!cleaned.kind) {
      onChange(null);
    } else {
      onChange(cleaned);
    }
  };

  // Snap shape fields when the kind changes so we never persist a
  // (kind, shape) combo the engine doesn't ship a file for. Per-kind
  // shape tables live in `lib/model-sizes.ts`.
  const switchKind = (nextKind: string) => {
    if (nextKind === kind) return;
    if (!nextKind) {
      // Clearing kind drops the whole override (see patch()).
      patch({
        kind: "",
        preset: undefined,
        input_width: undefined,
        input_height: undefined,
      });
      return;
    }
    const opts = shapesForKind(nextKind);
    if (opts.length === 0) {
      // No-shape kind: clear shape fields and let the engine apply its default.
      patch({
        kind: nextKind,
        preset: undefined,
        input_width: undefined,
        input_height: undefined,
      });
      return;
    }
    // Keep the current shape if the new kind ships it, else snap to Standard.
    const curKey =
      value?.input_width && value?.input_height
        ? `${value.input_width}x${value.input_height}`
        : "";
    const keep = shapeFromKey(nextKind, curKey) ?? defaultShapeForKind(nextKind);
    if (!keep) {
      patch({ kind: nextKind });
      return;
    }
    patch({
      kind: nextKind,
      preset: shapeKey(keep),
      input_width: keep.w,
      input_height: keep.h,
    });
  };

  // Parses a number input that may be empty. Returns undefined for empty
  // strings so the field is dropped from the payload, and NaN-guards bad
  // input (treats it as "unset" rather than poisoning the override).
  const num = (raw: string): number | undefined => {
    if (raw.trim() === "") return undefined;
    const n = Number(raw);
    return Number.isFinite(n) ? n : undefined;
  };

  return (
    <div className="space-y-3">
      <div className="space-y-2">
        <Label htmlFor="cam-model-kind">Model override kind</Label>
        <select
          id="cam-model-kind"
          className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
          value={kind}
          onChange={(e) => switchKind(e.target.value)}
        >
          <option value="">(use engine default)</option>
          {entries.map((e) => (
            <option key={e.kind} value={e.kind}>
              {e.kind}
              {e.loaded ? "" : " — restart engine to activate"}
            </option>
          ))}
        </select>
        {selected?.note ? (
          <p className="text-xs text-muted-foreground">{selected.note}</p>
        ) : null}
        {selected && !selected.loaded ? (
          <p className="text-xs text-amber-600">
            This kind is not currently loaded by the engine. Save the
            camera and restart the engine to materialise its inference
            layer; otherwise the camera will fall back to the default.
          </p>
        ) : null}
      </div>

      {kind ? (
        <div className="rounded-md border border-border/60 bg-muted/20 p-3 space-y-3">
          <p className="text-xs text-muted-foreground">
            Leave any field blank to inherit the engine default. The
            router dedups inference layers by <em>(kind, input shape)</em>,
            so structural fields (shape, pack path) only take effect when
            the engine restarts — pick a native-16:9 tier that the kind's
            model registry actually ships.
          </p>

          <div className="space-y-2">
            <Label htmlFor="cam-model-size">Analysis tier</Label>
            {showSizePicker ? (
              <>
                <select
                  id="cam-model-size"
                  className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
                  value={
                    value?.input_width && value?.input_height
                      ? `${value.input_width}x${value.input_height}`
                      : ""
                  }
                  onChange={(e) => {
                    const shape = shapeFromKey(kind, e.target.value);
                    if (!shape) {
                      // Clear all three coupled fields atomically.
                      patch({
                        preset: undefined,
                        input_width: undefined,
                        input_height: undefined,
                      });
                      return;
                    }
                    patch({
                      preset: shapeKey(shape),
                      input_width: shape.w,
                      input_height: shape.h,
                    });
                  }}
                >
                  <option value="">(use tier default)</option>
                  {availableShapes.map((s) => (
                    <option key={shapeKey(s)} value={shapeKey(s)}>
                      {describeShape(s)}
                    </option>
                  ))}
                </select>
                <p className="text-xs text-muted-foreground">
                  The router picks the matching
                  <code className="mx-1 font-mono">{kind}_&lt;W&gt;x&lt;H&gt;.onnx</code>
                  from the pack at boot. Named native-16:9 tiers only — a
                  mismatched shape would silently CPU-fall-back on Intel NPU
                  hardware (the engine hard fails on missing per-shape files
                  instead).
                </p>
              </>
            ) : fixedShape !== undefined ? (
              <p className="text-xs text-muted-foreground">
                Fixed at{" "}
                <code className="font-mono">
                  {fixedShape.w} × {fixedShape.h}
                </code>
                {" — "}
                <code className="font-mono">{kind}</code> ships a single
                input shape.
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                <code className="font-mono">{kind}</code> takes no detector
                input size — engine defaults apply.
              </p>
            )}
          </div>

          <div className="space-y-2">
            <Label htmlFor="cam-model-pack">Pack path</Label>
            <Input
              id="cam-model-pack"
              value={value?.pack_path ?? ""}
              onChange={(e) => patch({ pack_path: e.target.value })}
              placeholder="/abs/path/to/model-pack/"
              className="font-mono"
            />
            <p className="text-xs text-muted-foreground">
              Directory containing <code className="font-mono">models-manifest.json</code>
              and the <code className="font-mono">yolo26n_&lt;size&gt;.onnx</code>
              files. Leave blank to use the engine's default pack.
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="cam-model-score">Score threshold</Label>
            <Input
              id="cam-model-score"
              type="number"
              min={0}
              max={1}
              step={0.05}
              value={value?.score_threshold ?? ""}
              onChange={(e) =>
                patch({ score_threshold: num(e.target.value) })
              }
              placeholder="0.25"
            />
            <p className="text-xs text-muted-foreground">
              Applied live by the rule layer — no engine restart needed.
            </p>
          </div>

          {kind === "yoloe_promptfree" ? (
            <div className="space-y-2">
              <Label htmlFor="cam-model-topk">Top-K (prompt-free only)</Label>
              <Input
                id="cam-model-topk"
                type="number"
                min={0}
                step={1}
                value={value?.top_k ?? ""}
                onChange={(e) => patch({ top_k: num(e.target.value) })}
                placeholder="unbounded"
              />
              <p className="text-xs text-muted-foreground">
                Per-frame cap on detections after NMS. Blank keeps every
                detection.
              </p>
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Prompts chip input + datalist autocomplete.
// ---------------------------------------------------------------------------

function PromptsField({
  prompts,
  modelKind,
  onChange,
}: {
  prompts: string[];
  modelKind: string | undefined;
  onChange: (p: string[]) => void;
}) {
  const [value, setValue] = useState("");
  const catalog = useQuery({
    queryKey: ["models", "prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 5 * 60_000,
  });
  const suggestions = useMemo(
    () => buildPromptSuggestions(catalog.data, modelKind),
    [catalog.data, modelKind],
  );

  const add = (raw: string) => {
    const v = raw.trim();
    if (!v) return;
    if (prompts.includes(v)) return;
    onChange([...prompts, v]);
    setValue("");
  };

  return (
    <div className="space-y-2">
      <Label htmlFor="cam-prompt-input">Prompts</Label>
      <div className="flex flex-wrap gap-1">
        {prompts.length === 0 ? (
          <span className="text-xs text-muted-foreground">
            No prompts — detector emits all labels.
          </span>
        ) : (
          prompts.map((p) => (
            <span
              key={p}
              className="inline-flex items-center gap-1 rounded-md border border-border bg-muted/30 px-2 py-0.5 text-xs"
            >
              {p}
              <button
                type="button"
                onClick={() =>
                  onChange(prompts.filter((x) => x !== p))
                }
                className="rounded-sm hover:bg-muted"
                aria-label={`Remove ${p}`}
              >
                <X className="h-3 w-3" />
              </button>
            </span>
          ))
        )}
      </div>
      <div className="flex gap-2">
        <Input
          id="cam-prompt-input"
          list="cam-prompt-suggestions"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              add(value);
            }
          }}
          placeholder="excavator"
        />
        <Button type="button" variant="outline" onClick={() => add(value)}>
          Add
        </Button>
      </div>
      <datalist id="cam-prompt-suggestions">
        {suggestions.map((s) => (
          <option key={s} value={s} />
        ))}
      </datalist>
    </div>
  );
}

function buildPromptSuggestions(
  catalog: ModelPromptsCatalog | undefined,
  modelKind: string | undefined,
): string[] {
  if (!catalog) return [];
  const set = new Set<string>();
  const entries: ModelPromptsEntry[] = catalog.kinds ?? [];
  for (const entry of entries) {
    if (modelKind && entry.kind !== modelKind) continue;
    for (const label of entry.prompts ?? []) set.add(label);
  }
  // Fallback: if no model-kind filter matched anything, show everything
  // we know about so the operator still gets autocomplete hints.
  if (set.size === 0 && modelKind) {
    for (const entry of entries) {
      for (const label of entry.prompts ?? []) set.add(label);
    }
  }
  return Array.from(set).sort();
}

// ---------------------------------------------------------------------------
// Camera visual prompts editor (attach / detach).
// ---------------------------------------------------------------------------

function CameraVisualPromptsEditor({ cameraId }: { cameraId: string }) {
  const qc = useQueryClient();
  const attachmentsQuery = useQuery({
    queryKey: ["cameras", cameraId, "visual-prompts"],
    queryFn: () => listCameraVisualPrompts(cameraId),
  });
  const promptsQuery = useQuery({
    queryKey: ["visual-prompts", "list"],
    queryFn: listVisualPrompts,
  });

  const attachMutation = useMutation({
    mutationFn: (vpId: string) => attachVisualPrompt(cameraId, vpId),
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: ["cameras", cameraId, "visual-prompts"],
      }),
  });
  const detachMutation = useMutation({
    mutationFn: (vpId: string) => detachVisualPrompt(cameraId, vpId),
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: ["cameras", cameraId, "visual-prompts"],
      }),
  });

  const attached = attachmentsQuery.data ?? [];
  const attachedIds = new Set(attached.map((a) => a.visual_prompt_id));
  const available = (promptsQuery.data ?? []).filter(
    (vp) => !attachedIds.has(vp.id),
  );

  return (
    <div className="space-y-3">
      <div>
        <Label className="text-xs text-muted-foreground">Attached</Label>
        {attached.length === 0 ? (
          <p className="text-xs text-muted-foreground">None attached.</p>
        ) : (
          <ul className="space-y-1">
            {attached.map((a) => (
              <li
                key={a.visual_prompt_id}
                className="flex items-center justify-between rounded-md border border-border bg-muted/20 px-2 py-1 text-xs"
              >
                <span>
                  <span className="font-mono">{a.visual_prompt_id}</span> ·{" "}
                  <span className="text-muted-foreground">{a.label}</span>
                </span>
                <button
                  type="button"
                  onClick={() => detachMutation.mutate(a.visual_prompt_id)}
                  className="rounded-sm p-1 hover:bg-muted"
                  aria-label="Detach"
                >
                  <X className="h-3 w-3" />
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div>
        <Label className="text-xs text-muted-foreground">Available</Label>
        {available.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            No more visual prompts to attach.
          </p>
        ) : (
          <ul className="space-y-1">
            {available.map((vp) => (
              <li
                key={vp.id}
                className="flex items-center justify-between rounded-md border border-border px-2 py-1 text-xs"
              >
                <span>
                  <span className="font-mono">{vp.id}</span> ·{" "}
                  <span className="text-muted-foreground">{vp.name}</span>
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => attachMutation.mutate(vp.id)}
                >
                  Attach
                </Button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

