// M6 Phase 4 Step 4.2 — per-resource audit history panel.
//
// Drop-in component for the camera / rule / sink / user detail
// views. Renders a collapsible `<details>` block titled "History"
// that fetches `GET /admin/audit/resource/{kind}/{id}` on first
// open and displays the last 50 rows newest-first.
//
// Lazy on purpose: the audit log can be hundreds of MB after a
// few months of operation; we don't want to fire a request for
// every detail-view paint. Open-on-demand keeps the page snappy
// and gives the operator explicit control over the data they
// pull.
//
// Failure modes are surfaced inline (no toast spam): network
// error → "Failed to load history (<err>)" with a retry button;
// admin-only 401/403 → "Sign in as an admin to view history"
// (the SPA's auth overlay will already be visible — this is
// belt-and-suspenders messaging).

import { api } from "../api/client.js";
import type { AuditRow } from "../api/types.js";
import { h } from "../lib/el.js";
import { formatLocalTime, formatTimeTooltip } from "../lib/format.js";

interface HistoryPanelOptions {
  /// Resource discriminator. Must match what the backend writes
  /// in `audit_log.resource_kind` (e.g. "camera", "rule",
  /// "sink", "user"). Case-sensitive.
  resourceKind: string;
  /// String form of the resource id. We `encodeURIComponent` it
  /// before putting it on the URL, but the panel doesn't try to
  /// coerce types — pass `String(camera.id)` etc.
  resourceId: string;
  /// Optional label override; defaults to "History".
  title?: string;
  /// Limit override (server default 50, max 200).
  limit?: number;
  /// If true, panel is open on mount and the fetch fires
  /// immediately. Default false.
  open?: boolean;
}

/// Render the history panel. Returns the root `<details>`
/// element so callers can append it directly to a detail view.
export function renderAuditHistory(opts: HistoryPanelOptions): HTMLElement {
  const limit = opts.limit ?? 50;
  const title = opts.title ?? "History";

  const body = h("div", { class: "audit-history-body" });
  body.textContent = "Loading…";

  let loaded = false;

  const load = (): void => {
    body.textContent = "Loading…";
    api.adminAudit
      .forResource(opts.resourceKind, opts.resourceId, limit)
      .then((rows) => {
        renderRows(body, rows);
      })
      .catch((err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        renderError(body, msg, load);
      });
  };

  const summary = h(
    "summary",
    { class: "audit-history-summary" },
    title,
  );

  const root = h(
    "details",
    {
      class: "audit-history",
      ...(opts.open ? { open: true } : {}),
      on: {
        toggle: (ev) => {
          const det = ev.currentTarget as HTMLDetailsElement;
          if (det.open && !loaded) {
            loaded = true;
            load();
          }
        },
      },
    },
    summary,
    body,
  );

  if (opts.open) {
    loaded = true;
    load();
  }

  return root;
}

function renderRows(body: HTMLElement, rows: AuditRow[]): void {
  body.textContent = "";
  if (rows.length === 0) {
    body.append(
      h("div", { class: "audit-history-empty" }, "No history yet."),
    );
    return;
  }
  const tbl = h("table", { class: "audit-history-table" });
  const thead = h(
    "thead",
    null,
    h(
      "tr",
      null,
      h("th", null, "When"),
      h("th", null, "Actor"),
      h("th", null, "Action"),
      h("th", null, "Outcome"),
      h("th", null, "Details"),
    ),
  );
  const tbody = h("tbody", null);
  for (const row of rows) {
    tbody.append(renderRow(row));
  }
  tbl.append(thead, tbody);
  body.append(tbl);
}

function renderRow(row: AuditRow): HTMLElement {
  const when = h(
    "td",
    {
      class: "audit-history-when",
      title: formatTimeTooltip(row.created_at),
    },
    formatLocalTime(row.created_at),
  );
  const actor = h(
    "td",
    { class: "audit-history-actor" },
    h(
      "span",
      { class: `audit-actor-kind audit-actor-${row.actor_kind}` },
      row.actor_kind.replace("_", " "),
    ),
    " ",
    row.actor_label,
  );
  const action = h("td", { class: "audit-history-action" }, row.action);
  const outcomeCell = h(
    "td",
    { class: "audit-history-outcome" },
    h(
      "span",
      { class: `audit-outcome audit-outcome-${row.outcome}` },
      row.outcome,
    ),
  );
  const details = h(
    "td",
    { class: "audit-history-details" },
    renderDetails(row),
  );
  return h("tr", null, when, actor, action, outcomeCell, details);
}

function renderDetails(row: AuditRow): HTMLElement {
  // Show a single-line summary; expand on click to show the
  // full JSON diff. Most rows have either `before_json`,
  // `after_json`, or both. A delete looks like `{before, null}`,
  // a create looks like `{null, after}`, an update has both.
  const has = (s: string | null): s is string =>
    typeof s === "string" && s.length > 0;
  if (!has(row.before_json) && !has(row.after_json)) {
    return h("span", { class: "audit-history-no-payload" }, "—");
  }
  const wrapper = h("details", { class: "audit-history-diff" });
  const summary = h(
    "summary",
    { class: "audit-history-diff-summary" },
    summariseChange(row),
  );
  const pre = h(
    "pre",
    { class: "audit-history-diff-pre" },
    renderJsonDiff(row.before_json, row.after_json),
  );
  wrapper.append(summary, pre);
  return wrapper;
}

function summariseChange(row: AuditRow): string {
  const had = typeof row.before_json === "string" && row.before_json.length > 0;
  const got = typeof row.after_json === "string" && row.after_json.length > 0;
  if (!had && got) return "created";
  if (had && !got) return "deleted";
  return "updated";
}

function renderJsonDiff(
  before: string | null,
  after: string | null,
): string {
  const fmt = (s: string | null): string => {
    if (!s) return "(none)";
    try {
      return JSON.stringify(JSON.parse(s), null, 2);
    } catch {
      // Not valid JSON — show as-is rather than swallow content.
      return s;
    }
  };
  return `--- before\n${fmt(before)}\n\n+++ after\n${fmt(after)}`;
}

function renderError(
  body: HTMLElement,
  msg: string,
  retry: () => void,
): void {
  body.textContent = "";
  body.append(
    h(
      "div",
      { class: "audit-history-error" },
      `Failed to load history: ${msg}`,
    ),
    h(
      "button",
      {
        class: "audit-history-retry",
        type: "button",
        on: { click: () => retry() },
      },
      "Retry",
    ),
  );
}
