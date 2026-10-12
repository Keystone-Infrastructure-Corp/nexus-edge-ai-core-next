// First-boot setup wizard spec (M-Install Checkpoint 3c).
//
// globalSetup marks `engine_runtime_settings.setup_complete = "true"` so
// every OTHER spec lands on /dashboard without seeing the wizard. This
// spec flips the latch back off via the test-injection endpoint
// `POST /api/v1/_test/setup_reset`, walks the wizard end-to-end, and
// re-checks that the latch is flipped back on. That endpoint exists only
// when the engine is built with `--features test-injection`.
//
// globalSetup claims the admin account through the first-run form, so the
// operator has already chosen their password and the wizard has no
// password step (it appears only for a session with
// `force_password_reset`).
//
// IMPORTANT — this spec clears `setup_complete`, which would break sibling
// specs if they ran AFTER it. The wizard sets it back via
// `POST /v1/setup/complete` on Finish.
//
// File ordering is the safety net: Playwright runs specs alphabetically
// with `workers: 1`, and `setup.spec.ts` sorts after every other file
// in `ui/e2e/`. If you ever add a spec whose name sorts AFTER `setup`,
// either rename it or move it to a separate Playwright project.

import { expect, test } from "@playwright/test";
import type { BrowserContext, Page } from "@playwright/test";

import { loginAsAdmin, readSidecar } from "./helpers";

// One-shot, serial. We reuse a single BrowserContext across all tests in
// this file because each step depends on the wizard state the previous
// step established. Playwright DOES NOT share contexts across `test()`
// calls by default, even in serial mode — hoist into beforeAll. See the
// user-memory note "Playwright trap" for the trap this avoids.
test.describe.configure({ mode: "serial" });

test.describe("first-boot setup wizard", () => {
  let context: BrowserContext;
  let page: Page;

  test.beforeAll(async ({ browser }) => {
    context = await browser.newContext();
    page = await context.newPage();
    await loginAsAdmin(page);

    // Reset the setup-complete latch via the test-injection endpoint so
    // the next /v1/setup/status call reports `setup_complete: false`.
    const { baseUrl } = readSidecar();
    const r = await fetch(`${baseUrl}/api/v1/_test/setup_reset`, {
      method: "POST",
    });
    expect(r.status, "is the engine built with --features test-injection?").toBe(204);
  });

  test.afterAll(async () => {
    await context?.close();
  });

  test("router redirects to /setup when latch is unset", async () => {
    await page.goto("/dashboard");
    await expect(page).toHaveURL(/\/setup/, { timeout: 10_000 });
    // The Welcome step is identified by its unique CTA — the StepRail
    // breadcrumbs use the same step labels ("Welcome", "Password", …)
    // so plain text matching is ambiguous.
    await expect(
      page.getByRole("button", { name: /get started/i }),
    ).toBeVisible();
  });

  test("welcome step renders hostname + version, advances to cloud", async () => {
    // Already on /setup from the previous test. Hostname and version
    // are rendered into a definition list — assert that the labels are
    // present rather than pinning specific values (workdir-dependent).
    await expect(page.getByText(/hostname/i)).toBeVisible();
    await expect(page.getByText(/engine version/i)).toBeVisible();

    await page.getByRole("button", { name: /get started/i }).click();
    // No password step: the operator chose their password at first run.
    // The cloud step is identified by its "Connect to cloud" button.
    await expect(
      page.getByRole("button", { name: /connect to cloud/i }),
    ).toBeVisible();
  });

  test("cloud step skips forward to cameras", async () => {
    await page.getByRole("button", { name: /skip for now/i }).click();
    // The cameras step uniquely renders an "Add cameras" CTA button.
    await expect(
      page.getByRole("button", { name: /add cameras/i }),
    ).toBeVisible();
  });

  test("cameras step renders count and skipping it skips rules too", async () => {
    // Fresh DB → no cameras configured. With no camera there is nothing
    // for a rule to fire on, so Skip goes straight to Finish.
    await expect(page.getByText(/cameras configured/i)).toBeVisible();
    await page.getByRole("button", { name: /skip for now/i }).click();
    await expect(
      page.getByRole("button", { name: /finish setup/i }),
    ).toBeVisible();
  });

  test("rules step is one Back from Finish and skips forward", async () => {
    await page.getByRole("button", { name: /^back$/i }).click();
    await expect(page.getByText(/rules configured/i)).toBeVisible();
    await expect(
      page.getByRole("button", { name: /add rules/i }),
    ).toBeVisible();
    await page.getByRole("button", { name: /skip for now/i }).click();
    await expect(
      page.getByRole("button", { name: /finish setup/i }),
    ).toBeVisible();
  });

  test("finish flips the latch and lands on /dashboard", async () => {
    const finishResponse = page.waitForResponse(
      (resp) =>
        resp.url().endsWith("/api/v1/setup/complete") &&
        resp.request().method() === "POST",
    );
    await page.getByRole("button", { name: /finish setup/i }).click();
    const resp = await finishResponse;
    expect(resp.status()).toBe(204);

    await expect(page).toHaveURL(/\/dashboard/, { timeout: 10_000 });
    await expect(page.locator("main")).toBeVisible();
  });

  test("re-visiting /setup after completion redirects back to /dashboard", async () => {
    await page.goto("/setup");
    await expect(page).toHaveURL(/\/dashboard/, { timeout: 10_000 });
  });
});
