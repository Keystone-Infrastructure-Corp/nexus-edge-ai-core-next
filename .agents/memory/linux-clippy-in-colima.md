---
name: linux-clippy-in-colima
description: Before pushing a lint or clippy change, run every CI clippy invocation on Linux in a Colima container; macOS never compiles the target_os=linux branches
metadata:
  type: feedback
---

The edge has about 123 `#[cfg(target_os = "linux")]` gates, and
`nexus-hailo-backend --features linked` compiles a stub on macOS. A green macOS clippy
run proves nothing for those branches (see AGENTS.md). When the workspace lint policy
was tightened, CI failed twice: first `gpu.rs` and `network/apply.rs`, then the hailo
`ffi.rs`. CI's rust job stops at its first failing step, so each push revealed only the
next step's errors.

**Why:** fixing one CI round at a time is symptom-fixing. Enumerate the whole Linux
matrix once.

**How to apply:**

- Commit, then
  `git archive <sha> | docker run --rm -i -v <target-vol>:/target -v <cargo-vol>:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target rust:1-slim-bookworm bash -c 'mkdir /src && tar -x -C /src && cd /src && find . -name "*.rs" -exec touch {} + && rustup toolchain install <ci-rust-version> --profile minimal -c clippy; cargo +<ci-rust-version> clippy ...'`.
  Point `DOCKER_HOST` at the Colima socket.
- Run every clippy line in the `ci.yml` rust job: workspace, hailo `linked`, and the
  nexus-inference EP list.
- The `touch` is required. `git archive` stamps commit times, so an older commit's files
  look fresh and cargo prints Finished in 0.2s without checking anything.
- Colima only shares the home directory with its VM, so a worktree outside it goes in
  through `git archive`.
- The container is aarch64 and CI is x86-64. `c_char` is u8 there, so a `c as u8` cast
  in the hailo code raises `unnecessary_cast` only in the container. Pass
  `-A clippy::unnecessary_cast` and do not "fix" it.
- Delete the volumes afterwards.
