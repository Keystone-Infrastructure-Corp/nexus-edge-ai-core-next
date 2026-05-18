// M-Admin Phase 3 — Rules CRUD form.
//
// Sibling of `cameras-form.ts`: same dialog + form-primitive
// patterns, returns `true` on save / `false` on cancel. Covers every
// `RuleConfig` field exposed by the engine
// (`crates/nexus-config/src/lib.rs::RuleConfig`):
//
//   - id (string, readonly in edit mode)
//   - name
//   - severity (low | medium | high | critical)
//   - camera_filter (multi-select chips of cameras; empty = all)
//   - when (raw CEL — Phase 4 will replace this with a visual builder;
//           Phase 5 will add live syntax validation via
//           POST /api/rules/validate)
//   - min_track_age_ms
//   - consecutive_frames
//   - cooldown_ms
//   - enabled
//
// Client-side validation is cheap and obvious — empty name / id,
// negative numbers, empty `when`. Anything subtler (bad CEL syntax,
// references to unknown fields) currently surfaces as a 500 on
// PUT / runtime crash, which is exactly the silent-failure gap
// Phase 5 will close.

import { api } from "../api/client.js";
import { h } from "../lib/el.js";
import { openDialog, dialogFooter, type DialogHandle } from "../lib/dialog.js";
import {
  TextField,
  NumberField,
  TextArea,
  Toggle,
  Select,
  MultiSelect,
  FormSection,
  FieldRow,
} from "../lib/forms.js";
import { toast } from "../lib/toast.js";
import type {
  CameraConfig,
  CameraId,
  RuleConfig,
  RuleId,
  Severity,
} from "../api/types.js";

export interface OpenRuleFormOpts {
  mode: "create" | "edit";
  existing?: RuleConfig;
  /// All rule IDs currently in the table, used to flag duplicate-id
  /// collisions on create.
  existingIds: ReadonlyArray<RuleId>;
  /// All cameras currently configured, used to populate the
  /// camera_filter chip selector. We fetch this once in the parent
  /// and pass it through so the form can stay synchronous.
  cameras: ReadonlyArray<CameraConfig>;
}

interface FormState {
  id: string;
  name: string;
  severity: Severity;
  camera_filter: CameraId[];
  when: string;
  min_track_age_ms: number;
  consecutive_frames: number;
  cooldown_ms: number;
  enabled: boolean;
}

const SEVERITY_OPTIONS: ReadonlyArray<{ value: Severity; label: string }> = [
  { value: "low", label: "low" },
  { value: "medium", label: "medium" },
  { value: "high", label: "high" },
  { value: "critical", label: "critical" },
];

const ID_RE = /^[a-z0-9_]+$/;
const CEL_HELP =
  "CEL expression. Available: object.{label,confidence,box,age_ms,age_frames,track_id,attributes[]}, camera.id, now.{hour,day_of_week,unix_ms}.";

export function openRuleForm(opts: OpenRuleFormOpts): Promise<boolean> {
  const state: FormState = buildInitialState(opts);

  let dlg: DialogHandle | null = null;
  let saving = false;

  const errors: Record<keyof FormState, string | undefined> = {
    id: undefined,
    name: undefined,
    severity: undefined,
    camera_filter: undefined,
    when: undefined,
    min_track_age_ms: undefined,
    consecutive_frames: undefined,
    cooldown_ms: undefined,
    enabled: undefined,
  };

  const formHost = h("div", { class: "rule-form" });

  // M-Admin Phase 5 — debounce + race-guard for the on-blur CEL
  // validation. Each blur bumps `validateSeq`; only the latest
  // response is allowed to mutate `errors.when` so a slow request
  // can't clobber a fresher result.
  let validateSeq = 0;
  async function validateWhen(value: string): Promise<void> {
    const seq = ++validateSeq;
    const trimmed = value.trim();
    if (trimmed.length === 0) {
      // Empty-string is caught by the synchronous required check on save.
      // Don't fire a network call for it; just clear any stale error.
      if (errors.when !== undefined) {
        errors.when = undefined;
        rerender();
      }
      return;
    }
    try {
      const res = await api.rules.validate(trimmed);
      if (seq !== validateSeq) return; // stale response
      const next = res.ok ? undefined : (res.error ?? "Invalid CEL expression.");
      if (errors.when !== next) {
        errors.when = next;
        rerender();
      }
    } catch {
      // Network failure — don't spam the user; Save will surface the
      // real upstream error if the engine is reachable then.
    }
  }

  function rerender(): void {
    while (formHost.firstChild) formHost.removeChild(formHost.firstChild);
    formHost.append(buildForm(state, errors, opts, validateWhen));
  }

  async function onSave(): Promise<void> {
    if (saving) return;
    if (!validate(state, opts, errors)) {
      rerender();
      toast.error("Fix the highlighted fields and try again.");
      return;
    }
    const payload = toRuleConfig(state);
    saving = true;
    try {
      await api.rules.upsert(payload);
      toast.success(
        opts.mode === "create"
          ? `Rule ${payload.id} added`
          : `Rule ${payload.id} saved`,
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
    confirmLabel: opts.mode === "create" ? "Add rule" : "Save",
    onCancel: () => dlg?.close(false),
    onConfirm: () => void onSave(),
  });

  dlg = openDialog({
    title: opts.mode === "create" ? "Add rule" : `Edit rule ${state.id}`,
    body: formHost,
    footer,
    width: "640px",
  });

  return dlg.closed;
}

function buildInitialState(opts: OpenRuleFormOpts): FormState {
  if (opts.mode === "edit" && opts.existing) {
    const r = opts.existing;
    return {
      id: r.id,
      name: r.name,
      severity: (r.severity ?? "low") as Severity,
      camera_filter: r.camera_filter ? [...r.camera_filter] : [],
      when: r.when,
      min_track_age_ms: r.min_track_age_ms ?? 500,
      consecutive_frames: r.consecutive_frames ?? 2,
      cooldown_ms: r.cooldown_ms ?? 30_000,
      enabled: r.enabled !== false,
    };
  }
  return {
    id: "",
    name: "",
    severity: "low",
    camera_filter: [],
    when: "object.label == 'person'",
    min_track_age_ms: 500,
    consecutive_frames: 2,
    cooldown_ms: 30_000,
    enabled: true,
  };
}

function buildForm(
  state: FormState,
  errors: Record<string, string | undefined>,
  opts: OpenRuleFormOpts,
  validateWhen: (value: string) => void,
): HTMLElement {
  const idField =
    opts.mode === "edit"
      ? readOnlyField("ID", state.id)
      : TextField({
          label: "ID",
          value: state.id,
          required: true,
          placeholder: "loitering_after_hours",
          helpText: "Lowercase letters, digits, and underscores. Must be unique.",
          ...(errors["id"] !== undefined ? { error: errors["id"] } : {}),
          onChange: (v) => {
            state.id = v.trim();
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
          placeholder: "Loitering after hours",
          ...(errors["name"] !== undefined ? { error: errors["name"] } : {}),
          onChange: (v) => {
            state.name = v;
          },
        }),
      ),
      FieldRow(
        Select<Severity>({
          label: "Severity",
          value: state.severity,
          options: SEVERITY_OPTIONS,
          helpText: "Drives alert routing + colour in the events feed.",
          onChange: (next) => {
            state.severity = next;
          },
        }),
        Toggle({
          label: "Enabled",
          value: state.enabled,
          helpText: "Disable to keep the rule but stop firing alerts.",
          onChange: (b) => {
            state.enabled = b;
          },
        }),
      ),
      MultiSelect<string>({
        label: "Camera filter",
        value: state.camera_filter.map((id) => String(id)),
        options: opts.cameras.map((c) => ({
          value: String(c.id),
          label: `${c.id} · ${c.name}`,
        })),
        helpText:
          "Restrict the rule to specific cameras. Leave all chips off = applies to every camera.",
        onChange: (next) => {
          state.camera_filter = next
            .map((s) => Number(s))
            .filter((n) => Number.isFinite(n));
        },
      }),
    ),
    FormSection(
      "Condition",
      TextArea({
        label: "when (CEL)",
        value: state.when,
        rows: 4,
        required: true,
        placeholder:
          "object.label == 'person' && object.attributes['motion.dwell_seconds'] >= 60",
        helpText: CEL_HELP,
        ...(errors["when"] !== undefined ? { error: errors["when"] } : {}),
        onChange: (v) => {
          state.when = v;
        },
        onBlur: (v) => {
          state.when = v;
          void validateWhen(v);
        },
      }),
    ),
    FormSection(
      "Debounce",
      FieldRow(
        NumberField({
          label: "Min track age (ms)",
          value: state.min_track_age_ms,
          min: 0,
          step: 100,
          helpText: "Track must exist this long before the rule fires.",
          ...(errors["min_track_age_ms"] !== undefined
            ? { error: errors["min_track_age_ms"] }
            : {}),
          onChange: (v) => {
            state.min_track_age_ms = Math.max(0, Math.floor(v));
          },
        }),
        NumberField({
          label: "Consecutive frames",
          value: state.consecutive_frames,
          min: 1,
          step: 1,
          helpText:
            "Predicate must hold for this many consecutive frames per track.",
          ...(errors["consecutive_frames"] !== undefined
            ? { error: errors["consecutive_frames"] }
            : {}),
          onChange: (v) => {
            state.consecutive_frames = Math.max(1, Math.floor(v));
          },
        }),
        NumberField({
          label: "Cooldown (ms)",
          value: state.cooldown_ms,
          min: 0,
          step: 1000,
          helpText:
            "After firing, the same rule+track cannot re-fire for this long.",
          ...(errors["cooldown_ms"] !== undefined
            ? { error: errors["cooldown_ms"] }
            : {}),
          onChange: (v) => {
            state.cooldown_ms = Math.max(0, Math.floor(v));
          },
        }),
      ),
    ),
  );
}

function readOnlyField(label: string, value: string): HTMLElement {
  return h(
    "label",
    { class: "field" },
    h("span", { class: "field-label" }, label),
    h("input", { type: "text", value, disabled: true, readOnly: true }),
    h(
      "span",
      { class: "field-help" },
      "Rule id cannot be changed after creation.",
    ),
  );
}

function validate(
  state: FormState,
  opts: OpenRuleFormOpts,
  errors: Record<string, string | undefined>,
): boolean {
  for (const k of Object.keys(errors)) errors[k] = undefined;
  let ok = true;

  if (opts.mode === "create") {
    const id = state.id.trim();
    if (id === "") {
      errors["id"] = "ID is required.";
      ok = false;
    } else if (!ID_RE.test(id)) {
      errors["id"] = "Only lowercase letters, digits, and underscores.";
      ok = false;
    } else if (opts.existingIds.includes(id)) {
      errors["id"] = `ID '${id}' is already in use.`;
      ok = false;
    }
  }

  if (state.name.trim() === "") {
    errors["name"] = "Name is required.";
    ok = false;
  }

  if (state.when.trim() === "") {
    errors["when"] = "CEL expression is required.";
    ok = false;
  }

  if (state.min_track_age_ms < 0) {
    errors["min_track_age_ms"] = "Cannot be negative.";
    ok = false;
  }
  if (state.consecutive_frames < 1) {
    errors["consecutive_frames"] = "Must be at least 1.";
    ok = false;
  }
  if (state.cooldown_ms < 0) {
    errors["cooldown_ms"] = "Cannot be negative.";
    ok = false;
  }

  return ok;
}

function toRuleConfig(state: FormState): RuleConfig {
  return {
    id: state.id.trim(),
    name: state.name.trim(),
    severity: state.severity,
    camera_filter:
      state.camera_filter.length === 0 ? null : [...state.camera_filter],
    when: state.when.trim(),
    min_track_age_ms: state.min_track_age_ms,
    consecutive_frames: state.consecutive_frames,
    cooldown_ms: state.cooldown_ms,
    enabled: state.enabled,
  };
}
