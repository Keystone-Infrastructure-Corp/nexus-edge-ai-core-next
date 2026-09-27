// @vitest-environment jsdom
//
// BUG-225: the dashboard's Engine tile must render a degraded engine as
// degraded and name its issues, with the detail the engine sent. "…" is
// only for a health query that has not answered.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { DashboardPage } from "@/pages/dashboard";

const RECORDER_STUB = {
  component: "recorder",
  code: "recorder_stub",
  detail:
    'the clip recorder is the stub, so running cameras are detected but nothing is recorded; set [runtime.clips] recorder = "gstreamer" in /etc/nexus/nexus.toml (or re-run install.sh without --keep-config) and restart nexus-engine. This needs shell or remote-shell access to the box.',
};

// A detector issue as `health_body` in the engine's api.rs builds it: it
// also carries the detector `kind`.
const DETECTOR_UNAVAILABLE = {
  component: "detector",
  code: "detector_unavailable",
  kind: "yolo",
  detail:
    "yolo: no ONNX export for requested shape 640x640 in /opt/nexus/current/share/models; available: 512x288, 1024x576, 1536x864",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Stub `fetch`: `/health` answers with `health`; everything else 404s. */
function stubEngine(health: () => Promise<Response>) {
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/health")) return health();
      return Promise.resolve(json({ error: "not stubbed" }, 404));
    }),
  );
}

class SilentEventSource {
  onopen: (() => void) | null = null;
  onmessage: (() => void) | null = null;
  onerror: (() => void) | null = null;
  close() {}
}

function renderDashboard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <DashboardPage />
    </QueryClientProvider>,
  );
}

/** The Engine KPI tile: the Card whose CardTitle reads "Engine". */
async function engineTile(): Promise<HTMLElement> {
  const title = await screen.findByText("Engine");
  const card = title.parentElement?.parentElement; // CardTitle → CardHeader → Card
  if (!card) throw new Error("Engine tile not found");
  return card;
}

beforeEach(() => {
  vi.stubGlobal("EventSource", SilentEventSource);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("dashboard Engine tile", () => {
  it("renders a degraded engine as degraded and names its issues", async () => {
    stubEngine(() =>
      Promise.resolve(
        json({
          status: "degraded",
          version: "0.1.99",
          issues: [DETECTOR_UNAVAILABLE, RECORDER_STUB],
        }),
      ),
    );
    renderDashboard();

    const tile = await engineTile();
    expect(await within(tile).findByText("DEGRADED")).toBeTruthy();
    expect(tile.textContent).toContain("detector_unavailable");
    expect(tile.textContent).toContain("recorder_stub");
    expect(within(tile).queryByText("…")).toBeNull();
    // The detail the engine sent is on the page, not only in a tooltip.
    expect(screen.getByText(RECORDER_STUB.detail)).toBeTruthy();
    expect(screen.getByText(DETECTOR_UNAVAILABLE.detail)).toBeTruthy();
  });

  it("names the detector kind, which a detector's detail need not", async () => {
    // Verbatim from a real engine booted with `kind = "ppe"`: the detail
    // does not say which kind failed, so the row must.
    const ppe = {
      component: "detector",
      code: "detector_unavailable",
      kind: "ppe",
      detail: "no detector implementation ships for this model kind",
    };
    stubEngine(() =>
      Promise.resolve(json({ status: "degraded", version: "0.1.99", issues: [ppe] })),
    );
    renderDashboard();

    expect(await screen.findByText("detector_unavailable (ppe)")).toBeTruthy();
  });

  it("names the issues when the engine withholds their detail", async () => {
    const withoutDetail = { component: "recorder", code: "recorder_stub" };
    stubEngine(() =>
      Promise.resolve(
        json({ status: "degraded", version: "0.1.99", issues: [withoutDetail] }),
      ),
    );
    renderDashboard();

    const tile = await engineTile();
    expect(await within(tile).findByText("DEGRADED")).toBeTruthy();
    expect(tile.textContent).toContain("recorder_stub");
  });

  it("renders OK with the version when the engine reports ok", async () => {
    stubEngine(() =>
      Promise.resolve(json({ status: "ok", version: "0.1.99", issues: [] })),
    );
    renderDashboard();

    const tile = await engineTile();
    expect(await within(tile).findByText("OK")).toBeTruthy();
    expect(within(tile).getByText("v0.1.99")).toBeTruthy();
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });

  it("renders ERROR when the health query fails", async () => {
    stubEngine(() => Promise.resolve(json({ error: "boom" }, 500)));
    renderDashboard();

    const tile = await engineTile();
    expect(await within(tile).findByText("ERROR")).toBeTruthy();
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });

  it("renders … while the health query has not answered", async () => {
    stubEngine(() => new Promise<Response>(() => {}));
    renderDashboard();

    const tile = await engineTile();
    expect(within(tile).getByText("…")).toBeTruthy();
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });
});
