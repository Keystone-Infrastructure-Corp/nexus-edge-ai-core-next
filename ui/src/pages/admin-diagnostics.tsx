// Admin Diagnostics page.
//
// Live host metrics + sink delivery health (always available) plus a
// real diagnostics-tarball download via the streaming export endpoint.
// The engine generates the .tar.gz inline (no temp file) and pipes it
// straight to the browser, so the only memory overhead is the bounded
// mpsc buffer in the engine — even multi-MB bundles stream cleanly.

import { useMutation, useQuery } from "@tanstack/react-query";
import {
  Activity,
  Cpu,
  Download,
  HardDrive,
  Loader2,
  MemoryStick,
  ScrollText,
  Stethoscope,
} from "lucide-react";
import { toast } from "sonner";

import { downloadDiagnosticsBundle } from "@/api/admin";
import { getSinksHealth } from "@/api/storage";
import { getSystemMetrics } from "@/api/system";
import type { SinkHealthEntry } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { formatBytes, formatDuration, formatPct } from "@/lib/format";

export function AdminDiagnosticsPage() {
  const metricsQuery = useQuery({
    queryKey: ["system", "metrics"],
    queryFn: () => getSystemMetrics(),
    refetchInterval: 5_000,
  });

  const sinksQuery = useQuery({
    queryKey: ["admin", "sinks", "health"],
    queryFn: () => getSinksHealth(),
    refetchInterval: 15_000,
  });

  // Streaming export. Returns a Blob + the filename the engine
  // picked (e.g. `nexus-diagnostics-20251121-031245Z.tar.gz`).
  // We hand the blob to URL.createObjectURL and trigger a hidden
  // anchor click — no extra deps, works in every browser the SPA
  // already supports. The objectURL is revoked on the next tick so
  // we don't pin the bundle's bytes in memory longer than needed.
  const exportMutation = useMutation({
    mutationFn: () => downloadDiagnosticsBundle(),
    onSuccess: ({ blob, filename }) => {
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      a.remove();
      // Defer revoke so Safari has time to start the download.
      setTimeout(() => URL.revokeObjectURL(url), 1_000);
      toast.success(`Bundle saved: ${filename}`);
    },
    onError: (e: unknown) => {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Bundle generation failed: ${msg}`);
    },
  });

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Diagnostics</h1>
          <p className="text-sm text-muted-foreground">
            Live host metrics + sink delivery health. Download a redacted
            diagnostics tarball for support — the engine streams it inline,
            no temp file on the box.
          </p>
        </div>
        <Button
          variant="outline"
          onClick={() => exportMutation.mutate()}
          disabled={exportMutation.isPending}
          data-testid="diagnostics-download"
        >
          {exportMutation.isPending ? (
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
          ) : (
            <Download className="mr-2 h-4 w-4" />
          )}
          {exportMutation.isPending ? "Generating…" : "Download bundle"}
        </Button>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Activity className="h-4 w-4 text-muted-foreground" />
            Host metrics
          </CardTitle>
        </CardHeader>
        <CardContent>
          {metricsQuery.isLoading ? (
            <Skeleton className="h-24 w-full" />
          ) : metricsQuery.data ? (
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <MetricTile
                icon={<Cpu className="h-4 w-4" />}
                label="CPU"
                value={formatPct(metricsQuery.data.cpu.usage_pct, 1)}
                sub={`${metricsQuery.data.cpu.count} cores`}
              />
              <MetricTile
                icon={<MemoryStick className="h-4 w-4" />}
                label="Memory"
                value={`${formatBytes(metricsQuery.data.memory.used_bytes)} / ${formatBytes(metricsQuery.data.memory.total_bytes)}`}
                sub={formatPct(
                  (metricsQuery.data.memory.used_bytes /
                    Math.max(1, metricsQuery.data.memory.total_bytes)) *
                    100,
                  1,
                )}
              />
              <MetricTile
                icon={<Activity className="h-4 w-4" />}
                label="Load avg"
                value={(metricsQuery.data.cpu.load_avg_1m ?? 0).toFixed(2)}
                sub={`5m ${(metricsQuery.data.cpu.load_avg_5m ?? 0).toFixed(2)} · 15m ${(metricsQuery.data.cpu.load_avg_15m ?? 0).toFixed(2)}`}
              />
              <MetricTile
                icon={<Activity className="h-4 w-4" />}
                label="Uptime"
                value={formatDuration(metricsQuery.data.uptime_secs)}
              />
            </div>
          ) : null}

          {metricsQuery.data?.disks && metricsQuery.data.disks.length > 0 ? (
            <div className="mt-4">
              <h3 className="mb-2 flex items-center gap-2 text-sm font-medium text-muted-foreground">
                <HardDrive className="h-3 w-3" />
                Mounts
              </h3>
              <div className="overflow-x-auto">
                <table className="w-full text-xs">
                  <thead className="text-muted-foreground">
                    <tr>
                      <th className="px-2 py-1 text-left">Mount</th>
                      <th className="px-2 py-1 text-left">FS</th>
                      <th className="px-2 py-1 text-right">Used</th>
                      <th className="px-2 py-1 text-right">Total</th>
                      <th className="px-2 py-1 text-right">Free</th>
                    </tr>
                  </thead>
                  <tbody>
                    {metricsQuery.data.disks.map((d) => (
                      <tr
                        key={d.mount_point}
                        className="border-t border-border/30"
                      >
                        <td className="px-2 py-1 font-mono">
                          {d.mount_point}
                        </td>
                        <td className="px-2 py-1 text-muted-foreground">
                          {d.file_system}
                        </td>
                        <td className="px-2 py-1 text-right">
                          {formatBytes(d.total_bytes - d.available_bytes)}
                        </td>
                        <td className="px-2 py-1 text-right">
                          {formatBytes(d.total_bytes)}
                        </td>
                        <td className="px-2 py-1 text-right">
                          {formatBytes(d.available_bytes)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          ) : null}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <ScrollText className="h-4 w-4 text-muted-foreground" />
            Sink delivery health
          </CardTitle>
        </CardHeader>
        <CardContent>
          {sinksQuery.isLoading ? (
            <Skeleton className="h-32 w-full" />
          ) : sinksQuery.data ? (
            <SinksTable
              windows={sinksQuery.data.windows}
              sinks={sinksQuery.data.sinks}
            />
          ) : null}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Stethoscope className="h-4 w-4 text-muted-foreground" />
            Field diagnostics
          </CardTitle>
        </CardHeader>
        <CardContent className="text-sm text-muted-foreground">
          <p>
            The <strong>Download bundle</strong> button at the top of the
            page generates a redacted{" "}
            <code className="font-mono">.tar.gz</code> containing the
            sanitised <code className="font-mono">nexus.toml</code>, current
            host metrics, the last 24h of motion events, the recent
            admin-audit log, the registered storage backends, and the recent
            engine logs (<code className="font-mono">nexus-engine.log</code>,
            24h of <code className="font-mono">journalctl</code> output — the
            first place to look for stream drops). Secrets (API keys, RTSP
            passwords, OIDC client secrets, and any credentials embedded in
            log URLs) are stripped before the bundle hits the wire.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}

function MetricTile({
  icon,
  label,
  value,
  sub,
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  sub?: string;
}) {
  return (
    <div className="rounded-md border border-border bg-muted/20 p-3">
      <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
        {icon}
        {label}
      </div>
      <div className="mt-1 text-lg font-semibold">{value}</div>
      {sub ? <div className="text-xs text-muted-foreground">{sub}</div> : null}
    </div>
  );
}

function SinksTable({
  windows,
  sinks,
}: {
  windows: Array<{ label: string; secs: number }>;
  sinks: SinkHealthEntry[];
}) {
  if (sinks.length === 0) {
    return (
      <p className="py-6 text-center text-sm text-muted-foreground">
        No sinks configured.
      </p>
    );
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs">
        <thead className="text-muted-foreground">
          <tr>
            <th className="px-2 py-1 text-left">Sink</th>
            {windows.map((w) => (
              <th key={w.label} className="px-2 py-1 text-left">
                {w.label}
              </th>
            ))}
            <th className="px-2 py-1 text-right">Configured</th>
          </tr>
        </thead>
        <tbody>
          {sinks.map((s) => (
            <tr key={s.sink_id} className="border-t border-border/30">
              <td className="px-2 py-1">
                <code className="font-mono">{s.sink_id}</code>
              </td>
              {windows.map((w) => {
                const c = s.counts[w.label] ?? {
                  sent: 0,
                  failed: 0,
                  dead: 0,
                  suppressed: 0,
                  pending: 0,
                };
                return (
                  <td key={w.label} className="px-2 py-1">
                    <div className="flex flex-wrap gap-1">
                      <CountChip n={c.sent} label="ok" tone="success" />
                      <CountChip
                        n={c.failed}
                        label="fail"
                        tone="destructive"
                      />
                      <CountChip n={c.dead} label="dead" tone="warning" />
                      <CountChip
                        n={c.suppressed}
                        label="supp"
                        tone="secondary"
                      />
                      <CountChip
                        n={c.pending}
                        label="pending"
                        tone="secondary"
                      />
                    </div>
                  </td>
                );
              })}
              <td className="px-2 py-1 text-right">
                <Badge variant={s.configured ? "success" : "secondary"}>
                  {s.configured ? "yes" : "no"}
                </Badge>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function CountChip({
  n,
  label,
  tone,
}: {
  n: number;
  label: string;
  tone: "success" | "destructive" | "warning" | "secondary";
}) {
  if (n === 0) {
    return (
      <span className="text-[10px] text-muted-foreground">
        {label} 0
      </span>
    );
  }
  return (
    <Badge variant={tone} className="text-[10px]">
      {label} {n}
    </Badge>
  );
}
