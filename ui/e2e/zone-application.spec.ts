// SPEC-088 — the camera editor reports whether the running camera uses the
// zones it shows, and never claims Applied without a fresh, matching receipt.
//
// The engine is real: cameras, saves and the config snapshot (with the
// revision it computes) are the engine's. Only the stats read's
// `zones_runtime` is rewritten, so a receipt can say what a running camera
// would, with the engine's own revisions.

import { expect, test, type Page, type Request } from "@playwright/test";

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

/** The engine could not read the saved zones: every key but the state null, which `Receipt` cannot type. */
const READ_ERROR = {
  schema_version: 1,
  desired_revision: null,
  applied_revision: null,
  generation: null,
  applied_generation: null,
  state: "read_error",
  applied_at: null,
  error_code: null,
} as unknown as Receipt;

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

/** Hide or show the page, as a tab switch does. */
async function setVisibility(page: Page, state: "hidden" | "visible") {
  await page.evaluate((s) => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => s });
    document.dispatchEvent(new Event("visibilitychange", { bubbles: true }));
  }, state);
}

// A receipt change shows on the next poll: every 2 s just after a save,
// every 10 s while unresolved. Applied and Inactive read once a minute:
// `steadyRead` jumps the page's clock to that read.
const POLL = { timeout: 15_000 };

/** Jump the page's clock a minute ahead, to the editor's next steady stats read. */
const steadyRead = (page: Page) => page.clock.fastForward(60_000);

/** Click the zone canvas at normalised (`x`, `y`). */
async function clickCanvas(page: Page, x: number, y: number, clicks: 1 | 2 = 1) {
  const canvas = page.locator('svg[viewBox="0 0 1 1"]').locator("..");
  const box = await canvas.boundingBox();
  if (!box) throw new Error("the zone canvas is not laid out");
  await canvas.click({ position: { x: box.width * x, y: box.height * y }, clickCount: clicks });
}

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
    // Time flows as usual; a test jumps it ahead to reach a steady read.
    await page.clock.install();
    await loginAsAdmin(page);
    await page.goto("/cameras");
  });

  test.afterEach(async ({ page }) => {
    if (cameraId !== null) await engine(page, "DELETE", `/cameras/${cameraId}`);
    cameraId = null;
  });

  test("an applied camera reads its stats once a minute, not at a decode refresh", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    const statsUrl = `/api/v1/cameras/${cameraId}/stats`;
    let reads = 0;
    page.on("request", (r) => {
      if (new URL(r.url()).pathname === statsUrl) reads += 1;
    });
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    // A minute in 5 s steps, each long enough for a read it starts to land.
    const from = reads;
    for (let step = 0; step < 12; step++) {
      await page.clock.fastForward(5_000);
      await page.waitForTimeout(250);
    }
    expect(reads - from).toBeGreaterThanOrEqual(1);
    expect(reads - from).toBeLessThanOrEqual(2);
    await expect(status).toContainText("Applied to camera");
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

  test("a denied saved-zone read stops every stats read", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    await page.route(`**/api/v1/admin/cameras/${cameraId}/config-snapshot`, (route) =>
      route.fulfill({ status: 403, json: { error: "e2e: forbidden" } }),
    );
    const statsUrl = `/api/v1/cameras/${cameraId}/stats`;
    const reads: number[] = [];
    page.on("request", (r) => {
      if (new URL(r.url()).pathname === statsUrl) reads.push(Date.now());
    });
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Cannot verify: no access to the saved zones");

    // Not even the editor's shared decode refresh reads on.
    const from = Date.now();
    await page.waitForTimeout(7_000);
    expect(reads.filter((t) => t >= from)).toHaveLength(0);
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
    await steadyRead(page);
    await expect(status).toContainText("Saved; applying to camera", POLL);
    receipt.set(applied(4));
    await expect(status).toContainText("Applied to camera", POLL);

    // A generation older than one already read cannot recover the verdict.
    receipt.set(applied(1));
    await steadyRead(page);
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
    await steadyRead(page);
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
    await steadyRead(page);
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
    await steadyRead(page);
    await expect(status).toContainText("Unsaved changes; zones changed elsewhere", POLL);
    await expect(zoneRow(page, "Mine")).toBeVisible();
    await expect(zoneRow(page, "Loading dock")).toHaveCount(0);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
  });

  test("a role demoted by a token refresh drops the edit and shows the denial", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    await expect(status).toContainText("Unsaved changes");

    // The next stats read answers 401; the refresh it triggers rotates the
    // tokens and carries the operator role, which may not read saved zones.
    let demoted = false;
    await page.route("**/api/v1/auth/refresh", async (route) => {
      const res = await route.fetch();
      const tokens = (await res.json()) as { user: Record<string, unknown> };
      demoted = true;
      await route.fulfill({ response: res, json: { ...tokens, user: { ...tokens.user, role: "operator" } } });
    });
    await page.route(`**/api/v1/admin/cameras/${cameraId}/config-snapshot`, (route) =>
      demoted ? route.fulfill({ status: 403, json: { error: "e2e: forbidden" } }) : route.fallback(),
    );
    let expire = true;
    await page.route(`**/api/v1/cameras/${cameraId}/stats`, (route) => {
      if (!expire) return route.fallback();
      expire = false;
      return route.fulfill({ status: 401, json: { error: "e2e: token expired" } });
    });
    await steadyRead(page);

    await expect(status).toContainText("Cannot verify: no access to the saved zones", POLL);
    expect(demoted).toBe(true);
    await expect(zoneRow(page, "Gate")).toHaveCount(0);
    await expect(zoneRow(page, "Door")).toBeVisible();
    await expect(page.getByText("Unsaved changes")).toHaveCount(0);
  });

  test("a denied saved-zone read during an edit drops it and shows the denial", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await zoneRow(page, "Door").getByRole("textbox").fill("Mine");
    await expect(status).toContainText("Unsaved changes");

    // Another save makes the editor reload the saved zones, and that read
    // is now refused: the access is gone.
    await page.route(`**/api/v1/admin/cameras/${cameraId}/config-snapshot`, (route) =>
      route.fulfill({ status: 403, json: { error: "e2e: forbidden" } }),
    );
    receipt.set(applied(2));
    await saveZonesElsewhere(page, cameraId, [DOCK]);
    await steadyRead(page);

    await expect(status).toContainText("Cannot verify: no access to the saved zones", POLL);
    await expect(zoneRow(page, "Mine")).toHaveCount(0);
    await expect(page.getByText("Unsaved changes")).toHaveCount(0);
  });

  /** Park the camera's next PUT until released, then send it to the engine. */
  async function holdSave(page: Page, id: number) {
    let seen = () => {};
    let release = () => {};
    const parked = new Promise<void>((resolve) => (seen = resolve));
    const opened = new Promise<void>((resolve) => (release = resolve));
    await page.route(`**/api/v1/cameras/${id}`, async (route) => {
      if (route.request().method() !== "PUT") return route.fallback();
      seen();
      await opened;
      await route.continue();
    });
    return { parked, release };
  }

  /** After the denial, for `ms`: it stays shown, and no edit, save or verdict replaces it. */
  async function denialHolds(page: Page, denial: string, ms: number) {
    const status = page.getByTestId("zone-application");
    for (const until = Date.now() + ms; Date.now() < until; await page.waitForTimeout(200)) {
      await expect(status).toContainText(denial, { timeout: 100 });
      for (const masked of ["Unsaved changes", "Saving to camera…", "Applied to camera"]) {
        expect(await page.getByText(masked).count()).toBe(0);
      }
    }
  }

  test("a denied stats read during a save shows the denial, and the late success changes nothing", async ({
    page,
  }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    const save = await holdSave(page, id);
    await page.getByRole("button", { name: /^save$/i }).click();
    await save.parked;
    await expect(status).toContainText("Saving to camera…");

    // The access goes while the save is in flight: the next stats read is refused.
    await page.route(`**/api/v1/cameras/${id}/stats`, (route) =>
      route.fulfill({ status: 403, json: { error: "e2e: forbidden" } }),
    );
    const denial = "Cannot confirm: status read failed";
    await expect(status).toContainText(denial, POLL);
    await denialHolds(page, denial, 1_000);

    const saved = page.waitForResponse(
      (r) => r.request().method() === "PUT" && new URL(r.url()).pathname === `/api/v1/cameras/${id}`,
    );
    save.release();
    expect((await saved).ok()).toBe(true);
    await denialHolds(page, denial, 4_000);
  });

  test("a denied saved-zone read during a save shows the denial, and the late success changes nothing", async ({
    page,
  }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    // The editor's first saved-zone read is still in flight when the save starts.
    let answer = () => {};
    const answered = new Promise<void>((resolve) => (answer = resolve));
    await page.route(`**/api/v1/admin/cameras/${id}/config-snapshot`, async (route) => {
      await answered;
      await route.fulfill({ status: 403, json: { error: "e2e: forbidden" } });
    });
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    const save = await holdSave(page, id);
    await page.getByRole("button", { name: /^save$/i }).click();
    await save.parked;
    await expect(status).toContainText("Saving to camera…");

    answer();
    const denial = "Cannot verify: no access to the saved zones";
    await expect(status).toContainText(denial, POLL);
    await denialHolds(page, denial, 1_000);

    const saved = page.waitForResponse(
      (r) => r.request().method() === "PUT" && new URL(r.url()).pathname === `/api/v1/cameras/${id}`,
    );
    save.release();
    expect((await saved).ok()).toBe(true);
    await denialHolds(page, denial, 4_000);
    await expect(zoneRow(page, "Gate")).toHaveCount(0);
    await expect(zoneRow(page, "Door")).toBeVisible();
  });

  test("a hidden page reconciles a late mismatch only once visible again", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    const reads = [`/api/v1/admin/cameras/${cameraId}/config-snapshot`, `/api/v1/cameras/${cameraId}/stats`];
    const started: string[] = [];
    page.on("request", (r) => started.push(new URL(r.url()).pathname));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    // A stats read in flight when the page is hidden lands after another
    // operator's save and reports the mismatch.
    const held = receipt.holdNext();
    await steadyRead(page);
    await held.parked;
    await setVisibility(page, "hidden");
    receipt.set(applied(2));
    await saveZonesElsewhere(page, cameraId, [DOCK]);
    const from = started.length;
    held.release(applied(2));
    await page.waitForTimeout(4_000);
    expect(started.slice(from).filter((p) => reads.includes(p))).toEqual([]);

    await setVisibility(page, "visible");
    await expect(zoneRow(page, "Loading dock")).toBeVisible(POLL);
    await expect(status).toContainText("Applied to camera", POLL);
  });

  test("failed reads, old receipts and engine failures are never Applied, and recover", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    receipt.set(() => "error");
    await steadyRead(page);
    await expect(status).toContainText("Cannot confirm: status read failed", POLL);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);

    receipt.set(() => READ_ERROR);
    await expect(status).toContainText("Cannot confirm: engine could not read the saved zones", POLL);
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
    await steadyRead(page);
    await expect(status).toContainText("Saved; running application cannot be verified", POLL);
    await expect(page.getByText("Applied to camera")).toHaveCount(0);
  });

  for (const code of [503, 403]) {
    test(`a scheduled stats read answering ${code} drops Applied at once`, async ({ page }) => {
      cameraId = await createCamera(page, [DOOR]);
      const receipt = await controlReceipt(page, cameraId);
      receipt.set(applied(1));
      await page.goto(`/cameras/${cameraId}`);
      const status = page.getByTestId("zone-application");
      await expect(status).toContainText("Applied to camera");

      // The next scheduled read fails; any read after it never answers, so
      // a retry of the failed one would keep the old Applied on screen.
      const statsUrl = `/api/v1/cameras/${cameraId}/stats`;
      const failedRead = page.waitForResponse(
        (r) => new URL(r.url()).pathname === statsUrl && r.status() === code,
        POLL,
      );
      let failed = false;
      await page.route(`**${statsUrl}`, async (route) => {
        if (failed) return new Promise<void>(() => {});
        failed = true;
        await route.fulfill({ status: code, json: { error: `e2e: ${code}` } });
      });
      await steadyRead(page);
      await failedRead;
      await expect(page.getByText("Applied to camera")).toHaveCount(0, { timeout: 500 });
      await expect(status).toContainText("Cannot confirm: status read failed");
      await page.unrouteAll({ behavior: "ignoreErrors" });
    });
  }

  test("a sketch in progress is dropped when the editor's authority changes", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");
    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.6, 0.6);
    await clickCanvas(page, 0.8, 0.6);
    await expect(page.getByRole("button", { name: "Finish (2 pt)" })).toBeVisible();

    // The next stats read answers 401; the refresh it triggers carries the
    // operator role, and the one after it the admin role again.
    let role: string | null = "operator";
    let refreshes = 0;
    await page.route("**/api/v1/auth/refresh", async (route) => {
      const res = await route.fetch();
      const tokens = (await res.json()) as { user: Record<string, unknown> };
      refreshes += 1;
      await route.fulfill({ response: res, json: { ...tokens, user: { ...tokens.user, ...(role ? { role } : {}) } } });
    });
    let expire = true;
    await page.route(`**/api/v1/cameras/${cameraId}/stats`, (route) => {
      if (!expire) return route.fallback();
      expire = false;
      return route.fulfill({ status: 401, json: { error: "e2e: token expired" } });
    });
    await steadyRead(page);
    await expect.poll(() => refreshes, POLL).toBe(1);
    await expect(page.getByRole("button", { name: /^Finish/ })).toHaveCount(0);
    role = null;
    expire = true;
    await steadyRead(page);
    await expect.poll(() => refreshes, POLL).toBe(2);

    // Nothing of the sketch is left to finish or commit.
    await expect(page.getByRole("button", { name: "New zone" })).toBeVisible();
    await clickCanvas(page, 0.7, 0.8, 2);
    await expect(zoneRow(page, "Zone 2")).toHaveCount(0);
    await expect(zoneRow(page, "Door")).toBeVisible();
    await expect(status).toContainText("Applied to camera", POLL);
  });

  test("a sketch in progress is an unsaved change, never Applied", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const receipt = await controlReceipt(page, cameraId);
    receipt.set(applied(1));
    await page.goto(`/cameras/${cameraId}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.6, 0.6);
    await expect(status).toContainText("Unsaved changes");
    // Across the editor's next scheduled stats read.
    await steadyRead(page);
    await neverApplied(page, 3_000);

    const finish = page.getByRole("button", { name: /^Finish/ });
    await finish.locator("..").getByRole("button", { name: "Cancel" }).click();
    await expect(status).toContainText("Applied to camera", POLL);
  });

  test("a sketch keeps the zones it was drawn over when another operator's save arrives", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR, DOCK]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.3, 0.6);
    await clickCanvas(page, 0.5, 0.6);
    await clickCanvas(page, 0.4, 0.8);
    const finish = page.getByRole("button", { name: "Finish (3 pt)" });
    await expect(finish).toBeVisible();

    // Another operator removes the Door exclusion, and the editor reads that save.
    receipt.set(applied(2));
    await saveZonesElsewhere(page, id, [DOCK]);
    const remote = page.waitForResponse(
      async (r) =>
        new URL(r.url()).pathname === `/api/v1/admin/cameras/${id}/config-snapshot` &&
        ((await r.json()) as { settings: { zones: Zone[] } }).settings.zones.length === 1,
    );
    await steadyRead(page);
    await remote;
    await expect(status).toContainText("Unsaved changes; zones changed elsewhere", POLL);
    await expect(zoneRow(page, "Door")).toBeVisible();

    await finish.click();
    for (const name of ["Door", "Loading dock", "Zone 3"]) await expect(zoneRow(page, name)).toBeVisible();
    await expect(status).toContainText("Unsaved changes; zones changed elsewhere", POLL);
    await neverApplied(page, 1_000);

    const put = page.waitForRequest(
      (r) => r.method() === "PUT" && new URL(r.url()).pathname === `/api/v1/cameras/${id}`,
    );
    await page.getByRole("button", { name: /^save$/i }).click();
    const sent = ((await put).postDataJSON() as { zones: Zone[] }).zones;
    expect(sent.slice(0, 2)).toEqual([DOOR, DOCK]);
    expect(sent.map((z) => z.name)).toEqual(["Door", "Loading dock", "Zone 3"]);
    await expect(status).toContainText("Applied to camera", POLL);
  });

  test("a sketch cancelled after another operator's save shows that save", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.6, 0.6);
    receipt.set(applied(2));
    await saveZonesElsewhere(page, id, [DOCK]);
    const remote = page.waitForResponse(
      async (r) =>
        new URL(r.url()).pathname === `/api/v1/admin/cameras/${id}/config-snapshot` &&
        ((await r.json()) as { settings: { zones: Zone[] } }).settings.zones[0]?.id === DOCK.id,
    );
    await steadyRead(page);
    await remote;
    await expect(zoneRow(page, "Door")).toBeVisible();

    const finish = page.getByRole("button", { name: /^Finish/ });
    await finish.locator("..").getByRole("button", { name: "Cancel" }).click();
    await expect(zoneRow(page, "Loading dock")).toBeVisible();
    await expect(zoneRow(page, "Door")).toHaveCount(0);
    await expect(status).toContainText("Applied to camera", POLL);
  });

  /** For `ms`, the status reads exactly `title`: no other state, Applied included. */
  async function statusHolds(page: Page, title: string, ms: number) {
    const status = page.getByTestId("zone-application");
    for (const until = Date.now() + ms; Date.now() < until; await page.waitForTimeout(200)) {
      await expect(status.getByText(title, { exact: true })).toBeVisible({ timeout: 100 });
    }
  }

  const isPut = (id: number) => (r: Request) =>
    r.method() === "PUT" && new URL(r.url()).pathname === `/api/v1/cameras/${id}`;

  test("an edit typed while a save is pending stays unsaved when the save succeeds", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    const save = await holdSave(page, id);
    await page.getByRole("button", { name: /^save$/i }).click();
    await save.parked;
    await expect(status).toContainText("Saving to camera…");
    await zoneRow(page, "Gate").getByRole("textbox").fill("Gate 2");
    await expect(status.getByText("Unsaved changes", { exact: true })).toBeVisible();

    // The engine applies the save; the newer edit is not part of it.
    receipt.set(applied(2));
    const saved = page.waitForResponse((r) => isPut(id)(r.request()));
    save.release();
    expect((await saved).ok()).toBe(true);
    await statusHolds(page, "Unsaved changes", 5_000);
    await steadyRead(page);
    await statusHolds(page, "Unsaved changes", 2_000);
    await expect(zoneRow(page, "Gate 2")).toBeVisible();

    // Saved in turn, the newer edit is what applies.
    receipt.set(applied(3));
    const put = page.waitForRequest(isPut(id));
    await page.getByRole("button", { name: /^save$/i }).click();
    expect(((await put).postDataJSON() as { zones: Zone[] }).zones).toEqual([{ ...DOOR, name: "Gate 2" }]);
    await expect(status).toContainText("Applied to camera", POLL);
    await expect(zoneRow(page, "Gate 2")).toBeVisible();
  });

  test("a save while sketching on a clean view sends the zones shown, not the sketch", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.6, 0.6);
    await clickCanvas(page, 0.8, 0.6);
    const finish = page.getByRole("button", { name: "Finish (2 pt)" });
    await expect(finish).toBeVisible();
    await expect(status.getByText("Unsaved changes", { exact: true })).toBeVisible();

    receipt.set(applied(2));
    const put = page.waitForRequest(isPut(id));
    const saved = page.waitForResponse((r) => isPut(id)(r.request()));
    await page.getByRole("button", { name: /^save$/i }).click();
    expect(((await put).postDataJSON() as { zones: Zone[] }).zones).toEqual([DOOR]);
    expect((await saved).ok()).toBe(true);
    // The sketch is still open: an unsaved change across the save's reads.
    await expect(finish).toBeVisible();
    await statusHolds(page, "Unsaved changes", 5_000);
    await steadyRead(page);
    await statusHolds(page, "Unsaved changes", 2_000);

    await finish.locator("..").getByRole("button", { name: "Cancel" }).click();
    await expect(status).toContainText("Applied to camera", POLL);
    await expect(zoneRow(page, "Door")).toBeVisible();
    await expect(zoneRow(page, "Zone 2")).toHaveCount(0);
  });

  test("a sketch over a pending save is unsaved until it ends, then shows the save", async ({ page }) => {
    cameraId = await createCamera(page, [DOOR]);
    const id = cameraId;
    const receipt = await controlReceipt(page, id);
    receipt.set(applied(1));
    await page.goto(`/cameras/${id}`);
    const status = page.getByTestId("zone-application");
    await expect(status).toContainText("Applied to camera");

    await zoneRow(page, "Door").getByRole("textbox").fill("Gate");
    const save = await holdSave(page, id);
    const put = page.waitForRequest(isPut(id));
    await page.getByRole("button", { name: /^save$/i }).click();
    await save.parked;
    await expect(status).toContainText("Saving to camera…");
    await page.getByRole("button", { name: "New zone" }).click();
    await clickCanvas(page, 0.6, 0.6);
    const finish = page.getByRole("button", { name: "Finish (1 pt)" });
    await expect(finish).toBeVisible();
    await expect(status.getByText("Unsaved changes", { exact: true })).toBeVisible();

    // The save goes through and applies while the sketch is open.
    receipt.set(applied(2));
    const saved = page.waitForResponse((r) => isPut(id)(r.request()));
    save.release();
    expect((await saved).ok()).toBe(true);
    expect(((await put).postDataJSON() as { zones: Zone[] }).zones).toEqual([{ ...DOOR, name: "Gate" }]);
    await statusHolds(page, "Unsaved changes", 5_000);
    await steadyRead(page);
    await statusHolds(page, "Unsaved changes", 2_000);

    // Ended empty, it leaves no change: the saved zones show, applied.
    await finish.locator("..").getByRole("button", { name: "Cancel" }).click();
    await expect(status).toContainText("Applied to camera", POLL);
    await expect(zoneRow(page, "Gate")).toBeVisible();
    await expect(zoneRow(page, "Zone 2")).toHaveCount(0);
  });
});
