// M-Admin Phase 1B Step 4 — Camera Discover dialog.
//
// Opens a single modal that runs two discoveries in parallel:
//
//   - ONVIF WS-Discovery (multicast probe, ~5 s window).
//   - CIDR sweep — defaults to a `/24` the operator can edit. The
//     engine rejects prefixes shorter than `/22` outright; `/22`
//     also needs an explicit `confirm: true` because that's 1022
//     hosts × 3 ports per host.
//
// Live results stream into the table via a 750 ms poll on each
// session id. Rows have a Verify button (inline RTSP OPTIONS +
// DESCRIBE with optional Digest auth) and an Add button (opens
// the existing `cameras-form.ts` with `name` / `url` pre-filled).
//
// The dialog resolves `true` when at least one camera was added,
// `false` otherwise — so the caller can refresh the cameras table.

import { api } from "../api/client.js";
import { clear, h } from "../lib/el.js";
import { openDialog, dialogFooter, type DialogHandle } from "../lib/dialog.js";
import { TextField, Toggle } from "../lib/forms.js";
import { toast } from "../lib/toast.js";
import { openCameraForm } from "./cameras-form.js";
import type {
  CameraId,
  DiscoveredDevice,
  DiscoverySession,
  ProbeRtspResult,
  ScanReq,
} from "../api/types.js";

const POLL_INTERVAL_MS = 750;
const DEFAULT_CIDR = "192.168.1.0/24";

export interface OpenDiscoveryOpts {
  /// Forwarded to `cameras-form.ts` so the Add flow picks the
  /// next free id without collision.
  existingIds: ReadonlyArray<CameraId>;
}

export function openDiscoveryDialog(opts: OpenDiscoveryOpts): Promise<boolean> {
  let dlg: DialogHandle | null = null;
  let cidrInput = DEFAULT_CIDR;
  let confirmLarge = false;
  let onvifSessionId: string | null = null;
  let scanSessionId: string | null = null;
  /// The session id used to audit Verify probes (engine just needs
  /// any valid live id; we pick the first one started).
  let auditSessionId: string | null = null;
  let anyAdded = false;

  // Per-session snapshot map keyed by session id. Polling tasks
  // mutate this map then call `renderResults()`. The map outlives
  // the polling tasks because we leave finished sessions visible.
  const sessions = new Map<string, DiscoverySession>();
  /// Verify-result text per device key (`${ip}:${port}`) so the
  /// row keeps its annotation through re-renders.
  const verifyResults = new Map<
    string,
    { text: string; ok: boolean | null }
  >();

  const body = h("div", { class: "discovery-body" });
  const controlsHost = h("div", { class: "discovery-controls" });
  const progressHost = h("div", { class: "discovery-progress" });
  const resultsHost = h("div", { class: "discovery-results" });
  body.append(controlsHost, progressHost, resultsHost);

  function isLargeCidr(c: string): boolean {
    return /\/(?:22|23)\s*$/.test(c);
  }

  function rebuildControls(): void {
    clear(controlsHost);

    const cidrField = TextField({
      label: "CIDR",
      value: cidrInput,
      placeholder: DEFAULT_CIDR,
      helpText:
        "Defaults to /24. /23 and /22 are supported but require confirm.",
      onChange: (v) => {
        cidrInput = v.trim();
        rebuildControls();
      },
    });

    const confirmToggle = isLargeCidr(cidrInput)
      ? Toggle({
          label: "Confirm large scan",
          value: confirmLarge,
          helpText: `${cidrInput} sweeps up to 1022 hosts × 3 ports — explicit confirm is required by the engine.`,
          onChange: (b) => {
            confirmLarge = b;
            rebuildControls();
          },
        })
      : null;

    const onvifBtn = h(
      "button",
      {
        type: "button",
        class: "primary",
        disabled: !!onvifSessionId,
        on: { click: () => void startOnvif() },
      },
      onvifSessionId ? "ONVIF probe running…" : "Start ONVIF probe",
    );
    const scanBtn = h(
      "button",
      {
        type: "button",
        class: "primary",
        disabled: !!scanSessionId,
        on: { click: () => void startScan() },
      },
      scanSessionId ? "Scan running…" : `Scan ${cidrInput || "(set CIDR)"}`,
    );

    const actions = h(
      "div",
      { class: "discovery-actions" },
      onvifBtn,
      scanBtn,
    );

    const section = h(
      "div",
      { class: "admin-section" },
      cidrField,
      confirmToggle,
      actions,
    );
    controlsHost.append(section);
  }

  async function startOnvif(): Promise<void> {
    try {
      const r = await api.discovery.startOnvif();
      onvifSessionId = r.session_id;
      if (!auditSessionId) auditSessionId = r.session_id;
      rebuildControls();
      poll(r.session_id, "ONVIF");
    } catch (e) {
      toast.error(`ONVIF probe failed to start: ${(e as Error).message}`);
    }
  }

  async function startScan(): Promise<void> {
    if (!cidrInput) {
      toast.error("Set a CIDR first.");
      return;
    }
    try {
      const req: ScanReq = { cidr: cidrInput };
      if (confirmLarge) req.confirm = true;
      const r = await api.discovery.startScan(req);
      scanSessionId = r.session_id;
      if (!auditSessionId) auditSessionId = r.session_id;
      rebuildControls();
      poll(r.session_id, "Scan");
    } catch (e) {
      toast.error(`Scan failed to start: ${(e as Error).message}`);
    }
  }

  function poll(id: string, label: string): void {
    const tick = async (): Promise<void> => {
      try {
        const s = await api.discovery.getSession(id);
        sessions.set(id, s);
        renderProgress();
        renderResults();
        if (s.state === "done" || s.state === "error") {
          if (s.state === "error") {
            toast.error(
              `${label} failed: ${s.error ?? "unknown error"}`,
            );
          }
          return;
        }
        window.setTimeout(() => void tick(), POLL_INTERVAL_MS);
      } catch (e) {
        toast.error(`Session poll failed: ${(e as Error).message}`);
      }
    };
    void tick();
  }

  function renderProgress(): void {
    clear(progressHost);
    if (sessions.size === 0) {
      return;
    }
    for (const s of sessions.values()) {
      const pct =
        s.progress_total > 0
          ? Math.min(
              100,
              Math.round((s.progress_scanned / s.progress_total) * 100),
            )
          : s.state === "done"
            ? 100
            : 0;
      const fill = h("div", {
        class: "discovery-bar-fill",
        style: { width: `${pct}%` },
      });
      const bar = h("div", { class: "discovery-bar" }, fill);
      progressHost.append(
        h(
          "div",
          { class: "discovery-progress-row" },
          h(
            "span",
            { class: "discovery-label" },
            `${s.kind.toUpperCase()} · ${s.state}`,
          ),
          bar,
          h(
            "span",
            { class: "muted discovery-counts" },
            `${s.progress_scanned}/${s.progress_total || "?"} • ${s.found.length} found`,
          ),
        ),
      );
    }
  }

  function renderResults(): void {
    clear(resultsHost);
    // Merge devices across both sessions, dedup on `${ip}:${port}`,
    // keep the richer-metadata row.
    const merged = new Map<string, DiscoveredDevice>();
    for (const s of sessions.values()) {
      for (const d of s.found) {
        const k = `${d.ip}:${d.port}`;
        const prev = merged.get(k);
        if (!prev || metadataScore(d) > metadataScore(prev)) {
          merged.set(k, d);
        }
      }
    }
    if (merged.size === 0) {
      if (sessions.size > 0) {
        resultsHost.append(
          h("p", { class: "muted" }, "No devices found yet."),
        );
      }
      return;
    }

    const rows = [...merged.values()]
      .sort((a, b) => compareIps(a.ip, b.ip))
      .map((d) => deviceRow(d));

    const tbl = h(
      "table",
      { class: "admin-table" },
      h(
        "thead",
        null,
        h(
          "tr",
          null,
          h("th", null, "IP"),
          h("th", null, "Port"),
          h("th", null, "Vendor / Model"),
          h("th", null, "Source"),
          h("th", null, "Verify"),
          h("th", null, ""),
        ),
      ),
      h("tbody", null, ...rows),
    );
    resultsHost.append(tbl);
  }

  function deviceRow(d: DiscoveredDevice): HTMLElement {
    const key = `${d.ip}:${d.port}`;
    const verifyState = verifyResults.get(key);
    const verifyCell = h("span", {
      class:
        verifyState?.ok === true
          ? "discovery-ok"
          : verifyState?.ok === false
            ? "discovery-fail"
            : "muted",
    });
    verifyCell.textContent = verifyState?.text ?? "—";

    const vendorModel =
      [d.vendor, d.model].filter(Boolean).join(" ").trim() || null;

    return h(
      "tr",
      null,
      h("td", null, h("code", { class: "mono" }, d.ip)),
      h("td", null, String(d.port)),
      h(
        "td",
        null,
        vendorModel ? vendorModel : h("span", { class: "muted" }, "—"),
      ),
      h("td", null, d.kind),
      h("td", null, verifyCell),
      h(
        "td",
        { class: "discovery-row-actions" },
        h(
          "button",
          {
            type: "button",
            class: "ghost",
            on: { click: () => void onVerify(d, key) },
          },
          "Verify",
        ),
        h(
          "button",
          {
            type: "button",
            class: "primary",
            on: { click: () => void onAdd(d) },
          },
          "Add",
        ),
      ),
    );
  }

  async function onVerify(d: DiscoveredDevice, key: string): Promise<void> {
    if (!auditSessionId) {
      toast.error("Start ONVIF or Scan first so the probe can be audited.");
      return;
    }
    verifyResults.set(key, { text: "verifying…", ok: null });
    renderResults();
    try {
      const r: ProbeRtspResult = await api.discovery.probeRtsp(
        auditSessionId,
        {
          host: d.ip,
          port: 554,
          path: d.rtsp_paths[0] ?? "/",
        },
      );
      const codecs = r.sdp_streams.map((s) => s.codec).join("+");
      verifyResults.set(key, {
        text: r.ok ? `✓ ${codecs || "ok"}` : `✗ status ${r.status}`,
        ok: r.ok,
      });
    } catch (e) {
      verifyResults.set(key, {
        text: `✗ ${(e as Error).message}`,
        ok: false,
      });
    }
    renderResults();
  }

  async function onAdd(d: DiscoveredDevice): Promise<void> {
    const name =
      [d.vendor, d.model].filter(Boolean).join(" ").trim() || `Camera @${d.ip}`;
    const rtspPath = d.rtsp_paths[0] ?? "/";
    const url = `rtsp://${d.ip}:554${rtspPath}`;
    const ok = await openCameraForm({
      mode: "create",
      existingIds: opts.existingIds,
      prefill: { name: `${name} @ ${d.ip}`, url },
    });
    if (ok) {
      anyAdded = true;
    }
  }

  rebuildControls();

  const footer = dialogFooter({
    cancelLabel: "Close",
    confirmLabel: "Done",
    onCancel: () => dlg?.close(anyAdded),
    onConfirm: () => dlg?.close(anyAdded),
  });

  dlg = openDialog({
    title: "Discover cameras",
    body,
    footer,
    width: "820px",
  });

  return dlg.closed;
}

function metadataScore(d: DiscoveredDevice): number {
  return (
    (d.vendor ? 1 : 0) +
    (d.model ? 1 : 0) +
    (d.hardware ? 1 : 0) +
    (d.firmware ? 1 : 0) +
    (d.mac ? 1 : 0) +
    (d.rtsp_paths.length > 0 ? 1 : 0)
  );
}

/// Numeric sort on the four octets so the results table looks
/// natural to anyone who's ever stared at a `nmap` printout.
function compareIps(a: string, b: string): number {
  const pa = a.split(".").map((p) => parseInt(p, 10) || 0);
  const pb = b.split(".").map((p) => parseInt(p, 10) || 0);
  for (let i = 0; i < 4; i++) {
    const da = pa[i] ?? 0;
    const db = pb[i] ?? 0;
    if (da !== db) return da - db;
  }
  return 0;
}
