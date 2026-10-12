import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";

import {
  getDiscoverySession,
  probeOnvifStreams,
  probeRtsp,
  startCidrScan,
  startOnvifDiscovery,
} from "@/api/config";
import type {
  DiscoveredDevice,
  DiscoverySessionView,
  ProbeRtspStream,
} from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Sheet } from "@/components/ui/sheet";

// ---------------------------------------------------------------------------
// Discovery sheet — ONVIF + CIDR scan.
// ---------------------------------------------------------------------------

export function DiscoverySheet({
  onClose,
  onAdd,
}: {
  onClose: () => void;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
    nameOverride?: string,
  ) => void;
}) {
  const [mode, setMode] = useState<"onvif" | "scan">("onvif");
  // Credentials are global to the discovery session: nearly every
  // operator deploys cameras with one shared admin account, and
  // typing creds per-device after a /24 scan finds 20 IPs is
  // miserable. The same (username, password) is sent with every
  // Probe and embedded in every camera URL on Add.
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  // Session-global toggle: when on, an ONVIF-probed camera is named
  // from its plain-text OSD overlay (falling back to the device name).
  const [useOsdName, setUseOsdName] = useState(false);

  return (
    <Sheet
      open
      onClose={onClose}
      title="Discover cameras"
      description="Find devices via ONVIF multicast or by scanning a CIDR range."
      width="max-w-3xl"
    >
      <div className="border-b border-border px-5 py-4">
        <Label className="text-xs font-semibold uppercase text-muted-foreground">
          Camera credentials
        </Label>
        <p className="mt-1 text-xs text-muted-foreground">
          Used for every probe and embedded in the camera URL on Add.
          Leave blank for cameras without authentication.
        </p>
        <div className="mt-2 grid grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="discovery-username" className="text-xs">
              Username
            </Label>
            <Input
              id="discovery-username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="admin"
              autoComplete="username"
              className="h-8 font-mono text-xs"
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="discovery-password" className="text-xs">
              Password
            </Label>
            <Input
              id="discovery-password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
              className="h-8 font-mono text-xs"
            />
          </div>
        </div>
      </div>
      <div className="border-b border-border px-5">
        <div className="flex gap-1 py-3">
          <button
            type="button"
            onClick={() => setMode("onvif")}
            className={`rounded-md px-3 py-1 text-sm ${
              mode === "onvif"
                ? "bg-primary/10 text-primary"
                : "text-muted-foreground hover:bg-muted"
            }`}
          >
            ONVIF multicast
          </button>
          <button
            type="button"
            onClick={() => setMode("scan")}
            className={`rounded-md px-3 py-1 text-sm ${
              mode === "scan"
                ? "bg-primary/10 text-primary"
                : "text-muted-foreground hover:bg-muted"
            }`}
          >
            CIDR scan
          </button>
        </div>
      </div>
      {mode === "onvif" ? (
        <label className="mb-1 flex items-center gap-2 px-5 py-2 text-xs text-muted-foreground">
          <input
            type="checkbox"
            checked={useOsdName}
            onChange={(e) => setUseOsdName(e.target.checked)}
            className="h-3 w-3 rounded border-border"
          />
          Name cameras from their on-screen text (OSD) when present,
          otherwise use the device name
        </label>
      ) : null}
      {mode === "onvif" ? (
        <DiscoveryRunner
          kind="onvif"
          onAdd={onAdd}
          username={username}
          password={password}
          useOsdName={useOsdName}
        />
      ) : (
        <DiscoveryRunner
          kind="scan"
          onAdd={onAdd}
          username={username}
          password={password}
          useOsdName={false}
        />
      )}
    </Sheet>
  );
}

function DiscoveryRunner({
  kind,
  onAdd,
  username,
  password,
  useOsdName,
}: {
  kind: "onvif" | "scan";
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
    nameOverride?: string,
  ) => void;
  username: string;
  password: string;
  useOsdName: boolean;
}) {
  const [cidr, setCidr] = useState("");
  const [ports, setPorts] = useState("554,80,8080");
  const [confirm, setConfirm] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [startError, setStartError] = useState<string | null>(null);

  const startMutation = useMutation({
    mutationFn: async () => {
      if (kind === "onvif") {
        return startOnvifDiscovery();
      }
      const portsList = ports
        .split(",")
        .map((p) => Number.parseInt(p.trim(), 10))
        .filter((n) => Number.isFinite(n) && n > 0);
      return startCidrScan({
        cidr: cidr.trim(),
        ports: portsList.length ? portsList : undefined,
        confirm: confirm || undefined,
      });
    },
    onSuccess: (s) => {
      setSessionId(s.session_id);
      setStartError(null);
    },
    onError: (e: unknown) =>
      setStartError(e instanceof Error ? e.message : String(e)),
  });

  const sessionQuery = useQuery<DiscoverySessionView>({
    queryKey: ["discovery", "session", sessionId],
    queryFn: () => getDiscoverySession(sessionId!),
    enabled: !!sessionId,
    refetchInterval: (q) =>
      q.state.data?.state === "running" ? 1_000 : false,
  });

  const session = sessionQuery.data;

  return (
    <div className="space-y-5 p-5">
      {kind === "scan" ? (
        <div className="space-y-3 rounded-md border border-border p-4">
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-2">
              <Label htmlFor="scan-cidr">CIDR</Label>
              <Input
                id="scan-cidr"
                value={cidr}
                onChange={(e) => setCidr(e.target.value)}
                placeholder="192.168.1.0/24"
                className="font-mono"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="scan-ports">Ports (comma-separated)</Label>
              <Input
                id="scan-ports"
                value={ports}
                onChange={(e) => setPorts(e.target.value)}
                className="font-mono"
              />
            </div>
          </div>
          <label className="inline-flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={confirm}
              onChange={(e) => setConfirm(e.target.checked)}
              className="h-3 w-3 rounded border-border"
            />
            Confirm scanning a network larger than /22
          </label>
        </div>
      ) : null}

      <div className="flex items-center gap-2">
        <Button
          onClick={() => startMutation.mutate()}
          disabled={
            startMutation.isPending ||
            (kind === "scan" && !cidr.trim())
          }
        >
          {startMutation.isPending
            ? "Starting…"
            : sessionId
              ? "Restart"
              : "Start"}
        </Button>
        {session ? (
          <Badge
            variant={
              session.state === "done"
                ? "success"
                : session.state === "error"
                  ? "destructive"
                  : "secondary"
            }
          >
            {session.state} · {session.scanned}
            {session.total_targets ? `/${session.total_targets}` : ""}
          </Badge>
        ) : null}
      </div>

      {startError ? (
        <p className="text-sm text-destructive">{startError}</p>
      ) : null}
      {session?.error ? (
        <p className="text-sm text-destructive">{session.error}</p>
      ) : null}

      {session ? (
        <DiscoveredList
          session={session}
          onAdd={onAdd}
          username={username}
          password={password}
          useOsdName={useOsdName}
        />
      ) : null}
    </div>
  );
}

function DiscoveredList({
  session,
  onAdd,
  username,
  password,
  useOsdName,
}: {
  session: DiscoverySessionView;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
    nameOverride?: string,
  ) => void;
  username: string;
  password: string;
  useOsdName: boolean;
}) {
  if (session.found.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        {session.state === "running" ? "Scanning…" : "No devices found."}
      </p>
    );
  }

  return (
    <div className="space-y-2">
      <h4 className="text-xs font-semibold uppercase text-muted-foreground">
        Found {session.found.length}
      </h4>
      {session.found.map((d, i) => (
        <DiscoveredItem
          key={`${d.ip}:${d.port}:${i}`}
          sessionId={session.session_id}
          device={d}
          onAdd={onAdd}
          username={username}
          password={password}
          useOsdName={useOsdName}
        />
      ))}
    </div>
  );
}

function DiscoveredItem({
  sessionId,
  device,
  onAdd,
  username,
  password,
  useOsdName,
}: {
  sessionId: string;
  device: DiscoveredDevice;
  onAdd: (
    device: DiscoveredDevice,
    path: string,
    username: string,
    password: string,
    nameOverride?: string,
  ) => void;
  username: string;
  password: string;
  useOsdName: boolean;
}) {
  const [probing, setProbing] = useState(false);
  const [probeError, setProbeError] = useState<string | null>(null);
  // CIDR-scanned devices typically arrive with `rtsp_paths` empty
  // (port open, no enumeration). Default the picker to "/" so the
  // backend's parallel vendor-default path walker kicks in on
  // Probe; the operator can override at any time.
  const [picked, setPicked] = useState<string>(
    device.rtsp_paths[0] ?? "/",
  );
  // Streams returned by the most recent successful Probe. Each
  // entry is a path that DESCRIBE-200'd plus its primary video
  // codec / resolution. Populating this turns the path input
  // into a labeled dropdown ("/Streaming/Channels/101 — H264
  // 1920x1080" vs "/Streaming/Channels/102 — H264 640x360").
  const [streams, setStreams] = useState<ProbeRtspStream[]>([]);
  const [probedOk, setProbedOk] = useState(false);
  /// `true` IFF the populated `streams` came from ONVIF Media
  /// (`GetProfiles` + `GetStreamUri`) rather than the RTSP path
  /// sweep. Drives the "verified via ONVIF" badge so the
  /// operator can tell at a glance that the displayed paths are
  /// authoritative (vendor-reported) vs. heuristic.
  const [probedViaOnvif, setProbedViaOnvif] = useState(false);
  // Text OSD (e.g. "PASTORS") captured on the last ONVIF probe, used
  // by the "name cameras from OSD" discovery toggle. `null` until a
  // probe returns one.
  const [osdName, setOsdName] = useState<string | null>(null);
  // Add-time OSD resolution in flight (see handleAdd).
  const [adding, setAdding] = useState(false);
  const effectivePath = picked.trim() || "/";

  const onProbe = async () => {
    setProbing(true);
    setProbeError(null);
    setProbedOk(false);
    setProbedViaOnvif(false);
    setOsdName(null);
    setStreams([]);

    // ONVIF Media path: when the device carries an XAddrs URL
    // AND the operator supplied credentials, ask the camera for
    // its own profile list. This is authoritative (vendor-built)
    // and replaces the brute-force path sweep entirely. We fall
    // back silently on any failure so a misconfigured ONVIF
    // service can't block the RTSP path that's worked for years.
    if (device.onvif_xaddrs && username && password) {
      try {
        const onvif = await probeOnvifStreams(sessionId, {
          xaddr: device.onvif_xaddrs,
          username,
          password,
        });
        if (onvif.ok && onvif.streams.length > 0) {
          // Map OnvifMediaStream \u2192 ProbeRtspStream so the
          // existing dropdown rendering keeps working. We strip
          // the scheme/host/port off the camera-reported URI
          // because openFromDiscovered() rebuilds the URL with
          // creds + the device's IP / rtsp_port. Keeping just
          // the path+query preserves vendor-specific query
          // params (e.g. Hikvision's ?transportmode=unicast)
          // while letting us inject the operator-supplied creds
          // and the locally-routable IP.
          const mapped: ProbeRtspStream[] = onvif.streams.map((s) => ({
            path: extractPathAndQuery(s.uri),
            codec: s.codec ?? null,
            resolution: s.resolution ?? null,
          }));
          setStreams(mapped);
          setPicked(mapped[0]!.path);
          setProbedOk(true);
          setProbedViaOnvif(true);
          setOsdName(onvif.osd_name ?? null);
          setProbing(false);
          return;
        }
        // Empty / error \u2014 fall through to RTSP probe below.
        // Don't surface ONVIF errors yet: the RTSP fallback may
        // still succeed (and if it doesn't, its own error is
        // more actionable for the operator).
      } catch {
        // Network/parse error on the ONVIF endpoint itself \u2014
        // silently fall back. The engine handler returns 200
        // on every soft failure, so reaching this `catch`
        // means something is genuinely wrong with the request
        // path (auth header, missing route, etc.) and the
        // operator needs to know via the RTSP fallback path.
      }
    }

    try {
      const res = await probeRtsp(sessionId, {
        host: device.ip,
        // RTSP probes MUST hit the RTSP port, not the discovery
        // port. ONVIF-discovered devices report :80 in `port`
        // (the web service); sending RTSP DESCRIBE there gets
        // "405 Method Not Allowed" from the HTTP server for every
        // candidate path. Fall back to :554 when discovery didn't
        // confirm an RTSP port explicitly.
        port: device.rtsp_port ?? 554,
        // Send "/" to ask the backend to walk DEFAULT_PATHS in
        // parallel. When the user already picked a specific path
        // we send that verbatim so an explicit choice is honoured.
        path: effectivePath,
        username: username || undefined,
        password: password || undefined,
      });
      if (!res.ok) {
        if (res.status === 401 || res.status === 403) {
          setProbeError(
            username
              ? `auth rejected (status ${res.status}) — check credentials above`
              : `authentication required (status ${res.status}) — enter credentials above`,
          );
        } else if (res.status === 0) {
          setProbeError("no RTSP server answered (connect/timeout)");
        } else {
          setProbeError(`status ${res.status}`);
        }
        if (res.path) setPicked(res.path);
      } else {
        const probed = res.streams ?? [];
        setStreams(probed);
        if (probed[0]) {
          setPicked(probed[0].path);
        } else if (res.path) {
          setPicked(res.path);
        }
        setProbedOk(true);
      }
    } catch (e) {
      setProbeError(e instanceof Error ? e.message : String(e));
    } finally {
      setProbing(false);
    }
  };

  // Path options the dropdown shows. After a successful Probe
  // we prefer the probed streams (they carry codec/resolution
  // labels). Before that, fall back to whatever ONVIF reported
  // in `rtsp_paths`. A bare `<Input>` is only used as a last
  // resort for raw CIDR finds.
  const pathOptions: ProbeRtspStream[] =
    streams.length > 0
      ? streams
      : device.rtsp_paths.map((p) => ({ path: p }));

  // "Add as camera" handler. When the "name cameras from OSD" toggle
  // is on, resolve the OSD text before adding: prefer one captured by
  // a prior Probe, otherwise — for an ONVIF device with creds the
  // operator hasn't probed — fetch it once on demand so the toggle
  // works even from a bare Add click. Any failure falls back silently
  // to the device name (openFromDiscovered's default).
  const handleAdd = async () => {
    let nameOverride: string | undefined;
    if (useOsdName) {
      let resolved = osdName;
      if (!resolved && device.onvif_xaddrs && username && password) {
        setAdding(true);
        try {
          const r = await probeOnvifStreams(sessionId, {
            xaddr: device.onvif_xaddrs,
            username,
            password,
          });
          resolved = r.osd_name ?? null;
          setOsdName(resolved);
        } catch {
          // ignore — fall back to the device name
        } finally {
          setAdding(false);
        }
      }
      if (resolved && resolved.trim()) nameOverride = resolved.trim();
    }
    onAdd(device, effectivePath, username, password, nameOverride);
  };

  return (
    <div className="rounded-md border border-border p-3 text-sm">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-mono">{device.ip}:{device.port}</span>
            <Badge variant="outline">{device.kind}</Badge>
            {probedOk ? (
              <Badge variant="success">
                {probedViaOnvif ? "verified · ONVIF" : "verified"}
              </Badge>
            ) : null}
            {device.codec ? (
              <Badge variant="outline" className="font-mono uppercase">
                {device.codec.replace("_plus", "+")}
              </Badge>
            ) : null}
          </div>
          <p className="mt-0.5 text-xs text-muted-foreground">
            {[device.vendor, device.model, device.firmware]
              .filter(Boolean)
              .join(" · ") || "—"}
          </p>
          <p className="mt-0.5 text-xs text-muted-foreground">
            RTSP on port {device.rtsp_port ?? 554}
            {device.rtsp_port == null ? " (default)" : ""}
          </p>
        </div>
        <Button
          size="sm"
          onClick={handleAdd}
          disabled={probing || adding}
        >
          {adding ? "Adding…" : "Add as camera"}
        </Button>
      </div>

      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Label className="text-xs text-muted-foreground">Path</Label>
        {pathOptions.length > 0 ? (
          <select
            className="h-7 max-w-md rounded-md border border-input bg-transparent px-2 text-xs font-mono"
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
          >
            {pathOptions.map((s) => (
              <option key={s.path} value={s.path}>
                {streamLabel(s)}
              </option>
            ))}
          </select>
        ) : (
          <Input
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
            placeholder="/"
            className="h-7 w-64 font-mono text-xs"
          />
        )}
        <Button
          size="sm"
          variant="outline"
          onClick={onProbe}
          disabled={probing}
        >
          {probing ? "Probing…" : streams.length > 0 ? "Re-probe" : "Probe"}
        </Button>
        {probeError ? (
          <span className="text-xs text-destructive">{probeError}</span>
        ) : null}
      </div>

      {streams.length > 1 ? (
        <p className="mt-1 text-xs text-muted-foreground">
          {streams.length} streams available — pick one before Add.
        </p>
      ) : null}
      {useOsdName && osdName ? (
        <p className="mt-1 text-xs text-muted-foreground">
          Will name this camera{" "}
          <span className="font-medium text-foreground">{osdName}</span> from
          its OSD.
        </p>
      ) : null}
    </div>
  );
}

function streamLabel(s: ProbeRtspStream): string {
  const tail = [s.codec, s.resolution].filter(Boolean).join(" ");
  return tail ? `${s.path} — ${tail}` : s.path;
}

/// Pull just the `pathname + search` out of an ONVIF-reported
/// stream URI (e.g.
/// `"rtsp://192.168.1.66:554/Streaming/Channels/101?transportmode=unicast"`
/// \u2192 `"/Streaming/Channels/101?transportmode=unicast"`).
///
/// Used to translate `OnvifMediaStream.uri` into the
/// path-only shape `openFromDiscovered()` expects \u2014 it
/// rebuilds the URL with operator-supplied creds and the
/// locally-routable IP/port from `DiscoveredDevice`, which is
/// the right thing to do because the camera's reported URI may
/// embed a hostname or NAT-internal IP that the engine can't
/// reach.
///
/// Falls back to the verbatim input on parse failure (e.g.
/// vendors that return a non-URL path fragment). The downstream
/// composer will produce a malformed URL in that pathological
/// case, but it's no worse than what an operator would type
/// manually \u2014 they'll see the error on first stream-test.
// ---------------------------------------------------------------------------

function extractPathAndQuery(uri: string): string {
  try {
    const u = new URL(uri);
    return `${u.pathname}${u.search}`;
  } catch {
    return uri;
  }
}
