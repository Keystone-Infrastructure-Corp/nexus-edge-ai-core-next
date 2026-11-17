---
name: edge-pipeline-feature-gated-tests
description: "The recorder, pre-roll ingester and webrtc code in nexus-pipeline only compile with --features gstreamer-webrtc; a default-features test run never touches them"
metadata:
  type: feedback
---

In `nexus-pipeline`, `gst_clip_recorder.rs`, `preroll_ingester.rs` and `webrtc.rs` sit
behind the `gstreamer` / `gstreamer-webrtc` features. `cargo test -p nexus-pipeline`
(and the workspace test) compiles none of them, so a green default run says nothing
about the recorder. A change to the recorder once reported green on default features
while the changed code was never compiled.

**Why:** a verify that does not compile the changed code is a false pass, the same trap
as a skipped Docker suite.

**How to apply:** for recorder or ingester work, run
`cargo test --locked -p nexus-pipeline --features gstreamer-webrtc --tests` (the CI
"gstreamer + ort" job form), plus the four gstreamer clippy lines in `ci.yml`. A local
Homebrew GStreamer has x264enc, avdec_h264 and mp4mux, so the GStreamer tests really
run. Grep the log for `skipping` to confirm. Related: `linux-clippy-in-colima`.
