// Delivery → Alert sinks card. Exercises the full create → read →
// edit → test → delete round-trip for the generic SMTP `email` sink
// against the REAL engine, so it covers the pieces a component test
// can't: the `deny_unknown_fields` wire shape, the redaction-sentinel
// secret contract, and the async SinkRegistry rebuild that makes a
// freshly-created sink testable without a restart.
//
// SMTP host is a closed loopback port so the Test button's real
// delivery attempt fails fast (connection refused) instead of waiting
// out the 15s timeout — the assertion is on the failure being
// reported, not on mail actually arriving.

import { expect, test } from "@playwright/test";

import { loginAsAdmin } from "./helpers";

const SINK_NAME = "e2e-site-ops";

test.describe.configure({ mode: "serial" });

test.describe("delivery — alert sinks", () => {
  test.beforeEach(async ({ page }) => {
    await loginAsAdmin(page);
    await page.goto("/delivery");
    await expect(
      page.getByRole("heading", { name: /^alert sinks$/i }),
    ).toBeVisible({ timeout: 10_000 });
  });

  test("creates an email sink and lists it with a recipient summary", async ({
    page,
  }) => {
    await page.getByRole("button", { name: /add email sink/i }).click();

    await page.getByLabel("Name", { exact: true }).fill(SINK_NAME);
    // Mixed separators — the parser accepts commas, semicolons, newlines.
    await page
      .getByLabel("To", { exact: true })
      .fill("ops@example.com; security@example.com ");
    await page.getByLabel(/^cc/i).fill("records@example.com");
    await page.getByLabel(/^from address$/i).fill("nexus@example.com");
    await page.getByLabel(/^host$/i).fill("127.0.0.1");
    // Closed loopback port → the later Test fails immediately.
    await page.getByLabel(/^port$/i).fill("1");
    await page.getByLabel(/relay requires authentication/i).check();
    await page.getByLabel("Username", { exact: true }).fill("nexus@example.com");
    await page.getByLabel("Password", { exact: true }).fill("app-password");

    // The Delivery page has its own Save button — scope to the sheet.
    await page
      .getByRole("dialog")
      .getByRole("button", { name: /^save$/i })
      .click();

    const row = page.getByRole("listitem").filter({ hasText: SINK_NAME });
    await expect(row).toBeVisible({ timeout: 10_000 });
    await expect(row).toContainText("Email");
    await expect(row).toContainText(
      "127.0.0.1 \u00b7 ops@example.com +2 more",
    );
  });

  test("keeps the stored relay password when the field is left blank", async ({
    page,
  }) => {
    const row = page.getByRole("listitem").filter({ hasText: SINK_NAME });
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.getByRole("button", { name: /^edit$/i }).click();

    // Name is the sink id — immutable once created.
    await expect(page.getByLabel("Name", { exact: true })).toBeDisabled();
    // The engine redacted the stored secret, so the editor offers to
    // keep it rather than echoing anything back.
    await expect(page.getByLabel("Password", { exact: true })).toHaveAttribute(
      "placeholder",
      /leave blank to keep/i,
    );

    await page.getByLabel(/^subject prefix/i).fill("[E2E]");
    await page
      .getByRole("dialog")
      .getByRole("button", { name: /^save$/i })
      .click();

    // A successful save closes the sheet. If the sentinel had been
    // rejected the engine would 400 and the error banner would show.
    await expect(
      page.getByRole("heading", { name: new RegExp(`^edit ${SINK_NAME}$`, "i") }),
    ).toBeHidden({ timeout: 10_000 });
    await expect(row).toBeVisible();
  });

  test("reports a failed delivery from the Test button", async ({ page }) => {
    const row = page.getByRole("listitem").filter({ hasText: SINK_NAME });
    await expect(row).toBeVisible({ timeout: 10_000 });

    // The engine rebuilds its live registry off a bus signal, so the
    // sink becomes testable a beat after the PUT returns.
    const testButton = row.getByRole("button", { name: /^test/i });
    await expect(testButton).toBeEnabled({ timeout: 10_000 });
    await testButton.click();

    await expect(row).toContainText(/test failed/i, { timeout: 15_000 });
  });

  test("removes the sink", async ({ page }) => {
    const row = page.getByRole("listitem").filter({ hasText: SINK_NAME });
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.getByRole("button", { name: /remove this sink/i }).click();
    await row.getByRole("button", { name: /^confirm$/i }).click();

    await expect(
      page.getByRole("listitem").filter({ hasText: SINK_NAME }),
    ).toHaveCount(0, { timeout: 10_000 });
  });
});
