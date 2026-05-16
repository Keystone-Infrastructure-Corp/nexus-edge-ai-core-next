// M-Admin Phase 0 — modal dialog primitive.
//
// `openDialog({ title, body, footer? })` returns a handle whose
// `closed` promise resolves to `true` if `close(true)` was called
// (the conventional "saved" / "confirmed" path) and `false` otherwise
// (ESC, ✕ button, programmatic cancel).
//
// Behaviour:
// - Focus is trapped inside the dialog while open.
// - ESC dismisses with `false`.
// - Click on the backdrop is intentionally ignored — accidental click
//   loss of in-progress edits is the worst UX bug we already see in
//   admin-storage. Operators must hit Cancel or ✕ explicitly.
// - The dialog is mounted directly to `document.body`, so it works
//   from any tab including the Operations views with the alert pane.

import { h } from "./el.js";

export interface DialogOptions {
  title: string;
  body: HTMLElement | string;
  footer?: HTMLElement | null;
  /// CSS width override (e.g. `"720px"`). Default = `560px` per CSS.
  width?: string;
}

export interface DialogHandle {
  /// Resolves once the dialog is fully closed. `true` if the caller
  /// invoked `close(true)` (save/confirm), `false` for ESC / ✕ /
  /// programmatic `close(false)` / `close()`.
  readonly closed: Promise<boolean>;
  close(result?: boolean): void;
  /// The body container; callers can mutate it to swap content.
  readonly body: HTMLElement;
}

const FOCUSABLE_SELECTOR =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

export function openDialog(opts: DialogOptions): DialogHandle {
  let resolve!: (v: boolean) => void;
  const closed = new Promise<boolean>((r) => {
    resolve = r;
  });

  const bodyHost = h("div", { class: "dialog-body" });
  if (typeof opts.body === "string") {
    bodyHost.append(opts.body);
  } else {
    bodyHost.append(opts.body);
  }

  const closeBtn = h(
    "button",
    {
      type: "button",
      class: "dialog-close",
      title: "Close",
      on: { click: () => doClose(false) },
    },
    "✕",
  );

  const head = h(
    "div",
    { class: "dialog-head" },
    h("h2", null, opts.title),
    closeBtn,
  );

  const cardChildren: (HTMLElement | null)[] = [head, bodyHost];
  if (opts.footer) cardChildren.push(opts.footer);

  const card = h("div", { class: "dialog", role: "dialog" });
  if (opts.width) card.style.width = opts.width;
  for (const c of cardChildren) {
    if (c) card.append(c);
  }

  const backdrop = h("div", { class: "dialog-backdrop" }, card);
  document.body.appendChild(backdrop);

  // Focus first focusable element inside the dialog body, if any.
  const initial = card.querySelector<HTMLElement>(FOCUSABLE_SELECTOR);
  if (initial) initial.focus();

  const onKey = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      doClose(false);
      return;
    }
    if (ev.key !== "Tab") return;
    // Focus trap.
    const focusables = Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR));
    if (focusables.length === 0) return;
    const first = focusables[0]!;
    const last = focusables[focusables.length - 1]!;
    const active = document.activeElement as HTMLElement | null;
    if (ev.shiftKey && active === first) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && active === last) {
      ev.preventDefault();
      first.focus();
    }
  };
  document.addEventListener("keydown", onKey);

  let done = false;
  function doClose(result: boolean): void {
    if (done) return;
    done = true;
    document.removeEventListener("keydown", onKey);
    if (backdrop.parentElement) backdrop.parentElement.removeChild(backdrop);
    resolve(result);
  }

  return {
    closed,
    close: (result?: boolean) => doClose(result === true),
    body: bodyHost,
  };
}

/// Helper for the common Cancel / Save footer.
export function dialogFooter(opts: {
  cancelLabel?: string;
  confirmLabel?: string;
  onCancel: () => void;
  onConfirm: () => void;
  confirmDisabled?: boolean;
  confirmTone?: "primary" | "danger";
}): HTMLElement {
  const cancelBtn = h(
    "button",
    {
      type: "button",
      class: "ghost",
      on: { click: opts.onCancel },
    },
    opts.cancelLabel ?? "Cancel",
  );
  const confirmBtn = h(
    "button",
    {
      type: "button",
      class: opts.confirmTone === "danger" ? "danger" : "primary",
      disabled: !!opts.confirmDisabled,
      on: { click: opts.onConfirm },
    },
    opts.confirmLabel ?? "Save",
  );
  return h("div", { class: "dialog-footer" }, cancelBtn, confirmBtn);
}
