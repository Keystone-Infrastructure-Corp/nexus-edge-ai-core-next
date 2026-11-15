---
applyTo: "crates/nexus-pipeline/**,crates/nexus-inference/**,crates/nexus-hailo-backend/**"
---

# Pipeline and hardware rules

- Hardware is gated by Cargo features: `gstreamer`, `gstreamer-webrtc`, the
  ONNX-Runtime EPs (`ep-cpu`, `ep-coreml`, `ep-cuda`, `ep-openvino`,
  `ep-tensorrt`) and `test-injection`.
- Never gate testing code on `cfg(debug_assertions)`. Use an explicit Cargo feature.
- The supervisor frame is per camera: RGB, 16:9, on the native ladder
  (512x288, 1024x576, 1536x864). Do not add a parallel frame source that skips it.
- The recorder, pre-roll ingester and WebRTC code only compile with
  `--features gstreamer-webrtc`. A default-feature test run does not cover them.
- `#[cfg(target_os = "linux")]` branches are not compiled by macOS clippy.
  Check them on Linux before you push a lint change.
- The edge must degrade to local operation when the cloud is unreachable.
