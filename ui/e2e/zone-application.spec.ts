// SPEC-088 — the camera editor reports whether the running camera uses the
// zones it shows, and never claims Applied without a fresh, matching receipt.
//
// The engine is real: cameras, saves and the config snapshot (with the
// revision it computes) are the engine's. Only the stats read's
// `zones_runtime` is rewritten, so a receipt can say what a running camera
// would, with the engine's own revisions.

import { expect, test, type Page } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

type Zone = {
  id: string;
  name: string;
  polygon: Array<[number, number]>;
  kind: "inclusion" | "exclusion" | "dwell";
};
const DOOR: Zone = {
  id: "z-door",
  name: "Door",
  polygon: [[0.1, 0.1], [0.4, 0.1], [0.1, 0.4]],
  kind: "exclusion",
};
const DOCK: Zone = {
  id: "z-dock",
  name: "Loading dock",
  polygon: [[0.6, 0.1], [0.9, 0.1], [0.6, 0.4]],
  kind: "inclusion",
};

type Receipt = {
  schema_version: number;
  desired_revision: string;
  applied_revision: string | null;
  generation: number;
  applied_generation: number | null;
  state: string;
  applied_at: string | null;
  error_code: string | null;
};
/** `base` is the generation the engine first reported, so rewrites stay above it. */
type Rewrite = (real: Receipt, base: number) => Receipt | "missing" | "error";

const APPLIED_AT = "2026-11-24T16:30:00Z";
const applied =
  (gen: number): Rewrite =>
  (r, base) => ({
    ...r,
    applied_revision: r.desired_revision,
    generation: base + gen,
    applied_generation: base + gen,
    state: "applied",
    applied_at: APPLIED_AT,
    error_code: null,
  });
const applying =
  (gen: number, appliedGen: number, appliedRevision: (r: Receipt) => string): Rewrite =>
  (r, base) => ({
    ...r,
    applied_revision: appliedRevision(r),
    generation: base + gen,
    applied_generation: base + appliedGen,
    state: "applying",
    applied_at: APPLIED_AT,
    error_code: null,
  });

/** Call the engine as the signed-in admin, through the page's network. */
async function engine<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  return page.evaluate(
    async ({ method, path, body }) => {
      const s = JSON.parse(localStorage.getItem("nexus_session") ?? "{}");
      const r = await fetch(`/api/v1${path}`, {
        method,
        headers: {
          Authorization: `Bearer ${s.access_token}`,
          "Content-Type": "application/json",
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      if (!r.ok) throw new Error(`${method} ${path}: HTTP ${r.status} ${await r.text()}`);
      return r.status === 204 ? null : r.json();
    },
    { method, path, body },
  ) as Promise<T>;
}

const URL_WITH_CREDENTIALS = "rtsp://e2e:url-secret@127.0.0.1:9/e2e";
const ONVIF = {
  endpoint: "http://127.0.0.1:9/onvif/device_service",
  username: "e2e",
  password: "onvif-secret",
};

/** A disabled camera, so the engine never tries to stream it. */
async function createCamera(page: Page, zones: Zone[]): Promise<number> {
  const cam = await engine<{ id: number }>(page, "POST", "/cameras", {
    id: 0,
    name: "e2e zones",
    url: URL_WITH_CREDENTIALS,
    codec: "h264",
    enabled: false,
    onvif: ONVIF,
    zones,
  });
  return cam.id;
}

/** Another operator's full-config save of the camera with `zones`. */
async function saveZonesElsewhere(page: Page, id: number, zones: Zone[]) {
  const cams = await engine<Array<Record<string, unknown>>>(page, "GET", "/cameras");
  const cam = cams.find((c) => c.id === id);
  if (!cam) throw new Error(`camera ${id} not listed`);
  await engine(page, "PUT", `/cameras/${id}`, { ...cam, zones });
}

async function zonesRevision(page: Page, id: number): Promise<string> {
  return (await engine<{ zones_revision: string }>(page, "GET", `/admin/cameras/${id}/config-snapshot`))
    .zones_revision;
}

/**
 * Serve this camera's stats from the engine with `zones_runtime` rewritten.
 * `holdNext` parks the next request before it reaches the engine and
 * answers it with the rewrite given on release.
 */
async function controlReceipt(page: Page, id: number) {
  let rewrite: Rewrite = (r) => r;
  let base: number | null = null;
  let gate: { seen: () => void; opened: Promise<Rewrite> } | null = null;
  await page.route(`**/api/v1/cameras/${id}/stats`, async (route) => {
    let own: Rewrite | null = null;
    const g = gate;
    if (g) {
      gate = null;
      g.seen();
      own = await g.opened;
    }
    const res = await route.fetch();
    const body = (await res.json()) as { zones_runtime?: Receipt };
    if (!body.zones_runtime) throw new Error("the engine reported no zones_runtime");
    base ??= body.zones_runtime.generation;
    const out = (own ?? rewrite)(body.zones_runtime, base);
    if (out === "error") {
      await route.fulfill({ status: 500, json: { error: "e2e: stats unavailable" } });
      return;
    }
    if (out === "missing") delete body.zones_runtime;
    else body.zones_runtime = out;
    await route.fulfill({ response: res, json: body });
  });
  return {
    set: (r: Rewrite) => {
      rewrite = r;
    },
    holdNext: () => {
      let seen = () => {};
      let release: (r: Rewrite) => void = () => {};
      const parked = new Promise<void>((resolve) => {
        seen = resolve;
      });
      gate = { seen, opened: new Promise<Rewrite>((resolve) => (release = resolve)) };
      return { parked, release };
    },
  };
}

const zoneRow = (page: Page, name: string) =>
  page.getByRole("listitem").filter({ has: page.getByRole("button", { name: `Select ${name}` }) });
const polygon = (page: Page, zone: Zone) =>
  page.locator(`svg polygon[points="${zone.polygon.map(([x, y]) => `${x},${y}`).join(" ")}"]`);

// A receipt change shows on the next poll: every 2 s just after a save,
// otherwise at the editor's 5 s stats refresh.
const POLL = { timeout: 15_000 };

/** Applied never shows, checked every 200 ms for `ms`. */
async function neverApplied(page: Page, ms: number) {
  for (const until = Date.now() + ms; Date.now() < until; await page.waitForTimeout(200)) {
    expect(await page.getByText("Applied to camera").count()).toBe(0);
  }
}

test.describe("zone application", () => {
  test.describe.configure({ timeout: 90_000 });
  let cameraId: number | null = null;

  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
    await page.goto("/cameras");
  });

  test.afterEach(async ({ page }) => {
    if (cameraId !== null) await engine(page, "DELETE", `/cameras/${cameraId}`);
    cameraId = null;
  });

  test("a disabled camera's saved zones apply when enabled, without fast polling", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const statsUrl = `/api/v1/cameras/${cameraId}/stats`;
    const reads: number[] = [];
    page.on("request", (r) => {
      if (new URL(r.url()).pathname === statsUrl) reads.push(Date.now());
    });
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Saved; applies when enabled");
    await expect(zoneRow(page, "Door")).toBeVisible();

    await zoneRow(page, "Door").getByRole("textbox").fill("Side door");
    await expect(status).toContainText("Unsaved changes");
    await page.getByRole("button", { name: /^save$/i }).click();
    await expect(status).toContainText("Saved; applies when enabled", POLL);
    await expect(zoneRow(page, "Side door")).toBeVisible();

    // Inactive is resolved: no 2 s application polling, only the editor's
    // one shared stats refresh.
    const from = Date.now();
    await page.waitForTimeout(8_000);
    expect(reads.filter((t) => t >= from).length).toBeLessThanOrEqual(2);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
  });

  test("a save applies only on a fresh receipt for the saved zones", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const revA = await zonesRevision(page, cameraId);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await expect(polygon(page, DOOR)).toHaveCount(1);

    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    await expect(status).toContainText("Unsaved changes");
    await expect(page.getByText("Applied to camera")).toHaveCount(0);

    // Hold the PUT: nothing is applied while the save is pending.
    let releasePut = () => {};
    const putHeld = new Promise<void>((resolve) => (releasePut = resolve));
    await page.route(`**/api/v1/cameras/${cameraId}`, async (route) => {
      if (route.request().method() === "PUT") await putHeld;
      await route.continue();
    });
    receipt.set(applying(1, 1, () => revA));
    await page.getByRole("button", { name: /^save$/i }).click();
    await expect(status).toContainText("Saving to camera…");
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
    releasePut();
    await expect(status).toContainText("Saved; applying to camera");
    await expect(page.getByText("Applied to camera")).toHaveCount(0);

    receipt.set(applied(2));
    await expect(status).toContainText("Applied to camera", POLL);
    await expect(zoneRow(page, "Gate")).toBeVisible();

    // Equal digests from an older generation are not success; a fresh
    // generation's acknowledgement is.
    receipt.set(applying(3, 2, (r) => r.desired_revision));
    await expect(status).toContainText("Saved; applying to camera", POLL);
    receipt.set(applied(4));
    await expect(status).toContainText("Applied to camera", POLL);

    // A generation older than one already read cannot recover the verdict.
    receipt.set(applied(1));
    await expect(status).toContainText("Saved; applying to camera", POLL);
  });

  test("a save pending when the editor closes stays pending when it reopens", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const revA = await zonesRevision(page, cameraId);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    let releasePut = () => {};
    const putHeld = new Promise<void>((resolve) => (releasePut = resolve));
    await page.route(`**/api/v1/cameras/${cameraId}`, async (route) => {
      if (route.request().method() === "PUT") await putHeld;
      await route.continue();
    });
    await page.getByRole("button", { name: /^save$/i }).click();
    await expect(status).toContainText("Saving to camera…");

    // Closed and reopened mid-save, with the engine still reporting the
    // old zones freshly applied: the reopened editor knows the save.
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(status).toHaveCount(0);
    await page.getByRole("row").filter({ hasText: "e2e zones" }).getByRole("button").first().click();
    await expect(status).toContainText("Saving to camera…");
    await neverApplied(page, 6_000);

    receipt.set(applying(2, 1, () => revA));
    releasePut();
    await expect(status).toContainText("Saved; applying to camera", POLL);
    await expect(zoneRow(page, "Gate")).toBeVisible();
    await neverApplied(page, 3_000);

    receipt.set(applied(2));
    await expect(status).toContainText("Applied to camera", POLL);
  });

  test("a stats request begun before the save cannot certify it", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    const held = receipt.holdNext();
    await held.parked;
    // Every later read reports the saved zones still applying...
    receipt.set(applying(2, 1, () => `sha256:${"0".repeat(64)}`));
    await page.getByRole("button", { name: /^save$/i }).click();
    await expect(status).toContainText("Saved; applying to camera", POLL);
    // ...while the request parked before the save answers Applied for them.
    held.release(applied(2));
    await page.waitForTimeout(4_000);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
    await expect(status).toContainText("Saved; applying to camera");
  });

  test("another operator's save replaces Applied, then certifies the zones it loads", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await expect(zoneRow(page, "Door")).toBeVisible();

    receipt.set(applied(2));
    await saveZonesElsewhere(page, cameraId, [DOCK]);
    await expect(zoneRow(page, "Loading dock")).toBeVisible(POLL);
    await expect(status).toContainText("Applied to camera", POLL);
    // The verdict sits beside the zones it certifies.
    await expect(polygon(page, DOCK)).toHaveCount(1);
    await expect(polygon(page, DOOR)).toHaveCount(0);
    await expect(zoneRow(page, "Door")).toHaveCount(0);

    // A zone edit on the loaded zones saves them, not the editor's stale
    // base, and keeps every other field and credential of the camera.
    await zoneRow(page, "Loading dock").getByRole("textbox").fill("Dock C");
    const put = page.waitForRequest(
      (r) => r.method() === "PUT" && new URL(r.url()).pathname === `/api/v1/cameras/${cameraId}`,
    );
    await page.getByRole("button", { name: /^save$/i }).click();
    const body = (await put).postDataJSON() as Record<string, unknown>;
    expect(body.zones).toEqual([{ ...DOCK, name: "Dock C" }]);
    expect(body.url).toBe(URL_WITH_CREDENTIALS);
    expect(body.onvif).toEqual(ONVIF);
    expect(body.name).toBe("e2e zones");
    expect(body).not.toHaveProperty("zones_revision");
    await expect(status).toContainText("Applied to camera", POLL);
    await expect(zoneRow(page, "Dock C")).toBeVisible();
  });

  test("an unsaved edit survives another operator's save and is never Applied", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await zoneRow(page, "Door").getByRole("textbox").fill("Mine");
    await expect(status).toContainText("Unsaved changes");
    receipt.set(applied(2));
    await saveZonesElsewhere(page, cameraId, [DOCK]);
    await expect(status).toContainText("Unsaved changes; zones changed elsewhere", POLL);
    await expect(zoneRow(page, "Mine")).toBeVisible();
    await expect(zoneRow(page, "Loading dock")).toHaveCount(0);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
  });

  test("failed reads, old receipts and engine failures are never Applied, and recover", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    receipt.set(() => "error");
    await expect(status).toContainText("Cannot confirm: status read failed", POLL);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);

    receipt.set((r, base) => ({
      ...r,
      applied_revision: null,
      generation: base + 2,
      applied_generation: null,
      state: "failed",
      applied_at: null,
      error_code: "application_timeout",
    }));
    await expect(status).toContainText("Not applied: camera did not process a frame within 30 s", POLL);

    // A late acknowledgement recovers the verdict with no save or reload.
    receipt.set(applied(2));
    await expect(status).toContainText("Applied to camera", POLL);

    receipt.set(() => "missing");
    await expect(status).toContainText("Saved; running application cannot be verified", POLL);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
  });
});
