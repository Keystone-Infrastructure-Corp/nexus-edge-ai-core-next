import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import type { RuleConfig } from "../api/types.js";

export async function renderRules(root: HTMLElement): Promise<void> {
  clear(root);
  const list = await api.rules.list();
  root.append(h("h2", null, "Rules"));
  if (list.length === 0) {
    root.append(h("p", { class: "muted" }, "No rules configured."));
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
        h("th", null, "ID"),
        h("th", null, "Name"),
        h("th", null, "When"),
        h("th", null, "Severity"),
        h("th", null, "Enabled"),
      ),
    ),
    h("tbody", null, ...list.map(row)),
  );
  root.append(tbl);
}

function row(r: RuleConfig): HTMLElement {
  return h(
    "tr",
    null,
    h("td", null, r.id),
    h("td", null, r.name),
    h("td", null, h("code", null, r.when)),
    h("td", null, r.severity),
    h("td", null, r.enabled === false ? "no" : "yes"),
  );
}
