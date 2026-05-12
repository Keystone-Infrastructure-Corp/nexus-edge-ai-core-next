import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { drawFrame } from "../lib/canvas.js";

export async function renderViewer(root: HTMLElement): Promise<void> {
  clear(root);
  root.append(h("h2", null, "Live viewer"));
  const cams = await api.cameras.list();
  if (cams.length === 0) {
    root.append(h("p", { class: "muted" }, "No cameras configured."));
    return;
  }
  const grid = h("div", { class: "viewer-grid" });
  root.append(grid);
  for (const cam of cams) {
    const canvas = h("canvas", null);
    const cell = h(
      "div",
      { class: "viewer-cell" },
      h("h3", null, `${cam.name} (id ${cam.id})`),
      canvas,
    );
    grid.append(cell);
    void poll(cam.id, canvas);
  }
}

async function poll(cameraId: number, canvas: HTMLCanvasElement) {
  const img = new Image();
  while (canvas.isConnected) {
    try {
      const meta = await api.cameras.latestMetadata(cameraId);
      img.src = api.cameras.latestSnapshotUrl(cameraId, meta.frame_id);
      await once(img, "load");
      drawFrame(canvas, img, meta.objects, { width: meta.width, height: meta.height });
    } catch {
      // Engine restart, no frame yet, etc. Keep polling.
    }
    await sleep(500);
  }
}

function once(el: HTMLImageElement, evt: "load" | "error"): Promise<void> {
  return new Promise((resolve) => {
    const onEvt = () => {
      el.removeEventListener(evt, onEvt);
      resolve();
    };
    el.addEventListener(evt, onEvt);
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
