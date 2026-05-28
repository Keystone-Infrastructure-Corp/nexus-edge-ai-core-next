// /system — full host metrics breakdown.

import { useQuery } from "@tanstack/react-query";
import {
  Cpu,
  Gauge,
  HardDrive,
  MemoryStick,
  Server,
  Terminal,
} from "lucide-react";

import { getSystemMetrics } from "@/api/system";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
import { Skeleton } from "@/components/ui/skeleton";
import { formatBytes, formatDuration } from "@/lib/format";
import { PageHeader } from "@/pages/placeholder";

export function SystemPage() {
  const q = useQuery({
    queryKey: ["system", "metrics", "full"],
    queryFn: getSystemMetrics,
    refetchInterval: 2_000,
  });

  if (q.isLoading) {
    return (
      <div className="flex flex-col gap-6">
        <PageHeader title="System" description="Host telemetry for this appliance." />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  if (q.isError || !q.data) {
    return (
      <div className="flex flex-col gap-6">
        <PageHeader title="System" description="Host telemetry for this appliance." />
        <Card>
          <CardContent className="py-6 text-sm text-muted-foreground">
            Failed to load metrics. {q.error instanceof Error ? q.error.message : ""}
          </CardContent>
        </Card>
      </div>
    );
  }

  const m = q.data;
  const memPct = (m.memory.used_bytes / Math.max(1, m.memory.total_bytes)) * 100;

  return (
    <div className="flex flex-col gap-6">
      <PageHeader
        title="System"
        description="Live host metrics — refreshed every 2 seconds."
      />

      {/* Host info ---------------------------------------------------- */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="flex items-center gap-2 text-base">
            <Server className="h-4 w-4" />
            Host
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-2 lg:grid-cols-3">
            <Field label="Hostname" value={m.host.hostname ?? "—"} />
            <Field label="OS" value={`${m.host.os_name ?? "?"} ${m.host.os_version ?? ""}`.trim()} />
            <Field label="Kernel" value={m.host.kernel_version ?? "—"} />
            <Field label="Host uptime" value={formatDuration(m.host.uptime_secs)} />
            <Field label="Engine uptime" value={formatDuration(m.uptime_secs)} />
            <Field label="Snapshot" value={new Date(m.captured_at).toLocaleTimeString()} />
          </div>
        </CardContent>
      </Card>

      {/* CPU + Memory ------------------------------------------------- */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="flex items-center gap-2 text-base">
              <Cpu className="h-4 w-4" />
              CPU
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="flex items-baseline justify-between">
              <span className="text-3xl font-semibold tabular-nums">
                {m.cpu.usage_pct.toFixed(0)}%
              </span>
              <span className="text-xs text-muted-foreground">
                {m.cpu.count} logical cores · {m.cpu.frequency_mhz} MHz
              </span>
            </div>
            <Progress value={m.cpu.usage_pct} className="mt-2" />
            <div className="mt-4 grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-3">
              <Field
                label="Load 1m"
                value={m.cpu.load_avg_1m !== null ? m.cpu.load_avg_1m.toFixed(2) : "—"}
              />
              <Field
                label="Load 5m"
                value={m.cpu.load_avg_5m !== null ? m.cpu.load_avg_5m.toFixed(2) : "—"}
              />
              <Field
                label="Load 15m"
                value={m.cpu.load_avg_15m !== null ? m.cpu.load_avg_15m.toFixed(2) : "—"}
              />
            </div>
            {m.cpu.per_core_pct.length > 1 && (
              <div className="mt-4 grid grid-cols-4 gap-2 sm:grid-cols-6 lg:grid-cols-8">
                {m.cpu.per_core_pct.map((v, i) => (
                  <div key={i} className="space-y-1">
                    <div className="text-[10px] font-mono text-muted-foreground">
                      c{i}
                    </div>
                    <Progress value={v} />
                  </div>
                ))}
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="flex items-center gap-2 text-base">
              <MemoryStick className="h-4 w-4" />
              Memory
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="flex items-baseline justify-between">
              <span className="text-3xl font-semibold tabular-nums">
                {memPct.toFixed(0)}%
              </span>
              <span className="text-xs text-muted-foreground">
                {formatBytes(m.memory.used_bytes)} / {formatBytes(m.memory.total_bytes)}
              </span>
            </div>
            <Progress value={memPct} className="mt-2" />
            <div className="mt-4 grid grid-cols-2 gap-x-6 gap-y-2 text-sm">
              <Field label="Available" value={formatBytes(m.memory.available_bytes)} />
              <Field
                label="Swap"
                value={
                  m.memory.swap_total_bytes > 0
                    ? `${formatBytes(m.memory.swap_used_bytes)} / ${formatBytes(m.memory.swap_total_bytes)}`
                    : "—"
                }
              />
            </div>
          </CardContent>
        </Card>
      </div>

      {/* GPU ---------------------------------------------------------- */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="flex items-center gap-2 text-base">
            <Gauge className="h-4 w-4" />
            GPU
          </CardTitle>
        </CardHeader>
        <CardContent>
          {m.gpu ? (
            <div className="space-y-3">
              <div className="flex flex-wrap items-baseline gap-3">
                <span className="text-lg font-semibold">{m.gpu.name}</span>
                <Badge variant="outline" className="uppercase">
                  {m.gpu.kind}
                </Badge>
              </div>
              {m.gpu.utilization_pct !== null &&
              m.gpu.utilization_pct !== undefined ? (
                <div>
                  <div className="flex items-baseline justify-between">
                    <span className="text-3xl font-semibold tabular-nums">
                      {m.gpu.utilization_pct.toFixed(0)}%
                    </span>
                    <span className="text-xs text-muted-foreground">
                      GPU utilization
                    </span>
                  </div>
                  <Progress value={m.gpu.utilization_pct} className="mt-2" />
                </div>
              ) : null}
              <div className="grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-3">
                <Field
                  label="VRAM used"
                  value={
                    m.gpu.mem_used_bytes !== null &&
                    m.gpu.mem_used_bytes !== undefined &&
                    m.gpu.mem_total_bytes !== null &&
                    m.gpu.mem_total_bytes !== undefined
                      ? `${formatBytes(m.gpu.mem_used_bytes)} / ${formatBytes(m.gpu.mem_total_bytes)}`
                      : m.gpu.mem_total_bytes !== null &&
                          m.gpu.mem_total_bytes !== undefined
                        ? formatBytes(m.gpu.mem_total_bytes)
                        : "—"
                  }
                />
                <Field
                  label="Temperature"
                  value={
                    m.gpu.temp_c !== null && m.gpu.temp_c !== undefined
                      ? `${m.gpu.temp_c.toFixed(0)}°C`
                      : "—"
                  }
                />
                <Field
                  label="Utilization"
                  value={
                    m.gpu.utilization_pct !== null &&
                    m.gpu.utilization_pct !== undefined
                      ? `${m.gpu.utilization_pct.toFixed(0)}%`
                      : "—"
                  }
                />
              </div>
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">
              No GPU detected. NVIDIA cards require the proprietary driver;
              Intel iGPUs require the i915 kernel module; macOS reports
              integrated graphics via system_profiler.
            </p>
          )}
        </CardContent>
      </Card>

      {/* Disks -------------------------------------------------------- */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="flex items-center gap-2 text-base">
            <HardDrive className="h-4 w-4" />
            Disks
          </CardTitle>
        </CardHeader>
        <CardContent>
          {m.disks.length === 0 ? (
            <div className="text-sm text-muted-foreground">No disks reported.</div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="text-left text-xs uppercase text-muted-foreground">
                  <tr>
                    <th className="px-2 py-2">Mount</th>
                    <th className="px-2 py-2">FS</th>
                    <th className="px-2 py-2">Total</th>
                    <th className="px-2 py-2">Free</th>
                    <th className="px-2 py-2">Used</th>
                    <th className="px-2 py-2"></th>
                  </tr>
                </thead>
                <tbody>
                  {m.disks.map((d, i) => {
                    const used = d.total_bytes - d.available_bytes;
                    const pct = d.total_bytes > 0 ? (used / d.total_bytes) * 100 : 0;
                    return (
                      <tr key={i} className="border-t border-border/40">
                        <td className="px-2 py-2 font-mono">{d.mount_point}</td>
                        <td className="px-2 py-2">
                          {d.file_system}
                          {d.is_removable && (
                            <Badge variant="outline" className="ml-2 text-[10px]">
                              removable
                            </Badge>
                          )}
                        </td>
                        <td className="px-2 py-2 tabular-nums">
                          {formatBytes(d.total_bytes)}
                        </td>
                        <td className="px-2 py-2 tabular-nums">
                          {formatBytes(d.available_bytes)}
                        </td>
                        <td className="px-2 py-2 tabular-nums">{pct.toFixed(0)}%</td>
                        <td className="w-32 px-2 py-2">
                          <Progress
                            value={pct}
                            fillClassName={
                              pct >= 90
                                ? "bg-destructive"
                                : pct >= 75
                                  ? "bg-warning"
                                  : "bg-primary"
                            }
                          />
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {/* Process ------------------------------------------------------ */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="flex items-center gap-2 text-base">
            <Terminal className="h-4 w-4" />
            Engine process
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-4">
            <Field label="PID" value={String(m.process.pid)} />
            <Field label="CPU" value={`${m.process.cpu_pct.toFixed(0)}%`} />
            <Field label="RSS" value={formatBytes(m.process.rss_bytes)} />
            <Field label="VSZ" value={formatBytes(m.process.virtual_bytes)} />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <span className="font-mono">{value}</span>
    </div>
  );
}
