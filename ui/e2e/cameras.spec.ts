// Cameras page smoke + editor sheet round-trip.
//
// Fresh DB has no cameras → empty state visible.
// Add-camera button opens the editor sheet with required fields.

import { expect, test, type Page } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

// Hold every GET /models/prompts until the returned release is called, so a
// spec can look at the page while the engine's default model is unread.
async function holdPromptsCatalog(page: Page): Promise<() => void> {
  let release: () => void = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/v1/models/prompts", async (route) => {
    await held;
    await route.continue();
  });
  return release;
}

test.describe("cameras", () => {
  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
  });

  test("empty list + add-camera sheet opens", async ({ page }) => {
    await page.goto("/cameras");
    await expect(
      page.getByRole("heading", { name: /^cameras$/i }),
    ).toBeVisible();

    // Fresh DB: no cameras configured.
    await expect(page.getByText(/no cameras configured/i)).toBeVisible();

    // Open the editor sheet.
    await page.getByRole("button", { name: /add camera/i }).click();

    // Sheet header + the two required identifying fields.
    await expect(
      page.getByRole("heading", { name: /^new camera$/i }),
    ).toBeVisible();
    await expect(
      page.getByPlaceholder(/front door/i),
    ).toBeVisible();
    await expect(
      page.getByPlaceholder(/rtsp:\/\//i),
    ).toBeVisible();

    // A new camera has no model override, so the engine analyses it at its
    // default model's width: global-setup's 1024, a 1024 × 576 frame. The
    // tiling preview must state that frame, not a fixed 512.
    await expect(page.getByText(/^Analysis 1024 × 576 · /)).toBeVisible();

    // Cancel closes the sheet.
    await page.getByRole("button", { name: /^cancel$/i }).click();
    await expect(
      page.getByRole("heading", { name: /^new camera$/i }),
    ).toBeHidden();
  });

  // The preview's frame comes from GET /models/prompts, the engine's default
  // model. While that read is in flight the preview says so and states no
  // frame, not a 512 × 288 guess; once it lands, the engine's frame.
  test("add-camera preview states no frame until the default model is read", async ({
    page,
  }) => {
    const release = await holdPromptsCatalog(page);
    await page.goto("/cameras");
    await page.getByRole("button", { name: /add camera/i }).click();

    await expect(
      page.getByText("Reading the engine's default model…"),
    ).toBeVisible();
    await expect(page.getByText(/^Analysis \d/)).toHaveCount(0);

    release();
    await expect(page.getByText(/^Analysis 1024 × 576 · /)).toBeVisible();
    await expect(
      page.getByText("Reading the engine's default model…"),
    ).toHaveCount(0);
  });

  test("add-camera preview states no frame when the default model cannot be read", async ({
    page,
  }) => {
    await page.route("**/api/v1/models/prompts", (route) =>
      route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ error: "e2e: catalog unavailable" }),
      }),
    );
    await page.goto("/cameras");
    await page.getByRole("button", { name: /add camera/i }).click();

    await expect(
      page.getByText(
        "Analysis frame unknown: the engine's default model could not be read.",
      ),
    ).toBeVisible();
    await expect(page.getByText(/^Analysis \d/)).toHaveCount(0);
  });

  // The crowd-downscale rungs are the ones below the detector input, which a
  // camera with no model override takes from the same catalog. While it is
  // unread, the picker still shows the camera's saved rung, not "Off".
  test("a saved crowd-downscale rung is shown while the default model is unread", async ({
    page,
  }) => {
    await page.goto("/cameras");
    const id = await page.evaluate(async () => {
      const s = JSON.parse(localStorage.getItem("nexus_session") ?? "{}");
      const r = await fetch("/api/v1/cameras", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${s.access_token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          id: 0,
          name: "e2e crowd rung",
          url: "rtsp://127.0.0.1:9/e2e",
          codec: "h264",
          enabled: false,
          detector_downscale_to_width: 512,
          detector_downscale_to_height: 288,
        }),
      });
      if (!r.ok) throw new Error(`create camera: HTTP ${r.status}`);
      return ((await r.json()) as { id: number }).id;
    });
    try {
      const release = await holdPromptsCatalog(page);
      await page.goto(`/cameras/${id}`);

      await expect(page.locator("#downscale-target")).toHaveValue("512");
      await expect(
        page.getByText("Reading the engine's default model…"),
      ).toBeVisible();

      release();
      await expect(page.getByText(/^Analysis 1024 × 576 · /)).toBeVisible();
      await expect(page.locator("#downscale-target")).toHaveValue("512");
    } finally {
      await page.evaluate(async (camId) => {
        const s = JSON.parse(localStorage.getItem("nexus_session") ?? "{}");
        await fetch(`/api/v1/cameras/${camId}`, {
          method: "DELETE",
          headers: { Authorization: `Bearer ${s.access_token}` },
        });
      }, id);
    }
  });

  test("discover sheet opens", async ({ page }) => {
    await page.goto("/cameras");
    await page.getByRole("button", { name: /discover/i }).click();
    // Sheet defaults to ONVIF multicast mode; switch to CIDR scan to
    // reveal the CIDR input with its 192.168.1.0/24 placeholder.
    await page.getByRole("button", { name: /cidr scan/i }).click();
    await expect(page.getByPlaceholder(/192\.168\.1\.0\/24/i)).toBeVisible();
  });
});
