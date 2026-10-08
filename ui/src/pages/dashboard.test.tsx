// @vitest-environment jsdom
//
// BUG-225: the dashboard's Engine tile must render a degraded engine as
// degraded and name its issues, with the detail the engine sent. "…" is
// only for a health query that has not answered.

import { onlineManager, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import ENGINE from "@/lib/engineHealth.fixture.json";
import { DashboardPage } from "@/pages/dashboard";

const RECORDER_STUB = {
  component: "recorder",
  code: "recorder_stub",
  detail:
    'the clip recorder is the stub, so running cameras are detected but nothing is recorded; set [runtime.clips] recorder = "gstreamer" in /etc/nexus/nexus.toml (or re-run install.sh without --keep-config) and restart nexus-engine. This needs shell or remote-shell access to the box.',
};

// A detector issue as `health_body` in the engine's api.rs builds it: its
// detail starts with the detector kind.
const DETECTOR_UNAVAILABLE = {
  component: "detector",
  code: "detector_unavailable",
  detail:
    "yolo: no ONNX export for requested shape 640x640 in /opt/nexus/current/share/models; available: 512x288, 1024x576, 1536x864",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/**
 * Stub `fetch`: `/health` answers with `health`, a path in `routes` with its
 * answer (the query string is ignored); everything else 404s.
 */
function stubEngine(
  health: () => Promise<Response>,
  routes: Record<string, () => Promise<Response>> = {},
) {
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL) => {
      const path = new URL(String(input), "http://engine").pathname;
      if (path === "/api/v1/health") return health();
      const route = routes[path];
      if (route) return route();
      return Promise.resolve(json({ error: "not stubbed" }, 404));
    }),
  );
}

const pending = () => new Promise<Response>(() => {});
const failing = () => Promise.resolve(json({ error: "boom" }, 500));
const healthy = () =>
  Promise.resolve(json({ status: "ok", version: "0.1.99", issues: [] }));

/** The fields of `GET /api/v1/system/metrics` the dashboard reads. */
function metrics(disks: { total_bytes: number; available_bytes: number }[]) {
  return {
    uptime_secs: 60,
    cpu: { count: 4, usage_pct: 37.4, per_core_pct: [], frequency_mhz: 2000 },
    memory: { total_bytes: 8_000, used_bytes: 2_000, available_bytes: 6_000 },
    gpu: null,
    npu: null,
    disks: disks.map((d, i) => ({ name: `disk${i}`, mount_point: `/m${i}`, ...d })),
    captured_at: new Date().toISOString(),
  };
}

class SilentEventSource {
  onopen: (() => void) | null = null;
  onmessage: (() => void) | null = null;
  onerror: (() => void) | null = null;
  close() {}
}

/** An alert stream that connects and then stays quiet. */
class OpenEventSource extends SilentEventSource {
  constructor() {
    super();
    queueMicrotask(() => this.onopen?.());
  }
}

function renderDashboard(): QueryClient {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <DashboardPage />
    </QueryClientProvider>,
  );
  return client;
}

/**
 * The KPI or system card whose CardTitle reads `label` and that shows a
 * reading. The Cameras KPI shares its title with the Cameras card, which has
 * no reading.
 */
async function tile(label: string): Promise<HTMLElement> {
  for (const title of await screen.findAllByText(label)) {
    const card = title.parentElement?.parentElement; // CardTitle → CardHeader → Card
    if (card?.querySelector(".text-2xl")) return card;
  }
  throw new Error(`${label} tile not found`);
}

/** The Engine KPI tile. */
const engineTile = () => tile("Engine");

/** The reading a tile shows. */
async function reading(label: string): Promise<string> {
  return (await tile(label)).querySelector(".text-2xl")?.textContent ?? "";
}

/** The classes of a tile's reading, which carry its accent colour. */
function accentOf(card: HTMLElement): string {
  return card.querySelector(".text-2xl")?.className ?? "";
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
    expect((await within(tile).findByText("DEGRADED")).className).toContain("text-warning");
    expect(tile.textContent).toContain("detector_unavailable");
    expect(tile.textContent).toContain("recorder_stub");
    expect(within(tile).queryByText("…")).toBeNull();
    // The detail the engine sent is on the page, not only in a tooltip.
    expect(screen.getByText(RECORDER_STUB.detail)).toBeTruthy();
    expect(screen.getByText(DETECTOR_UNAVAILABLE.detail)).toBeTruthy();
  });

  it("names the detector kind, which the engine puts in the detail", async () => {
    const ppe = ENGINE.signed_in.issues.find((i) => i.component === "detector");
    stubEngine(() =>
      Promise.resolve(json({ status: "degraded", version: "0.1.99", issues: [ppe] })),
    );
    renderDashboard();

    expect(
      await screen.findByText("ppe: no detector implementation ships for this model kind"),
    ).toBeTruthy();
  });

  it("lists every issue the engine answers with, with the detail only a signed-in caller gets", async () => {
    for (const answer of [ENGINE.signed_in, ENGINE.anonymous]) {
      stubEngine(() => Promise.resolve(json({ ...answer, version: "0.1.99" })));
      renderDashboard();

      const tile = await engineTile();
      expect(await within(tile).findByText("DEGRADED")).toBeTruthy();
      for (const issue of ENGINE.signed_in.issues) {
        const row = screen.getByText(issue.code).closest("li");
        expect(row?.textContent).toBe(
          `${issue.component}${issue.code}${answer === ENGINE.signed_in ? issue.detail : ""}`,
        );
      }
      cleanup();
    }
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
    expect((await within(tile).findByText("OK")).className).toContain("text-success");
    expect(within(tile).getByText("v0.1.99")).toBeTruthy();
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });

  it("renders ERROR when the health query fails", async () => {
    stubEngine(() => Promise.resolve(json({ error: "boom" }, 500)));
    renderDashboard();

    const tile = await engineTile();
    expect((await within(tile).findByText("ERROR")).className).toContain("text-destructive");
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });

  it("renders ERROR without the last answer's version when a later request fails", async () => {
    let answer = () =>
      Promise.resolve(json({ status: "ok", version: "0.1.99", issues: [] }));
    stubEngine(() => answer());
    const client = renderDashboard();
    const tile = await engineTile();
    await within(tile).findByText("OK");

    answer = () => Promise.resolve(json({ error: "boom" }, 500));
    void client.refetchQueries({ queryKey: ["health"] });

    expect(await within(tile).findByText("ERROR")).toBeTruthy();
    // The stale answer is still in the cache; the tile did not render it.
    expect(client.getQueryData(["health"])).toBeDefined();
    expect(tile.textContent).not.toContain("0.1.99");
  });

  it("renders … while the health query has not answered", async () => {
    stubEngine(() => new Promise<Response>(() => {}));
    renderDashboard();

    const tile = await engineTile();
    expect(within(tile).getByText("…")).toBeTruthy();
    expect(screen.queryByText(/degraded/i)).toBeNull();
  });
});

// The rest of the dashboard states readings too, and must not state one it
// has not read: a query that has not answered shows "…", one that failed
// shows "—" and says it failed, and neither shows a default such as 0.
describe("dashboard readings", () => {
  it("does not count cameras before the camera list answers", async () => {
    stubEngine(healthy, { "/api/v1/cameras": pending });
    renderDashboard();

    expect(await reading("Cameras")).toBe("…");
    expect(screen.queryByText("0 configured")).toBeNull();
    expect(screen.queryByText("No cameras configured")).toBeNull();
  });

  it("says the camera list failed instead of that no cameras are configured", async () => {
    stubEngine(healthy, { "/api/v1/cameras": failing });
    renderDashboard();

    await waitFor(async () => expect(await reading("Cameras")).toBe("—"));
    expect(within(await tile("Cameras")).getByText("Failed to load cameras")).toBeTruthy();
    expect(screen.queryByText("0 configured")).toBeNull();
    expect(screen.queryByText("No cameras configured")).toBeNull();
    expect(screen.queryByText(/Add a camera/)).toBeNull();
    expect(screen.getAllByText("Failed to load cameras")).toHaveLength(2);
  });

  it("counts the cameras it read, and says none are configured only then", async () => {
    stubEngine(healthy, { "/api/v1/cameras": () => Promise.resolve(json([])) });
    renderDashboard();

    expect(await screen.findByText("No cameras configured")).toBeTruthy();
    expect(await reading("Cameras")).toBe("0");
    expect(screen.getByText("0 configured")).toBeTruthy();
  });

  it("does not count alerts before the event list answers", async () => {
    stubEngine(healthy, { "/api/v1/events": pending });
    renderDashboard();

    expect(await reading("Alerts (last hour)")).toBe("…");
    expect(accentOf(await tile("Alerts (last hour)"))).not.toContain("text-warning");
  });

  it("says the event list failed instead of counting no alerts", async () => {
    stubEngine(healthy, { "/api/v1/events": failing });
    renderDashboard();

    await waitFor(async () => expect(await reading("Alerts (last hour)")).toBe("—"));
    expect(within(await tile("Alerts (last hour)")).getByText("Failed to load events")).toBeTruthy();
  });

  it("counts the alerts it read in the last hour", async () => {
    const at = (msAgo: number) => new Date(Date.now() - msAgo).toISOString();
    stubEngine(healthy, {
      "/api/v1/events": () =>
        Promise.resolve(json([{ captured_at: at(60_000) }, { captured_at: at(7_200_000) }])),
    });
    renderDashboard();

    await waitFor(async () => expect(await reading("Alerts (last hour)")).toBe("1"));
    expect(accentOf(await tile("Alerts (last hour)"))).toContain("text-warning");
  });

  it("does not cap the hour's alerts at the events it asked for", async () => {
    // The page asks for the newest 100 events; when every one is inside the
    // hour, the hour may hold more than were read.
    const now = new Date().toISOString();
    stubEngine(healthy, {
      "/api/v1/events": () =>
        Promise.resolve(json(Array.from({ length: 100 }, () => ({ captured_at: now })))),
    });
    renderDashboard();

    await waitFor(async () => expect(await reading("Alerts (last hour)")).toBe("100+"));
  });

  it("counts the hour's alerts exactly when the events it read reach past the hour", async () => {
    const at = (msAgo: number) => new Date(Date.now() - msAgo).toISOString();
    const events = [
      ...Array.from({ length: 99 }, () => ({ captured_at: at(60_000) })),
      { captured_at: at(7_200_000) },
    ];
    stubEngine(healthy, { "/api/v1/events": () => Promise.resolve(json(events)) });
    renderDashboard();

    await waitFor(async () => expect(await reading("Alerts (last hour)")).toBe("99"));
  });

  it("does not report CPU, memory or disk use before the metrics answer", async () => {
    stubEngine(healthy, { "/api/v1/system/metrics": pending });
    renderDashboard();

    expect(await reading("Disk used (worst)")).toBe("…");
    expect(await reading("CPU")).toBe("…");
    expect(await reading("Memory")).toBe("…");
  });

  it("says the metrics failed instead of reporting 0% use", async () => {
    stubEngine(healthy, { "/api/v1/system/metrics": failing });
    renderDashboard();

    await waitFor(async () => expect(await reading("Disk used (worst)")).toBe("—"));
    expect(within(await tile("Disk used (worst)")).getByText("Failed to load metrics")).toBeTruthy();
    expect(await reading("CPU")).toBe("—");
    expect(await reading("Memory")).toBe("—");
  });

  it("does not report 0% disk use for an engine that measured no disk", async () => {
    stubEngine(healthy, {
      "/api/v1/system/metrics": () => Promise.resolve(json(metrics([]))),
    });
    renderDashboard();

    await screen.findByText("0 disks");
    expect(await reading("Disk used (worst)")).toBe("—");
  });

  it("reports the CPU, memory and worst disk use it read", async () => {
    stubEngine(healthy, {
      "/api/v1/system/metrics": () =>
        Promise.resolve(
          json(
            metrics([
              { total_bytes: 100, available_bytes: 60 },
              { total_bytes: 100, available_bytes: 20 },
              { total_bytes: 0, available_bytes: 0 },
            ]),
          ),
        ),
    });
    renderDashboard();

    await screen.findByText("3 disks");
    expect(await reading("Disk used (worst)")).toBe("80%");
    expect(accentOf(await tile("Disk used (worst)"))).toContain("text-warning");
    expect(await reading("CPU")).toBe("37%");
    expect(await reading("Memory")).toBe("25%");
  });

  it("does not call a camera stalled before its frame metadata answers", async () => {
    stubEngine(healthy, {
      "/api/v1/cameras": () =>
        Promise.resolve(json([{ id: 7, name: "Front door", url: "rtsp://cam" }])),
      "/api/v1/cameras/7/frames/latest.json": pending,
    });
    renderDashboard();

    expect(await screen.findByText("Front door")).toBeTruthy();
    expect(screen.queryByText("STALLED")).toBeNull();
  });

  it("calls a camera stalled when the engine has no frame for it", async () => {
    stubEngine(healthy, {
      "/api/v1/cameras": () =>
        Promise.resolve(json([{ id: 7, name: "Front door", url: "rtsp://cam" }])),
    });
    renderDashboard();

    expect(await screen.findByText("STALLED")).toBeTruthy();
  });

  it("does not say no alerts fired while the alert stream is not connected", async () => {
    stubEngine(healthy);
    renderDashboard();

    // The status badge proves the card rendered with the stream still connecting.
    expect(await screen.findByText("connecting")).toBeTruthy();
    expect(screen.queryByText("Quiet on the wire")).toBeNull();
    expect(screen.queryByText(/No alerts have/)).toBeNull();
    expect(screen.getByText("Not connected")).toBeTruthy();
  });

  it("says the wire is quiet once the alert stream is connected", async () => {
    vi.stubGlobal("EventSource", OpenEventSource);
    stubEngine(healthy);
    renderDashboard();

    expect(await screen.findByText("Quiet on the wire")).toBeTruthy();
    expect(screen.getByText("No alerts have arrived since you opened this page.")).toBeTruthy();
  });

  it("does not call the backends unavailable before their request has been sent", async () => {
    // Offline, a query is paused: it has not answered, but it is not loading.
    onlineManager.setOnline(false);
    try {
      stubEngine(healthy, {
        "/api/v1/backends": () => Promise.resolve(json({ mode: "in_process", slots: [] })),
      });
      renderDashboard();

      expect(await screen.findByText("Inference")).toBeTruthy();
      expect(screen.queryByText("Backends unavailable")).toBeNull();
    } finally {
      onlineManager.setOnline(true);
    }
  });

  it("says the backends are unavailable once their request fails", async () => {
    stubEngine(healthy, { "/api/v1/backends": failing });
    renderDashboard();

    expect(await screen.findByText("Backends unavailable")).toBeTruthy();
  });
});
