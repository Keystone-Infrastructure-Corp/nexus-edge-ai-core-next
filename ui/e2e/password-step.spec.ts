// Setup wizard — the mandatory password step for an admin-created account.
//
// An account an admin creates with a password (or a generated one-time
// password) is stored with `force_password_reset = true`
// (POST /api/v1/admin/users — auth/users_admin.rs). The only place the UI
// acts on that flag is the first-boot wizard: while `setup_complete` is
// false, `/setup` shows a Password step when
// `GET /api/v1/setup/status` reports `session_force_password_reset`. In that
// mode the step has no Skip button, and `_app`'s `beforeLoad` bounces every
// other route back to `/setup` until the wizard is finished. Changing the
// password (POST /api/v1/auth/change-password → 204) clears the flag, and
// the wizard then continues to the Cloud step.
//
// A flagged user who signs in after setup is complete is NOT gated — there is
// no overlay or redirect outside the wizard — so the spec flips the latch
// back off through the test-injection endpoint `POST /api/v1/_test/setup_reset`
// (engine built with `--features test-injection`).
//
// Ordering: this spec clears `setup_complete`, so it restores it in afterAll
// (POST /api/v1/setup/complete as the bootstrap admin) for the specs that sort
// after it (rules.spec.ts, setup.spec.ts). setup.spec.ts also resets the
// latch for itself, so the two never depend on each other's end state.

import { expect, test } from "@playwright/test";
import type { Browser, BrowserContext, Page } from "@playwright/test";

import { loginAsAdmin, readSidecar } from "./helpers";

test.describe.configure({ mode: "serial" });

const TEMP_PASSWORD = "Temp-Passw0rd-Rotate-Me";
const NEW_PASSWORD = "Chosen-By-The-User-42";

async function resetSetupLatch(): Promise<void> {
  const { baseUrl } = readSidecar();
  const r = await fetch(`${baseUrl}/api/v1/_test/setup_reset`, {
    method: "POST",
  });
  expect(r.status, "is the engine built with --features test-injection?").toBe(
    204,
  );
}

async function apiLogin(username: string, password: string): Promise<Response> {
  const { baseUrl } = readSidecar();
  return fetch(`${baseUrl}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
}

// Sign in through the real login form in a fresh browser context.
async function signInViaForm(
  browser: Browser,
  username: string,
  password: string,
): Promise<{ context: BrowserContext; page: Page }> {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto("/login");
  await page.getByLabel(/username/i).fill(username);
  await page.getByLabel(/password/i).fill(password);
  await page.getByRole("button", { name: /sign in/i }).click();
  return { context, page };
}

test.describe("setup wizard: mandatory password step", () => {
  const username = `fp${Date.now().toString(36)}`;
  let userContext: BrowserContext | undefined;
  let userPage: Page;
  let secondContext: BrowserContext | undefined;

  test.beforeAll(async ({ browser }) => {
    // The admin creates the account through the Users page, supplying a
    // temporary password and the admin role (only an admin can Finish the
    // wizard: POST /v1/setup/complete is AdminContext-gated).
    const adminContext = await browser.newContext();
    const adminPage = await adminContext.newPage();
    await loginAsAdmin(adminPage);
    await adminPage.goto("/admin/users");
    await adminPage.getByRole("button", { name: /new user/i }).click();
    await adminPage.getByLabel(/^username$/i).fill(username);
    await adminPage.getByLabel(/^role$/i).selectOption("admin");
    await adminPage.getByLabel(/set my own password/i).check();
    await adminPage.getByLabel(/^password \(min 8 chars\)/i).fill(TEMP_PASSWORD);
    await adminPage.getByRole("button", { name: /^create$/i }).click();

    // A supplied password is not echoed back, so there is no OTP modal; the
    // row appears in the table carrying the force-reset chip.
    const row = adminPage.locator("table tr", { hasText: username });
    await expect(row).toBeVisible({ timeout: 10_000 });
    await expect(row.getByText("force-reset")).toBeVisible();
    await adminContext.close();

    await resetSetupLatch();
  });

  test.afterAll(async () => {
    await userContext?.close();
    await secondContext?.close();

    // Restore the latch for the specs that sort after this one. Wizard
    // Finish already did this on the happy path; this covers a mid-spec
    // failure.
    const { baseUrl } = readSidecar();
    const tokens = (await (
      await apiLogin("admin", readSidecar().adminOtp)
    ).json()) as { access_token: string };
    const r = await fetch(`${baseUrl}/api/v1/setup/complete`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${tokens.access_token}`,
      },
      body: "{}",
    });
    expect(r.status).toBe(204);
  });

  test("signing in with the temporary password lands in the wizard", async ({
    browser,
  }) => {
    const signedIn = await signInViaForm(browser, username, TEMP_PASSWORD);
    userContext = signedIn.context;
    userPage = signedIn.page;

    // Login navigates to /dashboard; `_app` bounces to /setup because the
    // latch is unset.
    await expect(userPage).toHaveURL(/\/setup/, { timeout: 10_000 });
    await userPage.getByRole("button", { name: /get started/i }).click();

    // Welcome → Password (the next step is Password, not Cloud).
    await expect(userPage.getByText("Change your password")).toBeVisible();
    await expect(
      userPage.getByText(/choose a permanent one before continuing/i),
    ).toBeVisible();
    // The step rail marks the mandatory step.
    await expect(userPage.getByText("Password *", { exact: true })).toBeVisible();
  });

  test("the step cannot be skipped or navigated away from", async () => {
    // Mandatory: no Skip button, only Back and Change password.
    await expect(userPage.getByRole("button", { name: /^skip$/i })).toHaveCount(0);
    await expect(userPage.getByRole("button", { name: /^back$/i })).toBeVisible();

    // Any app route bounces back into the wizard while the latch is unset.
    for (const path of ["/dashboard", "/cameras"]) {
      await userPage.goto(path);
      await expect(userPage).toHaveURL(/\/setup/, { timeout: 10_000 });
    }

    // A reload restarts the wizard at Welcome; the next step is still Password.
    await userPage.getByRole("button", { name: /get started/i }).click();
    await expect(userPage.getByText("Change your password")).toBeVisible();
  });

  test("mismatched confirmation is rejected client-side", async () => {
    await userPage.getByLabel("Current password").fill(TEMP_PASSWORD);
    await userPage.getByLabel("New password", { exact: true }).fill(NEW_PASSWORD);
    await userPage.getByLabel("Confirm new password").fill(`${NEW_PASSWORD}-x`);
    await userPage.getByRole("button", { name: /^change password$/i }).click();
    await expect(userPage.getByText("New passwords don't match.")).toBeVisible();
    await expect(userPage.getByText("Change your password")).toBeVisible();
  });

  test("setting a new password advances to the cloud step and finishes on /dashboard", async () => {
    await userPage.getByLabel("Confirm new password").fill(NEW_PASSWORD);
    const changed = userPage.waitForResponse(
      (resp) =>
        resp.url().endsWith("/api/v1/auth/change-password") &&
        resp.request().method() === "POST",
    );
    await userPage.getByRole("button", { name: /^change password$/i }).click();
    expect((await changed).status()).toBe(204);

    // Cloud step follows the password step.
    await expect(
      userPage.getByRole("button", { name: /connect to cloud/i }),
    ).toBeVisible();

    // The rest of the wizard is unchanged; Finish flips the latch.
    await userPage.getByRole("button", { name: /skip for now/i }).click();
    await userPage.getByRole("button", { name: /skip for now/i }).click();
    await userPage.getByRole("button", { name: /finish setup/i }).click();
    await expect(userPage).toHaveURL(/\/dashboard/, { timeout: 10_000 });
    await expect(userPage.locator("main")).toBeVisible();
  });

  test("the temporary password is dead and the flag is cleared", async () => {
    expect((await apiLogin(username, TEMP_PASSWORD)).ok).toBe(false);

    const res = await apiLogin(username, NEW_PASSWORD);
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      user: { force_password_reset: boolean };
    };
    expect(body.user.force_password_reset).toBe(false);
  });

  test("signing in again with the new password skips the password step", async ({
    browser,
  }) => {
    await resetSetupLatch();

    const signedIn = await signInViaForm(browser, username, NEW_PASSWORD);
    secondContext = signedIn.context;
    const page = signedIn.page;

    await expect(page).toHaveURL(/\/setup/, { timeout: 10_000 });
    await page.getByRole("button", { name: /get started/i }).click();

    // Welcome goes straight to Cloud; the rail has no Password step.
    await expect(
      page.getByRole("button", { name: /connect to cloud/i }),
    ).toBeVisible();
    await expect(page.getByText("Change your password")).toHaveCount(0);
    await expect(page.getByText("Password", { exact: true })).toHaveCount(0);
  });
});
