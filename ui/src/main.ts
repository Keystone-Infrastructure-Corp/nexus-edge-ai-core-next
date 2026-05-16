import { renderCameras } from "./ui/cameras.js";
import { renderRules } from "./ui/rules.js";
import { renderEvents } from "./ui/events.js";
import { renderViewer } from "./ui/viewer.js";
import { renderBackends } from "./ui/backends.js";
import { renderHealth } from "./ui/health.js";
import { renderStorage } from "./ui/storage.js";
import { renderAdminStorage } from "./ui/admin-storage.js";
import { renderTimeline } from "./ui/timeline.js";
import { mountAlertTicker } from "./ui/alert-ticker.js";
import { api } from "./api/client.js";
import { getToken, setToken, onAuthStatusChange } from "./lib/auth.js";

type TabRender = (root: HTMLElement) => void | Promise<void>;

interface SidebarItem {
  id: string;
  label: string;
  render: TabRender;
}

interface SidebarSection {
  label: string;
  items: SidebarItem[];
}

// M-Admin Phase 0 — sidebar shell. Three groups; the right-hand
// alert pane stays mounted only on the Operations group, where
// the live alert stream is the point. Configuration + System get
// the full main-pane width.
const SECTIONS: SidebarSection[] = [
  {
    label: "Operations",
    items: [
      { id: "viewer", label: "Viewer", render: renderViewer },
      { id: "events", label: "Events", render: renderEvents },
      { id: "timeline", label: "Timeline", render: renderTimeline },
    ],
  },
  {
    label: "Configuration",
    items: [
      { id: "cameras", label: "Cameras", render: renderCameras },
      { id: "rules", label: "Rules", render: renderRules },
    ],
  },
  {
    label: "System",
    items: [
      { id: "storage", label: "Storage", render: renderStorage },
      { id: "admin-storage", label: "Storage Admin", render: renderAdminStorage },
      { id: "backends", label: "Backends", render: renderBackends },
      { id: "health", label: "Health", render: renderHealth },
    ],
  },
];

const FLAT_TABS: SidebarItem[] = SECTIONS.flatMap((s) => s.items);
const ALERTS_VISIBLE: ReadonlySet<string> = new Set(
  SECTIONS[0]!.items.map((i) => i.id),
);

function readTab(): string {
  const hash = location.hash.replace(/^#\/?/, "");
  const found = FLAT_TABS.find((t) => t.id === hash);
  return found ? found.id : (FLAT_TABS[0]?.id ?? "viewer");
}

function activate(
  id: string,
  main: HTMLElement,
  sidebar: HTMLElement,
  app: HTMLElement,
): void {
  for (const link of Array.from(
    sidebar.querySelectorAll<HTMLElement>(".sidebar-link"),
  )) {
    link.classList.toggle("active", link.dataset["tab"] === id);
  }
  while (main.firstChild) main.removeChild(main.firstChild);
  const tab = FLAT_TABS.find((t) => t.id === id);
  if (tab) {
    void tab.render(main);
  }
  app.classList.toggle("no-alerts", !ALERTS_VISIBLE.has(id));
}

function buildSidebar(sidebar: HTMLElement): void {
  while (sidebar.firstChild) sidebar.removeChild(sidebar.firstChild);
  for (const section of SECTIONS) {
    const sectEl = document.createElement("div");
    sectEl.className = "sidebar-section";
    const head = document.createElement("div");
    head.className = "sidebar-section-label";
    head.textContent = section.label;
    sectEl.appendChild(head);
    for (const item of section.items) {
      const link = document.createElement("a");
      link.className = "sidebar-link";
      link.href = `#/${item.id}`;
      link.dataset["tab"] = item.id;
      link.textContent = item.label;
      sectEl.appendChild(link);
    }
    sidebar.appendChild(sectEl);
  }
}

function mountTokenField(): void {
  const input = document.getElementById("admin-token") as HTMLInputElement | null;
  const pill = document.getElementById("token-pill") as HTMLSpanElement | null;
  if (!input || !pill) return;

  input.value = getToken() ?? "";

  // Debounce so we don't write to localStorage on every keystroke.
  let timer: number | undefined;
  input.addEventListener("input", () => {
    if (timer != null) window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      setToken(input.value === "" ? null : input.value);
    }, 300);
  });

  onAuthStatusChange((s) => {
    pill.classList.remove("token-pill-ok", "token-pill-bad", "token-pill-unknown");
    const cls = s === "ok" ? "token-pill-ok" : s === "unauthorized" ? "token-pill-bad" : "token-pill-unknown";
    pill.classList.add(cls);
    pill.title = `Auth: ${s}`;
  });
}

async function pollHealth(dot: HTMLElement) {
  try {
    const h = await api.health();
    dot.className = h.status === "ok" ? "dot dot-ok" : "dot dot-warn";
    dot.title = `${h.status} · ${h.version}`;
  } catch {
    dot.className = "dot dot-crit";
    dot.title = "engine unreachable";
  }
}

function main() {
  const sidebarEl = document.getElementById("sidebar") as HTMLElement;
  const mainEl = document.getElementById("main") as HTMLElement;
  const appEl = document.getElementById("app") as HTMLElement;
  const dot = document.getElementById("health-dot") as HTMLElement;

  buildSidebar(sidebarEl);
  mountTokenField();

  window.addEventListener("hashchange", () =>
    activate(readTab(), mainEl, sidebarEl, appEl),
  );
  activate(readTab(), mainEl, sidebarEl, appEl);

  mountAlertTicker(document.getElementById("alert-ticker") as HTMLElement);
  void pollHealth(dot);
  setInterval(() => void pollHealth(dot), 10_000);
}

main();
