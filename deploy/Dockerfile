# syntax=docker/dockerfile:1.7
# ---------------------------------------------------------------------------
# Single multi-stage Dockerfile for the Nexus engine.
#
#  stage 1: build the TypeScript UI with Node/Vite
#  stage 2: build the Rust binary (engine + probe) with GStreamer + ORT headers
#  stage 3: minimal runtime — debian-slim + GStreamer runtime + ORT shared libs
#
# Output layout in the runtime image:
#   /usr/local/bin/nexus-engine
#   /usr/local/bin/nexus-probe
#   /usr/share/nexus/ui/...      (served by tower_http::ServeDir)
#   /etc/nexus/nexus.toml        (mount your own config over this)
# ---------------------------------------------------------------------------

# ----------------------- stage 1: UI -------------------------------
FROM node:22-alpine AS ui-build
WORKDIR /work
COPY ui/package.json ui/package.json
RUN cd ui && npm install --no-fund --no-audit --silent
COPY ui ui
RUN cd ui && npm run build

# ----------------------- stage 2: Rust -----------------------------
FROM rust:1.82-slim-bookworm AS rust-build
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config build-essential cmake git ca-certificates curl \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-libav \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Pin ORT version. The runtime image ships ORT shared libs (load-dynamic
# at runtime via LD_LIBRARY_PATH).
#
# Must track the `ort` crate's ABI requirement — `ort 2.0.0-rc.10` calls
# GetVersionString() at session creation and panics if it's not in the 1.22.x
# line. Bump this in lock-step with the workspace `ort` dependency.
#
# Two-step layering:
#   1. Download Microsoft's official onnxruntime-linux-x64-${ORT_VERSION}.tgz
#      (CPU-only, but ships headers + a clean libonnxruntime.so) so the Rust
#      build can link.
#   2. AFTER the Rust build, overlay the *same-version* libs from the
#      onnxruntime-openvino PyPI wheel — that wheel bundles a
#      libonnxruntime.so.${ORT_VERSION} compiled with the OpenVINO EP +
#      libonnxruntime_providers_openvino.so + the full OpenVINO runtime
#      (libopenvino*.so + intel CPU/GPU/NPU plugins + TBB). Drop them into
#      /opt/onnxruntime/lib/ where the existing ORT load-dynamic path
#      already looks for them.
#
# Net effect: stage 2's cargo link uses the small CPU tarball; stage 3 ends
# up with the fat OpenVINO-enabled libs at exactly the same SONAME — so
# `ort::Session::run` finds the OpenVINO provider when ep_priority asks
# for it, and silently falls back to CPU when it doesn't.
ARG ORT_VERSION=1.22.0
RUN curl -fsSL https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-x64-${ORT_VERSION}.tgz \
    | tar -xz -C /opt && mv /opt/onnxruntime-linux-x64-${ORT_VERSION} /opt/onnxruntime

# M-Install Checkpoint 2 — kitchen-sink EP build. The runtime image
# always ships the same set of binaries; which execution providers
# they're linked against is decided at build time via this ARG.
# The default is the empty set ("just CPU + stub ORT"), matching
# the M0/M1 dev cadence. The release pipeline overrides this to
# `ort,ep-cpu,ep-openvino,ep-cuda,ep-tensorrt` so a single image
# works on every tier from T10 (CPU only) through T64 (NVIDIA).
#
# The features are forwarded both to nexus-engine (for its own
# `gstreamer` flag) and to nexus-inference via cargo's --features
# resolution against workspace deps. ORT shared libs are pulled
# from /opt/onnxruntime at runtime regardless; only the cargo-side
# wiring changes between feature sets.
ARG FEATURES=

WORKDIR /work
# Copy manifests first for better layer caching.
COPY rust-toolchain.toml Cargo.toml ./
COPY crates/nexus-types/Cargo.toml      crates/nexus-types/
COPY crates/nexus-config/Cargo.toml     crates/nexus-config/
COPY crates/nexus-bus/Cargo.toml        crates/nexus-bus/
COPY crates/nexus-telemetry/Cargo.toml  crates/nexus-telemetry/
COPY crates/nexus-store/Cargo.toml      crates/nexus-store/
COPY crates/nexus-rules/Cargo.toml      crates/nexus-rules/
COPY crates/nexus-tracker/Cargo.toml    crates/nexus-tracker/
COPY crates/nexus-inference/Cargo.toml  crates/nexus-inference/
COPY crates/nexus-pipeline/Cargo.toml   crates/nexus-pipeline/
COPY crates/nexus-engine/Cargo.toml     crates/nexus-engine/
COPY crates/nexus-probe/Cargo.toml      crates/nexus-probe/

# Now the sources.
COPY crates crates

# Build the binaries. nexus-engine takes the FEATURES override
# (forwarded into nexus-inference for ORT + per-EP wiring); nexus-probe
# has no EP-relevant features so it builds with workspace defaults.
# Two separate `cargo build` calls because `--features` at workspace
# root requires `-p` or a single-package selection.
RUN cargo build --release -p nexus-engine --features "${FEATURES}" --bin nexus-engine \
    && cargo build --release -p nexus-probe --bin nexus-probe

# ----------------------- stage 2b: OpenVINO overlay ----------------
# Extract the matching-ABI OpenVINO-enabled runtime libs from the
# `onnxruntime-openvino` PyPI wheel. The wheel ships
# libonnxruntime.so.${ORT_VERSION} compiled with the OpenVINO EP plus
# libonnxruntime_providers_openvino.so + the full OpenVINO runtime
# (libopenvino*.so, intel CPU/GPU/NPU plugins, TBB) — all self-contained
# in `onnxruntime/capi/`. No apt repo, no Intel SDK install.
#
# We *replace* /opt/onnxruntime/lib's libonnxruntime.so* with the wheel's
# fatter copy and *add* the OpenVINO provider + plugin .so files. The
# Microsoft tarball's headers under /opt/onnxruntime/include/ are kept
# untouched — they already match the 1.22 ABI.
#
# The wheel's libonnxruntime.so.${ORT_VERSION} is the versioned soname
# only; the ort crate dlopens "libonnxruntime.so" (unversioned), so we
# add the symlink explicitly.
FROM rust-build AS ort-openvino
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip \
    && pip3 install --break-system-packages --no-cache-dir \
        --target=/tmp/ovwheel "onnxruntime-openvino==${ORT_VERSION}" \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    SRC=/tmp/ovwheel/onnxruntime/capi; \
    DST=/opt/onnxruntime/lib; \
    ls -la "$SRC" | head -40; \
    # Overlay every shared lib (preserves symlinks).
    cp -aP "$SRC"/*.so* "$DST"/; \
    # Ensure the unversioned soname exists for ort's load-dynamic dlopen.
    if [ ! -e "$DST/libonnxruntime.so" ]; then \
        ln -s "libonnxruntime.so.${ORT_VERSION}" "$DST/libonnxruntime.so"; \
    fi; \
    echo "--- /opt/onnxruntime/lib after OpenVINO overlay ---"; \
    ls -la "$DST" | head -60; \
    rm -rf /tmp/ovwheel

# ----------------------- stage 3: runtime --------------------------
FROM debian:bookworm-slim AS runtime
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tini \
    libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-libav gstreamer1.0-tools \
    libssl3 \
    # OpenVINO iGPU needs an OpenCL ICD loader to talk to the
    # Intel compute-runtime kernel driver (`i915` / `xe`) via
    # /dev/dri/renderD12x. The Intel userspace itself is bundled
    # by the onnxruntime-openvino wheel (libopenvino_intel_gpu_plugin.so);
    # only the OpenCL dispatcher is system-side.
    ocl-icd-libopencl1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home --shell /usr/sbin/nologin nexus

# ORT shared libs (load-dynamic). The OpenVINO-enabled libonnxruntime.so
# + provider + OpenVINO runtime libs come from the ort-openvino stage,
# which layered them on top of the Microsoft tarball checked out by
# stage 2 (rust-build).
COPY --from=ort-openvino /opt/onnxruntime /opt/onnxruntime
ENV LD_LIBRARY_PATH=/opt/onnxruntime/lib:${LD_LIBRARY_PATH}

# Engine + probe binaries.
COPY --from=rust-build /work/target/release/nexus-engine /usr/local/bin/nexus-engine
COPY --from=rust-build /work/target/release/nexus-probe  /usr/local/bin/nexus-probe

# UI assets — served at `/` by the engine.
COPY --from=ui-build /work/ui/dist /usr/share/nexus/ui

# Default model pack — baked into the image so a fresh `docker compose up`
# works without §6.4 host-side model staging. ~100 MB total (yolo26n ~10 MB,
# yolo_world ~48 MB, yoloe26_s ~42 MB, manifest ~7 KB). Mounted read-only via
# the image layer; the tier configs in `config/tiers/` point `pack_path` here.
# Operators with custom model packs override by setting
# `[inference.model].pack_path = "/var/lib/nexus/models"` in their
# nexus.toml and bind-mounting `/var/lib/nexus/models/` over the read-only
# image dir. Path mirrors `/usr/share/nexus/ui/` so neither is shadowed
# by the `/var/lib/nexus` state bind from the per-tier overlays.
#
# `models/` is gitignored — release builds and CI bake the files in by
# placing them in the build context before `docker build`. From-source
# operator builds either (a) scp / generate the pack into models/ per
# §6.4 before building, or (b) build with an empty models/ (just the
# tracked .gitkeep) and bind-mount /var/lib/nexus/models at runtime.
# `COPY models/ /usr/share/nexus/models/` copies whatever's there —
# empty is fine, the engine then expects the runtime bind-mount.
RUN mkdir -p /usr/share/nexus/models
COPY models/ /usr/share/nexus/models/

# Default config + state dirs.
RUN mkdir -p /etc/nexus /var/lib/nexus && chown -R nexus:nexus /var/lib/nexus
COPY config/single-camera.toml /etc/nexus/nexus.toml

USER nexus
EXPOSE 8089
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/nexus-engine"]
CMD ["--config", "/etc/nexus/nexus.toml"]
