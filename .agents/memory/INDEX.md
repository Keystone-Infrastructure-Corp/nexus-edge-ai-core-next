# Field notes

Traps, gotchas and procedures for this repo that are too small for a vault record.
Before debugging env, CI or test trouble, scan this list, and record a new trap here in the same PR.

Cross-repo and agent-behaviour notes live in `nexus-home/.agents/memory/INDEX.md`.

- [edge-pipeline-feature-gated-tests](edge-pipeline-feature-gated-tests.md): recorder, ingester and webrtc tests need `--features gstreamer-webrtc`
- [linux-clippy-in-colima](linux-clippy-in-colima.md): run every CI clippy line on Linux before pushing a lint change
- [defender-holds-test-binary-udp](defender-holds-test-binary-udp.md): Defender can hold UDP from fresh test binaries on a Mac
