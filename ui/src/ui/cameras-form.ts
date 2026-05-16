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
import type {
  CameraConfig,
  CameraId,
  ModelConfig,
} from "../api/types.js";

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

  const errors: Record<keyof FormState | "model_override_text", string | undefined> = {
    id: undefined,
    name: undefined,
    url: undefined,
    enabled: undefined,
    prompts: undefined,
    max_fps: undefined,
    parking_lot_mode: undefined,
    model_override_text: undefined,
  };

  // The form host is rebuilt from scratch on every re-render so we can
  // surface field-level errors without growing a small reactive layer.
  const formHost = h("div", { class: "camera-form" });

  function rerender(): void {
    while (formHost.firstChild) formHost.removeChild(formHost.firstChild);
    formHost.append(buildForm(state, errors, opts));
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

  const footer = dialogFooter({
    cancelLabel: "Cancel",
    confirmLabel: opts.mode === "create" ? "Add camera" : "Save",
    onCancel: () => dlg?.close(false),
    onConfirm: () => void onSave(),
  });

  dlg = openDialog({
    title: opts.mode === "create" ? "Add camera" : `Edit camera ${state.id}`,
    body: formHost,
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
  };
}

function buildForm(
  state: FormState,
  errors: Record<string, string | undefined>,
  opts: OpenCameraFormOpts,
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
      ChipsInput({
        label: "Prompts",
        value: state.prompts,
        placeholder: "person, car, package…",
        helpText: "Open-vocab prompts (yolo_world) or labels-of-interest (ensemble).",
        onChange: (next) => {
          state.prompts = next;
        },
      }),
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
    advancedExpander(state, errors),
  );
}

function advancedExpander(
  state: FormState,
  errors: Record<string, string | undefined>,
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
        "Per-camera ModelConfig override. Leave blank to inherit the engine's default.",
      ...(errors["model_override_text"] !== undefined
        ? { error: errors["model_override_text"] }
        : {}),
      onChange: (v) => {
        state.model_override_text = v;
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
  };
}
