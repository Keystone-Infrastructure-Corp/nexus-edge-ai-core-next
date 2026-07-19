// Events page: paginated list + live SSE toggle + detail drawer with clip
// player and per-sink delivery status.
//
// The drawer is a hand-rolled fixed-right panel (no Radix install yet).
// Selection is mirrored in `?focus=<event_id>` for shareability via the
// router's search params, though for Phase 3 we keep state local.

import { useQuery } from "@tanstack/react-query";
import {
  AlertTriangle,
  Camera as CameraIcon,
  CheckCircle2,
  Clock,
  Filter,
  Radio,
  X,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import {
  getEventClipId,
  getEventDelivery,
  listEvents,
  type EventListItem,
} from "@/api/system";
import type { AlertEvent, OutboxRow, OutboxStatus, Severity } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ClipPlayer } from "@/components/clip-player";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { useSSE } from "@/hooks/useSSE";
import { formatAgo } from "@/lib/format";

const SEVERITY_OPTIONS: ReadonlyArray<Severity | "all"> = [
  "all",
  "low",
  "medium",
  "high",
  "critical",
];

export function EventsPage() {
  const [liveMode, setLiveMode] = useState(true);
  const [severity, setSeverity] = useState<Severity | "all">("all");
  const [cameraFilter, setCameraFilter] = useState("");
  const [disposition, setDisposition] = useState<"all" | "alerts" | "audit">(
    "all",
  );
  const [focusId, setFocusId] = useState<string | null>(null);

  const listQuery = useQuery({
    queryKey: ["events", "list", 200],
    queryFn: () => listEvents(200),
    refetchInterval: liveMode ? false : 30_000,
  });

  const sse = useSSE<AlertEvent>({
    url: "/api/v1/stream/events",
    maxBuffer: 100,
    enabled: liveMode,
  });

  // Merge SSE events on top of the polled list, dedup by event_id, sort
  // by captured_at desc.
  const events = useMemo<EventListItem[]>(() => {
    const seen = new Map<string, EventListItem>();
    if (liveMode) {
      // Live SSE payloads are AlertEvents without the audit `alerted`
      // flag; default them to alert (true). The 30s poll reconciles the
      // real disposition for off-schedule (audit-only) matches.
      for (const e of sse.events) seen.set(e.event_id, { ...e, alerted: true });
    }
    for (const e of listQuery.data ?? []) {
      if (!seen.has(e.event_id)) seen.set(e.event_id, e);
    }
    const arr = Array.from(seen.values());
    arr.sort((a, b) => (a.captured_at > b.captured_at ? -1 : 1));
    return arr;
  }, [listQuery.data, sse.events, liveMode]);

  const filtered = useMemo(() => {
    const lower = cameraFilter.trim().toLowerCase();
    return events.filter((e) => {
      if (severity !== "all" && e.severity !== severity) return false;
      if (disposition === "alerts" && !e.alerted) return false;
      if (disposition === "audit" && e.alerted) return false;
      if (lower && !String(e.camera_id).toLowerCase().includes(lower))
        return false;
      return true;
    });
  }, [events, severity, cameraFilter, disposition]);

  const focused = useMemo(
    () => events.find((e) => e.event_id === focusId) ?? null,
    [events, focusId],
  );

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Events</h1>
          <p className="text-sm text-muted-foreground">
            Full event audit with per-sink delivery status and clip
            playback. Off-schedule matches are logged as{" "}
            <span className="font-medium">Audit only</span> — no alert clip
            or notification.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Badge
            variant={
              !liveMode
                ? "outline"
                : sse.status === "open"
                  ? "success"
                  : sse.status === "connecting"
                    ? "secondary"
                    : "destructive"
            }
          >
            <Radio className="mr-1 h-3 w-3" />
            {liveMode
              ? sse.status === "open"
                ? "Live"
                : sse.status === "connecting"
                  ? "Connecting"
                  : "Live · offline"
              : "Paused"}
          </Badge>
          <Button
            variant="outline"
            onClick={() => {
              setLiveMode((v) => !v);
              if (liveMode) sse.clear();
            }}
          >
            {liveMode ? "Pause" : "Resume"}
          </Button>
        </div>
      </header>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-2 space-y-0 pb-3">
          <CardTitle className="flex items-center gap-2 text-sm font-medium">
            <Filter className="h-4 w-4" />
            Filters
          </CardTitle>
          <span className="text-xs text-muted-foreground">
            {filtered.length} of {events.length}
          </span>
        </CardHeader>
        <CardContent className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <label className="text-xs text-muted-foreground">Show</label>
            <div className="flex gap-1">
              {(
                [
                  ["all", "All events"],
                  ["alerts", "Alerts"],
                  ["audit", "Audit only"],
                ] as const
              ).map(([key, text]) => (
                <button
                  key={key}
                  type="button"
                  onClick={() => setDisposition(key)}
                  className={`rounded-md border px-2.5 py-1 text-xs transition ${
                    disposition === key
                      ? "border-primary bg-primary/10 text-primary"
                      : "border-border text-muted-foreground hover:border-border/80"
                  }`}
                >
                  {text}
                </button>
              ))}
            </div>
          </div>
          <div className="space-y-1">
            <label className="text-xs text-muted-foreground">Severity</label>
            <div className="flex gap-1">
              {SEVERITY_OPTIONS.map((s) => (
                <button
                  key={s}
                  type="button"
                  onClick={() => setSeverity(s)}
                  className={`rounded-md border px-2.5 py-1 text-xs capitalize transition ${
                    severity === s
                      ? "border-primary bg-primary/10 text-primary"
                      : "border-border text-muted-foreground hover:border-border/80"
                  }`}
                >
                  {s}
                </button>
              ))}
            </div>
          </div>
          <div className="space-y-1">
            <label
              htmlFor="camera-filter"
              className="text-xs text-muted-foreground"
            >
              Camera
            </label>
            <Input
              id="camera-filter"
              placeholder="filter by camera id"
              value={cameraFilter}
              onChange={(e) => setCameraFilter(e.target.value)}
              className="h-8 w-56"
            />
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="p-0">
          {listQuery.isLoading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2, 3, 4].map((i) => (
                <Skeleton key={i} className="h-12 w-full" />
              ))}
            </div>
          ) : filtered.length === 0 ? (
            <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
              <AlertTriangle className="h-8 w-8 opacity-50" />
              <p>No events match the current filters.</p>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="bg-muted/30 text-xs uppercase text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left">When</th>
                    <th className="px-3 py-2 text-left">Severity</th>
                    <th className="px-3 py-2 text-left">Label</th>
                    <th className="px-3 py-2 text-left">Camera</th>
                    <th className="px-3 py-2 text-left">Rule</th>
                    <th className="px-3 py-2 text-left">Artifacts</th>
                  </tr>
                </thead>
                <tbody>
                  {filtered.map((e) => (
                    <tr
                      key={e.event_id}
                      onClick={() => setFocusId(e.event_id)}
                      className={`cursor-pointer border-t border-border/40 transition hover:bg-muted/30 ${
                        focusId === e.event_id ? "bg-muted/40" : ""
                      }`}
                    >
                      <td className="px-3 py-2 text-muted-foreground">
                        <div className="flex items-center gap-1.5">
                          <Clock className="h-3 w-3" />
                          {formatAgo(e.captured_at)}
                        </div>
                      </td>
                      <td className="px-3 py-2">
                        <SeverityBadge severity={e.severity} />
                      </td>
                      <td className="px-3 py-2 font-medium">
                        <div className="flex items-center gap-2">
                          {e.label}
                          {!e.alerted ? (
                            <Badge
                              variant="outline"
                              title="Off-schedule — logged to the audit; no alert clip or notification"
                            >
                              Audit only
                            </Badge>
                          ) : null}
                        </div>
                      </td>
                      <td className="px-3 py-2 text-muted-foreground">
                        {String(e.camera_id)}
                      </td>
                      <td className="px-3 py-2 text-muted-foreground">
                        {e.rule_id}
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex gap-1">
                          {e.artifacts.clip ? (
                            <Badge variant="outline">clip</Badge>
                          ) : null}
                          {e.artifacts.snapshot ? (
                            <Badge variant="outline">snapshot</Badge>
                          ) : null}
                          {e.artifacts.cloud_receipt ? (
                            <Badge variant="success">cloud</Badge>
                          ) : null}
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {focused ? (
        <EventDetailDrawer
          event={focused}
          onClose={() => setFocusId(null)}
        />
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Detail drawer.
// ---------------------------------------------------------------------------

function EventDetailDrawer({
  event,
  onClose,
}: {
  event: AlertEvent;
  onClose: () => void;
}) {
  const deliveryQuery = useQuery({
    queryKey: ["events", event.event_id, "delivery"],
    queryFn: () => getEventDelivery(event.event_id),
    refetchInterval: 5_000,
  });

  const clipQuery = useQuery({
    queryKey: ["events", event.event_id, "clip"],
    queryFn: () => getEventClipId(event.event_id),
    retry: false,
  });

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-40 flex justify-end" role="dialog" aria-modal="true">
      <div
        className="flex-1 bg-background/60 backdrop-blur-sm"
        onClick={onClose}
        aria-hidden
      />
      <aside className="flex w-full max-w-xl flex-col overflow-y-auto border-l border-border bg-background shadow-2xl">
        <header className="flex items-start justify-between gap-2 border-b border-border px-5 py-4">
          <div className="min-w-0">
            <h2 className="truncate text-lg font-semibold">{event.label}</h2>
            <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
              <SeverityBadge severity={event.severity} />
              <span>{formatAgo(event.captured_at)}</span>
              <span>·</span>
              <span>camera {String(event.camera_id)}</span>
              <span>·</span>
              <span>rule {event.rule_id}</span>
            </div>
          </div>
          <button
            type="button"
            className="rounded-md p-1.5 hover:bg-muted"
            onClick={onClose}
            aria-label="Close"
          >
            <X className="h-4 w-4" />
          </button>
        </header>

        <div className="space-y-6 p-5">
          {clipQuery.data?.clip_id ? (
            <section>
              <h3 className="mb-2 text-sm font-semibold">Clip</h3>
              <ClipPlayer clipId={clipQuery.data.clip_id} />
            </section>
          ) : clipQuery.isLoading ? (
            <Skeleton className="aspect-video w-full" />
          ) : null}

          <section>
            <h3 className="mb-2 text-sm font-semibold">Delivery</h3>
            {deliveryQuery.isLoading ? (
              <Skeleton className="h-20 w-full" />
            ) : (deliveryQuery.data ?? []).length === 0 ? (
              <p className="text-xs text-muted-foreground">
                No outbox rows for this event.
              </p>
            ) : (
              <div className="overflow-hidden rounded-md border border-border">
                <table className="w-full text-xs">
                  <thead className="bg-muted/30 text-muted-foreground">
                    <tr>
                      <th className="px-3 py-2 text-left">Sink</th>
                      <th className="px-3 py-2 text-left">Status</th>
                      <th className="px-3 py-2 text-left">Attempts</th>
                      <th className="px-3 py-2 text-left">Detail</th>
                    </tr>
                  </thead>
                  <tbody>
                    {(deliveryQuery.data ?? []).map((row) => (
                      <DeliveryRow key={row.id} row={row} />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>

          <section>
            <h3 className="mb-2 text-sm font-semibold">Identifiers</h3>
            <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              <dt className="text-muted-foreground">event_id</dt>
              <dd className="font-mono break-all">{event.event_id}</dd>
              <dt className="text-muted-foreground">trace_id</dt>
              <dd className="font-mono break-all">{event.trace_id}</dd>
              <dt className="text-muted-foreground">frame_id</dt>
              <dd className="font-mono">{String(event.frame_id)}</dd>
              <dt className="text-muted-foreground">track_id</dt>
              <dd className="font-mono">
                {event.track_id !== null ? String(event.track_id) : "—"}
              </dd>
              <dt className="text-muted-foreground">captured_at</dt>
              <dd className="font-mono">{event.captured_at}</dd>
            </dl>
          </section>

          {Object.keys(event.context ?? {}).length > 0 ? (
            <section>
              <h3 className="mb-2 text-sm font-semibold">Context</h3>
              <pre className="overflow-x-auto rounded-md border border-border bg-muted/20 p-3 text-xs">
                {JSON.stringify(event.context, null, 2)}
              </pre>
            </section>
          ) : null}
        </div>
      </aside>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Small bits.
// ---------------------------------------------------------------------------

function SeverityBadge({ severity }: { severity: Severity }) {
  const variant: "secondary" | "warning" | "destructive" =
    severity === "critical" || severity === "high"
      ? "destructive"
      : severity === "medium"
        ? "warning"
        : "secondary";
  return (
    <Badge variant={variant} className="capitalize">
      {severity}
    </Badge>
  );
}

function DeliveryRow({ row }: { row: OutboxRow }) {
  const detail = row.suppression_reason
    ? row.suppression_reason.replace(/_/g, " ")
    : row.last_error
      ? row.last_error
      : row.delivered_at
        ? `delivered ${formatAgo(row.delivered_at)}`
        : row.next_attempt_at
          ? `retry ${formatAgo(row.next_attempt_at)}`
          : "—";
  return (
    <tr className="border-t border-border/40">
      <td className="px-3 py-2 font-mono">
        <div className="flex items-center gap-1.5">
          <CameraIcon className="h-3 w-3 text-muted-foreground" />
          {row.sink_id}
        </div>
      </td>
      <td className="px-3 py-2">
        <OutboxStatusBadge status={row.status} />
      </td>
      <td className="px-3 py-2 font-mono">{row.attempts}</td>
      <td className="px-3 py-2 text-muted-foreground">{detail}</td>
    </tr>
  );
}

function OutboxStatusBadge({ status }: { status: OutboxStatus }) {
  const variant: "secondary" | "success" | "warning" | "destructive" =
    status === "sent"
      ? "success"
      : status === "pending"
        ? "secondary"
        : status === "suppressed"
          ? "warning"
          : "destructive";
  const Icon =
    status === "sent" ? CheckCircle2 : status === "pending" ? Clock : AlertTriangle;
  return (
    <Badge variant={variant} className="capitalize">
      <Icon className="mr-1 h-3 w-3" />
      {status}
    </Badge>
  );
}
