#!/usr/bin/env bash
# Camera-capacity bench for a Nexus edge box. Create it, ramp it, throw it away.
#
# Answers one question on whatever silicon you point it at: how many cameras
# can this box carry with detection (+ re-id, if enabled) before frames go
# stale? Everything it creates is namespaced and removed by `teardown` —
# phantom cameras are prefixed `perfbench-`, and every other artifact lives
# under a single work directory.
#
#   ./nexus-perf-bench.sh setup                 # deps, RTSP server, source clip
#   ./nexus-perf-bench.sh run --steps 10,20,30,45,60
#   ./nexus-perf-bench.sh status
#   ./nexus-perf-bench.sh teardown              # removes ALL of the above
#
# Run it ON the box under test, as a user that can read the engine's admin
# secret (the `nexus` user, or via sudo). It needs no cloud connectivity.
#
# Environment overrides:
#   NEXUS_PERF_DIR        work directory            (default /tmp/nexus-perf-bench)
#   NEXUS_PERF_API        engine admin API base     (default http://127.0.0.1:8089/api/v1)
#   NEXUS_PERF_SECRET     admin secret path         (default /var/lib/nexus/state/admin-secret)
#   NEXUS_PERF_RTSP_PORT  loopback RTSP port        (default 9554)
#
# WHY THE ODD PORTS: the bench's RTSP server shares the box with the engine,
# which already owns :8554/:8000/:8189 in some configurations. Every mediamtx
# listener is shifted +1000 so the two can coexist.
set -euo pipefail

WORKDIR="${NEXUS_PERF_DIR:-/tmp/nexus-perf-bench}"
API="${NEXUS_PERF_API:-http://127.0.0.1:8089/api/v1}"
SECRET_PATH="${NEXUS_PERF_SECRET:-/var/lib/nexus/state/admin-secret}"
RTSP_PORT="${NEXUS_PERF_RTSP_PORT:-9554}"
CAM_PREFIX="perfbench-"
MEDIAMTX_VERSION="v1.20.0"
HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '[perf-bench] %s\n' "$*" >&2; }
die() { printf '[perf-bench] ERROR: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# The engine's admin secret is 0600 root:root. Rather than stage a copy of it
# somewhere world-readable (which is what every ad-hoc version of this bench
# ends up doing), the bench simply refuses to run without direct read access.
require_secret() {
    [[ -r "$SECRET_PATH" ]] && return
    die "cannot read $SECRET_PATH — re-run under sudo (do not copy the secret)"
}

# ---------------------------------------------------------------------------
# setup
# ---------------------------------------------------------------------------

fetch_mediamtx() {
    [[ -x "$WORKDIR/mediamtx" ]] && { log "mediamtx already staged"; return; }
    local arch tarball url
    case "$(uname -m)" in
        x86_64)  arch="amd64" ;;
        aarch64) arch="arm64v8" ;;
        *)       die "unsupported arch for mediamtx: $(uname -m)" ;;
    esac
    tarball="mediamtx_${MEDIAMTX_VERSION}_linux_${arch}.tar.gz"
    url="https://github.com/bluenviron/mediamtx/releases/download/${MEDIAMTX_VERSION}/${tarball}"
    log "fetching mediamtx ${MEDIAMTX_VERSION} (${arch})"
    curl -fsSL "$url" -o "$WORKDIR/$tarball" || die "mediamtx download failed"
    tar -xzf "$WORKDIR/$tarball" -C "$WORKDIR" mediamtx
    rm -f "$WORKDIR/$tarball"
}

write_mediamtx_config() {
    # `all_others` is load-bearing: without a paths section mediamtx answers
    # every publish with 400 Bad Request and gives no hint why.
    cat > "$WORKDIR/mediamtx.yml" <<EOF
logLevel: error
logDestinations: [stdout]
readTimeout: 10s
writeTimeout: 10s
writeQueueSize: 2048
rtsp: yes
rtspTransports: [tcp]
rtspAddress: :${RTSP_PORT}
rtpAddress: :$((RTSP_PORT + 446))
rtcpAddress: :$((RTSP_PORT + 447))
rtmp: no
hls: no
webrtc: no
srt: no
api: no
metrics: no
pprof: no
playback: no
authMethod: internal
authInternalUsers:
  - user: any
    pass:
    ips: []
    permissions:
      - action: publish
      - action: read
paths:
  all_others:
EOF
}

# Pick a source clip. Synthetic patterns are useless here — a bench with no
# people in frame measures decode and nothing else, so default to real
# footage the box already recorded.
resolve_source() {
    local explicit="${1:-}"
    if [[ -n "$explicit" ]]; then
        [[ -f "$explicit" ]] || die "source clip not found: $explicit"
        printf '%s' "$explicit"
        return
    fi
    local found
    found="$(find /var/lib/nexus -name '*.mp4' -size +2M -printf '%s\t%p\n' 2>/dev/null \
        | sort -rn | head -1 | cut -f2)"
    [[ -n "$found" ]] || die "no recorded clip found under /var/lib/nexus; pass --source <file.mp4>"
    printf '%s' "$found"
}

# Re-encode with per-frame noise and a burned-in timecode.
#
# THIS IS NOT COSMETIC. The engine's source watchdog kills any stream whose
# decoded frames repeat on a fixed cycle ("decoded frames are repeating on a
# fixed cycle ... Internal data stream error"). A looped recording trips it
# during every static moment, because a re-encode of unchanging pixels is
# byte-identical frame to frame — something a real sensor never produces.
# `noise` restores the per-frame entropy; `drawtext` makes loop boundaries
# visible when you eyeball a stream.
build_source_clip() {
    local src="$1" out="$WORKDIR/source.mp4" secs="${2:-300}"
    [[ -f "$out" ]] && { log "source clip already built: $out"; return; }
    log "building ${secs}s de-duplicated source clip from $src"
    ffmpeg -hide_banner -loglevel error -y \
        -stream_loop -1 -i "$src" -t "$secs" \
        -vf "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:-1:-1,noise=alls=12:allf=t+u,drawtext=text='%{pts\\:hms}':x=16:y=16:fontsize=36:fontcolor=white:box=1:boxcolor=black@0.5" \
        -c:v libx264 -preset veryfast -tune zerolatency -g 30 -pix_fmt yuv420p \
        -an "$out" || die "source clip encode failed"
    log "source clip: $(du -h "$out" | cut -f1)"
}

cmd_setup() {
    local source="" secs=300
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --source)   source="$2"; shift 2 ;;
            --duration) secs="$2"; shift 2 ;;
            *) die "unknown setup flag: $1" ;;
        esac
    done

    need ffmpeg; need python3; need curl; need tar
    require_secret

    mkdir -p "$WORKDIR"
    fetch_mediamtx
    write_mediamtx_config
    build_source_clip "$(resolve_source "$source")" "$secs"

    if pgrep -f "$WORKDIR/mediamtx" >/dev/null 2>&1; then
        log "mediamtx already running"
    else
        # setsid + </dev/null, not `nohup &`: a plain background job dies with
        # the ssh session that started it.
        ( cd "$WORKDIR" && setsid ./mediamtx mediamtx.yml < /dev/null > mediamtx.log 2>&1 & )
        sleep 2
        pgrep -f "$WORKDIR/mediamtx" >/dev/null 2>&1 \
            || { tail -5 "$WORKDIR/mediamtx.log" >&2; die "mediamtx failed to start"; }
        log "mediamtx listening on rtsp://127.0.0.1:${RTSP_PORT}"
    fi
    log "setup complete — next: $0 run --steps 10,20,30,45,60"
}

# ---------------------------------------------------------------------------
# run / status / teardown
# ---------------------------------------------------------------------------

driver() {
    NEXUS_PERF_DIR="$WORKDIR" \
    NEXUS_PERF_API="$API" \
    NEXUS_PERF_SECRET="$SECRET_PATH" \
    NEXUS_PERF_RTSP_PORT="$RTSP_PORT" \
    NEXUS_PERF_CAM_PREFIX="$CAM_PREFIX" \
        python3 "$HERE/ramp.py" "$@"
}

cmd_run() {
    require_secret
    [[ -f "$WORKDIR/source.mp4" ]] || die "run setup first"
    pgrep -f "$WORKDIR/mediamtx" >/dev/null 2>&1 || die "mediamtx is not running; run setup first"
    driver run "$@"
}

cmd_status() { require_secret; driver status; }

cmd_teardown() {
    require_secret
    log "deleting ${CAM_PREFIX}* cameras"
    driver teardown || log "camera cleanup reported errors; continuing with process cleanup"

    log "stopping publishers and RTSP server"
    pkill -f "rtsp://127.0.0.1:${RTSP_PORT}/${CAM_PREFIX}" 2>/dev/null || true
    pkill -f "$WORKDIR/mediamtx" 2>/dev/null || true
    sleep 1

    log "removing $WORKDIR"
    rm -rf "$WORKDIR"
    log "teardown complete"
}

case "${1:-}" in
    setup)    shift; cmd_setup "$@" ;;
    run)      shift; cmd_run "$@" ;;
    status)   shift; cmd_status ;;
    teardown) shift; cmd_teardown ;;
    *) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
