// M-Admin Phase 1 — Cameras admin tab. Replaces the read-only +
// Delete-only stub with a full CRUD surface built on the Phase 0
// shared primitives (`openDialog`, `toast`, `forms.ts`).
//
// Layout:
//   page-toolbar: title + "+ New camera" + "🔍 Discover" (disabled
//                 until Phase 1B lands; the title attribute explains).
//   admin-table : id / name / url / prompts / status / actions
//
// All mutations refresh the table in-place via `reloadTable()`. No
// `location.reload()`; that was the worst UX bug in the old version
// and the whole point of Phase 0's shared primitives.

import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { openDialog, dialogFooter, type DialogHandle } from "../lib/dialog.js";
import { toast } from "../lib/toast.js";
import { openCameraForm } from "./cameras-form.js";
import { openDiscoveryDialog } from "./cameras-discovery.js";
import type { CameraConfig, CameraId } from "../api/types.js";

export async function renderCameras(root: HTMLElement): Promise<void> {
  clear(root);

  const tableHost = h("div", { class: "admin-section" });
  const head = h(
    "div",
    { class: "page-toolbar" },
    h("h2", { class: "page-toolbar-title" }, "Cameras"),
    h("div", { class: "page-toolbar-actions" }, ...buildToolbar(() => reload())),
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
          const list = await api.cameras.list();
          const ok = await openCameraForm({
            mode: "create",
            existingIds: list.map((c) => c.id),
          });
          if (ok) await onChange();
        },
      },
    },
    "+ New camera",
  );
  const discoverBtn = h(
    "button",
    {
      class: "ghost",
      type: "button",
      title: "ONVIF + CIDR sweep, then pre-fill the Add form.",
      on: {
        click: async () => {
          const list = await api.cameras.list();
          const added = await openDiscoveryDialog({
            existingIds: list.map((c) => c.id),
          });
          if (added) await onChange();
        },
      },
    },
    "🔍 Discover",
  );
  return [newBtn, discoverBtn];
}

async function renderTable(
  host: HTMLElement,
  onChange: () => Promise<void>,
): Promise<void> {
  clear(host);
  let list: CameraConfig[];
  try {
    list = await api.cameras.list();
  } catch (err) {
    host.append(
      h(
        "p",
        { class: "muted" },
        `Failed to load cameras: ${(err as Error).message}`,
      ),
    );
    return;
  }

  if (list.length === 0) {
    host.append(
      h(
        "p",
        { class: "muted" },
        "No cameras configured. Click ",
        h("strong", null, "+ New camera"),
        " to add one.",
      ),
    );
    return;
  }

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
        h("th", null, "URL"),
        h("th", null, "Prompts"),
        h("th", null, "Status"),
        h("th", null, ""),
      ),
    ),
    h(
      "tbody",
      null,
      ...list.map((cam) => row(cam, list, onChange)),
    ),
  );
  host.append(tbl);
}

function row(
  cam: CameraConfig,
  list: CameraConfig[],
  onChange: () => Promise<void>,
): HTMLElement {
  const promptCell =
    cam.prompts && cam.prompts.length > 0
      ? cam.prompts.join(", ")
      : h("span", { class: "muted" }, "—");

  const statusPill = cam.enabled
    ? h(
        "span",
        { class: "state-pill state-ready", title: "Pipeline enabled" },
        "enabled",
      )
    : h(
        "span",
        { class: "state-pill state-failed", title: "Pipeline disabled in config" },
        "disabled",
      );

  return h(
    "tr",
    null,
    h("td", null, String(cam.id)),
    h("td", null, cam.name),
    h("td", null, h("code", { class: "mono" }, cam.url)),
    h("td", null, promptCell),
    h("td", null, statusPill),
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
              const ok = await openCameraForm({
                mode: "edit",
                existing: cam,
                existingIds: list.map((c) => c.id),
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
          class: "ghost",
          type: "button",
          on: {
            click: () => openSnapshotPreview(cam),
          },
        },
        "Snapshot",
      ),
      h(
        "button",
        {
          class: "ghost danger",
          type: "button",
          on: {
            click: () => void confirmDelete(cam, onChange),
          },
        },
        "Delete",
      ),
    ),
  );
}

async function confirmDelete(
  cam: CameraConfig,
  onChange: () => Promise<void>,
): Promise<void> {
  const body = h(
    "p",
    null,
    "Delete camera ",
    h("strong", null, `${cam.name} (id ${cam.id})`),
    "? This stops the pipeline and removes the row from config. Recorded clips are kept.",
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
    title: "Delete camera",
    body,
    footer,
    width: "440px",
  });
  async function doDelete(): Promise<void> {
    try {
      await api.cameras.remove(cam.id);
      toast.success(`Camera ${cam.id} deleted`);
      dlg?.close(true);
      await onChange();
    } catch (err) {
      toast.error(`Delete failed: ${(err as Error).message}`);
    }
  }
}

function openSnapshotPreview(cam: CameraConfig): void {
  const url = api.cameras.latestSnapshotUrl(cam.id);
  const img = h("img", {
    src: url,
    alt: `Latest snapshot from ${cam.name}`,
    class: "camera-snapshot-img",
  });
  img.addEventListener("error", () => {
    img.replaceWith(
      h(
        "div",
        { class: "camera-snapshot-error" },
        "No frame available yet for this camera.",
      ),
    );
  });
  const body = h("div", { class: "camera-snapshot" }, img);
  const dlg = openDialog({
    title: `${cam.name} — latest snapshot`,
    body,
    width: "720px",
  });
  void dlg;
}

// Re-export so `main.ts`'s import keeps working unchanged.
export type { CameraId };
