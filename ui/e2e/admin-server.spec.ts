// Admin Server — read-only view + Phase 0 callout panels.

import { expect, test } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

test.describe("admin: server", () => {
  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
  });

  test("page renders with Phase 0 callouts", async ({ page }) => {
    await page.goto("/admin/server");
    // Page heading is "Server settings", not "Server".
    await expect(
      page.getByRole("heading", { name: /server settings/i }),
    ).toBeVisible();

    // Live editors landed in Phase 0 closeout — bind address +
    // storage watermarks now have inline forms.
    await expect(page.getByLabel(/primary bind address/i)).toBeVisible();
    await expect(page.getByText(/storage watermarks/i).first()).toBeVisible();
    await expect(page.getByLabel(/low watermark/i)).toBeVisible();
    await expect(page.getByLabel(/panic watermark/i)).toBeVisible();
  });
});
