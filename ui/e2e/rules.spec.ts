// Rules page smoke + editor sheet round-trip.

import { expect, test } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

test.describe("rules", () => {
  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
  });

  test("empty list + new-rule sheet opens with CEL textarea", async ({
    page,
  }) => {
    await page.goto("/rules");
    await expect(
      page.getByRole("heading", { name: /^rules$/i }),
    ).toBeVisible();

    await expect(page.getByText(/no rules configured/i)).toBeVisible();

    await page.getByRole("button", { name: /new rule/i }).click();
    await expect(
      page.getByRole("heading", { name: /^new rule$/i }),
    ).toBeVisible();

    // Identity fields confirm the sheet rendered. A new rule has no id
    // input: the engine assigns `rule-<N>` on save.
    await expect(page.getByText(/auto-assigned on save/i)).toBeVisible();
    await expect(page.getByPlaceholder(/person in dwell zone/i)).toBeVisible();

    await page.getByRole("button", { name: /^cancel$/i }).click();
    await expect(
      page.getByRole("heading", { name: /^new rule$/i }),
    ).toBeHidden();
  });
});
