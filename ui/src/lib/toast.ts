// M-Admin Phase 0 — toast singleton.
//
// `toast.success(msg)` / `toast.error(msg)` / `toast.info(msg)` mounts
// a transient notification card to a fixed bottom-right container.
// Auto-dismisses after 4 s; click to dismiss early. CSS lives in
// `ui/src/ui/styles.css` under the "Toast" section (Phase 0 block).

import { h } from "./el.js";

const DISMISS_MS = 4000;
const FADE_MS = 200;

let container: HTMLElement | null = null;

function ensureContainer(): HTMLElement {
  if (container && container.isConnected) return container;
  container = h("div", { class: "toast-container", id: "toast-container" });
  document.body.appendChild(container);
  return container;
}

function show(message: string, kind: "success" | "error" | "info"): void {
  const root = ensureContainer();
  const el = h("div", { class: `toast toast-${kind}`, role: "status" }, message);
  root.appendChild(el);
  // Trigger CSS transition on next frame.
  requestAnimationFrame(() => el.classList.add("toast-in"));

  let dismissed = false;
  const remove = (): void => {
    if (dismissed) return;
    dismissed = true;
    el.classList.remove("toast-in");
    el.classList.add("toast-out");
    window.setTimeout(() => {
      if (el.parentElement) el.parentElement.removeChild(el);
    }, FADE_MS);
  };

  el.addEventListener("click", remove);
  window.setTimeout(remove, DISMISS_MS);
}

export const toast = {
  success(msg: string): void {
    show(msg, "success");
  },
  error(msg: string): void {
    show(msg, "error");
  },
  info(msg: string): void {
    show(msg, "info");
  },
};
