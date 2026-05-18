// M-Admin Phase 3 — Rules admin tab. Same shape as the cameras tab:
// a `page-toolbar` with `+ New rule`, an `admin-table`, and per-row
// Edit / Delete actions. All mutations refresh the table in place
// via `reload()` — no `location.reload()`. CEL editing lives in
// `rules-form.ts`; this file is just the list view + action wiring.

import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { openDialog, dialogFooter, type DialogHandle } from "../lib/dialog.js";
import { toast } from "../lib/toast.js";
import { openRuleForm } from "./rules-form.js";
import type { CameraConfig, RuleConfig } from "../api/types.js";

export async function renderRules(root: HTMLElement): Promise<void> {
  clear(root);

  const tableHost = h("div", { class: "admin-section" });
  const head = h(
    "div",
    { class: "page-toolbar" },
    h("h2", { class: "page-toolbar-title" }, "Rules"),
    h(
      "div",
      { class: "page-toolbar-actions" },
      ...buildToolbar(() => reload()),
    ),
  );
  root.append(head, tableHost);

  async function reload(): Promise<void> {
    await renderTable(tableHost, () => reload());
  }
  await reload();
}

function buildToolbar(onChange: () => Promise<void>): HTMLElement[] {
  const newBtn = h(
    "button",
    {
      class: "primary",
      type: "button",
      on: {
        click: async () => {
          // Fetch fresh ids + cameras on every Open so the form
          // never shows stale collision warnings or stale camera
          // names in the chip selector.
          const [rules, cameras] = await Promise.all([
            api.rules.list(),
            api.cameras.list(),
          ]);
          const ok = await openRuleForm({
            mode: "create",
            existingIds: rules.map((r) => r.id),
            cameras,
          });
          if (ok) await onChange();
        },
      },
    },
    "+ New rule",
  );
  return [newBtn];
}

async function renderTable(
  host: HTMLElement,
  onChange: () => Promise<void>,
): Promise<void> {
  clear(host);

  let rules: RuleConfig[];
  let cameras: CameraConfig[];
  try {
    [rules, cameras] = await Promise.all([
      api.rules.list(),
      api.cameras.list(),
    ]);
  } catch (err) {
    host.append(
      h(
        "p",
        { class: "muted" },
        `Failed to load rules: ${(err as Error).message}`,
      ),
    );
    return;
  }

  if (rules.length === 0) {
    host.append(
      h(
        "p",
        { class: "muted" },
        "No rules configured. Click ",
        h("strong", null, "+ New rule"),
        " to add one.",
      ),
    );
    return;
  }

  // Build a quick lookup so the Cameras column can render names
  // instead of bare ids.
  const camById = new Map<number, string>();
  for (const c of cameras) camById.set(c.id, c.name);

  const tbl = h(
    "table",
    { class: "admin-table" },
    h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", null, "ID"),
        h("th", null, "Name"),
        h("th", null, "Severity"),
        h("th", null, "Cameras"),
        h("th", null, "When"),
        h("th", null, "Enabled"),
        h("th", null, ""),
      ),
    ),
    h(
      "tbody",
      null,
      ...rules.map((r) => row(r, rules, cameras, camById, onChange)),
    ),
  );
  host.append(tbl);
}

function row(
  r: RuleConfig,
  list: RuleConfig[],
  cameras: CameraConfig[],
  camById: Map<number, string>,
  onChange: () => Promise<void>,
): HTMLElement {
  const camerasCell =
    r.camera_filter && r.camera_filter.length > 0
      ? r.camera_filter
          .map((id) => camById.get(id) ?? `id ${id}`)
          .join(", ")
      : h("span", { class: "muted" }, "all");

  const enabled = r.enabled !== false;
  const enabledPill = enabled
    ? h(
        "span",
        { class: "state-pill state-ready", title: "Rule firing" },
        "enabled",
      )
    : h(
        "span",
        { class: "state-pill state-failed", title: "Rule disabled in config" },
        "disabled",
      );

  return h(
    "tr",
    null,
    h("td", null, h("code", { class: "mono" }, r.id)),
    h("td", null, r.name),
    h("td", null, h("span", { class: `severity-pill sev-${r.severity}` }, r.severity)),
    h("td", null, camerasCell),
    h("td", { class: "rule-when-cell" }, h("code", { class: "mono" }, r.when)),
    h("td", null, enabledPill),
    h(
      "td",
      null,
      h(
        "button",
        {
          class: "ghost",
          type: "button",
          on: {
            click: async () => {
              const ok = await openRuleForm({
                mode: "edit",
                existing: r,
                existingIds: list.map((x) => x.id),
                cameras,
              });
              if (ok) await onChange();
            },
          },
        },
        "Edit",
      ),
      h(
        "button",
        {
          class: "ghost danger",
          type: "button",
          on: {
            click: () => void confirmDelete(r, onChange),
          },
        },
        "Delete",
      ),
    ),
  );
}

async function confirmDelete(
  r: RuleConfig,
  onChange: () => Promise<void>,
): Promise<void> {
  const body = h(
    "p",
    null,
    "Delete rule ",
    h("strong", null, `${r.name} (id ${r.id})`),
    "? Past alerts are kept; the rule will simply stop firing.",
  );
  let dlg: DialogHandle | null = null;
  const footer = dialogFooter({
    cancelLabel: "Cancel",
    confirmLabel: "Delete",
    confirmTone: "danger",
    onCancel: () => dlg?.close(false),
    onConfirm: () => void doDelete(),
  });
  dlg = openDialog({
    title: "Delete rule",
    body,
    footer,
    width: "440px",
  });
  async function doDelete(): Promise<void> {
    try {
      await api.rules.remove(r.id);
      toast.success(`Rule ${r.id} deleted`);
      dlg?.close(true);
      await onChange();
    } catch (err) {
      toast.error(`Delete failed: ${(err as Error).message}`);
    }
  }
}

