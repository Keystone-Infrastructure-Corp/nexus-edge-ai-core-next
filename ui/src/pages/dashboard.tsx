// Dashboard — Phase 2 of the UI rewrite.
//
// 4-up KPI row + cameras-at-a-glance + alert ticker + system
// sparklines. Polls:
//   * /v1/system/metrics every 2s
//   * /events?limit=100 once per minute (for "events last hour")
//   * /cameras once per minute (list rarely changes)
//   * /api/v1/stream/events SSE for the live ticker
//   * /cameras/:id/frames/latest as JPEG every 2s (cache-busted)
//
// Cards that need fast feedback (CPU, RAM) poll once a second via
// the metrics query's refetchInterval.

import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Activity,
  AlertTriangle,
  BrainCircuit,
  Camera,
  Cpu,
  Gauge,
  HardDrive,
  HeartPulse,
  MemoryStick,
  Radio,
  Server,
} from "lucide-react";

import {
  getBackends,
  getCameraStats,
  getHealth,
  getSystemMetrics,
  latestFrameJpegUrl,
  listCameras,
  listEvents,
} from "@/api/system";
import { getModelPromptsCatalog } from "@/api/config";
import type { AlertEvent, CameraConfig } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Sparkline } from "@/components/ui/sparkline";
import { useSSE } from "@/hooks/useSSE";
import { engineHealth, issueCodes } from "@/lib/engineHealth";
import { ageMs, formatAgo, formatBytes, formatDuration } from "@/lib/format";
import { cn } from "@/lib/utils";
import { PageHeader } from "@/pages/placeholder";

const STALE_FRAME_MS = 5_000;
const SPARK_WINDOW = 60;
// The events query reads the newest this many, so an hour that holds all of
// them may hold more than were read.
const RECENT_EVENTS = 100;

export function DashboardPage() {
  const metricsQuery = useQuery({
    queryKey: ["system", "metrics"],
    queryFn: getSystemMetrics,
    refetchInterval: 2_000,
    staleTime: 0,
  });
  const camerasQuery = useQuery({
    queryKey: ["cameras"],
    queryFn: listCameras,
    refetchInterval: 60_000,
  });
  const eventsQuery = useQuery({
    queryKey: ["events", "recent"],
    queryFn: () => listEvents(RECENT_EVENTS),
    refetchInterval: 60_000,
  });
  const healthQuery = useQuery({
    queryKey: ["health"],
    queryFn: getHealth,
    refetchInterval: 10_000,
  });
  const backendsQuery = useQuery({
    queryKey: ["backends"],
    queryFn: getBackends,
    refetchInterval: 10_000,
  });
  // Default model kind + per-kind metadata (open-vocab flag, prompt
  // count, whether the engine actually loaded a layer for it). Used
  // by the Inference card so operators can see "what's running" at
  // a glance without opening Cameras > Model override.
  const promptsQuery = useQuery({
    queryKey: ["model-prompts"],
    queryFn: getModelPromptsCatalog,
    staleTime: 60_000,
  });

  // Live alert ticker from SSE.
  const sse = useSSE<AlertEvent>({ url: "/api/v1/stream/events", maxBuffer: 20 });

  // Rolling buffer of CPU/RAM percentages for sparklines.
  const cpuBuf = useRollingBuffer(metricsQuery.data?.cpu.usage_pct ?? null);
  const ramBuf = useRollingBuffer(
    metricsQuery.data
      ? (metricsQuery.data.memory.used_bytes /
          Math.max(1, metricsQuery.data.memory.total_bytes)) *
          100
      : null,
  );
  // GPU utilization (Intel i915/xe PMU, NVIDIA NVML, or Apple
  // Silicon). `null` until the engine reports a non-null reading,
  // which keeps the sparkline empty on hosts with no GPU or while
  // the PMU baseline is warming up after engine restart.
  const gpuBuf = useRollingBuffer(
    metricsQuery.data?.gpu?.utilization_pct ?? null,
  );
  // NPU utilization (Intel AI Boost via the intel_vpu sysfs path).
  // `null` on hosts without an NPU or while its first-sample
  // baseline warms up, which keeps the sparkline empty rather than
  // pinned at zero.
  const npuBuf = useRollingBuffer(
    metricsQuery.data?.npu?.utilization_pct ?? null,
  );
  // Busiest GPU engine class from the Intel i915/xe per-engine
  // breakdown. On a headless video appliance the headline GPU
  // aggregate can read near-zero while video-decode / video-enhance
  // are busy, so surface the hottest engine next to the aggregate.
  // `null` on backends without a per-engine breakdown (NVIDIA /
  // AMD / Apple).
  const busiestGpuEngine = useMemo(() => {
    const engines = metricsQuery.data?.gpu?.engines ?? [];
    if (engines.length === 0) return null;
    return engines.reduce((a, b) =>
      b.utilization_pct > a.utilization_pct ? b : a,
    );
  }, [metricsQuery.data]);

  // Readings are `null` / `undefined` until their query has read them, so a
  // tile shows `notRead` in their place rather than a default such as 0.
  const eventsLastHour = useMemo(() => {
    if (eventsQuery.data === undefined) return null;
    const cutoff = Date.now() - 60 * 60 * 1000;
    return eventsQuery.data.filter((e) => {
      const t = new Date(e.captured_at).getTime();
      return Number.isFinite(t) && t >= cutoff;
    }).length;
  }, [eventsQuery.data]);

  const memPct = metricsQuery.data
    ? (metricsQuery.data.memory.used_bytes /
        Math.max(1, metricsQuery.data.memory.total_bytes)) *
      100
    : null;
  const diskPct = useMemo(() => {
    // Pick the disk with the lowest free %. `null` when no disk reported a
    // size, which is no reading, not 0%.
    let worst: number | null = null;
    for (const d of metricsQuery.data?.disks ?? []) {
      if (d.total_bytes === 0) continue;
      const used = ((d.total_bytes - d.available_bytes) / d.total_bytes) * 100;
      if (worst === null || used > worst) worst = used;
    }
    return worst;
  }, [metricsQuery.data]);

  const cameras = camerasQuery.data;
  const engine = engineHealth(healthQuery);

  // System sparkline cards: CPU + Memory + Inference are always
  // shown; GPU and NPU are added when the host reports them. Pick a
  // column count that fits the present cards without leaving a gap.
  const sparkCount =
    3 +
    (metricsQuery.data?.gpu ? 1 : 0) +
    (metricsQuery.data?.npu ? 1 : 0);
  const sparkColumns =
    sparkCount >= 5
      ? "sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5"
      : sparkCount === 4
        ? "sm:grid-cols-2 lg:grid-cols-4"
        : "lg:grid-cols-3";

  return (
    <div className="flex flex-col gap-6">
      <PageHeader
        title="Dashboard"
        description="System health, live camera feeds, and recent alerts at a glance."
      />

      {/* KPI row ------------------------------------------------------ */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <KpiCard
          icon={<Camera className="h-4 w-4" />}
          label="Cameras"
          value={cameras ? `${cameras.length}` : notRead(camerasQuery)}
          hint={
            cameras
              ? `${cameras.length} configured`
              : camerasQuery.isError
                ? "Failed to load cameras"
                : "Loading…"
          }
        />
        <KpiCard
          icon={<Activity className="h-4 w-4" />}
          label="Alerts (last hour)"
          value={
            eventsLastHour === null
              ? notRead(eventsQuery)
              : eventsLastHour === RECENT_EVENTS
                ? `${eventsLastHour}+`
                : `${eventsLastHour}`
          }
          hint={
            eventsLastHour === null && eventsQuery.isError
              ? "Failed to load events"
              : `SSE: ${sse.status}`
          }
          accent={eventsLastHour !== null && eventsLastHour > 0 ? "warning" : "default"}
        />
        <KpiCard
          icon={<HardDrive className="h-4 w-4" />}
          label="Disk used (worst)"
          value={
            !metricsQuery.data
              ? notRead(metricsQuery)
              : diskPct === null
                ? "—"
                : `${diskPct.toFixed(0)}%`
          }
          hint={
            metricsQuery.data
              ? `${metricsQuery.data.disks.length} disks`
              : metricsQuery.isError
                ? "Failed to load metrics"
                : "—"
          }
          accent={
            diskPct === null
              ? "default"
              : diskPct >= 85
                ? "destructive"
                : diskPct >= 70
                  ? "warning"
                  : "default"
          }
        />
        <KpiCard
          icon={<HeartPulse className="h-4 w-4" />}
          label="Engine"
          value={
            engine.verdict === "ok"
              ? "OK"
              : engine.verdict === "degraded"
                ? "DEGRADED"
                : engine.verdict === "unreachable"
                  ? "ERROR"
                  : "…"
          }
          hint={
            engine.verdict === "degraded"
              ? `${issueCodes(engine.read.issues)} · v${engine.read.version}`
              : engine.verdict === "ok"
                ? `v${engine.read.version}`
                : ""
          }
          accent={
            engine.verdict === "ok"
              ? "success"
              : engine.verdict === "degraded"
                ? "warning"
                : engine.verdict === "unreachable"
                  ? "destructive"
                  : "default"
          }
        />
      </div>

      {engine.verdict === "degraded" ? (
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base text-warning">
              <AlertTriangle className="h-4 w-4" />
              Engine degraded
            </CardTitle>
          </CardHeader>
          <CardContent>
            <ul className="flex flex-col gap-2">
              {engine.read.issues.map((issue, i) => (
                <li key={`${issue.component}:${issue.code}:${i}`} className="text-sm">
                  <div className="flex items-baseline gap-2">
                    <span className="font-medium">{issue.component}</span>
                    {/* Verbatim: a code this UI has no copy for still shows. */}
                    <span className="font-mono text-xs text-muted-foreground">
                      {issue.code}
                    </span>
                  </div>
                  {issue.detail ? (
                    <div className="mt-0.5 text-xs text-muted-foreground">
                      {issue.detail}
                    </div>
                  ) : null}
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      ) : null}

      <div className="grid grid-cols-1 gap-6 lg:grid-cols-3">
        {/* Cameras at a glance -------------------------------------- */}
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Camera className="h-4 w-4" />
              Cameras
            </CardTitle>
          </CardHeader>
          <CardContent>
            {!cameras && camerasQuery.isError ? (
              <EmptyState title="Failed to load cameras" detail="" />
            ) : !cameras ? (
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">
                {[0, 1, 2].map((i) => (
                  <Skeleton key={i} className="aspect-video w-full" />
                ))}
              </div>
            ) : cameras.length === 0 ? (
              <EmptyState
                title="No cameras configured"
                detail="Add a camera from the Cameras page to start seeing live frames."
              />
            ) : (
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">
                {cameras.map((c) => (
                  <CameraTile key={c.id} camera={c} />
                ))}
              </div>
            )}
          </CardContent>
        </Card>

        {/* Live alert ticker ---------------------------------------- */}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Radio className="h-4 w-4" />
              Live alerts
              <Badge
                variant={
                  sse.status === "open"
                    ? "success"
                    : sse.status === "connecting"
                      ? "secondary"
                      : "destructive"
                }
                className="ml-auto text-[10px]"
              >
                {sse.status}
              </Badge>
            </CardTitle>
          </CardHeader>
          <CardContent>
            {/* An empty buffer says nothing arrived, not that nothing fired:
                the stream does not replay what fires while it is down. */}
            {sse.events.length === 0 && sse.status !== "open" ? (
              <EmptyState
                title="Not connected"
                detail="Alerts that fire before the stream connects do not show here."
              />
            ) : sse.events.length === 0 ? (
              <EmptyState
                title="Quiet on the wire"
                detail="No alerts have arrived since you opened this page."
              />
            ) : (
              <ul className="flex flex-col gap-2">
                {sse.events.slice(0, 10).map((e) => (
                  <li
                    key={e.event_id}
                    className="flex items-start gap-3 rounded-md border border-border/60 bg-muted/20 px-3 py-2 text-sm"
                  >
                    <SeverityDot severity={e.severity} />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="font-medium">{e.label}</span>
                        <span className="text-xs text-muted-foreground">
                          {String(e.camera_id)}
                        </span>
                      </div>
                      <div className="text-xs text-muted-foreground">
                        {formatAgo(e.captured_at)}
                      </div>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>
      </div>

      {/* System sparklines ---------------------------------------- */}
      <div className={cn("grid grid-cols-1 gap-6", sparkColumns)}>
        <SystemSparkCard
          icon={<Cpu className="h-4 w-4" />}
          title="CPU"
          values={cpuBuf}
          max={100}
          primary={
            metricsQuery.data
              ? `${metricsQuery.data.cpu.usage_pct.toFixed(0)}%`
              : notRead(metricsQuery)
          }
          secondary={
            metricsQuery.data
              ? `${metricsQuery.data.cpu.count} cores · ${metricsQuery.data.cpu.frequency_mhz} MHz`
              : ""
          }
        />
        <SystemSparkCard
          icon={<MemoryStick className="h-4 w-4" />}
          title="Memory"
          values={ramBuf}
          max={100}
          primary={memPct === null ? notRead(metricsQuery) : `${memPct.toFixed(0)}%`}
          secondary={
            metricsQuery.data
              ? `${formatBytes(metricsQuery.data.memory.used_bytes)} / ${formatBytes(metricsQuery.data.memory.total_bytes)}`
              : ""
          }
        />
        {metricsQuery.data?.gpu && (
          <SystemSparkCard
            icon={<Gauge className="h-4 w-4" />}
            title="GPU"
            values={gpuBuf}
            max={100}
            primary={
              metricsQuery.data.gpu.utilization_pct !== null &&
              metricsQuery.data.gpu.utilization_pct !== undefined
                ? `${metricsQuery.data.gpu.utilization_pct.toFixed(0)}%`
                : "—"
            }
            secondary={
              busiestGpuEngine
                ? `${engineShort(busiestGpuEngine.class)} ${busiestGpuEngine.utilization_pct.toFixed(0)}%`
                : metricsQuery.data.gpu.name
            }
          />
        )}
        {metricsQuery.data?.npu && (
          <SystemSparkCard
            icon={<BrainCircuit className="h-4 w-4" />}
            title="NPU"
            values={npuBuf}
            max={100}
            primary={
              metricsQuery.data.npu.utilization_pct !== null &&
              metricsQuery.data.npu.utilization_pct !== undefined
                ? `${metricsQuery.data.npu.utilization_pct.toFixed(0)}%`
                : "\u2014"
            }
            secondary={metricsQuery.data.npu.name}
          />
        )}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Server className="h-4 w-4" />
              Inference
            </CardTitle>
          </CardHeader>
          <CardContent>
            {backendsQuery.isPending ? (
              <Skeleton className="h-16 w-full" />
            ) : backendsQuery.data ? (
              <div className="space-y-2 text-sm">
                {/* Default model kind — the kind every camera without
                    a model_override runs against. `loaded=false` means
                    the engine config picked a kind the router didn't
                    build a layer for at boot (cameras fall back to a
                    stub); surface it loudly. */}
                {promptsQuery.data && (
                  <div className="flex items-center justify-between gap-2">
                    <span className="text-muted-foreground">Model</span>
                    <div className="flex items-center gap-1">
                      <Badge
                        variant={
                          promptsQuery.data.by_kind[
                            promptsQuery.data.default_kind
                          ]?.loaded === false
                            ? "destructive"
                            : "outline"
                        }
                        className="font-mono"
                      >
                        {promptsQuery.data.default_kind}
                      </Badge>
                      {(() => {
                        const k =
                          promptsQuery.data.by_kind[
                            promptsQuery.data.default_kind
                          ];
                        if (!k) return null;
                        return (
                          <Badge
                            variant="outline"
                            className="text-[10px] uppercase tracking-wide"
                            title={
                              k.open_vocab
                                ? "Open-vocab — accepts arbitrary prompts"
                                : "Fixed-vocab detector"
                            }
                          >
                            {k.open_vocab ? "open" : "fixed"}
                          </Badge>
                        );
                      })()}
                    </div>
                  </div>
                )}
                {/* Prompt count for the default kind. Empty for mock /
                    yoloe_visual / classifier_ensemble — hide the row
                    instead of showing "0". */}
                {promptsQuery.data &&
                  (promptsQuery.data.by_kind[promptsQuery.data.default_kind]
                    ?.prompts.length ?? 0) > 0 && (
                    <div className="flex items-center justify-between">
                      <span className="text-muted-foreground">Prompts</span>
                      <span className="font-mono">
                        {
                          promptsQuery.data.by_kind[
                            promptsQuery.data.default_kind
                          ]?.prompts.length
                        }
                      </span>
                    </div>
                  )}
                <div className="flex items-center justify-between">
                  <span className="text-muted-foreground">Mode</span>
                  <Badge variant="outline">{backendsQuery.data.mode}</Badge>
                </div>
                <div className="flex items-center justify-between">
                  <span className="text-muted-foreground">Slots</span>
                  <span className="font-mono">
                    {backendsQuery.data.slots.length}
                  </span>
                </div>
                {metricsQuery.data && (
                  <div className="flex items-center justify-between">
                    <span className="text-muted-foreground">Uptime</span>
                    <span className="font-mono">
                      {formatDuration(metricsQuery.data.uptime_secs)}
                    </span>
                  </div>
                )}
                {/* "Restart engine to activate" hint when the operator's
                    configured default model kind isn't loaded — mirrors
                    the same warning the camera form shows for
                    model_override of an unloaded kind. */}
                {promptsQuery.data &&
                  promptsQuery.data.by_kind[promptsQuery.data.default_kind]
                    ?.loaded === false && (
                    <p className="text-[11px] text-destructive">
                      Default model kind not loaded — restart engine to
                      activate.
                    </p>
                  )}
              </div>
            ) : (
              <EmptyState title="Backends unavailable" detail="" />
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// KPI card.
// ---------------------------------------------------------------------------

interface KpiCardProps {
  icon: React.ReactNode;
  label: string;
  value: string;
  hint?: string;
  accent?: "default" | "success" | "warning" | "destructive";
}

function KpiCard({ icon, label, value, hint, accent = "default" }: KpiCardProps) {
  const accentClass = {
    default: "",
    success: "text-success",
    warning: "text-warning",
    destructive: "text-destructive",
  }[accent];
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
          {icon}
          {label}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className={cn("text-2xl font-semibold tabular-nums", accentClass)}>
          {value}
        </div>
        {hint && (
          <div className="mt-1 text-xs text-muted-foreground">{hint}</div>
        )}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// System spark card.
// ---------------------------------------------------------------------------

function SystemSparkCard({
  icon,
  title,
  values,
  max,
  primary,
  secondary,
}: {
  icon: React.ReactNode;
  title: string;
  values: number[];
  max?: number;
  primary: string;
  secondary?: string;
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          {icon}
          {title}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="flex items-baseline justify-between gap-2">
          <span className="text-2xl font-semibold tabular-nums">{primary}</span>
          {secondary && (
            <span className="text-xs text-muted-foreground">{secondary}</span>
          )}
        </div>
        <div className="mt-2 -mx-1">
          <Sparkline values={values} max={max} height={56} />
        </div>
      </CardContent>
    </Card>
  );
}

// Compact GPU engine-class label for the sparkline secondary line
// (the System page uses the long form). Falls back to the raw class
// for any class the engine adds later.
function engineShort(cls: string): string {
  switch (cls) {
    case "video-decode":
      return "decode";
    case "video-enhance":
      return "enhance";
    case "render":
      return "render";
    case "copy":
      return "copy";
    case "compute":
      return "compute";
    default:
      return cls;
  }
}

// ---------------------------------------------------------------------------
// Camera tile.
// ---------------------------------------------------------------------------

function CameraTile({ camera }: { camera: CameraConfig }) {
  // Cache-bust the JPEG every N seconds so the browser actually refetches.
  const [bust, setBust] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setBust(Date.now()), 2_000);
    return () => window.clearInterval(t);
  }, []);

  const metaQuery = useQuery({
    queryKey: ["camera", camera.id, "meta"],
    queryFn: async () => {
      const r = await fetch(
        `/api/v1/cameras/${encodeURIComponent(camera.id)}/frames/latest.json`,
      );
      if (!r.ok) return null;
      return (await r.json()) as { captured_at?: string };
    },
    refetchInterval: 2_000,
    staleTime: 0,
    retry: 0,
  });

  // M-Admin Phase 0 closeout — live fps + dropped-frame counter
  // from the per-camera supervisor. Refetches on the same cadence
  // as the frame JPEG so the badge stays in sync visually.
  const statsQuery = useQuery({
    queryKey: ["camera", camera.id, "stats"],
    queryFn: () => getCameraStats(camera.id),
    refetchInterval: 2_000,
    staleTime: 0,
    retry: 0,
  });

  // Only an answer can say the camera stalled; a metadata request that has
  // not answered says nothing about it.
  const stale =
    metaQuery.data !== undefined &&
    ageMs(metaQuery.data?.captured_at) > STALE_FRAME_MS;
  const src = `${latestFrameJpegUrl(String(camera.id))}?t=${bust}`;
  const fps = statsQuery.data?.fps_ema ?? 0;
  const dropped = statsQuery.data?.frames_dropped ?? 0;
  // The gate drops on purpose and is the expected steady state; frames the
  // analysis loop never saw are the backpressure signal worth surfacing
  // separately, and used to be counted nowhere at all (BUG-218).
  const missed = statsQuery.data?.frames_backpressure_dropped ?? 0;
  // Engine-side detector frame dims (after videoscale). The playback
  // <img> is the same JPEG the engine just emitted, so this IS the
  // resolution the viewer is showing — not the camera's native
  // capture resolution.
  const srcW = statsQuery.data?.source_width ?? 0;
  const srcH = statsQuery.data?.source_height ?? 0;
  const hasDims = srcW > 0 && srcH > 0;

  return (
    <div
      className={cn(
        "group relative overflow-hidden rounded-md border bg-muted/20",
        stale ? "border-destructive/70" : "border-border/60",
      )}
    >
      <div className="relative aspect-video w-full">
        <img
          src={src}
          alt={camera.name ?? camera.id}
          className="absolute inset-0 h-full w-full object-cover"
          onError={(e) => {
            (e.currentTarget as HTMLImageElement).style.opacity = "0";
          }}
        />
        {stale && (
          <div className="absolute right-2 top-2">
            <Badge variant="destructive" className="text-[10px]">
              STALLED
            </Badge>
          </div>
        )}
        {!stale && fps > 0 && (
          <div className="absolute right-2 top-2 flex flex-col items-end gap-1">
            <Badge variant="outline" className="bg-card/90 text-[10px] font-mono">
              {fps.toFixed(1)} fps
            </Badge>
            {hasDims && (
              <Badge variant="outline" className="bg-card/90 text-[10px] font-mono">
                {srcW}×{srcH}
              </Badge>
            )}
          </div>
        )}
        {stale && hasDims && (
          <div className="absolute left-2 top-2">
            <Badge variant="outline" className="bg-card/90 text-[10px] font-mono">
              {srcW}×{srcH}
            </Badge>
          </div>
        )}
      </div>
      <div className="flex items-center justify-between gap-2 border-t border-border/60 bg-card/80 px-3 py-2 text-xs">
        <span className="truncate font-medium">
          {camera.name ?? camera.id}
        </span>
        <span className="font-mono text-muted-foreground">
          {formatAgo(metaQuery.data?.captured_at)}
          {dropped > 0 ? ` · ${dropped} gated` : ""}
          {missed > 0 ? ` · ${missed} missed` : ""}
        </span>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Misc small bits.
// ---------------------------------------------------------------------------

function SeverityDot({ severity }: { severity: string | null | undefined }) {
  const cls =
    severity === "critical"
      ? "bg-destructive"
      : severity === "high"
        ? "bg-destructive/70"
        : severity === "medium"
          ? "bg-warning"
          : "bg-primary";
  return <span className={cn("mt-1 h-2 w-2 flex-none rounded-full", cls)} />;
}

/** What a tile shows for a reading it does not have: "…" while its query is
 * still asking, "—" once it has failed. */
function notRead(q: { isError: boolean }): string {
  return q.isError ? "—" : "…";
}

function EmptyState({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-1 rounded-md border border-dashed border-border/60 bg-muted/10 px-4 py-6 text-center">
      <AlertTriangle className="h-4 w-4 text-muted-foreground" />
      <div className="text-sm font-medium">{title}</div>
      {detail && (
        <div className="text-xs text-muted-foreground">{detail}</div>
      )}
    </div>
  );
}

// Internal hook: append a value to a fixed-size sliding buffer when
// the source changes.
function useRollingBuffer(latest: number | null): number[] {
  const ref = useRef<number[]>([]);
  const [snapshot, setSnapshot] = useState<number[]>([]);
  useEffect(() => {
    if (latest === null || !Number.isFinite(latest)) return;
    const next = [...ref.current, latest];
    while (next.length > SPARK_WINDOW) next.shift();
    ref.current = next;
    setSnapshot(next);
  }, [latest]);
  return snapshot;
}
