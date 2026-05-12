import { renderCameras } from "./ui/cameras.js";
import { renderRules } from "./ui/rules.js";
import { renderEvents } from "./ui/events.js";
import { renderViewer } from "./ui/viewer.js";
import { renderBackends } from "./ui/backends.js";
import { renderHealth } from "./ui/health.js";
import { mountAlertTicker } from "./ui/alert-ticker.js";
import { api } from "./api/client.js";

type TabRender = (root: HTMLElement) => void | Promise<void>;

const TABS: { id: string; label: string; render: TabRender }[] = [
  { id: "viewer", label: "Viewer", render: renderViewer },
  { id: "cameras", label: "Cameras", render: renderCameras },
  { id: "rules", label: "Rules", render: renderRules },
  { id: "events", label: "Events", render: renderEvents },
  { id: "backends", label: "Backends", render: renderBackends },
  { id: "health", label: "Health", render: renderHealth },
];

function readTab(): string {
  const hash = location.hash.replace(/^#\/?/, "");
  const found = TABS.find((t) => t.id === hash);
  return found ? found.id : (TABS[0]?.id ?? "viewer");
}

function activate(id: string, main: HTMLElement, tabs: HTMLElement): void {
  for (const btn of Array.from(tabs.querySelectorAll("button"))) {
    btn.classList.toggle("active", btn.dataset.tab === id);
  }
  while (main.firstChild) main.removeChild(main.firstChild);
  const tab = TABS.find((t) => t.id === id);
  if (tab) {
    void tab.render(main);
  }
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
  const tabsEl = document.getElementById("tabs") as HTMLElement;
  const mainEl = document.getElementById("main") as HTMLElement;
  const dot = document.getElementById("health-dot") as HTMLElement;

  for (const t of TABS) {
    const btn = document.createElement("button");
    btn.dataset.tab = t.id;
    btn.textContent = t.label;
    btn.addEventListener("click", () => {
      location.hash = `#/${t.id}`;
    });
    tabsEl.appendChild(btn);
  }

  window.addEventListener("hashchange", () => activate(readTab(), mainEl, tabsEl));
  activate(readTab(), mainEl, tabsEl);

  mountAlertTicker(document.getElementById("alert-ticker") as HTMLElement);
  void pollHealth(dot);
  setInterval(() => void pollHealth(dot), 10_000);
}

main();
