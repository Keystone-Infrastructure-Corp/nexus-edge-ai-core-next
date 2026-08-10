#!/usr/bin/env bash
# BUG-036 verification: does repeated source teardown leak?
#
# The record's central claim is that engine RSS tracks the *source teardown
# rate*, not the camera count — so a camera that flaps (a dying PoE switch, a
# camera on a reboot schedule) walks a healthy box toward OOM. This flaps a
# fixed set of cameras a fixed number of times and reports the trend.
#
# Deterministic where the bulk-delete repro is not: no camera-id reuse, no
# reliance on per-camera stats that a leaked supervisor can also increment.
set -uo pipefail

API="http://127.0.0.1:8089/api/v1"
RIG=/tmp/nexus-perf-bench
N=${N:-4}
CYCLES=${CYCLES:-6}
OUT=/tmp/bug036-flap.log

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT"; }

TOKEN=$(python3 - <<'PY'
import base64, hashlib, hmac, json, time
secret = open('/var/lib/nexus/state/admin-secret').read().strip().encode()
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b'=').decode()
hdr = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(',', ':')).encode())
now = int(time.time())
pl = b64(json.dumps({"sub": "bug036", "iat": now, "exp": now + 7200},
                    separators=(',', ':')).encode())
msg = f"{hdr}.{pl}".encode()
print(f"{hdr}.{pl}." + b64(hmac.new(secret, msg, hashlib.sha256).digest()))
PY
) || { echo "cannot mint token"; exit 1; }

api() {
  local m=$1 p=$2 b=${3:-}
  if [[ -n $b ]]; then
    curl -s -m 20 -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d "$b" "$API$p"
  else
    curl -s -m 20 -X "$m" -H "Authorization: Bearer $TOKEN" "$API$p"
  fi
}

pid()     { pgrep -x nexus-engine; }
rss_mb()  { echo $(( $(awk '/VmRSS/{print $2}' "/proc/$(pid)/status") / 1024 )); }
threads() { awk '/Threads/{print $2}' "/proc/$(pid)/status"; }

bench_ids() {
  api GET /admin/cameras | python3 -c \
    'import json,sys; print(" ".join(str(c["id"]) for c in json.load(sys.stdin) if "perfbench" in c.get("url","")))'
}

# Attribute RSS by mapping when PROFILE=1 and the profiler is staged alongside.
PROFILE=${PROFILE:-0}
SMAPS=${SMAPS:-/tmp/smaps-cat.py}
profile() {
  [[ $PROFILE == 1 && -r $SMAPS ]] || return 0
  python3 "$SMAPS" "$(pid)" "$1" >>"$OUT" 2>&1
}

pubs_up()   { pgrep -cf 'ffmpeg .*perfbench-' 2>/dev/null || echo 0; }
pubs_kill() { pkill -f 'ffmpeg .*perfbench-' 2>/dev/null; sleep 3; }
pubs_start() {
  for i in $(seq 0 $((N - 1))); do
    nohup ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -ss $((i * 11)) \
      -i "$RIG/source.mp4" -c copy -f rtsp -rtsp_transport tcp \
      "rtsp://127.0.0.1:9554/perfbench-$(printf '%03d' "$i")" >/dev/null 2>&1 &
  done
  sleep 5
}

: > "$OUT"
VER=$(readlink -f /opt/nexus/current)
log "=== BUG-036 flap test on $VER (N=$N, cycles=$CYCLES) ==="

for id in $(bench_ids); do api DELETE "/admin/cameras/$id" >/dev/null; done
pubs_kill
sudo systemctl restart nexus-engine
sleep 45
BASE_RSS=$(rss_mb); BASE_THR=$(threads)
log "baseline (real cameras only): rss_mb=$BASE_RSS threads=$BASE_THR"
profile baseline

pubs_start
for i in $(seq 0 $((N - 1))); do
  slot=$(printf '%03d' "$i")
  api POST /admin/cameras "{\"name\":\"bug036-$slot\",\"url\":\"rtsp://127.0.0.1:9554/perfbench-$slot\",\"enabled\":true,\"codec\":\"h264\",\"max_fps\":15}" >/dev/null
done
sleep 40
START_RSS=$(rss_mb); START_THR=$(threads)
log "after $N cameras started: rss_mb=$START_RSS threads=$START_THR (publishers=$(pubs_up))"
profile started

MARK=$(date --iso-8601=seconds)
for c in $(seq 1 "$CYCLES"); do
  pubs_kill                # every source is now parked mid-read -> teardown
  sleep 20
  down_rss=$(rss_mb); down_thr=$(threads)
  pubs_start               # sources come back -> respawn
  sleep 20
  log "cycle $c: after-kill rss_mb=$down_rss threads=$down_thr | after-restart rss_mb=$(rss_mb) threads=$(threads)"
done

pubs_kill
for id in $(bench_ids); do api DELETE "/admin/cameras/$id" >/dev/null; done
sleep 60
END_RSS=$(rss_mb); END_THR=$(threads)
log "settled back to real cameras only: rss_mb=$END_RSS threads=$END_THR"
profile settled
if [[ $PROFILE == 1 && -r /tmp/smaps-baseline.json && -r /tmp/smaps-settled.json ]]; then
  python3 "$SMAPS" --diff /tmp/smaps-baseline.json /tmp/smaps-settled.json >>"$OUT" 2>&1
fi

GROWTH=$((END_RSS - BASE_RSS))
THR_GROWTH=$((END_THR - BASE_THR))
log "--- RSS growth over $CYCLES flap cycles: ${GROWTH}MB; thread growth: $THR_GROWTH ---"

W=$(journalctl -u nexus-engine --since "$MARK" --no-pager 2>/dev/null | grep -c 'gst teardown' || true)
S=$(journalctl -u nexus-engine --since "$MARK" --no-pager 2>/dev/null | grep -c 'gst teardown blocked' || true)
log "teardown log lines: $W (of which slow: $S)"

# A leak-free run returns to roughly where it started. 512MB of slack covers
# allocator retention and the real cameras' own jitter.
if (( GROWTH < 512 && THR_GROWTH < 40 )); then
  log "PASS: teardown released its memory and threads"
  exit 0
fi
log "FAIL: retained ${GROWTH}MB / $THR_GROWTH threads after returning to baseline camera set"
exit 1
