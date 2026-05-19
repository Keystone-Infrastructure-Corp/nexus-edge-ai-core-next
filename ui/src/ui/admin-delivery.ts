// M7 Step 6B — `/admin/delivery` page.
//
// Three stacked sections, each with its own host so the page can
// reload an individual card without reflowing the others:
//
//   1. Global policy — master enabled toggle, IANA timezone
//      picker, schedule editor. PUT /v1/admin/delivery.
//   2. Sinks health — table of every sink (configured + historical
//      orphans), one column per window in the server's
//      `SinksHealthResponse.windows`. Read-only; refreshes on a
//      30s interval while the tab is mounted.
//   3. Cascade primer — short prose section explaining the
//      enabled-AND / schedule-REPLACE rules so the operator doesn't
//      have to re-read the M7 doc to understand what a per-rule
//      override means.
//
// Authentication: both `/v1/admin/delivery` and
// `/v1/admin/sinks/health` go through the HS256 bearer gate. The
// shared `request()` helper attaches the token from `authHeader()`;
// 401s surface via the standard error toast.

import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { Combobox, Toggle } from "../lib/forms.js";
import {
  WeeklyScheduleEditor,
  alwaysSchedule,
  cloneSchedule,
  type ScheduleGrid,
} from "../lib/schedule-editor.js";
import { toast } from "../lib/toast.js";
import { renderAuditHistory } from "./audit-history.js";
import type {
  DeliverySettings,
  PutAdminDeliveryRequest,
  SinksHealthResponse,
  SinkHealthRow,
  OutboxSinkCounts,
} from "../api/types.js";

/// A short curated list of common IANA timezones for the
/// `<Combobox>` suggestions. The field is freeform — the operator
/// can type any IANA name (e.g. `Australia/Sydney`) and the engine
/// will validate it server-side. The curated chips just cover the
/// 90% case so the picker is one click for most ops.
const COMMON_TIMEZONES: ReadonlyArray<string> = [
  "UTC",
  "America/New_York",
  "America/Chicago",
  "America/Denver",
  "America/Los_Angeles",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Asia/Tokyo",
  "Asia/Singapore",
  "Asia/Kolkata",
  "Australia/Sydney",
];

/// Refresh cadence for the sinks-health card. Long enough to keep
/// the network quiet, short enough that a recently failed sink
/// shows red within a coffee sip. The interval is cleared when the
/// section host detaches from the DOM (see `attachReloadTimer`).
const HEALTH_RELOAD_MS = 30_000;

export async function renderAdminDelivery(root: HTMLElement): Promise<void> {
  clear(root);
  root.append(h("h2", null, "Alert Delivery"));
  root.append(
    h(
      "p",
      { class: "muted" },
      "Global enable + weekly schedule for every alert sink. Per-rule overrides live on each rule's Delivery tab.",
    ),
  );

  // Stacked section hosts. Each renderer owns its own host so a
  // sub-section reload doesn't redraw the entire page (and so a
  // partially-failed fetch can render an error inside its own
  // card without nuking the page header).
  const policyHost = h("section", { class: "admin-section" });
  const healthHost = h("section", { class: "admin-section" });
  const cascadeHost = h("section", { class: "admin-section" });
  root.append(policyHost, healthHost, cascadeHost);

  renderCascadePrimer(cascadeHost);
  await Promise.all([
    renderPolicyCard(policyHost),
    renderSinksHealthCard(healthHost),
  ]);
}

// ---------------------------------------------------------------------------
// Section 1 — global delivery policy.
// ---------------------------------------------------------------------------

async function renderPolicyCard(host: HTMLElement): Promise<void> {
  clear(host);
  host.append(h("h3", null, "Global policy"));
  const status = h("p", { class: "muted" }, "Loading delivery settings…");
  host.append(status);

  let settings: DeliverySettings;
  try {
    settings = await api.delivery.getAdmin();
  } catch (err) {
    status.textContent = `Failed to load delivery settings: ${String((err as Error).message ?? err)}`;
    return;
  }

  status.remove();

  // Working copy. We mutate `draft` on every form interaction and
  // PUT the whole thing on Save (engine treats the request as
  // atomic; there is no partial-update endpoint).
  const draft: {
    enabled: boolean;
    schedule: ScheduleGrid | null;
    timezone: string;
  } = {
    enabled: settings.enabled,
    schedule: settings.schedule ? cloneSchedule(settings.schedule.grid) : null,
    timezone: settings.timezone || "UTC",
  };

  const dirtyBadge = h("span", { class: "muted" }, "");
  const updatedAtLine = h(
    "p",
    { class: "muted" },
    `Last saved ${formatTs(settings.updated_at)}`,
  );

  function markDirty(): void {
    dirtyBadge.textContent = " · unsaved changes";
  }
  function markClean(): void {
    dirtyBadge.textContent = "";
  }

  const enabledField = Toggle({
    label: "Enable alert delivery",
    value: draft.enabled,
    helpText:
      "Master kill-switch. When off, every sink is suppressed with reason=global_disabled regardless of per-rule overrides.",
    onChange: (b) => {
      draft.enabled = b;
      markDirty();
    },
  });

  const tzField = Combobox({
    label: "Timezone",
    value: draft.timezone,
    suggestions: COMMON_TIMEZONES,
    helpText:
      "IANA name (e.g. America/Los_Angeles). The schedule grid is evaluated in this timezone. Server validates on Save.",
    onChange: (s) => {
      draft.timezone = s.trim();
      markDirty();
    },
  });

  // Schedule editor + "use a schedule?" toggle. The latter swaps
  // between `schedule = null` (always-on) and a 7×48 grid. We keep
  // the grid in memory while the toggle is off so the operator
  // can flick it back without losing their work in-session.
  let cachedGrid: ScheduleGrid = draft.schedule ?? alwaysSchedule();
  const scheduleHost = h("div", null);

  function renderScheduleSection(): void {
    clear(scheduleHost);
    const useSchedule = draft.schedule !== null;
    const scheduleToggle = Toggle({
      label: "Restrict to a weekly schedule",
      value: useSchedule,
      helpText:
        "When off, alerts may deliver at any time. When on, the grid below decides which half-hour slots are eligible.",
      onChange: (b) => {
        if (b) {
          draft.schedule = cloneSchedule(cachedGrid);
        } else {
          if (draft.schedule) cachedGrid = cloneSchedule(draft.schedule);
          draft.schedule = null;
        }
        markDirty();
        renderScheduleSection();
      },
    });
    scheduleHost.append(scheduleToggle);

    if (draft.schedule) {
      const editor = WeeklyScheduleEditor({
        value: draft.schedule,
        timezone: draft.timezone || "UTC",
        onChange: (next) => {
          draft.schedule = next;
          cachedGrid = next;
          markDirty();
        },
      });
      scheduleHost.append(editor);
    }
  }
  renderScheduleSection();

  const saveBtn = h("button", { type: "button", class: "primary" }, "Save");
  const resetBtn = h("button", { type: "button", class: "ghost" }, "Reset");

  saveBtn.addEventListener("click", () => {
    const body: PutAdminDeliveryRequest = {
      enabled: draft.enabled,
      schedule: draft.schedule ? { grid: draft.schedule } : null,
      timezone: draft.timezone,
    };
    saveBtn.setAttribute("disabled", "true");
    resetBtn.setAttribute("disabled", "true");
    api.delivery
      .putAdmin(body)
      .then((next) => {
        toast.success("Delivery settings saved.");
        markClean();
        // Re-render the card so the updated_at + canonical
        // settings come straight from the server (the engine
        // normalises grids and may reseed updated_at).
        void renderPolicyCard(host).catch((err) => {
          updatedAtLine.textContent = `Failed to refresh: ${String(err)}`;
        });
        return next;
      })
      .catch((err) => {
        toast.error(`Save failed: ${String((err as Error).message ?? err)}`);
        saveBtn.removeAttribute("disabled");
        resetBtn.removeAttribute("disabled");
      });
  });

  resetBtn.addEventListener("click", () => {
    void renderPolicyCard(host);
  });

  host.append(
    enabledField,
    tzField,
    scheduleHost,
    h("div", { class: "field-row" }, saveBtn, resetBtn, dirtyBadge),
    updatedAtLine,
    // M6 Phase 4 Step 4.2 — audit trail for changes to the global
    // delivery policy. resource_kind = "admin/delivery" with id
    // "singleton" matches what the PUT handler records.
    renderAuditHistory({
      resourceKind: "admin/delivery",
      resourceId: "singleton",
      title: "Change history",
    }),
  );
}

// ---------------------------------------------------------------------------
// Section 2 — sinks health card.
// ---------------------------------------------------------------------------

async function renderSinksHealthCard(host: HTMLElement): Promise<void> {
  clear(host);
  host.append(
    h("h3", null, "Sinks health"),
    h(
      "p",
      { class: "muted" },
      "Per-sink delivery counts over the engine's reporting windows. Orphaned rows (sink id no longer in cfg.sinks) are tagged for cleanup.",
    ),
  );

  const tableHost = h("div", null);
  const reloadBtn = h(
    "button",
    { type: "button", class: "ghost" },
    "Reload",
  );
  const status = h("span", { class: "muted" }, "");

  host.append(
    h("div", { class: "field-row" }, reloadBtn, status),
    tableHost,
  );

  async function reload(): Promise<void> {
    status.textContent = "Loading…";
    try {
      const resp = await api.delivery.sinksHealth();
      status.textContent = `Updated ${new Date().toLocaleTimeString()}`;
      renderHealthTable(tableHost, resp);
    } catch (err) {
      status.textContent = `Failed: ${String((err as Error).message ?? err)}`;
    }
  }

  reloadBtn.addEventListener("click", () => void reload());

  await reload();
  attachReloadTimer(host, reload, HEALTH_RELOAD_MS);
}

function renderHealthTable(
  tableHost: HTMLElement,
  resp: SinksHealthResponse,
): void {
  clear(tableHost);
  if (resp.sinks.length === 0) {
    tableHost.append(
      h(
        "p",
        { class: "muted" },
        "No sinks configured and no historical delivery rows. Configure sinks in the engine's TOML to begin attempting deliveries.",
      ),
    );
    return;
  }

  const table = h("table", { class: "sinks-health-table" });
  const headRow = h("tr", null);
  headRow.append(h("th", null, "Sink"));
  // Per-window columns: one super-header per window with the four
  // count subcolumns underneath would be busy; for v1 the column
  // header is `<label> sent/failed/dead/suppressed/pending` and
  // each cell is a `/`-joined number cluster.
  for (const w of resp.windows) {
    headRow.append(
      h(
        "th",
        { class: "sinks-health-num", title: `Sliding window: last ${w.secs}s` },
        `${w.label}  sent / failed / dead / suppressed / pending`,
      ),
    );
  }
  table.append(h("thead", null, headRow));

  const body = h("tbody", null);
  // Sort: configured sinks first (alphabetical), orphans last.
  const sorted = [...resp.sinks].sort((a, b) => {
    if (a.configured !== b.configured) return a.configured ? -1 : 1;
    return a.sink_id.localeCompare(b.sink_id);
  });
  for (const row of sorted) {
    body.append(renderHealthRow(row, resp.windows.map((w) => w.label)));
  }
  table.append(body);
  tableHost.append(table);
}

function renderHealthRow(
  row: SinkHealthRow,
  windowLabels: ReadonlyArray<string>,
): HTMLElement {
  const tr = h("tr", null);
  const nameCell = h(
    "td",
    null,
    h("strong", null, row.sink_id),
    row.configured
      ? null
      : h(
          "span",
          {
            class: "sinks-health-pill health-muted",
            title:
              "Sink id appears in alert_sink_outbox history but is no longer in cfg.sinks. Remove the orphaned rows or re-add the sink in TOML.",
          },
          "orphan",
        ),
  );
  tr.append(nameCell);

  for (const label of windowLabels) {
    const counts = row.counts[label];
    tr.append(h("td", { class: "sinks-health-num" }, formatCounts(counts)));
  }
  return tr;
}

function formatCounts(c: OutboxSinkCounts | undefined): string {
  if (!c) return "—";
  return `${c.sent} / ${c.failed} / ${c.dead} / ${c.suppressed} / ${c.pending}`;
}

// ---------------------------------------------------------------------------
// Section 3 — cascade primer.
// ---------------------------------------------------------------------------

function renderCascadePrimer(host: HTMLElement): void {
  clear(host);
  host.append(
    h("h3", null, "Cascade rules"),
    h(
      "ul",
      { class: "muted" },
      h(
        "li",
        null,
        h("strong", null, "Enabled "),
        "is AND-combined: a delivery only fires when ",
        h("em", null, "both"),
        " the global toggle and the rule's policy are enabled.",
      ),
      h(
        "li",
        null,
        h("strong", null, "Schedule "),
        "is replaced (not intersected): a per-rule schedule overrides the global grid entirely. Leave the per-rule schedule empty to inherit.",
      ),
      h(
        "li",
        null,
        "Suppression reasons in the per-event delivery log map 1:1 to the gate that blocked it: ",
        h("code", null, "global_disabled"),
        ", ",
        h("code", null, "rule_disabled"),
        ", ",
        h("code", null, "off_schedule_global"),
        ", ",
        h("code", null, "off_schedule_rule"),
        ".",
      ),
    ),
  );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Attach a `setInterval`-driven reload to `host`. The timer is
/// auto-cleared the first tick after the host detaches from the
/// DOM, so navigating away from the tab doesn't leak a forever
/// HTTP poller. Uses `MutationObserver` on `document.body` because
/// the vanilla SPA has no formal unmount hook.
function attachReloadTimer(
  host: HTMLElement,
  reload: () => Promise<void>,
  intervalMs: number,
): void {
  const timer = window.setInterval(() => {
    if (!host.isConnected) {
      window.clearInterval(timer);
      obs.disconnect();
      return;
    }
    void reload();
  }, intervalMs);
  const obs = new MutationObserver(() => {
    if (!host.isConnected) {
      window.clearInterval(timer);
      obs.disconnect();
    }
  });
  obs.observe(document.body, { childList: true, subtree: true });
}

/// Format an RFC3339 timestamp as `<date> <time>` in the operator's
/// browser locale. Best-effort — falls back to the raw string if
/// `new Date(...)` cannot parse it (defensive against e.g. SQLite
/// rows storing fractional seconds in a non-standard format).
function formatTs(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return `${d.toLocaleDateString()} ${d.toLocaleTimeString()}`;
}
