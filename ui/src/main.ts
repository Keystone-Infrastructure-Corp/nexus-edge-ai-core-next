import { renderCameras } from "./ui/cameras.js";
import { renderRules } from "./ui/rules.js";
import { renderEvents } from "./ui/events.js";
import { renderViewer } from "./ui/viewer.js";
import { renderBackends } from "./ui/backends.js";
import { renderHealth } from "./ui/health.js";
import { renderStorage } from "./ui/storage.js";
import { renderAdminStorage } from "./ui/admin-storage.js";
import { renderAdminDelivery } from "./ui/admin-delivery.js";
import { renderAdminUsers } from "./ui/admin-users.js";
import { renderAdminAudit } from "./ui/admin-audit.js";
import { renderTimeline } from "./ui/timeline.js";
import { mountAlertTicker } from "./ui/alert-ticker.js";
import { api } from "./api/client.js";
import {
  getSession,
  getToken,
  hydrateFromOidcHandoff,
  loadAuthInfo,
  logout,
  onAuthStatusChange,
  onAuthInfoChange,
  onSessionChange,
  setToken,
} from "./lib/auth.js";
import { mountForcePasswordResetModal } from "./ui/change-password-modal.js";
import {
  hideLoginOverlay,
  hasUsableSession,
  showLoginOverlay,
} from "./ui/login.js";

type TabRender = (root: HTMLElement) => void | Promise<void>;

interface SidebarItem {
  id: string;
  label: string;
  render: TabRender;
  /// When true, the item is hidden from the sidebar unless
  /// the current session principal has `role = admin`. The
  /// route remains registered (so a direct hash-link still
  /// resolves), but the API calls will 403 for non-admins.
  requireAdmin?: boolean;
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
      { id: "admin-delivery", label: "Alert Delivery", render: renderAdminDelivery },
      { id: "admin-users", label: "Users", render: renderAdminUsers, requireAdmin: true },
      { id: "admin-audit", label: "Audit Log", render: renderAdminAudit, requireAdmin: true },
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
  const isAdmin = getSession()?.user.role === "admin";
  for (const section of SECTIONS) {
    const visibleItems = section.items.filter(
      (item) => !item.requireAdmin || isAdmin,
    );
    if (visibleItems.length === 0) continue;
    const sectEl = document.createElement("div");
    sectEl.className = "sidebar-section";
    const head = document.createElement("div");
    head.className = "sidebar-section-label";
    head.textContent = section.label;
    sectEl.appendChild(head);
    for (const item of visibleItems) {
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

/// M6 Phase 2 Step 2.9 — mode-aware topbar.
///
/// * `none` / `dev_token`: legacy paste-field stays visible
///   (loopback mode + dev workflows). Status pill driven by
///   the legacy bearer.
/// * `local` / `oidc` / `hybrid`: hide the paste-field, show
///   the current user + role + a Sign-out link instead. Status
///   pill is driven by the session.
function mountTopbarAuth(): void {
  const inputEl = document.getElementById("admin-token") as HTMLInputElement | null;
  const pillEl = document.getElementById("token-pill") as HTMLSpanElement | null;
  const authBoxEl = document.getElementById("topbar-auth") as HTMLElement | null;
  if (!inputEl || !pillEl || !authBoxEl) return;
  // Re-alias to non-null bindings so nested closures don't
  // need `!` everywhere — TS can't propagate the early-return
  // narrowing across function boundaries.
  const input: HTMLInputElement = inputEl;
  const pill: HTMLSpanElement = pillEl;
  const authBox: HTMLElement = authBoxEl;

  // Legacy paste-field — preserved exactly as before so dev
  // workflows keep working.
  input.value = getToken() ?? "";
  let timer: number | undefined;
  input.addEventListener("input", () => {
    if (timer != null) window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      setToken(input.value === "" ? null : input.value);
    }, 300);
  });

  // User-info chip (rendered next to the pill when a session
  // exists). Lazily created so we don't pollute the DOM when
  // we're in dev-token mode.
  let userChip: HTMLElement | null = null;
  let signOutBtn: HTMLElement | null = null;

  function renderSessionWidget(): void {
    const session = getSession();
    if (session) {
      input.style.display = "none";
      if (!userChip) {
        userChip = document.createElement("span");
        userChip.id = "topbar-user";
        userChip.className = "topbar-user";
        authBox.insertBefore(userChip, pill);
      }
      userChip.textContent = `${session.user.username} · ${session.user.role}`;
      userChip.title = `Signed in as ${session.user.username} (${session.user.role})`;
      if (!signOutBtn) {
        signOutBtn = document.createElement("button");
        signOutBtn.id = "topbar-signout";
        signOutBtn.className = "topbar-signout";
        signOutBtn.textContent = "Sign out";
        signOutBtn.addEventListener("click", () => {
          void logout();
        });
        authBox.insertBefore(signOutBtn, pill);
      }
    } else {
      if (userChip) {
        authBox.removeChild(userChip);
        userChip = null;
      }
      if (signOutBtn) {
        authBox.removeChild(signOutBtn);
        signOutBtn = null;
      }
      // Re-show the paste-field only if the mode allows it
      // (mode-aware visibility is applied separately below).
    }
  }

  function applyModeVisibility(): void {
    const info = getCachedAuthInfoFromAttribute();
    const hidesDevField =
      info?.mode === "local" || info?.mode === "oidc" || info?.mode === "hybrid";
    if (hidesDevField && !getSession()) {
      // Login overlay is up — don't bother showing the
      // paste-field; the overlay covers the whole viewport
      // anyway.
      input.style.display = "none";
    } else if (!hidesDevField) {
      input.style.display = "";
    }
  }

  onAuthStatusChange((s) => {
    pill.classList.remove("token-pill-ok", "token-pill-bad", "token-pill-unknown");
    const cls =
      s === "ok"
        ? "token-pill-ok"
        : s === "unauthorized"
          ? "token-pill-bad"
          : "token-pill-unknown";
    pill.classList.add(cls);
    pill.title = `Auth: ${s}`;
  });

  onSessionChange(() => {
    renderSessionWidget();
    applyModeVisibility();
  });

  onAuthInfoChange((info) => {
    if (info) {
      authBox.dataset["mode"] = info.mode;
    } else {
      delete authBox.dataset["mode"];
    }
    applyModeVisibility();
  });

  renderSessionWidget();
  applyModeVisibility();
}

/// Tiny read-through of the topbar's `data-mode` attribute,
/// kept in sync by `mountTopbarAuth`. Used by the visibility
/// helper above to avoid threading `info` through callbacks.
function getCachedAuthInfoFromAttribute(): { mode: string } | null {
  const authBox = document.getElementById("topbar-auth");
  const mode = authBox?.dataset["mode"];
  return mode ? { mode } : null;
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

/// Mount the long-running app shell (sidebar, main pane, alert
/// ticker, health dot). Idempotent — guarded by an internal
/// flag so re-mounts after logout-then-login don't duplicate
/// the alert ticker subscriptions.
let shellMounted = false;
function mountShell(): void {
  if (shellMounted) return;
  shellMounted = true;

  const sidebarEl = document.getElementById("sidebar") as HTMLElement;
  const mainEl = document.getElementById("main") as HTMLElement;
  const appEl = document.getElementById("app") as HTMLElement;
  const dot = document.getElementById("health-dot") as HTMLElement;

  appEl.style.display = "";
  buildSidebar(sidebarEl);
  window.addEventListener("hashchange", () =>
    activate(readTab(), mainEl, sidebarEl, appEl),
  );
  activate(readTab(), mainEl, sidebarEl, appEl);

  mountAlertTicker(document.getElementById("alert-ticker") as HTMLElement);
  void pollHealth(dot);
  setInterval(() => void pollHealth(dot), 10_000);
}

/// Hide / show the shell DOM. We don't unmount on logout —
/// just hide the app grid so the login overlay covers the
/// viewport on its own.
function setShellVisible(visible: boolean): void {
  const appEl = document.getElementById("app");
  if (appEl) appEl.style.display = visible ? "" : "none";
}

async function main() {
  mountTopbarAuth();

  // M6 Phase 3 Step 3.3 UI — the OIDC callback handler lands
  // the browser back on `/` with a short-lived
  // `nexus_oidc_handoff` cookie. Decode it BEFORE the
  // auth-info probe + `hasUsableSession()` check so the
  // session is in place by the time we decide whether to
  // render the login overlay vs. mount the shell.
  hydrateFromOidcHandoff();

  // Probe the engine to learn which login surface to render.
  // On network failure we fall back to "show the shell" — that
  // keeps the dev_token paste-field reachable so the operator
  // can recover even with the auth endpoint temporarily
  // unreachable.
  const info = await loadAuthInfo();
  const mode = info?.mode ?? "dev_token";
  // M6 Phase 3 Step 3.3 UI — pure `oidc` mode also needs the
  // overlay; there's no username+password field but the user
  // still has to click "Sign in with X" before the shell is
  // reachable. `hybrid` and `local` already required the
  // overlay; `dev_token` / `none` fall through to the shell
  // (paste-field topbar widget for dev_token).
  const needsSessionLogin =
    mode === "local" || mode === "hybrid" || mode === "oidc";

  if (needsSessionLogin && !hasUsableSession()) {
    // No session yet — render the overlay first; mount the
    // shell only after the user completes login.
    setShellVisible(false);
    showLoginOverlay(() => {
      hideLoginOverlay();
      setShellVisible(true);
      mountShell();
    });
  } else if (needsSessionLogin && getSession()?.user.force_password_reset) {
    // Have a session but flagged for forced reset (e.g. the
    // tab was reloaded right after admin reset-password).
    // Modal-only until they resolve it.
    setShellVisible(false);
    mountForcePasswordResetModal(getSession()!, () => {
      setShellVisible(true);
      mountShell();
    });
  } else {
    // dev_token / none / already-logged-in: straight to the
    // shell.
    mountShell();
  }

  // Watch for logout while the shell is up — drop back into
  // the overlay loop. Watch for session re-acquisition (e.g.
  // someone hand-edits localStorage) — mount the shell.
  // Also rebuild the sidebar on every session change so the
  // admin-only "Users" link appears/disappears with role
  // changes (login as admin, demote-self, etc).
  onSessionChange((session) => {
    if (shellMounted) {
      const sidebarEl = document.getElementById("sidebar");
      if (sidebarEl) buildSidebar(sidebarEl);
    }
    if (!needsSessionLogin) return;
    if (!session) {
      setShellVisible(false);
      showLoginOverlay(() => {
        hideLoginOverlay();
        setShellVisible(true);
        mountShell();
      });
    } else if (!shellMounted && !session.user.force_password_reset) {
      setShellVisible(true);
      mountShell();
    }
  });
}

void main();
