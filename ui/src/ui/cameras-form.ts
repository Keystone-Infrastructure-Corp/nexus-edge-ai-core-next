// M-Admin Phase 1 — Cameras CRUD form.
//
// Opens an `openDialog()` modal with a fully-typed editor for one
// `CameraConfig`. Used by both the "+ New camera" and "Edit" buttons
// in `cameras.ts`. Returns `true` if the user saved (and the engine
// accepted the upsert), `false` otherwise.
//
// Validation strategy: cheap client-side checks for the obvious
// mistakes (empty name, malformed URL, bad JSON in `model_override`,
// duplicate id on create, max_fps < 0). Anything subtler — bad
// scheme, unreachable host, etc. — falls through to the engine and
// surfaces as `toast.error(err)`. The dialog stays open on engine
// error so the operator can fix and retry without re-typing.

import { api } from "../api/client.js";
import { h } from "../lib/el.js";
import { openDialog, dialogFooter, type DialogHandle } from "../lib/dialog.js";
import {
  TextField,
  NumberField,
  TextArea,
  Toggle,
  ChipsInput,
  FormSection,
  FieldRow,
} from "../lib/forms.js";
import { toast } from "../lib/toast.js";
import { renderAuditHistory } from "./audit-history.js";
import type {
  CameraConfig,
  CameraId,
  DetectorPromptInfo,
  ModelConfig,
  ModelPromptsResponse,
  ZoneConfig,
} from "../api/types.js";
import { openZonesEditor } from "./zones-editor.js";

export interface OpenCameraFormOpts {
  /// `"create"` opens a blank form with an auto-suggested id.
  /// `"edit"` pre-fills from `existing` and disables the id input.
  mode: "create" | "edit";
  existing?: CameraConfig;
  /// All camera ids currently in the table, used to pick the next
  /// suggested id on create AND to flag duplicate-id collisions.
  existingIds: ReadonlyArray<CameraId>;
  /// Optional create-mode pre-fill (M-Admin Phase 1B Step 4).
  /// The Discover dialog uses this to drop a vendor/model-derived
  /// name and a guessed RTSP URL into the create form so the
  /// operator only has to confirm + Save.
  prefill?: {
    name?: string;
    url?: string;
  };
}

interface FormState {
  id: number;
  name: string;
  url: string;
  enabled: boolean;
  prompts: string[];
  max_fps: number;
  parking_lot_mode: boolean;
  /// Raw JSON text in the Advanced expander. Empty string = "no
  /// override" (sent as `null`). Validated on Save.
  model_override_text: string;
  /// Polygon zones — edited via the zones-editor modal. Always
  /// round-tripped through the form so an edit of an unrelated
  /// field does not silently drop zones loaded from TOML.
  zones: ZoneConfig[];
}

const URL_HELP =
  "Supported schemes: rtsp:// · rtsps:// · file:// · virtual://";

export function openCameraForm(opts: OpenCameraFormOpts): Promise<boolean> {
  const initial = buildInitialState(opts);
  const state: FormState = { ...initial };

  // Track the dialog handle so the Save button can close on success.
  // The closure captures it via `let` because the buttons are wired up
  // before the dialog is created (chicken-and-egg with `dialogFooter`).
  let dlg: DialogHandle | null = null;
  let saving = false;

  // M-Admin Phase 5 — detector prompt catalog. Loaded asynchronously
  // so the dialog can open instantly; once it arrives we re-render
  // the prompts field with kind-appropriate suggestions. Null while
  // loading; remains null on transport failure (the form still works
  // — it just falls back to a plain free-text ChipsInput with no
  // chip strip and no <datalist>).
  let catalog: ModelPromptsResponse | null = null;

  const errors: Record<keyof FormState | "model_override_text", string | undefined> = {
    id: undefined,
    name: undefined,
    url: undefined,
    enabled: undefined,
    prompts: undefined,
    max_fps: undefined,
    parking_lot_mode: undefined,
    model_override_text: undefined,
    zones: undefined,
  };

  // The form host is rebuilt from scratch on every re-render so we can
  // surface field-level errors without growing a small reactive layer.
  const formHost = h("div", { class: "camera-form" });

  function rerender(): void {
    while (formHost.firstChild) formHost.removeChild(formHost.firstChild);
    try {
      formHost.append(buildForm(state, errors, opts, rerender, catalog));
    } catch (err) {
      // A throw here used to leave the modal completely blank because
      // the only caller that can throw asynchronously is the catalog
      // fetch's .then(), whose .catch() silently swallows the error.
      // Surface it visibly so the operator (and devs) see what went
      // wrong instead of staring at an empty dialog.
      // eslint-disable-next-line no-console
      console.error("camera form render failed:", err);
      const msg = err instanceof Error ? err.message : String(err);
      formHost.append(
        h(
          "div",
          { class: "form-error-banner" },
          h("strong", null, "Form failed to render."),
          h("div", null, msg),
        ),
      );
    }
  }

  async function onSave(): Promise<void> {
    if (saving) return;
    if (!validate(state, opts, errors)) {
      rerender();
      toast.error("Fix the highlighted fields and try again.");
      return;
    }
    const payload = toCameraConfig(state);
    saving = true;
    try {
      await api.cameras.upsert(payload);
      toast.success(
        opts.mode === "create"
          ? `Camera ${payload.id} added`
          : `Camera ${payload.id} saved`,
      );
      dlg?.close(true);
    } catch (err) {
      toast.error(`Save failed: ${(err as Error).message}`);
    } finally {
      saving = false;
    }
  }

  rerender();

  // Fire-and-forget catalog fetch. The form is fully usable before
  // it returns; once it arrives we re-render so the prompts chooser
  // matches the active detector kind.
  api.models
    .prompts()
    .then((cat) => {
      catalog = cat;
      rerender();
    })
    .catch((err) => {
      // Don't toast — the form still works without suggestions.
      // eslint-disable-next-line no-console
      console.warn("model prompts catalog unavailable:", err);
    });

  const footer = dialogFooter({
    cancelLabel: "Cancel",
    confirmLabel: opts.mode === "create" ? "Add camera" : "Save",
    onCancel: () => dlg?.close(false),
    onConfirm: () => void onSave(),
  });

  // M6 Phase 4 Step 4.2 — surface the audit trail for an existing
  // camera below the form. Closed by default; lazy-loads on
  // expand. No-op for create mode (the row doesn't exist yet).
  const bodyHost = h("div", null, formHost);
  if (opts.mode === "edit" && opts.existing) {
    bodyHost.append(
      renderAuditHistory({
        resourceKind: "camera",
        resourceId: String(opts.existing.id),
      }),
    );
  }

  dlg = openDialog({
    title: opts.mode === "create" ? "Add camera" : `Edit camera ${state.id}`,
    body: bodyHost,
    footer,
    width: "640px",
  });

  return dlg.closed;
}

function buildInitialState(opts: OpenCameraFormOpts): FormState {
  if (opts.mode === "edit" && opts.existing) {
    return {
      id: opts.existing.id,
      name: opts.existing.name,
      url: opts.existing.url,
      enabled: opts.existing.enabled,
      prompts: opts.existing.prompts ? [...opts.existing.prompts] : [],
      max_fps: opts.existing.max_fps ?? 0,
      parking_lot_mode: opts.existing.parking_lot_mode ?? false,
      model_override_text: opts.existing.model_override
        ? JSON.stringify(opts.existing.model_override, null, 2)
        : "",
      zones: opts.existing.zones ? opts.existing.zones.map(cloneZone) : [],
    };
  }
  // Create — auto-suggest `max(existing) + 1`, falling back to 1.
  const nextId =
    opts.existingIds.length === 0 ? 1 : Math.max(...opts.existingIds) + 1;
  return {
    id: nextId,
    name: opts.prefill?.name ?? "",
    url: opts.prefill?.url ?? "",
    enabled: true,
    prompts: [],
    max_fps: 0,
    parking_lot_mode: false,
    model_override_text: "",
    zones: [],
  };
}

function cloneZone(z: ZoneConfig): ZoneConfig {
  return {
    id: z.id,
    name: z.name,
    polygon: z.polygon.map(([x, y]) => [x, y] as [number, number]),
    ...(z.kind !== undefined ? { kind: z.kind } : {}),
  };
}

function buildForm(
  state: FormState,
  errors: Record<string, string | undefined>,
  opts: OpenCameraFormOpts,
  rerender: () => void,
  catalog: ModelPromptsResponse | null,
): HTMLElement {
  const idField =
    opts.mode === "edit"
      ? readOnlyField("ID", String(state.id))
      : NumberField({
          label: "ID",
          value: state.id,
          required: true,
          min: 1,
          step: 1,
          helpText: `Auto-suggested as max(existing) + 1. Must be unique.`,
          ...(errors["id"] !== undefined ? { error: errors["id"] } : {}),
          onChange: (v) => {
            state.id = Math.max(1, Math.floor(v));
          },
        });

  // Repaint shim — the prompts subtree re-renders on its own when
  // the chip list changes, but it ALSO needs to re-paint when the
  // operator edits `model_override` (the active detector kind may
  // flip between yolo and yolo_world). The advancedExpander calls
  // this on every textarea input.
  const promptRepaint: { fn: () => void } = { fn: () => {} };

  return h(
    "div",
    null,
    FormSection(
      "Basics",
      FieldRow(
        idField,
        TextField({
          label: "Name",
          value: state.name,
          required: true,
          placeholder: "Front door",
          ...(errors["name"] !== undefined ? { error: errors["name"] } : {}),
          onChange: (v) => {
            state.name = v;
          },
        }),
      ),
      TextField({
        label: "URL",
        value: state.url,
        required: true,
        placeholder: "rtsp://user:pass@192.168.1.42:554/stream1",
        helpText: URL_HELP,
        ...(errors["url"] !== undefined ? { error: errors["url"] } : {}),
        onChange: (v) => {
          state.url = v.trim();
        },
      }),
      FieldRow(
        Toggle({
          label: "Enabled",
          value: state.enabled,
          helpText: "Disable to keep config but stop the pipeline.",
          onChange: (b) => {
            state.enabled = b;
          },
        }),
        Toggle({
          label: "Parking-lot mode",
          value: state.parking_lot_mode,
          helpText:
            "Enable static-object filter (drops vehicles that promote to static).",
          onChange: (b) => {
            state.parking_lot_mode = b;
          },
        }),
      ),
    ),
    FormSection(
      "Detection",
      promptsField(state, catalog, promptRepaint),
      NumberField({
        label: "Max FPS",
        value: state.max_fps,
        min: 0,
        step: 1,
        helpText: "Per-camera FPS cap. 0 = uncapped.",
        ...(errors["max_fps"] !== undefined ? { error: errors["max_fps"] } : {}),
        onChange: (v) => {
          state.max_fps = Math.max(0, Math.floor(v));
        },
      }),
    ),
    zonesSection(state, opts, rerender),
    advancedExpander(state, errors, promptRepaint),
  );
}

/// Prompts field + kind-aware label chooser.
///
/// The chooser surface adapts to whichever detector the active
/// camera will run against (default kind, or `model_override.kind`
/// if set):
///   * Closed-vocab (yolo / classifier_ensemble) → titled chip
///     strip ordered by group (People / Vehicles / …). Clicking a
///     chip toggles it in `state.prompts`.
///   * Open-vocab (yolo_world) → no chip strip; the ChipsInput
///     gets a `<datalist>` of the baked vocab so the operator
///     gets type-ahead but is still free to add prompts off the
///     list (with the obvious caveat that the detector will only
///     emit labels from its baked vocab).
///   * Unknown / no catalog → free-text only, no suggestions.
///
/// The chooser re-paints when:
///   * The catalog finishes loading (whole form rerenders).
///   * The operator edits chips (local repaint).
///   * The operator edits `model_override` in the advanced
///     expander, possibly flipping the detector kind. The
///     advancedExpander triggers `repaintRef.fn()` on every
///     keystroke; we re-resolve the effective kind from the
///     current state and rebuild the subtree without rebuilding
///     the whole dialog (so the textarea keeps focus).
function promptsField(
  state: FormState,
  catalog: ModelPromptsResponse | null,
  repaintRef: { fn: () => void },
): HTMLElement {
  const host = h("div", { class: "prompts-field-host" });

  function paint(): void {
    while (host.firstChild) host.removeChild(host.firstChild);
    const info = resolveDetectorInfo(state, catalog);
    const suggestions = info?.prompts ?? [];
    const help =
      info?.note ??
      "Open-vocab prompts (yolo_world) or labels-of-interest (ensemble).";
    host.append(
      ChipsInput({
        label: "Prompts",
        value: state.prompts,
        placeholder: info?.open_vocab
          ? "Type a prompt, then Enter (suggestions appear as you type)"
          : "person, car, package…",
        helpText: help,
        suggestions,
        onChange: (next) => {
          state.prompts = next;
          paint();
        },
      }),
    );
    if (info && !info.open_vocab && info.groups.length > 0) {
      host.append(closedVocabChooser(state, paint, info));
    }
  }

  paint();
  // Expose paint so the advanced expander can re-render after a
  // model_override edit flips the active kind.
  repaintRef.fn = paint;
  return host;
}

/// Decide which detector kind the operator's edits will produce.
/// Returns `null` when the catalog hasn't loaded yet or the
/// resolved kind isn't in the catalog (typo, brand-new kind the
/// engine knows about but this UI hasn't been rebuilt for, etc.).
function resolveDetectorInfo(
  state: FormState,
  catalog: ModelPromptsResponse | null,
): DetectorPromptInfo | null {
  if (!catalog) return null;
  // 1) Try to parse the override JSON. If it parses AND has a
  //    `kind` string, that wins. We deliberately tolerate parse
  //    failures here — the operator is mid-edit; we don't want
  //    the chip strip flashing on every keystroke.
  const text = state.model_override_text.trim();
  if (text !== "") {
    try {
      const parsed: unknown = JSON.parse(text);
      if (
        parsed &&
        typeof parsed === "object" &&
        "kind" in parsed &&
        typeof (parsed as { kind: unknown }).kind === "string"
      ) {
        const overrideKind = (parsed as { kind: string }).kind;
        const match = catalog.kinds.find((k) => k.kind === overrideKind);
        if (match) return match;
        // Override is set but the kind is unknown to the engine —
        // fall through to the default so the operator at least
        // sees a chooser instead of nothing.
      }
    } catch {
      /* mid-edit; ignore */
    }
  }
  // 2) Otherwise the camera will run against the engine default.
  const def = catalog.kinds.find((k) => k.kind === catalog.default_kind);
  return def ?? null;
}

/// Closed-vocab chip strip — renders one row per group in the
/// `info.groups` array (preserved insertion order, so the engine's
/// canonical "People → Vehicles → Animals → Carried" layout is
/// honored without re-sorting on the client).
function closedVocabChooser(
  state: FormState,
  onChange: () => void,
  info: DetectorPromptInfo,
): HTMLElement {
  const sel = new Set(state.prompts);
  const groupNodes: HTMLElement[] = info.groups.map((g) => {
    const chips = g.labels.map((label) => {
      const isOn = sel.has(label);
      return h(
        "button",
        {
          type: "button",
          class: "chip combobox-chip" + (isOn ? " chip-on" : ""),
          title: isOn
            ? `Click to remove ${label}`
            : `Click to add ${label}`,
          on: {
            click: () => {
              const idx = state.prompts.indexOf(label);
              if (idx >= 0) state.prompts.splice(idx, 1);
              else state.prompts.push(label);
              onChange();
            },
          },
        },
        label,
      );
    });
    return h(
      "div",
      { class: "coco-chooser-group" },
      h("span", { class: "coco-chooser-group-name" }, g.name),
      h("div", { class: "coco-chooser-group-chips" }, ...chips),
    );
  });

  return h(
    "div",
    { class: "coco-chooser" },
    h(
      "div",
      { class: "coco-chooser-head" },
      h(
        "span",
        { class: "field-help" },
        `Quick add — labels emitted by the active detector (${info.kind}). Click to add/remove.`,
      ),
    ),
    ...groupNodes,
  );
}

function zonesSection(
  state: FormState,
  opts: OpenCameraFormOpts,
  rerender: () => void,
): HTMLElement {
  // In create mode the camera has no live snapshot yet, so the
  // overlay editor would draft on a placeholder background. We
  // still allow it — geometry is normalized — but flag the
  // limitation so the operator knows why the backdrop is blank.
  const isCreate = opts.mode === "create";
  const label = `Polygon zones (${state.zones.length})`;
  const help = isCreate
    ? "Drafted before the camera streams — backdrop is a placeholder. Save the camera once, then re-open Edit for a live snapshot."
    : "Inclusion zones drive motion.zone_state for rules. Exclusion zones drop detections on the engine before they hit rules.";
  const editBtn = h(
    "button",
    {
      type: "button",
      class: "ghost",
      on: {
        click: () => {
          // Build a transient CameraConfig stand-in for the editor;
          // it only reads `id` and `name` for the snapshot URL +
          // dialog title.
          const cam: CameraConfig = toCameraConfig(state);
          void openZonesEditor(cam, state.zones).then((next) => {
            if (next === null) return;
            state.zones = next;
            rerender();
          });
        },
      },
    },
    state.zones.length === 0 ? "Add zones" : "Edit zones",
  );
  return FormSection(
    "Zones",
    h(
      "div",
      { class: "camera-form-zones-row" },
      h(
        "div",
        { class: "camera-form-zones-meta" },
        h("div", { class: "field-label" }, label),
        h("div", { class: "field-help" }, help),
      ),
      editBtn,
    ),
  );
}

function advancedExpander(
  state: FormState,
  errors: Record<string, string | undefined>,
  promptRepaint: { fn: () => void },
): HTMLElement {
  const details = h("details", { class: "form-section camera-form-advanced" });
  const summary = h("summary", { class: "form-section-title" }, "Advanced (model override)");
  details.append(summary);
  details.append(
    TextArea({
      label: "model_override (JSON)",
      value: state.model_override_text,
      rows: 6,
      placeholder:
        '{ "kind": "yolo26n", "score_threshold": 0.45 }  — leave blank to use the engine default',
      helpText:
        "Per-camera ModelConfig override. Leave blank to inherit the engine's default. Editing `kind` re-renders the Prompts chooser above.",
      ...(errors["model_override_text"] !== undefined
        ? { error: errors["model_override_text"] }
        : {}),
      onChange: (v) => {
        state.model_override_text = v;
        // Re-paint the prompts chooser so the chip strip /
        // suggestions follow the operator's edits without
        // requiring a save-and-reopen round trip.
        promptRepaint.fn();
      },
    }),
  );
  if (state.model_override_text.trim() !== "") {
    details.open = true;
  }
  return details;
}

function readOnlyField(label: string, value: string): HTMLElement {
  return h(
    "label",
    { class: "field" },
    h("span", { class: "field-label" }, label),
    h("input", { type: "text", value, disabled: true, readOnly: true }),
    h("span", { class: "field-help" }, "Camera id cannot be changed after creation."),
  );
}

const URL_SCHEME_RE = /^(rtsp|rtsps|file|virtual):\/\//i;

function validate(
  state: FormState,
  opts: OpenCameraFormOpts,
  errors: Record<string, string | undefined>,
): boolean {
  for (const k of Object.keys(errors)) errors[k] = undefined;
  let ok = true;

  if (opts.mode === "create") {
    if (!Number.isInteger(state.id) || state.id < 1) {
      errors["id"] = "ID must be a positive integer.";
      ok = false;
    } else if (opts.existingIds.includes(state.id)) {
      errors["id"] = `ID ${state.id} is already in use.`;
      ok = false;
    }
  }

  if (state.name.trim() === "") {
    errors["name"] = "Name is required.";
    ok = false;
  }

  const url = state.url.trim();
  if (url === "") {
    errors["url"] = "URL is required.";
    ok = false;
  } else if (!URL_SCHEME_RE.test(url)) {
    errors["url"] = "Scheme must be rtsp, rtsps, file, or virtual.";
    ok = false;
  } else {
    try {
      // `URL` requires `file://` and `rtsp://` to round-trip; `virtual://`
      // is recognised by the engine but the standards-mode `URL` parser
      // accepts it as a generic scheme.
      // eslint-disable-next-line no-new
      new URL(url);
    } catch {
      errors["url"] = "URL is not parseable.";
      ok = false;
    }
  }

  if (state.max_fps < 0) {
    errors["max_fps"] = "Max FPS cannot be negative.";
    ok = false;
  }

  const text = state.model_override_text.trim();
  if (text !== "") {
    try {
      const parsed = JSON.parse(text) as unknown;
      if (
        parsed === null ||
        typeof parsed !== "object" ||
        typeof (parsed as { kind?: unknown }).kind !== "string"
      ) {
        errors["model_override_text"] =
          'JSON must be an object with at least a string "kind" field.';
        ok = false;
      }
    } catch (err) {
      errors["model_override_text"] = `Invalid JSON: ${(err as Error).message}`;
      ok = false;
    }
  }

  return ok;
}

function toCameraConfig(state: FormState): CameraConfig {
  let modelOverride: ModelConfig | null = null;
  const text = state.model_override_text.trim();
  if (text !== "") {
    modelOverride = JSON.parse(text) as ModelConfig;
  }
  // Build the wire payload omitting `undefined` so `serde` defaults
  // apply on the engine side. We always send `prompts`, `max_fps`,
  // `enabled`, and `parking_lot_mode` because they're meaningful
  // even at their defaults; `model_override` is sent as null when
  // unset (the engine accepts both `null` and a missing field).
  return {
    id: state.id,
    name: state.name.trim(),
    url: state.url.trim(),
    enabled: state.enabled,
    prompts: state.prompts,
    max_fps: state.max_fps,
    parking_lot_mode: state.parking_lot_mode,
    model_override: modelOverride,
    zones: state.zones,
  };
}
