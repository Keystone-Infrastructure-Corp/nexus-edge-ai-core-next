// Admin Users — list shows bootstrap admin, create-user round-trip exposes OTP.

import { expect, test } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

test.describe("admin: users", () => {
  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
  });

  test("bootstrap admin row is visible", async ({ page }) => {
    await page.goto("/admin/users");
    await expect(
      page.getByRole("heading", { name: /^users$/i }),
    ).toBeVisible();

    // The bootstrap admin always exists after first boot.
    await expect(
      page.locator("table").getByText("admin", { exact: true }).first(),
    ).toBeVisible();
  });

  test("create user with generated OTP reveals it once", async ({ page }) => {
    await page.goto("/admin/users");

    const username = `t${Date.now().toString(36)}`;
    await page.getByRole("button", { name: /new user/i }).click();
    await expect(
      page.getByRole("heading", { name: /^new user$/i }),
    ).toBeVisible();

    await page.getByLabel(/^username$/i).fill(username);
    // Leave "set my own password" unchecked → engine mints an OTP.
    await page.getByRole("button", { name: /^create$/i }).click();

    // OtpModal appears with the username and a copy button.
    // The username also appears in the underlying users table row, so
    // we just need *some* match — first() is fine.
    await expect(
      page.getByRole("heading", { name: /user created/i }),
    ).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(username).first()).toBeVisible();
    await expect(page.getByRole("button", { name: /copy/i })).toBeVisible();
  });

  test("create user rejects an 11-character password with the 12-character rule", async ({
    page,
  }) => {
    await page.goto("/admin/users");
    await page.getByRole("button", { name: /new user/i }).click();
    await expect(
      page.getByRole("heading", { name: /^new user$/i }),
    ).toBeVisible();

    let createPosts = 0;
    page.on("request", (req) => {
      if (req.method() === "POST" && req.url().includes("/api/v1/admin/users")) {
        createPosts += 1;
      }
    });

    await page.getByLabel(/^username$/i).fill(`t${Date.now().toString(36)}`);
    await page.getByLabel(/set my own password/i).check();
    await expect(page.getByLabel(/^password/i)).toBeVisible();
    await page.getByLabel(/^password/i).fill("a".repeat(11));
    await page.getByRole("button", { name: /^create$/i }).click();

    await expect(
      page.getByText(/password must be at least 12 characters/i),
    ).toBeVisible();
    expect(createPosts).toBe(0);
  });
});
