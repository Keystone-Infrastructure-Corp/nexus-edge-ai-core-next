import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import type { CameraConfig } from "../api/types.js";

export async function renderCameras(root: HTMLElement): Promise<void> {
  clear(root);
  const list = await api.cameras.list();
  root.append(h("h2", null, "Cameras"));
  if (list.length === 0) {
    root.append(h("p", { class: "muted" }, "No cameras configured."));
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
        h("th", null, "URL"),
        h("th", null, "Prompts"),
        h("th", null, ""),
      ),
    ),
    h("tbody", null, ...list.map(row)),
  );
  root.append(tbl);
}

function row(c: CameraConfig): HTMLElement {
  return h(
    "tr",
    null,
    h("td", null, String(c.id)),
    h("td", null, c.name),
    h("td", null, h("code", null, c.url)),
    h("td", null, (c.prompts ?? []).join(", ")),
    h(
      "td",
      null,
      h(
        "button",
        {
          class: "ghost",
          on: {
            click: async () => {
              if (!confirm(`Delete camera ${c.id}?`)) return;
              await api.cameras.remove(c.id);
              location.reload();
            },
          },
        },
        "Delete",
      ),
    ),
  );
}
