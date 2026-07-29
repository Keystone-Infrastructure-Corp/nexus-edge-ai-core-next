// Delivery page — global settings + schedule editor + sink health.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Activity,
  AlertCircle,
  CheckCircle2,
  Clock,
  Mail,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import {
  getDeliverySettings,
  getSinksHealth,
  putDeliverySettings,
} from "@/api/storage";
import type {
  DeliverySchedule,
  DeliverySettings,
  SinkHealthEntry,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { FleetManagedBadge } from "@/components/fleet-managed-badge";
import {
  ScheduleGrid,
  makeEmptyGrid,
} from "@/components/schedule-grid";
import { SinksCard } from "@/components/sinks-card";

// Slots: 0..47 (half-hour each). Schedule grid is rendered by the
// shared `ScheduleGrid` component; we only deal with the boolean
// matrix here.

const COMMON_TZ = [
  "UTC",
  "America/Los_Angeles",
  "America/Denver",
  "America/Chicago",
  "America/New_York",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Asia/Tokyo",
  "Asia/Singapore",
  "Australia/Sydney",
];

export function DeliveryPage() {
  const qc = useQueryClient();
  const settingsQuery = useQuery({
    queryKey: ["delivery", "settings"],
    queryFn: getDeliverySettings,
  });
  const healthQuery = useQuery({
    queryKey: ["delivery", "sinks-health"],
    queryFn: getSinksHealth,
    refetchInterval: 30_000,
  });

  return (
    <div className="space-y-6">
      <header className="flex items-start justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold">Delivery</h1>
          <p className="text-sm text-muted-foreground">
            Alert sinks, outbox dispatch settings, weekly schedule, and
            per-sink health.
          </p>
        </div>
        <FleetManagedBadge category="delivery_settings" />
      </header>

      <CascadeAlert />

      {settingsQuery.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : settingsQuery.isError || !settingsQuery.data ? (
        <Card>
          <CardContent className="py-6 text-center text-sm text-destructive">
            Failed to load delivery settings.
          </CardContent>
        </Card>
      ) : (
        <DeliveryEditor
          settings={settingsQuery.data}
          onSaved={() =>
            qc.invalidateQueries({ queryKey: ["delivery", "settings"] })
          }
        />
      )}

      <SinksCard />

      <SinkHealthCard
        data={healthQuery.data?.sinks ?? []}
        windows={healthQuery.data?.windows ?? []}
        loading={healthQuery.isLoading}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Cascade primer.
// ---------------------------------------------------------------------------

function CascadeAlert() {
  const [open, setOpen] = useState(false);
  return (
    <Card>
      <CardContent className="p-4">
        <button
          onClick={() => setOpen((v) => !v)}
          className="flex w-full items-center justify-between text-left"
        >
          <span className="flex items-center gap-2 text-sm font-medium">
            <AlertCircle className="h-4 w-4 text-accent" />
            How the delivery cascade works
          </span>
          <span className="text-xs text-muted-foreground">
            {open ? "Hide" : "Show"}
          </span>
        </button>
        {open ? (
          <div className="mt-3 space-y-2 text-xs text-muted-foreground">
            <p>
              <strong>Global enabled</strong> is a master kill switch. When
              off, every outbox row is marked{" "}
              <code className="font-mono">suppressed</code> with reason{" "}
              <code className="font-mono">global_disabled</code>.
            </p>
            <p>
              <strong>Global schedule</strong> is a 7×48 grid of half-hour
              slots in the configured timezone. Off-schedule rows are
              suppressed with{" "}
              <code className="font-mono">off_schedule_global</code>.
            </p>
            <p>
              <strong>Per-rule policy</strong> overrides the global setting.
              Rule schedules <em>replace</em> the global one (no
              intersection). Rule-disabled rows show{" "}
              <code className="font-mono">rule_disabled</code>; off-rule
              schedule shows{" "}
              <code className="font-mono">off_schedule_rule</code>.
            </p>
            <p>
              <strong>Effective</strong> ={" "}
              <code className="font-mono">
                global.enabled && (policy.enabled ?? true)
              </code>{" "}
              with{" "}
              <code className="font-mono">policy.schedule ?? global.schedule</code>.
              Edit per-rule overrides from the Rules page.
            </p>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Editor.
// ---------------------------------------------------------------------------

function DeliveryEditor({
  settings,
  onSaved,
}: {
  settings: DeliverySettings;
  onSaved: () => void;
}) {
  const [enabled, setEnabled] = useState(settings.enabled);
  const [tz, setTz] = useState(settings.timezone);
  const [scheduleOn, setScheduleOn] = useState(settings.schedule !== null);
  const [grid, setGrid] = useState<boolean[][]>(
    settings.schedule?.grid ?? makeEmptyGrid(),
  );
  const [saveError, setSaveError] = useState<string | null>(null);

  // Reset local state if upstream changes.
  useEffect(() => {
    setEnabled(settings.enabled);
    setTz(settings.timezone);
    setScheduleOn(settings.schedule !== null);
    setGrid(settings.schedule?.grid ?? makeEmptyGrid());
  }, [settings]);

  const saveMutation = useMutation({
    mutationFn: () => {
      const schedule: DeliverySchedule | null = scheduleOn ? { grid } : null;
      return putDeliverySettings({ enabled, timezone: tz, schedule });
    },
    onSuccess: () => {
      setSaveError(null);
      onSaved();
    },
    onError: (e: unknown) =>
      setSaveError(e instanceof Error ? e.message : String(e)),
  });

  const dirty =
    enabled !== settings.enabled ||
    tz !== settings.timezone ||
    scheduleOn !== (settings.schedule !== null) ||
    (scheduleOn &&
      JSON.stringify(grid) !==
        JSON.stringify(settings.schedule?.grid ?? grid));

  return (
    <Card>
      <CardContent className="space-y-5 p-5">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <label className="inline-flex items-center gap-2">
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
              className="h-4 w-4 rounded border-border"
            />
            <span className="text-sm font-medium">
              Delivery enabled (master switch)
            </span>
          </label>

          <div className="flex items-center gap-2">
            <Label htmlFor="tz" className="text-xs text-muted-foreground">
              Timezone
            </Label>
            <select
              id="tz"
              className="h-9 rounded-md border border-input bg-transparent px-2 text-sm"
              value={tz}
              onChange={(e) => setTz(e.target.value)}
            >
              {!COMMON_TZ.includes(tz) ? (
                <option value={tz}>{tz}</option>
              ) : null}
              {COMMON_TZ.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </div>
        </div>

        <div>
          <label className="inline-flex items-center gap-2">
            <input
              type="checkbox"
              checked={scheduleOn}
              onChange={(e) => setScheduleOn(e.target.checked)}
              className="h-4 w-4 rounded border-border"
            />
            <span className="text-sm font-medium">
              Restrict to a weekly schedule
            </span>
          </label>
          <p className="ml-6 text-xs text-muted-foreground">
            When off, delivery runs at all times (subject to per-rule
            overrides). When on, only slots that are filled will deliver.
          </p>
        </div>

        {scheduleOn ? (
          <ScheduleGrid grid={grid} onChange={setGrid} />
        ) : null}

        {saveError ? (
          <p className="text-sm text-destructive">{saveError}</p>
        ) : null}

        <div className="flex items-center justify-between border-t border-border/40 pt-3">
          <p className="text-xs text-muted-foreground">
            Last saved {settings.updated_at.replace("T", " ").slice(0, 19)}
          </p>
          <Button
            disabled={!dirty || saveMutation.isPending}
            onClick={() => saveMutation.mutate()}
          >
            {saveMutation.isPending ? "Saving…" : "Save"}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Sink health.
// ---------------------------------------------------------------------------

function SinkHealthCard({
  data,
  windows,
  loading,
}: {
  data: SinkHealthEntry[];
  windows: Array<{ label: string; secs: number }>;
  loading: boolean;
}) {
  const labels = useMemo(() => windows.map((w) => w.label), [windows]);

  return (
    <Card>
      <CardContent className="space-y-3 p-5">
        <div className="flex items-center justify-between">
          <h2 className="flex items-center gap-2 text-base font-semibold">
            <Activity className="h-4 w-4" />
            Sink health
          </h2>
          <span className="text-xs text-muted-foreground">
            Refreshes every 30s
          </span>
        </div>

        {loading ? (
          <Skeleton className="h-24 w-full" />
        ) : data.length === 0 ? (
          <p className="text-center text-sm text-muted-foreground">
            No sinks configured.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="text-xs uppercase text-muted-foreground">
                <tr>
                  <th className="px-2 py-2 text-left">Sink</th>
                  <th className="px-2 py-2 text-left">Status</th>
                  {labels.map((l) => (
                    <th key={l} className="px-2 py-2 text-right">
                      {l}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {data.map((s) => (
                  <tr key={s.sink_id} className="border-t border-border/40">
                    <td className="px-2 py-2">
                      <div className="flex items-center gap-2">
                        <Mail className="h-3 w-3 text-muted-foreground" />
                        <span className="font-mono text-xs">{s.sink_id}</span>
                      </div>
                    </td>
                    <td className="px-2 py-2">
                      {s.configured ? (
                        <Badge variant="success">
                          <CheckCircle2 className="mr-1 h-3 w-3" />
                          configured
                        </Badge>
                      ) : (
                        <Badge variant="secondary">
                          <Clock className="mr-1 h-3 w-3" />
                          unconfigured
                        </Badge>
                      )}
                    </td>
                    {labels.map((l) => (
                      <td
                        key={l}
                        className="px-2 py-2 text-right text-xs"
                      >
                        <SinkCountsCell counts={s.counts[l]} />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function SinkCountsCell({
  counts,
}: {
  counts?: { sent: number; failed: number; dead: number; suppressed: number; pending: number };
}) {
  if (!counts) return <span className="text-muted-foreground">—</span>;
  const parts: { label: string; value: number; className: string }[] = [];
  if (counts.sent > 0)
    parts.push({
      label: "sent",
      value: counts.sent,
      className: "text-success",
    });
  if (counts.failed > 0)
    parts.push({
      label: "fail",
      value: counts.failed,
      className: "text-warning",
    });
  if (counts.dead > 0)
    parts.push({
      label: "dead",
      value: counts.dead,
      className: "text-destructive",
    });
  if (counts.pending > 0)
    parts.push({
      label: "pend",
      value: counts.pending,
      className: "text-muted-foreground",
    });
  if (counts.suppressed > 0)
    parts.push({
      label: "supp",
      value: counts.suppressed,
      className: "text-muted-foreground",
    });
  if (parts.length === 0) return <span className="text-muted-foreground">—</span>;
  return (
    <span className="flex justify-end gap-2 font-mono">
      {parts.map((p) => (
        <span key={p.label} className={p.className}>
          {p.value}
          <span className="ml-0.5 text-[10px] opacity-70">{p.label}</span>
        </span>
      ))}
    </span>
  );
}
