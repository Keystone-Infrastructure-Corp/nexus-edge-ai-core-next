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
    await expect(page.getByRole("heading", { name: /^rules$/i })).toBeVisible();

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

  // Rule writes need an admin login on the engine, so this proves the
  // console sends its session bearer on create and delete.
  test("save then delete a rule as admin", async ({ page }) => {
    await page.goto("/rules");
    await page.getByRole("button", { name: /new rule/i }).click();
    await page.getByPlaceholder(/person in dwell zone/i).fill("e2e auth rule");

    const created = page.waitForResponse(
      (r) =>
        r.url().endsWith("/api/v1/rules") && r.request().method() === "POST",
    );
    await page.getByRole("button", { name: /^save$/i }).click();
    expect((await created).status()).toBe(200);
    const row = page.getByRole("row", { name: /e2e auth rule/i });
    await expect(row).toBeVisible();

    page.once("dialog", (d) => d.accept());
    const deleted = page.waitForResponse(
      (r) =>
        r.request().method() === "DELETE" && r.url().includes("/api/v1/rules/"),
    );
    await row.getByRole("button").last().click();
    expect((await deleted).status()).toBe(204);
    await expect(page.getByText(/no rules configured/i)).toBeVisible();
  });
});
