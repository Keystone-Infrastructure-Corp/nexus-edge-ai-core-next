import type { TrackedObject } from "../api/types.js";

const PALETTE = [
  "#38e1ff",
  "#ffd166",
  "#ef476f",
  "#06d6a0",
  "#a78bfa",
  "#ff9f1c",
];

function colorFor(track_id: number): string {
  const i = Math.abs(Math.floor(track_id)) % PALETTE.length;
  return PALETTE[i] ?? "#38e1ff";
}

/** Draw a video frame from an `<img>` plus tracked-object overlays. */
export function drawFrame(
  canvas: HTMLCanvasElement,
  img: HTMLImageElement,
  objects: TrackedObject[],
  meta: { width: number; height: number },
): void {
  // Fit the natural-image canvas to the displayed canvas size.
  const ratio = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth;
  const cssH = canvas.clientHeight;
  canvas.width = Math.round(cssW * ratio);
  canvas.height = Math.round(cssH * ratio);

  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, cssW, cssH);

  if (img.complete && img.naturalWidth > 0) {
    // Letterbox.
    const sw = img.naturalWidth;
    const sh = img.naturalHeight;
    const scale = Math.min(cssW / sw, cssH / sh);
    const dw = sw * scale;
    const dh = sh * scale;
    const dx = (cssW - dw) / 2;
    const dy = (cssH - dh) / 2;
    ctx.drawImage(img, dx, dy, dw, dh);

    // Overlay tracked objects in the same scaled coords.
    ctx.lineWidth = 2;
    ctx.font = "12px monospace";
    for (const o of objects) {
      const x = dx + o.bbox.x1 * scale;
      const y = dy + o.bbox.y1 * scale;
      const w = (o.bbox.x2 - o.bbox.x1) * scale;
      const h = (o.bbox.y2 - o.bbox.y1) * scale;
      const c = colorFor(o.track_id);
      ctx.strokeStyle = c;
      ctx.fillStyle = c;
      ctx.strokeRect(x, y, w, h);
      const tag = `#${o.track_id} ${o.label} ${(o.confidence * 100).toFixed(0)}%`;
      const padding = 3;
      const tw = ctx.measureText(tag).width + padding * 2;
      ctx.fillRect(x, y - 16, tw, 16);
      ctx.fillStyle = "#0b0d10";
      ctx.fillText(tag, x + padding, y - 4);
    }
  }
  // Suppress meta-unused warning while keeping the API stable.
  void meta;
}
