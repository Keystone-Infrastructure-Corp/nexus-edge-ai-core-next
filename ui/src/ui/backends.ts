import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import type { BackendStatus } from "../api/types.js";

export async function renderBackends(root: HTMLElement): Promise<void> {
  clear(root);
  root.append(h("h2", null, "Detector pool"));
  let stop = false;
  const list = h("div", { style: { display: "flex", flexDirection: "column", gap: "8px" } });
  const summary = h("p", { class: "muted" }, "loading…");
  root.append(summary, list);
  const tick = async () => {
    if (stop || !root.isConnected) return;
    try {
      const r = await api.backends();
      summary.textContent = r.mode === "pool"
        ? `Pool: ${r.slots.length} slot(s).`
        : "In-process detector (single).";
      while (list.firstChild) list.removeChild(list.firstChild);
      for (const s of r.slots) list.append(card(s));
    } catch (e) {
      summary.textContent = `error: ${(e as Error).message}`;
    }
    setTimeout(tick, 1500);
  };
  void tick();
  const obs = new MutationObserver(() => {
    if (!root.isConnected) {
      stop = true;
      obs.disconnect();
    }
  });
  obs.observe(document.body, { childList: true, subtree: true });
}

function card(s: BackendStatus): HTMLElement {
  return h(
    "div",
    { class: "backend-card" },
    h("span", { class: `state-pill state-${s.state}` }, s.state),
    h(
      "div",
      null,
      h("div", null, `slot ${s.slot} · ${s.name}`),
      h("div", { class: "muted" }, `generation ${s.generation}`),
    ),
  );
}
