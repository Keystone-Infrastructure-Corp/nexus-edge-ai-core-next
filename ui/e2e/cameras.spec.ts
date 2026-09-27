// Cameras page smoke + editor sheet round-trip.
//
// Fresh DB has no cameras → empty state visible.
// Add-camera button opens the editor sheet with required fields.

import { expect, test } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

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

  test("discover sheet opens", async ({ page }) => {
    await page.goto("/cameras");
    await page.getByRole("button", { name: /discover/i }).click();
    // Sheet defaults to ONVIF multicast mode; switch to CIDR scan to
    // reveal the CIDR input with its 192.168.1.0/24 placeholder.
    await page.getByRole("button", { name: /cidr scan/i }).click();
    await expect(page.getByPlaceholder(/192\.168\.1\.0\/24/i)).toBeVisible();
  });
});
