import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";

export async function renderHealth(root: HTMLElement): Promise<void> {
  clear(root);
  root.append(h("h2", null, "Health"));
  try {
    const h0 = await api.health();
    root.append(
      h(
        "dl",
        { class: "kv" },
        h("dt", null, "Status"),
        h("dd", null, h0.status),
        h("dt", null, "Version"),
        h("dd", null, h0.version),
      ),
    );
  } catch (e) {
    root.append(h("p", { class: "muted" }, `Engine unreachable: ${(e as Error).message}`));
  }
}
