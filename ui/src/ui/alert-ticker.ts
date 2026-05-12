import { subscribeSse } from "../api/sse.js";
import { h } from "../lib/el.js";
import type { AlertEvent } from "../api/types.js";

export function mountAlertTicker(root: HTMLElement): void {
  const list = h("div", null);
  root.append(h("h3", null, "Live alerts"), list);
  let count = 0;
  subscribeSse<AlertEvent>("/api/stream/events", (ev) => {
    list.prepend(card(ev));
    count++;
    while (list.childElementCount > 50) {
      const last = list.lastElementChild;
      if (last) list.removeChild(last);
    }
    void count;
  });
}

function card(ev: AlertEvent): HTMLElement {
  return h(
    "div",
    { class: `alert severity-${ev.severity}` },
    h("strong", null, ev.label),
    " ",
    h("span", { class: "muted" }, `· cam ${ev.camera_id} · ${ev.rule_id}`),
    h("br", null),
    h("span", { class: "muted" }, ev.captured_at),
  );
}
