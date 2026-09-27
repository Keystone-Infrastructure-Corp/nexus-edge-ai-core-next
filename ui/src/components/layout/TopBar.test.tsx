// @vitest-environment jsdom
//
// BUG-225: the engine answers `GET /api/v1/health` with `status: "degraded"`
// and an `issues[]` list when it runs with a known loss of function (a stub
// clip recorder behind enabled cameras, a detector that failed to build). The
// pill must say so, and must still say "starting…" only while the query has
// not answered — the verdict comes from the query's state, never from a
// defaulted value.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { TopBar } from "@/components/layout/TopBar";
import { AuthProvider } from "@/lib/auth";

// The recorder issue exactly as `recorder_issue_for` in the engine's
// cloud_tunnel.rs builds it.
const RECORDER_STUB = {
  component: "recorder",
  code: "recorder_stub",
  detail:
    'the clip recorder is the stub, so running cameras are detected but nothing is recorded; set [runtime.clips] recorder = "gstreamer" in /etc/nexus/nexus.toml (or re-run install.sh without --keep-config) and restart nexus-engine. This needs shell or remote-shell access to the box.',
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Stub `fetch`: `/health` answers with `health`, `/cloud/status` with `cloud` (default: not enrolled). */
function stubEngine(
  health: () => Promise<Response>,
  cloud: () => Promise<Response> = () =>
    Promise.resolve(json({ enrolled: false, connected: false })),
) {
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/health")) return health();
      if (url.endsWith("/api/v1/cloud/status")) return cloud();
      return Promise.resolve(json({ error: "not stubbed" }, 404));
    }),
  );
}

function renderTopBar(): QueryClient {
  const client = new QueryClient();
  render(
    <QueryClientProvider client={client}>
      <AuthProvider>
        <TopBar />
      </AuthProvider>
    </QueryClientProvider>,
  );
  return client;
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("TopBar engine-health pill", () => {
  it("names a degraded engine's issues instead of saying it is starting", async () => {
    stubEngine(() =>
      Promise.resolve(
        json({ status: "degraded", version: "0.1.99", issues: [RECORDER_STUB] }),
      ),
    );
    renderTopBar();

    const pill = await screen.findByText(/degraded/);
    expect(pill.textContent).toContain("recorder_stub");
    expect(pill.getAttribute("title")).toContain(RECORDER_STUB.detail);
    expect(screen.queryByText("starting…")).toBeNull();
  });

  it("names the issue when the engine withholds its detail", async () => {
    const withoutDetail = { component: "recorder", code: "recorder_stub" };
    stubEngine(() =>
      Promise.resolve(
        json({ status: "degraded", version: "0.1.99", issues: [withoutDetail] }),
      ),
    );
    renderTopBar();

    const pill = await screen.findByText(/degraded/);
    expect(pill.textContent).toContain("recorder_stub");
    expect(pill.getAttribute("title")).toBe("recorder_stub");
  });

  it("names the detector kind in the hover text", async () => {
    // Verbatim from a real engine booted with `kind = "ppe"`: the detail
    // does not say which kind failed.
    const ppe = {
      component: "detector",
      code: "detector_unavailable",
      kind: "ppe",
      detail: "no detector implementation ships for this model kind",
    };
    stubEngine(() =>
      Promise.resolve(json({ status: "degraded", version: "0.1.99", issues: [ppe] })),
    );
    renderTopBar();

    const pill = await screen.findByText(/degraded/);
    expect(pill.getAttribute("title")).toBe(
      "detector_unavailable (ppe): no detector implementation ships for this model kind",
    );
  });

  it("says online with the version when the engine reports ok", async () => {
    stubEngine(() =>
      Promise.resolve(json({ status: "ok", version: "0.1.99", issues: [] })),
    );
    renderTopBar();

    expect(await screen.findByText("online • 0.1.99")).toBeTruthy();
  });

  it("says starting while the health query has not answered", async () => {
    stubEngine(() => new Promise<Response>(() => {}));
    renderTopBar();

    expect(await screen.findByText("starting…")).toBeTruthy();
    expect(screen.queryByText(/degraded|online/)).toBeNull();
  });

  it("says unreachable, not the last answer, when a later request fails", async () => {
    let answer = () =>
      Promise.resolve(
        json({ status: "degraded", version: "0.1.99", issues: [RECORDER_STUB] }),
      );
    stubEngine(() => answer());
    const client = renderTopBar();
    await screen.findByText(/degraded/);

    answer = () => Promise.resolve(json({ error: "boom" }, 500));
    void client.refetchQueries({ queryKey: ["health"] });

    expect(
      await screen.findByText("engine unreachable", undefined, { timeout: 4_000 }),
    ).toBeTruthy();
    // The stale answer is still in the cache; the pill did not render it.
    expect(client.getQueryData(["health"])).toBeDefined();
    expect(screen.queryByText(/degraded/)).toBeNull();
  });

  it("says unreachable when the health query fails", async () => {
    stubEngine(() => Promise.resolve(json({ error: "boom" }, 500)));
    renderTopBar();

    expect(
      await screen.findByText("engine unreachable", undefined, { timeout: 4_000 }),
    ).toBeTruthy();
  });
});

describe("TopBar cloud pill", () => {
  it("does not call the core not enrolled before the cloud status has answered", async () => {
    stubEngine(
      () => Promise.resolve(json({ status: "ok", version: "0.1.99", issues: [] })),
      () => new Promise<Response>(() => {}),
    );
    renderTopBar();

    // The health pill answering proves the render settled with the cloud
    // query still pending.
    expect(await screen.findByText("online • 0.1.99")).toBeTruthy();
    expect(screen.getByText("cloud: …")).toBeTruthy();
    expect(screen.queryByText("cloud: not enrolled")).toBeNull();
  });
});
