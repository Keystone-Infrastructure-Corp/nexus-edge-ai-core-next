#!/bin/sh
# Test-only stand-in for nexus-tts: the same argv contract
# (`--model-dir <dir> --voice <id>`), with its behaviour picked by the voice
# id, since the engine clears the environment and fixes the argv.
#   sleep  never answers
#   fail   reads stdin, exits 1
#   big    writes more than the engine's output cap
#   slow   waits 0.3 s, then answers like any other voice
#   other  800 samples (100 ms) of a constant non-zero s16le sample per stdin byte
PATH=/usr/bin:/bin
voice=
while [ $# -gt 0 ]; do
  case "$1" in
    --voice) voice=$2; shift 2 ;;
    --model-dir) shift 2 ;;
    *) exit 2 ;;
  esac
done
case "$voice" in
  sleep) exec sleep 30 ;;
  fail) cat >/dev/null; exit 1 ;;
  big) cat >/dev/null; yes AB | tr -d '\n' | head -c 2000000; exit 0 ;;
  slow) sleep 0.3 ;;
esac
n=$(wc -c | tr -d ' ')
yes AB | tr -d '\n' | head -c $((n * 1600))
