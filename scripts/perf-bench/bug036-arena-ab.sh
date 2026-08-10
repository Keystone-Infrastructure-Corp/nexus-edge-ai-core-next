#!/usr/bin/env bash
# BUG-036: does MALLOC_ARENA_MAX=2 cost throughput?
#
# Capping glibc arenas recovered ~2.7 GB of retained RSS, and the mapping
# histogram confirms ~1.5 GB of a 4.7 GB retention sits in 64 MiB per-thread
# arenas. That makes the cap worth shipping in the systemd unit -- but a
# 16-thread engine funnelled through two arenas can trade memory for allocator
# contention, and the flap harness measures no throughput at all. This runs the
# same steady-state workload with and without the cap and reports frames/s and
# CPU alongside retained RSS, so the trade is a measurement rather than a guess.
#
# Run under sudo. Removes its drop-in and restarts the engine on exit.
set -uo pipefail

API="http://127.0.0.1:8089/api/v1"
RIG=/tmp/nexus-perf-bench
N=${N:-4}
WARM=${WARM:-60}
MEASURE=${MEASURE:-120}
DROPIN=/etc/systemd/system/nexus-engine.service.d/98-arena-ab.conf
OUT=/tmp/bug036-arena-ab.log

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT"; }

cleanup() {
  rm -f "$DROPIN"
  systemctl daemon-reload
  systemctl restart nexus-engine
  log "removed $DROPIN, engine restarted"
}
trap cleanup EXIT

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
    curl -s -m 20 -X "$m" -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' -d "$b" "$API$p"
  else
    curl -s -m 20 -X "$m" -H "Authorization: Bearer $TOKEN" "$API$p"
  fi
}

pid()    { pgrep -x nexus-engine; }
rss_mb() { echo $(( $(awk '/VmRSS/{print $2}' "/proc/$(pid)/status") / 1024 )); }

bench_ids() {
  api GET /admin/cameras | python3 -c \
    'import json,sys; print(" ".join(str(c["id"]) for c in json.load(sys.stdin) if "perfbench" in c.get("url","")))'
}

# Sum of frames_emitted across the bench cameras -- the throughput signal that
# does not depend on which accelerator this box happens to use.
frames_total() {
  local sum=0 s
  for id in $(bench_ids); do
    s=$(api GET "/cameras/$id/stats" | python3 -c \
      'import json,sys; d=json.load(sys.stdin); print(int(d.get("frames_emitted") or 0))' 2>/dev/null || echo 0)
    sum=$((sum + s))
  done
  echo "$sum"
}

cpu_pct() {
  api GET /system/metrics | python3 -c \
    'import json,sys; print(round(json.load(sys.stdin)["cpu"]["usage_pct"],1))' 2>/dev/null || echo -1
}

pubs_kill() { pkill -f 'ffmpeg .*perfbench-' 2>/dev/null; sleep 3; }
pubs_start() {
  for i in $(seq 0 $((N - 1))); do
    nohup ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -ss $((i * 11)) \
      -i "$RIG/source.mp4" -c copy -f rtsp -rtsp_transport tcp \
      "rtsp://127.0.0.1:9554/perfbench-$(printf '%03d' "$i")" >/dev/null 2>&1 &
  done
  sleep 5
}

arm() {
  local label=$1 arena=$2
  log "=== arm: $label (MALLOC_ARENA_MAX=${arena:-unset}) ==="
  mkdir -p "$(dirname "$DROPIN")"
  if [[ -n $arena ]]; then
    printf '[Service]\nEnvironment=MALLOC_ARENA_MAX=%s\n' "$arena" >"$DROPIN"
  else
    rm -f "$DROPIN"
  fi
  systemctl daemon-reload

  for id in $(bench_ids); do api DELETE "/admin/cameras/$id" >/dev/null; done
  pubs_kill
  systemctl restart nexus-engine
  sleep 45
  local base_rss; base_rss=$(rss_mb)

  pubs_start
  for i in $(seq 0 $((N - 1))); do
    local slot; slot=$(printf '%03d' "$i")
    api POST /admin/cameras \
      "{\"name\":\"arena-$slot\",\"url\":\"rtsp://127.0.0.1:9554/perfbench-$slot\",\"enabled\":true,\"codec\":\"h264\",\"max_fps\":15}" >/dev/null
  done
  sleep "$WARM"

  local f0 c0 f1 c1
  f0=$(frames_total); c0=$(cpu_pct)
  sleep "$MEASURE"
  f1=$(frames_total); c1=$(cpu_pct)
  local fps; fps=$(python3 -c "print(round(($f1-$f0)/$MEASURE,2))")
  local peak_rss; peak_rss=$(rss_mb)

  pubs_kill
  for id in $(bench_ids); do api DELETE "/admin/cameras/$id" >/dev/null; done
  sleep 60
  local end_rss; end_rss=$(rss_mb)

  log "$label: fps=$fps cpu=${c0}/${c1}% base_rss=${base_rss}MB peak_rss=${peak_rss}MB settled_rss=${end_rss}MB retained=$((end_rss - base_rss))MB"
}

: > "$OUT"
log "=== arena A/B on $(readlink -f /opt/nexus/current), N=$N ==="
arm uncapped ""
arm arena-max-2 "2"
log "=== summary ==="
grep -E 'fps=' "$OUT"
