import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import type { AlertEvent } from "../api/types.js";

export async function renderEvents(root: HTMLElement): Promise<void> {
  clear(root);
  const list = await api.events.recent(200);
  root.append(h("h2", null, "Recent events"));
  if (list.length === 0) {
    root.append(h("p", { class: "muted" }, "No events yet."));
    return;
  }
  const tbl = h(
    "table",
    null,
    h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", null, "When"),
        h("th", null, "Camera"),
        h("th", null, "Rule"),
        h("th", null, "Label"),
        h("th", null, "Severity"),
        h("th", null, "Trace"),
      ),
    ),
    h("tbody", null, ...list.map(row)),
  );
  root.append(tbl);
}

function row(e: AlertEvent): HTMLElement {
  return h(
    "tr",
    null,
    h("td", null, e.captured_at),
    h("td", null, String(e.camera_id)),
    h("td", null, e.rule_id),
    h("td", null, e.label),
    h("td", null, e.severity),
    h("td", null, h("code", null, e.trace_id)),
  );
}
