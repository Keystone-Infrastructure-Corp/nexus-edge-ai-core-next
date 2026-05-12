#!/usr/bin/env bash
# yt-rtsp-bridge — pull a YouTube live stream into a local RTSP feed.
#
# Pipeline: yt-dlp → ffmpeg → mediamtx (RTSP server).
#
# Requirements: yt-dlp, ffmpeg, mediamtx (https://github.com/bluenviron/mediamtx).
#
# Usage:
#   ./yt-rtsp-bridge.sh <youtube-url> [stream-name]
#
# Then point the engine at:
#   rtsp://127.0.0.1:8554/<stream-name>

set -euo pipefail

URL="${1:?usage: yt-rtsp-bridge.sh <youtube-url> [stream-name]}"
NAME="${2:-cam1}"

PORT="${MEDIAMTX_PORT:-8554}"

DIRECT_URL="$(yt-dlp -g -f 'best[ext=mp4]/best' "$URL" | head -n1)"

# Push the direct URL into mediamtx with re-mux only — no transcode.
exec ffmpeg \
    -hide_banner -loglevel warning \
    -re -i "$DIRECT_URL" \
    -c copy -f rtsp -rtsp_transport tcp \
    "rtsp://127.0.0.1:${PORT}/${NAME}"
