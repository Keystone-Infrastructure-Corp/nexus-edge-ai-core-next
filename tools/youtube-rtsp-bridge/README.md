# youtube-rtsp-bridge

Dev-only helper that pulls YouTube live streams into a local RTSP server so
the engine can be soak-tested without real cameras.

## Setup

```bash
brew install yt-dlp ffmpeg              # or apt-get install equivalent
brew install mediamtx                   # https://github.com/bluenviron/mediamtx
mediamtx                                # default config listens on :8554
```

## Single stream

```bash
./bin/yt-rtsp-bridge.sh "https://www.youtube.com/watch?v=jfKfPfyJRdk" cam1
# Engine config:  url = "rtsp://127.0.0.1:8554/cam1"
```

## Fleet

```bash
./bin/yt-rtsp-fleet.py examples/streams.json
```

Each child is supervised with exponential backoff (1s → 60s).
