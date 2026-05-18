// M-Admin Phase 0 — form primitives.
//
// Each builder returns an `HTMLElement` constructed via `h()`. Callers
// own the value state and pass an `onChange` callback. This avoids
// building any framework-style state machine: pages still hold their
// edited model in a plain object and re-render the form on save.
//
// Styling: see the "Form primitives" + "Chip input" sections of
// `ui/src/ui/styles.css` (Phase 0 block).

import { h } from "./el.js";

interface FieldMeta {
  helpText?: string;
  error?: string;
  required?: boolean;
}

export interface FieldOpts<T> extends FieldMeta {
  label: string;
  value: T;
  placeholder?: string;
  onChange: (next: T) => void;
}

export function TextField(opts: FieldOpts<string>): HTMLElement {
  const input = h("input", {
    type: "text",
    value: opts.value ?? "",
    placeholder: opts.placeholder ?? "",
    required: !!opts.required,
    on: {
      input: (ev) => {
        opts.onChange((ev.currentTarget as HTMLInputElement).value);
      },
    },
  });
  return wrap(opts.label, input, opts);
}

export function PasswordField(opts: FieldOpts<string>): HTMLElement {
  const input = h("input", {
    type: "password",
    value: opts.value ?? "",
    placeholder: opts.placeholder ?? "",
    required: !!opts.required,
    autocomplete: "off",
    on: {
      input: (ev) => {
        opts.onChange((ev.currentTarget as HTMLInputElement).value);
      },
    },
  });
  return wrap(opts.label, input, opts);
}

export function NumberField(
  opts: FieldOpts<number> & { min?: number; max?: number; step?: number },
): HTMLElement {
  const input = h("input", {
    type: "number",
    value: String(opts.value ?? 0),
    placeholder: opts.placeholder ?? "",
    required: !!opts.required,
    on: {
      input: (ev) => {
        const raw = (ev.currentTarget as HTMLInputElement).value;
        const v = raw === "" ? 0 : Number(raw);
        opts.onChange(Number.isFinite(v) ? v : 0);
      },
    },
  });
  if (opts.min != null) input.min = String(opts.min);
  if (opts.max != null) input.max = String(opts.max);
  if (opts.step != null) input.step = String(opts.step);
  return wrap(opts.label, input, opts);
}

export function TextArea(
  opts: FieldOpts<string> & {
    rows?: number;
    /// M-Admin Phase 5 — optional blur callback so callers can run
    /// expensive validation (e.g. server-side CEL compile) without
    /// firing on every keystroke. Receives the current value.
    onBlur?: (value: string) => void;
  },
): HTMLElement {
  const ta = h("textarea", {
    rows: opts.rows ?? 4,
    placeholder: opts.placeholder ?? "",
    required: !!opts.required,
    value: opts.value ?? "",
    on: {
      input: (ev) => {
        opts.onChange((ev.currentTarget as HTMLTextAreaElement).value);
      },
      ...(opts.onBlur
        ? {
            blur: (ev: FocusEvent) => {
              opts.onBlur!((ev.currentTarget as HTMLTextAreaElement).value);
            },
          }
        : {}),
    },
  });
  return wrap(opts.label, ta, opts);
}

export function Toggle(opts: {
  label: string;
  value: boolean;
  onChange: (b: boolean) => void;
  helpText?: string;
}): HTMLElement {
  const input = h("input", {
    type: "checkbox",
    checked: opts.value,
    on: {
      change: (ev) =>
        opts.onChange((ev.currentTarget as HTMLInputElement).checked),
    },
  });
  return h(
    "label",
    { class: "field field-toggle" },
    input,
    h("span", { class: "field-label-inline" }, opts.label),
    opts.helpText ? h("span", { class: "field-help" }, opts.helpText) : null,
  );
}

export interface SelectOption<V extends string> {
  value: V;
  label: string;
}

export function Select<V extends string>(opts: {
  label: string;
  value: V;
  options: ReadonlyArray<SelectOption<V>>;
  helpText?: string;
  onChange: (next: V) => void;
}): HTMLElement {
  const sel = h("select", {
    on: {
      change: (ev) => {
        opts.onChange((ev.currentTarget as HTMLSelectElement).value as V);
      },
    },
  });
  for (const o of opts.options) {
    const optEl = h("option", { value: o.value }, o.label);
    if (o.value === opts.value) optEl.selected = true;
    sel.append(optEl);
  }
  const meta: FieldMeta = {};
  if (opts.helpText !== undefined) meta.helpText = opts.helpText;
  return wrap(opts.label, sel, meta);
}

export function ChipsInput(opts: {
  label: string;
  value: string[];
  placeholder?: string;
  helpText?: string;
  onChange: (next: string[]) => void;
}): HTMLElement {
  const value = [...opts.value];
  const chips = h("div", { class: "chip-list" });

  function emit(): void {
    opts.onChange([...value]);
    renderChips();
  }

  function renderChips(): void {
    while (chips.firstChild) chips.removeChild(chips.firstChild);
    for (let i = 0; i < value.length; i++) {
      const v = value[i]!;
      const chip = h(
        "span",
        { class: "chip" },
        v,
        h(
          "button",
          {
            type: "button",
            class: "chip-x",
            title: `Remove ${v}`,
            on: {
              click: () => {
                const idx = value.indexOf(v);
                if (idx >= 0) {
                  value.splice(idx, 1);
                  emit();
                }
              },
            },
          },
          "✕",
        ),
      );
      chips.append(chip);
    }
  }

  const input = h("input", {
    type: "text",
    placeholder: opts.placeholder ?? "Type and press Enter…",
    class: "chip-input-text",
    on: {
      keydown: (ev) => {
        const target = ev.currentTarget as HTMLInputElement;
        if (ev.key === "Enter" || ev.key === ",") {
          ev.preventDefault();
          const v = target.value.trim();
          if (v && !value.includes(v)) {
            value.push(v);
            emit();
          }
          target.value = "";
        } else if (
          ev.key === "Backspace" &&
          target.value === "" &&
          value.length > 0
        ) {
          value.pop();
          emit();
        }
      },
    },
  });

  renderChips();
  const wrapper = h("div", { class: "chip-input" }, chips, input);
  const meta: FieldMeta = {};
  if (opts.helpText !== undefined) meta.helpText = opts.helpText;
  return wrap(opts.label, wrapper, meta);
}

export function MultiSelect<V extends string>(opts: {
  label: string;
  value: V[];
  options: ReadonlyArray<SelectOption<V>>;
  helpText?: string;
  onChange: (next: V[]) => void;
}): HTMLElement {
  const selected = new Set<V>(opts.value);
  const list = h("div", { class: "chip-list" });

  function render(): void {
    while (list.firstChild) list.removeChild(list.firstChild);
    for (const o of opts.options) {
      const isOn = selected.has(o.value);
      const chip = h(
        "button",
        {
          type: "button",
          class: "chip" + (isOn ? " chip-on" : ""),
          on: {
            click: () => {
              if (selected.has(o.value)) selected.delete(o.value);
              else selected.add(o.value);
              opts.onChange(Array.from(selected));
              render();
            },
          },
        },
        o.label,
      );
      list.append(chip);
    }
  }
  render();

  const wrapper = h("div", { class: "chip-input" }, list);
  const meta: FieldMeta = {};
  if (opts.helpText !== undefined) meta.helpText = opts.helpText;
  return wrap(opts.label, wrapper, meta);
}

export function FormSection(title: string, ...children: (HTMLElement | null)[]): HTMLElement {
  const section = h("div", { class: "form-section" });
  section.append(h("h3", { class: "form-section-title" }, title));
  for (const c of children) if (c) section.append(c);
  return section;
}

export function FieldRow(...children: (HTMLElement | null)[]): HTMLElement {
  const row = h("div", { class: "field-row" });
  for (const c of children) if (c) row.append(c);
  return row;
}

function wrap(label: string, control: HTMLElement, meta: FieldMeta): HTMLElement {
  const cls = "field" + (meta.error ? " field-error" : "");
  return h(
    "label",
    { class: cls },
    h(
      "span",
      { class: "field-label" },
      label,
      meta.required ? h("span", { class: "field-req" }, " *") : null,
    ),
    control,
    meta.helpText ? h("span", { class: "field-help" }, meta.helpText) : null,
    meta.error ? h("span", { class: "field-error-msg" }, meta.error) : null,
  );
}
