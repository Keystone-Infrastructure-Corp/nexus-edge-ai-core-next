// Rules page — list + add/edit slide-over with CEL validate + preview.
//
// CEL editor is a CodeMirror 6 component (`<CelEditor>`) configured with
// the JS language mode for syntax highlighting — CEL is a strict subset
// of JS expression syntax so highlighting is faithful. Validation calls
// /rules/validate on demand; preview calls /rules/preview on demand.
//
// Deep-link: `/rules/$id` mounts this same component but with a route
// param, which auto-opens the editor for the matching rule (and pushes
// back to `/rules` on close).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import {
  AlertTriangle,
  CheckCircle2,
  Clock,
  Pencil,
  Play,
  Plus,
  ScrollText,
  Trash2,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import {
  deleteRule,
  listRules,
  previewRule,
  saveRule,
  validateRuleCel,
} from "@/api/config";
import { listCameras } from "@/api/system";
import { getRuleDelivery, putRuleDelivery } from "@/api/storage";
import type {
  CameraConfig,
  PreviewRuleResponse,
  RuleConfig,
  RuleDeliveryPolicy,
  RuleSeverity,
  ZoneConfig,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet, SheetSection } from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { CelEditor } from "@/components/cel-editor";
import {
  ScheduleGrid,
  cloneGrid,
  makeEmptyGrid,
} from "@/components/schedule-grid";
import { FleetManagedBadge } from "@/components/fleet-managed-badge";
import { formatAgo } from "@/lib/format";

const EMPTY_RULE: RuleConfig = {
  id: "",
  name: "",
  when: "object.label == 'person'",
  severity: "warning",
  camera_filter: null,
  zones: null,
  min_track_age_ms: 500,
  consecutive_frames: 2,
  cooldown_ms: 30_000,
  enabled: true,
};

const SEVERITIES: RuleSeverity[] = ["low", "warning", "critical"];

export function RulesPage() {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const deepLinkId = params.id;
  const rulesQuery = useQuery({
    queryKey: ["rules", "list"],
    queryFn: listRules,
    staleTime: 10_000,
  });
  // Cameras feed both the row's "Cameras" column (id → name
  // lookup) and the editor's camera / zone multi-selects. Same
  // cache key as cameras.tsx / viewer.tsx so we share data.
  const camerasQuery = useQuery({
    queryKey: ["cameras", "list"],
    queryFn: listCameras,
    staleTime: 10_000,
  });

  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<RuleConfig | null>(null);

  const rules = rulesQuery.data ?? [];
  const cameras = camerasQuery.data ?? [];

  const openNew = () => {
    setEditing({ ...EMPTY_RULE });
    setEditorOpen(true);
  };
  const openExisting = (r: RuleConfig) => {
    setEditing({ ...EMPTY_RULE, ...r });
    setEditorOpen(true);
  };
  const closeEditor = () => {
    setEditorOpen(false);
    setEditing(null);
    if (deepLinkId !== undefined) {
      navigate({ to: "/rules" });
    }
  };
  const handleSaved = () => {
    closeEditor();
    qc.invalidateQueries({ queryKey: ["rules", "list"] });
  };

  // Deep-link: auto-open editor for the rule named in the URL.
  useEffect(() => {
    if (!deepLinkId || editorOpen) return;
    const r = rules.find((x) => x.id === deepLinkId);
    if (r) openExisting(r);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deepLinkId, rules.length]);

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Rules</h1>
          <p className="text-sm text-muted-foreground">
            CEL expressions that match motion events and emit alerts.
          </p>
        </div>
        <div className="flex items-center gap-3">
          <FleetManagedBadge category="rules" />
          <Button onClick={openNew}>
            <Plus className="mr-2 h-4 w-4" />
            New rule
          </Button>
        </div>
      </header>

      <Card>
        <CardContent className="p-0">
          {rulesQuery.isLoading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-10 w-full" />
              ))}
            </div>
          ) : rules.length === 0 ? (
            <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
              <ScrollText className="h-8 w-8 opacity-50" />
              <p>No rules configured.</p>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="bg-muted/30 text-xs uppercase text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left">ID</th>
                    <th className="px-3 py-2 text-left">Name</th>
                    <th className="px-3 py-2 text-left">Severity</th>
                    <th className="px-3 py-2 text-left">When</th>
                    <th className="px-3 py-2 text-left">Cameras</th>
                    <th className="px-3 py-2 text-left">Status</th>
                    <th className="px-3 py-2 text-right">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {rules.map((r) => (
                    <RuleRow
                      key={r.id}
                      rule={r}
                      cameras={cameras}
                      onEdit={() => openExisting(r)}
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {editorOpen && editing ? (
        <RuleEditor
          rule={editing}
          existing={rules}
          cameras={cameras}
          onClose={closeEditor}
          onSaved={handleSaved}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// List row.
// ---------------------------------------------------------------------------

function RuleRow({
  rule,
  cameras,
  onEdit,
}: {
  rule: RuleConfig;
  cameras: CameraConfig[];
  onEdit: () => void;
}) {
  const qc = useQueryClient();
  const delMutation = useMutation({
    mutationFn: (id: string) => deleteRule(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["rules", "list"] }),
  });

  const enabled = rule.enabled !== false;
  const cams = rule.camera_filter ?? [];
  // Resolve numeric camera ids to display names. Falls back to
  // the raw id when a camera referenced by the rule no longer
  // exists (e.g. deleted out from under it) so the row stays
  // truthful rather than silently dropping the entry.
  const camLabels = cams.map((id) => {
    const found = cameras.find((c) => c.id === id);
    return found ? found.name || `#${id}` : `#${id} (missing)`;
  });

  return (
    <tr className="border-t border-border/40">
      <td className="px-3 py-2 font-mono text-xs text-muted-foreground">
        {rule.id}
      </td>
      <td className="px-3 py-2 font-medium">{rule.name}</td>
      <td className="px-3 py-2">
        <SeverityChip severity={(rule.severity as RuleSeverity) ?? "warning"} />
      </td>
      <td className="max-w-md truncate px-3 py-2 font-mono text-xs text-muted-foreground">
        {rule.when}
      </td>
      <td className="px-3 py-2 text-xs text-muted-foreground">
        {camLabels.length === 0 ? "all" : camLabels.join(", ")}
      </td>
      <td className="px-3 py-2">
        {enabled ? (
          <Badge variant="success">enabled</Badge>
        ) : (
          <Badge variant="secondary">disabled</Badge>
        )}
      </td>
      <td className="px-3 py-2 text-right">
        <div className="flex justify-end gap-1">
          <Button size="sm" variant="ghost" onClick={onEdit}>
            <Pencil className="h-4 w-4" />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              if (confirm(`Delete rule ${rule.id}? This cannot be undone.`)) {
                delMutation.mutate(rule.id);
              }
            }}
            disabled={delMutation.isPending}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </td>
    </tr>
  );
}

// ---------------------------------------------------------------------------
// Editor.
// ---------------------------------------------------------------------------

function RuleEditor({
  rule,
  existing,
  cameras,
  onClose,
  onSaved,
}: {
  rule: RuleConfig;
  existing: RuleConfig[];
  cameras: CameraConfig[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const [draft, setDraft] = useState<RuleConfig>(rule);
  const [validation, setValidation] = useState<{
    ok: boolean | null;
    error: string | null;
  }>({ ok: null, error: null });
  const [preview, setPreview] = useState<PreviewRuleResponse | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  const isNew = !existing.some((r) => r.id === rule.id);

  const set = <K extends keyof RuleConfig>(
    k: K,
    v: RuleConfig[K],
  ) => setDraft((d) => ({ ...d, [k]: v }));

  const validateMutation = useMutation({
    mutationFn: (when: string) => validateRuleCel(when),
    onSuccess: (r) =>
      setValidation({ ok: r.ok, error: r.error ?? null }),
    onError: (e: unknown) =>
      setValidation({
        ok: false,
        error: e instanceof Error ? e.message : String(e),
      }),
  });

  const previewMutation = useMutation({
    mutationFn: () => previewRule({ rule: draft }),
    onSuccess: (r) => {
      setPreview(r);
      setPreviewError(null);
    },
    onError: (e: unknown) => {
      setPreview(null);
      setPreviewError(e instanceof Error ? e.message : String(e));
    },
  });

  const saveMutation = useMutation({
    // `saveRule` routes new rules (empty id) to `POST /rules` for
    // server-assigned ids, mirroring the camera create path. The
    // server returns the populated config with the new id, so
    // `onSaved` can refresh the list.
    mutationFn: (cfg: RuleConfig) => saveRule(cfg),
    onSuccess: onSaved,
    onError: (e: unknown) =>
      setSaveError(e instanceof Error ? e.message : String(e)),
  });

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setSaveError(null);
    if (!draft.name.trim() || !draft.when.trim()) {
      setSaveError("Name and CEL expression are required.");
      return;
    }
    saveMutation.mutate(draft);
  };

  // Selected sets — kept normalised so toggle handlers are O(1)
  // and don't have to think about preserving null vs [] semantics
  // (the wire shape uses `null = no gate`, which we materialise
  // back from the empty case on every set).
  const selectedCameras = useMemo(
    () => new Set<number>(draft.camera_filter ?? []),
    [draft.camera_filter],
  );
  const selectedZones = useMemo(
    () => new Set<string>(draft.zones ?? []),
    [draft.zones],
  );

  const toggleCamera = (id: number) => {
    const next = new Set(selectedCameras);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    set("camera_filter", next.size === 0 ? null : Array.from(next));
  };
  const toggleZone = (id: string) => {
    const next = new Set(selectedZones);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    set("zones", next.size === 0 ? null : Array.from(next));
  };

  // Zones across all cameras the rule could match against. When
  // `camera_filter` is non-empty we restrict to those cameras'
  // zones — picking a zone outside the camera scope can never
  // fire (engine intersects per-camera at eval time) and is pure
  // confusion. Falls back to every camera's zones when no filter
  // is set, so an "all cameras" rule can still target a zone.
  type ZoneOption = {
    zone: ZoneConfig;
    camera: CameraConfig;
  };
  const zoneOptions: ZoneOption[] = useMemo(() => {
    const scope =
      (draft.camera_filter?.length ?? 0) > 0
        ? cameras.filter((c) => selectedCameras.has(c.id))
        : cameras;
    const seen = new Set<string>();
    const out: ZoneOption[] = [];
    for (const cam of scope) {
      for (const z of cam.zones ?? []) {
        // Dedupe by zone id across cameras — id collisions are
        // rare (zone ids are globally unique in practice) but
        // if they happen we keep the first camera's binding so
        // the displayed parent stays stable.
        if (seen.has(z.id)) continue;
        seen.add(z.id);
        out.push({ zone: z, camera: cam });
      }
    }
    return out;
  }, [cameras, draft.camera_filter, selectedCameras]);

  // Zones the user previously picked that aren't in the current
  // scope (e.g. their parent camera was removed from the filter
  // after selection). Surface them at the bottom of the zone
  // picker as toggleable chips so the user can see + remove them
  // rather than the selection silently disappearing.
  const orphanZones = useMemo(
    () =>
      (draft.zones ?? []).filter(
        (id) => !zoneOptions.some((o) => o.zone.id === id),
      ),
    [draft.zones, zoneOptions],
  );

  return (
    <Sheet
      open
      onClose={onClose}
      title={isNew ? "New rule" : `Edit ${draft.name || draft.id}`}
      description="Validate the CEL expression before saving — engine rejects PUT with a 400 if it doesn't compile."
      width="max-w-3xl"
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={saveMutation.isPending}>
            {saveMutation.isPending ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      <form onSubmit={onSubmit} className="space-y-0">
        {saveError ? (
          <div className="border-b border-destructive/50 bg-destructive/10 px-5 py-3 text-sm text-destructive">
            {saveError}
          </div>
        ) : null}

        <SheetSection title="Identity">
          <div className="grid grid-cols-2 gap-3">
            {/* Existing rules show their immutable id; new rules
                omit the field entirely — the engine assigns a
                `rule-<N>` id server-side on POST, matching the
                camera create UX. */}
            {isNew ? (
              <div className="space-y-2">
                <Label>ID</Label>
                <p className="flex h-9 items-center rounded-md border border-dashed border-input bg-muted/30 px-2 font-mono text-xs text-muted-foreground">
                  Auto-assigned on save
                </p>
              </div>
            ) : (
              <div className="space-y-2">
                <Label htmlFor="rule-id">ID</Label>
                <Input
                  id="rule-id"
                  value={draft.id}
                  disabled
                  className="font-mono"
                />
              </div>
            )}
            <div className="space-y-2">
              <Label htmlFor="rule-name">Name</Label>
              <Input
                id="rule-name"
                value={draft.name}
                onChange={(e) => set("name", e.target.value)}
                placeholder="Person in dwell zone"
              />
            </div>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="rule-severity">Severity</Label>
              <select
                id="rule-severity"
                className="h-9 w-full rounded-md border border-input bg-transparent px-2 text-sm"
                value={(draft.severity as string) ?? "warning"}
                onChange={(e) => set("severity", e.target.value)}
              >
                {SEVERITIES.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
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
        </SheetSection>

        <SheetSection
          title="Predicate (CEL)"
          description="Evaluated per motion event. Variables: object, track, frame, camera, zones."
        >
          <CelEditor
            value={draft.when}
            onChange={(next) => {
              set("when", next);
              setValidation({ ok: null, error: null });
            }}
            minHeight="9rem"
          />
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => validateMutation.mutate(draft.when)}
              disabled={validateMutation.isPending || !draft.when.trim()}
            >
              <CheckCircle2 className="mr-1 h-4 w-4" />
              {validateMutation.isPending ? "Validating…" : "Validate"}
            </Button>
            {validation.ok === true ? (
              <Badge variant="success">CEL OK</Badge>
            ) : validation.ok === false ? (
              <span className="text-xs text-destructive">
                {validation.error}
              </span>
            ) : null}
          </div>
        </SheetSection>

        <SheetSection
          title="Gates"
          description="Restrict where the rule fires. Leave both empty to match every camera and every zone."
        >
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <Label>Cameras</Label>
              <span className="text-xs text-muted-foreground">
                {selectedCameras.size === 0
                  ? "All cameras"
                  : `${selectedCameras.size} selected`}
              </span>
            </div>
            {cameras.length === 0 ? (
              <p className="rounded-md border border-dashed border-input bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
                No cameras configured yet — rule will match every
                camera once any are added.
              </p>
            ) : (
              <div className="max-h-44 overflow-y-auto rounded-md border border-input bg-background">
                {cameras.map((c) => (
                  <label
                    key={c.id}
                    className="flex cursor-pointer items-center gap-2 border-b border-border/40 px-3 py-1.5 text-sm last:border-b-0 hover:bg-muted/40"
                  >
                    <input
                      type="checkbox"
                      checked={selectedCameras.has(c.id)}
                      onChange={() => toggleCamera(c.id)}
                      className="h-4 w-4 rounded border-border"
                    />
                    <span className="flex-1 truncate">
                      {c.name || `Camera #${c.id}`}
                    </span>
                    <span className="font-mono text-xs text-muted-foreground">
                      #{c.id}
                    </span>
                  </label>
                ))}
              </div>
            )}
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <Label>Zones</Label>
              <span className="text-xs text-muted-foreground">
                {selectedZones.size === 0
                  ? "All zones"
                  : `${selectedZones.size} selected`}
              </span>
            </div>
            {zoneOptions.length === 0 && orphanZones.length === 0 ? (
              <p className="rounded-md border border-dashed border-input bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
                {cameras.length === 0
                  ? "Add a camera with zones to enable zone gating."
                  : (draft.camera_filter?.length ?? 0) > 0
                    ? "Selected cameras have no zones defined."
                    : "No zones defined on any camera."}
              </p>
            ) : (
              <div className="max-h-44 overflow-y-auto rounded-md border border-input bg-background">
                {zoneOptions.map(({ zone, camera }) => (
                  <label
                    key={zone.id}
                    className="flex cursor-pointer items-center gap-2 border-b border-border/40 px-3 py-1.5 text-sm last:border-b-0 hover:bg-muted/40"
                  >
                    <input
                      type="checkbox"
                      checked={selectedZones.has(zone.id)}
                      onChange={() => toggleZone(zone.id)}
                      className="h-4 w-4 rounded border-border"
                    />
                    <span className="flex-1 truncate">
                      <span className="text-muted-foreground">
                        {camera.name || `Camera #${camera.id}`}
                      </span>
                      <span className="px-1 text-muted-foreground">›</span>
                      <span>{zone.name || zone.id}</span>
                      {zone.kind && zone.kind !== "inclusion" ? (
                        <span className="ml-2 text-xs text-muted-foreground">
                          ({zone.kind})
                        </span>
                      ) : null}
                    </span>
                    <span className="font-mono text-xs text-muted-foreground">
                      {zone.id}
                    </span>
                  </label>
                ))}
                {orphanZones.map((id) => (
                  <label
                    key={id}
                    className="flex cursor-pointer items-center gap-2 border-b border-border/40 bg-amber-500/5 px-3 py-1.5 text-sm last:border-b-0 hover:bg-amber-500/10"
                  >
                    <input
                      type="checkbox"
                      checked
                      onChange={() => toggleZone(id)}
                      className="h-4 w-4 rounded border-border"
                    />
                    <span className="flex-1 truncate font-mono text-xs">
                      {id}
                    </span>
                    <span className="text-xs text-amber-600 dark:text-amber-400">
                      out of scope
                    </span>
                  </label>
                ))}
              </div>
            )}
          </div>
        </SheetSection>

        <SheetSection title="Debounce">
          <div className="grid grid-cols-3 gap-3">
            <div className="space-y-2">
              <Label htmlFor="rule-min-age">Min track age (ms)</Label>
              <Input
                id="rule-min-age"
                type="number"
                min={0}
                value={draft.min_track_age_ms ?? 0}
                onChange={(e) =>
                  set(
                    "min_track_age_ms",
                    Number.parseInt(e.target.value, 10) || 0,
                  )
                }
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="rule-frames">Consecutive frames</Label>
              <Input
                id="rule-frames"
                type="number"
                min={1}
                value={draft.consecutive_frames ?? 1}
                onChange={(e) =>
                  set(
                    "consecutive_frames",
                    Number.parseInt(e.target.value, 10) || 1,
                  )
                }
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="rule-cooldown">Cooldown (ms)</Label>
              <Input
                id="rule-cooldown"
                type="number"
                min={0}
                value={draft.cooldown_ms ?? 0}
                onChange={(e) =>
                  set(
                    "cooldown_ms",
                    Number.parseInt(e.target.value, 10) || 0,
                  )
                }
              />
            </div>
          </div>
        </SheetSection>

        {!isNew ? (
          <SheetSection
            title="Delivery"
            description="Override the global delivery schedule for this rule. Default is to inherit."
          >
            <RuleDeliveryEditor ruleId={draft.id} />
          </SheetSection>
        ) : null}

        <SheetSection
          title="Preview"
          description="Replays this rule against the last 24h of motion events. No debounce applied."
        >
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => previewMutation.mutate()}
            disabled={previewMutation.isPending || !draft.when.trim()}
          >
            <Play className="mr-1 h-4 w-4" />
            {previewMutation.isPending ? "Running…" : "Run preview"}
          </Button>
          {previewError ? (
            <p className="text-xs text-destructive">{previewError}</p>
          ) : null}
          {preview ? <PreviewResults preview={preview} /> : null}
        </SheetSection>
      </form>
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Preview results panel.
// ---------------------------------------------------------------------------

function PreviewResults({ preview }: { preview: PreviewRuleResponse }) {
  const topLabels = useMemo(
    () =>
      [...preview.scanned_labels]
        .sort((a, b) => b.matched - a.matched)
        .slice(0, 8),
    [preview.scanned_labels],
  );

  return (
    <div className="space-y-3 rounded-md border border-border bg-muted/20 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant="outline">
          {preview.matches.length} match
          {preview.matches.length === 1 ? "" : "es"}
        </Badge>
        <Badge variant="outline">{preview.scanned} scanned</Badge>
        {preview.limit_hit ? (
          <Badge variant="warning">limit hit</Badge>
        ) : null}
        {preview.zone_filtered > 0 ? (
          <Badge variant="secondary">
            {preview.zone_filtered} zone-filtered
          </Badge>
        ) : null}
        {preview.eval_errors > 0 ? (
          <Badge variant="destructive">
            {preview.eval_errors} eval errors
          </Badge>
        ) : null}
      </div>

      {preview.error ? (
        <div className="flex items-start gap-2 text-destructive">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          <span>{preview.error}</span>
        </div>
      ) : null}

      {preview.eval_first_error ? (
        <p className="text-muted-foreground">
          First eval error:{" "}
          <span className="font-mono">{preview.eval_first_error}</span>
        </p>
      ) : null}

      {topLabels.length > 0 ? (
        <div>
          <p className="mb-1 text-muted-foreground">Top labels</p>
          <table className="w-full">
            <thead className="text-muted-foreground">
              <tr>
                <th className="text-left">label</th>
                <th className="text-right">count</th>
                <th className="text-right">matched</th>
                <th className="text-right">zone-filt</th>
              </tr>
            </thead>
            <tbody>
              {topLabels.map((l) => (
                <tr key={l.label} className="border-t border-border/40">
                  <td className="py-0.5">{l.label}</td>
                  <td className="text-right">{l.count}</td>
                  <td className="text-right">{l.matched}</td>
                  <td className="text-right">{l.zone_filtered}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      {preview.matches.length > 0 ? (
        <div>
          <p className="mb-1 text-muted-foreground">
            First {Math.min(preview.matches.length, 5)} match
            {preview.matches.length === 1 ? "" : "es"}
          </p>
          <ul className="space-y-1">
            {preview.matches.slice(0, 5).map((m) => (
              <li
                key={m.motion_event_id}
                className="flex items-center justify-between gap-2 rounded-md border border-border/60 bg-background/50 px-2 py-1"
              >
                <span className="flex items-center gap-2">
                  <Clock className="h-3 w-3 text-muted-foreground" />
                  {formatAgo(m.captured_at)}
                </span>
                <span className="font-mono text-muted-foreground">
                  cam {String(m.camera_id)} · {m.label} ·{" "}
                  {(m.confidence * 100).toFixed(0)}%
                </span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Bits.
// ---------------------------------------------------------------------------

function SeverityChip({ severity }: { severity: RuleSeverity | string }) {
  const variant: "secondary" | "warning" | "destructive" =
    severity === "critical"
      ? "destructive"
      : severity === "warning"
        ? "warning"
        : "secondary";
  return (
    <Badge variant={variant} className="capitalize">
      {severity}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Per-rule delivery override editor.
//
// Three states:
//   1. Inherit (policy === null on the row): rule uses global settings.
//   2. Override + enabled=false: drop everything from this rule.
//   3. Override + enabled=true: optional `schedule` restricts delivery
//      to the painted half-hour slots.
//
// The engine round-trips the *override* row only; effective policy is
// computed server-side on every event dispatch. We surface the inherited
// effective policy as read-only context above the toggle.
// ---------------------------------------------------------------------------

function RuleDeliveryEditor({ ruleId }: { ruleId: string }) {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["rule-delivery", ruleId],
    queryFn: () => getRuleDelivery(ruleId),
    staleTime: 5_000,
  });

  const [mode, setMode] = useState<"inherit" | "override">("inherit");
  const [enabled, setEnabled] = useState(true);
  const [scheduleOn, setScheduleOn] = useState(false);
  const [grid, setGrid] = useState<boolean[][]>(() => makeEmptyGrid());
  const [saveError, setSaveError] = useState<string | null>(null);

  // Hydrate local form state from server response once it lands.
  useEffect(() => {
    if (!q.data) return;
    if (q.data.policy === null) {
      setMode("inherit");
      setEnabled(q.data.effective.enabled);
      setScheduleOn(q.data.effective.schedule != null);
      setGrid(q.data.effective.schedule?.grid ?? makeEmptyGrid());
    } else {
      setMode("override");
      setEnabled(q.data.policy.enabled);
      setScheduleOn(q.data.policy.schedule != null);
      setGrid(q.data.policy.schedule?.grid ?? makeEmptyGrid());
    }
    setSaveError(null);
  }, [q.data]);

  const saveMutation = useMutation({
    mutationFn: (req: { policy: RuleDeliveryPolicy | null }) =>
      putRuleDelivery(ruleId, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["rule-delivery", ruleId] });
    },
    onError: (e: unknown) =>
      setSaveError(e instanceof Error ? e.message : String(e)),
  });

  const onSave = () => {
    setSaveError(null);
    if (mode === "inherit") {
      saveMutation.mutate({ policy: null });
      return;
    }
    saveMutation.mutate({
      policy: {
        enabled,
        schedule: scheduleOn ? { grid: cloneGrid(grid) } : null,
      },
    });
  };

  if (q.isLoading) {
    return <Skeleton className="h-24 w-full" />;
  }
  if (q.isError) {
    return (
      <p className="text-sm text-destructive">
        Could not load delivery policy:{" "}
        {q.error instanceof Error ? q.error.message : String(q.error)}
      </p>
    );
  }

  return (
    <div className="space-y-4">
      <div className="rounded-md border border-border/40 bg-muted/20 px-3 py-2 text-xs text-muted-foreground">
        <span className="font-semibold">Effective policy:</span>{" "}
        {q.data!.effective.enabled
          ? q.data!.effective.schedule
            ? "on, restricted to schedule"
            : "on, no schedule"
          : "off"}{" "}
        ({q.data!.inherited ? "inherited from global" : "from this rule"})
      </div>

      <div className="space-y-2">
        <label className="flex items-center gap-2">
          <input
            type="radio"
            name="rule-delivery-mode"
            checked={mode === "inherit"}
            onChange={() => setMode("inherit")}
          />
          <span className="text-sm">Inherit global delivery settings</span>
        </label>
        <label className="flex items-center gap-2">
          <input
            type="radio"
            name="rule-delivery-mode"
            checked={mode === "override"}
            onChange={() => setMode("override")}
          />
          <span className="text-sm">Override for this rule</span>
        </label>
      </div>

      {mode === "override" ? (
        <div className="ml-6 space-y-3 border-l-2 border-border/40 pl-4">
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            <span className="text-sm font-medium">
              Enable delivery for this rule
            </span>
          </label>

          {enabled ? (
            <div className="space-y-2">
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={scheduleOn}
                  onChange={(e) => setScheduleOn(e.target.checked)}
                />
                <span className="text-sm">
                  Restrict to a weekly schedule
                </span>
              </label>
              {scheduleOn ? (
                <ScheduleGrid grid={grid} onChange={setGrid} />
              ) : null}
            </div>
          ) : null}
        </div>
      ) : null}

      {saveError ? (
        <p className="text-sm text-destructive">{saveError}</p>
      ) : null}

      <div className="flex justify-end">
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={saveMutation.isPending}
          onClick={onSave}
        >
          {saveMutation.isPending ? "Saving…" : "Save delivery policy"}
        </Button>
      </div>
    </div>
  );
}
