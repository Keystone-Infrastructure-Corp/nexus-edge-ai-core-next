#!/usr/bin/env bash
# Shared helpers for scripts/install.sh, scripts/uninstall.sh, and
# scripts/bootstrap.sh.  POSIX-ish bash (we rely on `set -o pipefail`
# and `[[ ]]`); intentionally no zsh-isms — install.sh has to run on a
# fresh Ubuntu Server box where `bash` is the only certain shell.
#
# This file is sourced, never executed.  Every function it defines is
# idempotent so install.sh can be re-run to upgrade in place.

set -euo pipefail

# --- Paths --------------------------------------------------------------------

# Customisable via env so test harnesses can install into a tmpdir.
NEXUS_PREFIX="${NEXUS_PREFIX:-/opt/nexus}"
NEXUS_CONFIG_DIR="${NEXUS_CONFIG_DIR:-/etc/nexus}"
NEXUS_STATE_DIR="${NEXUS_STATE_DIR:-/var/lib/nexus}"
NEXUS_SERVICE_USER="${NEXUS_SERVICE_USER:-nexus}"
NEXUS_SERVICE_GROUP="${NEXUS_SERVICE_GROUP:-nexus}"
NEXUS_SYSTEMD_DIR="${NEXUS_SYSTEMD_DIR:-/etc/systemd/system}"

# --- Logging ------------------------------------------------------------------

_color() { [[ -t 1 ]] && printf '\033[%sm' "$1" || true; }
_reset() { [[ -t 1 ]] && printf '\033[0m' || true; }

log()  { printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" "$*"; }
warn() { printf '%s[nexus]%s %s\n' "$(_color '1;33')" "$(_reset)" "$*" >&2; }
err()  { printf '%s[nexus]%s %s\n' "$(_color '1;31')" "$(_reset)" "$*" >&2; }
die()  { err "$*"; exit 1; }

# --- Pre-flight ---------------------------------------------------------------

require_root() {
    if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
        die "must run as root (try: sudo $0 $*)"
    fi
}

require_cmd() {
    local cmd
    for cmd in "$@"; do
        command -v "$cmd" >/dev/null 2>&1 \
            || die "required command '$cmd' not found on PATH"
    done
}

# Linux x86_64 only for now.  Add an arm64 release in a follow-up.
require_linux_x86_64() {
    local kernel arch
    kernel="$(uname -s)"
    arch="$(uname -m)"
    [[ "$kernel" == "Linux"  ]] || die "only Linux is supported (saw: $kernel)"
    [[ "$arch"   == "x86_64" ]] || die "only x86_64 is supported (saw: $arch)"
}

# Best-effort Ubuntu detection.  Other glibc distros may work but we
# only ship-test on Ubuntu 24.04.
detect_distro_id() {
    if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091
        ( . /etc/os-release && printf '%s' "${ID:-unknown}" )
    else
        printf 'unknown'
    fi
}

# --- User + dirs --------------------------------------------------------------

ensure_user() {
    if ! id -u "$NEXUS_SERVICE_USER" >/dev/null 2>&1; then
        log "creating service user: $NEXUS_SERVICE_USER"
        useradd --system \
            --home-dir "$NEXUS_STATE_DIR" \
            --shell /usr/sbin/nologin \
            "$NEXUS_SERVICE_USER"
    fi
}

ensure_dirs() {
    install -d -o root                  -g root                  -m 0755 "$NEXUS_PREFIX"
    install -d -o root                  -g root                  -m 0755 "$NEXUS_PREFIX/releases"
    install -d -o root                  -g root                  -m 0755 "$NEXUS_CONFIG_DIR"
    # M-HTTPS Phase 1 — staging dir for the in-process TLS
    # listener's cert+key. Mode 2750 (setgid) so any cert
    # files dropped here later inherit the nexus group; the
    # service user reads them via group permission (key is
    # 0640). Owner stays root so only root (the installer
    # and the `tls init` invocation) can write the PEMs.
    install -d -o root                  -g "$NEXUS_SERVICE_GROUP" -m 2750 "$NEXUS_CONFIG_DIR/tls"
    install -d -o "$NEXUS_SERVICE_USER" -g "$NEXUS_SERVICE_GROUP" -m 0750 "$NEXUS_STATE_DIR"
    install -d -o "$NEXUS_SERVICE_USER" -g "$NEXUS_SERVICE_GROUP" -m 0750 "$NEXUS_STATE_DIR/state"
    install -d -o "$NEXUS_SERVICE_USER" -g "$NEXUS_SERVICE_GROUP" -m 0750 "$NEXUS_STATE_DIR/clips"

    # Self-heal ownership of any pre-existing contents under the state
    # tree. `install -d` only sets the directory's own owner — files
    # inside (e.g. an `admin-secret` auto-provisioned by a prior `sudo
    # nexus-engine` invocation, or migrated from an older layout where
    # the engine ran as root) keep whatever owner they had. If a single
    # file is left root-owned the service (running as `nexus`) gets
    # EACCES on startup and the unit crash-loops with "reading admin
    # secret … Permission denied". Recursive chown is safe here because
    # both subtrees are owned entirely by the service user by design.
    chown -R "$NEXUS_SERVICE_USER:$NEXUS_SERVICE_GROUP" "$NEXUS_STATE_DIR/state" "$NEXUS_STATE_DIR/clips"
}

# Add the service user to `render` and `video` groups so the engine can
# open `/dev/dri/renderD128` (every iGPU/dGPU tier) and `/dev/accel/accel0`
# (T36-S NPU). Also adds `hailo` (created by the HailoRT .deb postinst
# on T24 boxes) so the engine can open `/dev/hailo0`. Idempotent:
# usermod -aG is a no-op if the user is already a member. Groups that
# don't exist on the host are silently skipped — `render` only exists
# once the GPU userspace from §5 is installed, but the install script
# may run before that on a freshly-flashed box. Same for `hailo` —
# created only after HailoRT installs (T24-EQR7 boxes only); call this
# again after `install_drivers` to pick it up.
#
# HailoRT 4.23.x ships a udev rule that leaves /dev/hailo* WORLD-rw
# (MODE="0666", root:root, no group). `install.sh` overrides that with
# its own 99-nexus-hailo.rules that group-gates the node to `video`
# 0660 (`_hailo_install_udev_rule`), so `video` membership is what lets
# the service user open /dev/hailo0 on a modern install. We keep the
# `hailo` group on the optional list for back-compat with operators
# still on HailoRT ≤ 4.20, whose postinst DID create a `hailo` group —
# it's only added if that group actually exists on the host.
ensure_accelerator_groups() {
    local group required
    for group in render:required video:required hailo:optional; do
        required="${group#*:}"
        group="${group%:*}"
        if getent group "$group" >/dev/null 2>&1; then
            if id -nG "$NEXUS_SERVICE_USER" | tr ' ' '\n' | grep -qx "$group"; then
                log "service user $NEXUS_SERVICE_USER already in $group"
            else
                usermod -aG "$group" "$NEXUS_SERVICE_USER"
                log "added $NEXUS_SERVICE_USER to $group group"
            fi
        elif [[ "$required" == "required" ]]; then
            log "group '$group' does not exist on host yet (install GPU userspace first); skipping"
        fi
    done
}

# --- System preparation (idempotent OS hardening + prereq install) -----------
#
# Runs the boilerplate steps that every bare-metal install needs anyway:
#
#   * `apt update` (only if the cache is older than 24 h)
#   * apt-installs runtime prerequisites that are NOT inside the tarball:
#       - GStreamer runtime plugins (clip recorder needs these — without
#         them every motion event writes a 0-byte mp4 and the UI shows
#         "no playable data")
#       - VA-API userspace driver (`va-driver-all` pulls iHD for Gen9+
#         Intel + i965 fallback + Mesa for AMD; without it both
#         `gstreamer1.0-vaapi` and the modern `va` plugin register 0
#         features and the clip recorder's `vaapih264enc` element fails
#         to construct, silently falling back to software `x264enc`
#         which pegs a CPU core per camera on 1080p+)
#       - chrony (clip timestamps + alert correlation get ugly past 1 s
#         drift; the install banner refuses to declare success if
#         `timedatectl status` is `unsynchronized`)
#       - ufw + jq + curl + python3 (script + manifest plumbing)
#   * Adds an `nftables`-backed `ufw` rule pair for the engine's two
#     listeners (80 + 8089) only if ufw is already enabled — we never
#     enable ufw ourselves because doing so on an ssh-connected box
#     without OpenSSH allow first is a fleet-bricking foot-gun.
#   * Creates an 8 GB swap file at /swapfile (only if /proc/swaps is
#     empty — preserves any existing LVM/partition swap).
#
# Each sub-step is independently togglable via a flag so an operator
# who already has a hardened image can skip the parts they own. The
# whole function is gated behind `--skip-system-prep` for the
# all-or-nothing case.
#
# Returns 0 unconditionally — none of these prep steps are install
# blockers. Warnings are emitted but never escalate to die().
system_prep() {
    local install_deps="${NEXUS_PREP_DEPS:-1}"
    local install_swap="${NEXUS_PREP_SWAP:-1}"
    local install_firewall="${NEXUS_PREP_FIREWALL:-1}"
    local install_autoupdates="${NEXUS_PREP_AUTO_UPDATES:-0}"

    log "preparing host (idempotent — pass --skip-system-prep to bypass)"

    if (( install_deps )); then
        _system_prep_apt
    else
        log "skipping apt prereqs (NEXUS_PREP_DEPS=0)"
    fi

    if (( install_swap )); then
        _system_prep_swap
    else
        log "skipping swap setup (NEXUS_PREP_SWAP=0)"
    fi

    if (( install_firewall )); then
        _system_prep_firewall
    else
        log "skipping firewall rules (NEXUS_PREP_FIREWALL=0)"
    fi

    if (( install_autoupdates )); then
        _system_prep_unattended_upgrades
    fi
}

# apt-install the runtime prereqs the tarball does NOT bundle. Skips
# `apt update` if the package cache has been refreshed in the last 24 h
# (avoids a 10–30 s network hit on every install.sh re-run).
_system_prep_apt() {
    if ! command -v apt-get >/dev/null 2>&1; then
        warn "no apt-get on PATH; skipping apt prep (non-Debian-family distro?)"
        return 0
    fi

    # /var/cache/apt/pkgcache.bin mtime is the canonical "last apt update"
    # timestamp. Older than 24 h → refresh. Missing → refresh.
    local cache=/var/cache/apt/pkgcache.bin
    local stale=1
    if [[ -r "$cache" ]]; then
        local age_secs
        age_secs=$(( $(date +%s) - $(stat -c %Y "$cache" 2>/dev/null || stat -f %m "$cache" 2>/dev/null || echo 0) ))
        if (( age_secs < 86400 )); then
            stale=0
        fi
    fi
    if (( stale )); then
        log "apt-get update (cache > 24 h old or missing)"
        DEBIAN_FRONTEND=noninteractive apt-get update -qq
    fi

    # Two install groups to keep the noise grep-able in the install log.
    log "installing GStreamer runtime + script prereqs"
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        gstreamer1.0-tools \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-libav \
        gstreamer1.0-vaapi \
        va-driver-all \
        vainfo \
        chrony ufw \
        curl jq python3 ca-certificates \
        || warn "apt-get install returned non-zero — continuing, but motion clips may not record"

    if ! systemctl is-active --quiet chrony; then
        log "enabling chrony for NTP sync"
        systemctl enable --now chrony || warn "could not enable chrony"
    fi
}

# Allocate an 8 GB swap file at /swapfile IF the box has no active
# swap. Idempotent: a second invocation sees `swapon --show` non-empty
# and does nothing.
_system_prep_swap() {
    if [[ -s /proc/swaps && $(awk 'NR>1' /proc/swaps | wc -l) -gt 0 ]]; then
        log "swap already configured ($(awk 'NR>1 {print $1; exit}' /proc/swaps)); skipping"
        return 0
    fi
    if [[ -e /swapfile ]]; then
        warn "/swapfile exists but is not active; leaving it alone"
        return 0
    fi
    log "allocating 8 GB swap at /swapfile"
    if ! fallocate -l 8G /swapfile 2>/dev/null; then
        # fallocate fails on tmpfs / some fs types; fall back to dd.
        warn "fallocate failed; falling back to dd (slower)"
        dd if=/dev/zero of=/swapfile bs=1M count=8192 status=none || {
            warn "dd swap allocation failed; skipping swap"
            rm -f /swapfile
            return 0
        }
    fi
    chmod 0600 /swapfile
    mkswap -q /swapfile >/dev/null
    swapon /swapfile
    if ! grep -qE '^/swapfile[[:space:]]' /etc/fstab; then
        printf '/swapfile none swap sw 0 0\n' >> /etc/fstab
        log "added /swapfile to /etc/fstab"
    fi
}

# Add ufw rules for the engine's two TCP listeners IF ufw is enabled.
# We never enable ufw ourselves: doing so on an ssh-connected fresh
# install without an OpenSSH allow rule first locks the operator out.
# If ufw is inactive, log + return — the operator can run
# `ufw enable` later and re-run install.sh to pick up the rules.
_system_prep_firewall() {
    if ! command -v ufw >/dev/null 2>&1; then
        log "ufw not installed; skipping firewall rules"
        return 0
    fi
    if ! ufw status 2>/dev/null | grep -q 'Status: active'; then
        log "ufw is inactive; skipping rules (enable ufw + re-run install.sh)"
        return 0
    fi
    log "adding ufw rules for engine ports (80/tcp UI alias, 443/tcp HTTPS, 8089/tcp API)"
    ufw allow 80/tcp   comment 'nexus-engine UI alias'  >/dev/null 2>&1 || true
    ufw allow 443/tcp  comment 'nexus-engine HTTPS'     >/dev/null 2>&1 || true
    ufw allow 8089/tcp comment 'nexus-engine API + UI' >/dev/null 2>&1 || true
}

# Optional: configure unattended-upgrades for security patches. Off
# by default because some operators centralise patch management; opt
# in with --enable-auto-updates.
_system_prep_unattended_upgrades() {
    if ! command -v apt-get >/dev/null 2>&1; then
        return 0
    fi
    log "installing + enabling unattended-upgrades (security patches only)"
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        unattended-upgrades || { warn "unattended-upgrades install failed"; return 0; }
    # Force the periodic enable (equivalent to dpkg-reconfigure -plow).
    cat >/etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF
    # Disable auto-reboot — losing in-flight clips on a midnight reboot
    # is worse than carrying an extra day of patch lag.
    if [[ -r /etc/apt/apt.conf.d/50unattended-upgrades ]]; then
        sed -i \
            's#^//\?\s*Unattended-Upgrade::Automatic-Reboot\s.*#Unattended-Upgrade::Automatic-Reboot "false";#' \
            /etc/apt/apt.conf.d/50unattended-upgrades
    fi
}

# --- Hardware detection + driver install --------------------------------------
#
# What this provides (paired with §5 in docs/INSTALL.md):
#
#   intel-igpu      → kobuk-team PPA + iGPU + media + compute stack.
#                     Covers UHD (Alder Lake-N / T10), Iris Xe (T24),
#                     Arc 140V (Lunar Lake / T36-S iGPU side),
#                     Arc Graphics (Meteor Lake).
#   intel-arc-dgpu  → identical PPA + package set; same DG2 stack
#                     works for the A380 (T36). The PPA already
#                     handles firmware via linux-firmware updates.
#   intel-npu       → upstream linux-npu-driver tarball v1.32.1
#                     (4 .deb files). Preconditions: Lunar Lake or
#                     Meteor Lake hardware AND kernel >= 6.10. If
#                     kernel is too old we install linux-generic-
#                     hwe-24.04 and exit asking for a reboot.
#   nvidia-gpu      → skipped with a warning. T64 lands when M5
#                     ships the CUDA / TensorRT EPs.
#
# All sub-steps are idempotent: re-running install.sh sees the
# packages already present and short-circuits.
#
# The orchestrator is gated on `NEXUS_INSTALL_DRIVERS` (default 1).
# Operators with hardened images / golden disks pass `--no-drivers`.
install_drivers() {
    local enable="${NEXUS_INSTALL_DRIVERS:-1}"
    if (( ! enable )); then
        log "skipping driver install (NEXUS_INSTALL_DRIVERS=0)"
        return 0
    fi
    if ! command -v apt-get >/dev/null 2>&1; then
        warn "no apt-get on PATH; skipping driver install (non-Debian distro?)"
        return 0
    fi

    log "detecting accelerator hardware"
    local tags
    tags="$(_detect_hardware)" || {
        warn "hardware detection failed; skipping driver install"
        return 0
    }
    if [[ -z "$tags" ]]; then
        log "no recognised accelerators detected; CPU-only fallback in effect"
        return 0
    fi
    log "detected: $(echo "$tags" | tr '\n' ' ')"

    local has_igpu=0 has_arc=0 has_npu=0 has_nvidia=0 has_amd=0 has_hailo=0
    while IFS= read -r tag; do
        case "$tag" in
            intel-igpu)     has_igpu=1 ;;
            intel-arc-dgpu) has_arc=1 ;;
            intel-npu)      has_npu=1 ;;
            nvidia-gpu)     has_nvidia=1 ;;
            amd-igpu)       has_amd=1 ;;
            hailo-m2)       has_hailo=1 ;;
        esac
    done <<<"$tags"

    # NPU prerequisite check FIRST. If we'd need an HWE kernel
    # upgrade, do that and exit so the operator can reboot — the
    # NPU driver install requires the new kernel's uAPI.
    if (( has_npu )) && ! _kernel_at_least 6 10; then
        warn "NPU hardware detected but running kernel $(uname -r) (< 6.10)"
        log  "installing linux-generic-hwe-24.04 (HWE kernel) for NPU support"
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
            linux-generic-hwe-24.04 || {
                warn "HWE kernel install failed; skipping NPU driver"
                has_npu=0
            }
        if (( has_npu )); then
            warn ""
            warn "========================================================="
            warn "REBOOT REQUIRED — HWE kernel staged for NPU support."
            warn ""
            warn "After reboot, re-run the same install.sh one-liner; it"
            warn "will skip everything already installed and proceed with"
            warn "the NPU driver + engine install."
            warn "========================================================="
            warn ""
            exit 0
        fi
    fi

    # iGPU + dGPU share the same PPA + package list — install once if
    # either is present. The PPA provides iHD 25.x (kernel 6.11+ uAPI)
    # and libze 25.x which the Ubuntu archive does NOT carry.
    if (( has_igpu || has_arc )); then
        _drivers_intel_graphics
    fi

    # NPU after iGPU is fine but no longer required: _drivers_intel_npu
    # explicitly calls _ensure_libze1 itself so an NPU-only box (no
    # iGPU at all) still gets the Level Zero loader.
    if (( has_npu )); then
        _drivers_intel_npu
    fi

    if (( has_nvidia )); then
        warn "NVIDIA GPU detected — T64 tier lands when M5 ships the CUDA / TensorRT"
        warn "execution providers. Skipping nvidia driver install for now; engine"
        warn "will run on the CPU EP fallback. See docs/INSTALL.md §5.4."
    fi

    if (( has_amd )); then
        _drivers_amd_graphics
        # AMD GPU execution-provider selection:
        #   * `--force-profile amd-vulkan`  -> Vulkan(WebGPU), operator's choice
        #   * officially ROCm-supported GPU -> ROCm EP (discrete RDNA/CDNA)
        #   * everything else (iGPUs etc.)  -> Vulkan(WebGPU), no ROCm force-fit
        # `_amd_gpu_rocm_supported` classifies ROCm-free, so we never install
        # the multi-hundred-MB ROCm stack on a GPU it cannot drive, and never
        # depend on the HSA_OVERRIDE_GFX_VERSION force-fit (now an unsupported
        # manual escape hatch only). The classification is the Rust nexus-probe
        # allowlist, queried via `nexus-probe accel-tags`, so the
        # ROCm-vs-Vulkan decision and the generated ep_priority come from one
        # source of truth.
        if [[ "${FORCE_PROFILE:-}" == "amd-vulkan" ]] || ! _amd_gpu_rocm_supported; then
            if [[ "${FORCE_PROFILE:-}" == "amd-vulkan" ]]; then
                log "AMD: '--force-profile amd-vulkan' selected; using Vulkan(WebGPU) EP"
            else
                log "AMD: GPU not on the ROCm-supported allowlist; using Vulkan(WebGPU) EP"
            fi
            # Install the Vulkan diagnostic userspace and a WebGPU-capable
            # ONNX Runtime (1.27.0, Dawn->Vulkan). No-op (CPU fallback) if no
            # Vulkan ICD / render node is present.
            _drivers_amd_vulkan
            _install_ort_webgpu
        else
            log "AMD: GPU on the ROCm-supported allowlist; using ROCm EP"
            # ROCm compute for inference (officially-supported discrete GPU).
            # Gracefully skips if amdgpu module is not yet bound.
            _drivers_amd_rocm
            # Fetch a ROCm-capable ONNX Runtime (1.21.0) and wire the engine
            # to load it. No-op when /dev/kfd / rocminfo are absent.
            _install_ort_rocm
        fi
    fi

    if (( has_hailo )); then
        _drivers_hailo_pcie
    fi
}

# Probe each accelerator class the engine will try to attach to.
# Re-detects via lspci so the caller doesn't have to thread the
# hardware tags through. Call AFTER ensure_user + ensure_accelerator_groups
# so the service-user open() probe is meaningful.
#
# Non-fatal — every check emits a `warn` with a remediation hint
# instead of die(). Rationale: the engine has `fail_soft=true` and
# will fall back to the CPU EP, so a missing-userspace situation
# should produce a loud install banner but not block the install.
verify_accelerators() {
    if ! command -v lspci >/dev/null 2>&1; then
        log "verify_accelerators: lspci unavailable; skipping accelerator probes"
        return 0
    fi

    local tags
    tags="$(_detect_hardware 2>/dev/null)" || return 0
    if [[ -z "$tags" ]]; then
        log "verify_accelerators: no accelerators detected; CPU-only install (nothing to verify)"
        return 0
    fi

    local has_igpu=0 has_arc=0 has_npu=0 has_amd=0 has_hailo=0
    while IFS= read -r tag; do
        case "$tag" in
            intel-igpu)     has_igpu=1 ;;
            intel-arc-dgpu) has_arc=1 ;;
            intel-npu)      has_npu=1 ;;
            amd-igpu)       has_amd=1 ;;
            hailo-m2)       has_hailo=1 ;;
        esac
    done <<<"$tags"

    log ""
    log "===== Accelerator verification ====="
    if (( has_igpu || has_arc )); then
        _verify_intel_gpu_userspace || true
    fi
    if (( has_npu )); then
        _verify_intel_npu_userspace || true
    fi
    if (( has_amd )); then
        _verify_amd_gpu_userspace || true
    fi
    if (( has_hailo )); then
        _verify_hailo_userspace || true
    fi
    log "===================================="
    log ""
}

# Verify GStreamer can actually instantiate the VA hardware decode
# elements the pipeline selects (`vah264dec` / `vah265dec`, provided
# by gstreamer1.0-plugins-bad). This is distinct from the `vainfo`
# probe: vainfo proves the libva *driver* enumerates the GPU, while
# this proves the GStreamer *plugin* sees that driver and the
# `[runtime.decode] mode = "va"` / "auto" chain in preroll_ingester.rs
# will negotiate at launch instead of silently falling back to the
# software `avdec_*` decoder. Bonus probe — hardware decode is
# optional and the engine fails open to software, so this logs
# success/info without affecting the caller's return code.
_verify_va_gst_decoder() {
    if ! command -v gst-inspect-1.0 >/dev/null 2>&1; then
        log "  [info] gst-inspect-1.0 not on PATH (gstreamer1.0-tools missing); skipping VA decoder probe"
        return 0
    fi
    local missing=()
    local dec
    for dec in vah264dec vah265dec; do
        if ! gst-inspect-1.0 "$dec" >/dev/null 2>&1; then
            missing+=("$dec")
        fi
    done
    if (( ${#missing[@]} == 0 )); then
        log "  [ OK ] GStreamer VA decoders loadable (vah264dec, vah265dec) -- hardware decode chain available"
    else
        log "  [info] GStreamer VA decoders unavailable (${missing[*]}); pipeline will fall back to software decode"
        log "         (gstreamer1.0-plugins-bad missing, or libva driver not visible to GStreamer)"
    fi
    return 0
}

# Verify the iGPU / Arc dGPU userspace the engine will load:
#   1. intel-opencl-icd (NEO compute runtime) enumerates the device
#      \u2014 this is the ground-truth probe the engine's OpenVINO GPU
#      plugin uses internally. A missing ICD here is the 192.168.1.99
#      bug.
#   2. libze_loader.so.1 (oneAPI Level Zero) is in ldconfig's cache
#      \u2014 required by OV 2025.4.x GPU/NPU plugins for device init.
#   3. The systemd service user can actually open
#      /dev/dri/renderD128 \u2014 catches a missing `render` group
#      membership that would otherwise only surface at engine
#      startup as `Device GPU is not available`.
# Returns 0 if every required probe passes, 1 otherwise. VA-API
# (vainfo) is a *bonus* probe \u2014 hardware-decode is optional, the
# engine works without it \u2014 so we log success/warn without
# affecting the return code.
_verify_intel_gpu_userspace() {
    local ok=1

    log "verifying Intel GPU userspace..."

    if ! command -v clinfo >/dev/null 2>&1; then
        warn "  [FAIL] clinfo not installed (intel-opencl-icd / clinfo packages missing)"
        warn "         fix: sudo apt-get install -y intel-opencl-icd clinfo"
        ok=0
    elif ! clinfo -l 2>/dev/null \
            | grep -qE 'Intel\(R\) (Iris|HD|UHD|Arc|Graphics)'; then
        warn "  [FAIL] OpenCL ICD does not enumerate any Intel GPU device"
        warn "         (clinfo -l finds no platform \u2014 intel-opencl-icd missing,"
        warn "         broken, or i915/xe kernel module not bound to the device)"
        warn "         OpenVINO GPU plugin will report 'Device GPU is not available'"
        warn "         and the engine will silently fall back to the CPU EP."
        warn "         fix: sudo apt-get install -y --reinstall intel-opencl-icd libze-intel-gpu1"
        ok=0
    else
        local dev
        dev="$(clinfo -l 2>/dev/null | grep -oE 'Intel\(R\) [A-Z][^[:space:]]+( [A-Z][^[:space:]]+)*' | head -n1)"
        log "  [ OK ] OpenCL ICD enumerates Intel GPU: ${dev:-Intel device}"
    fi

    if ! ldconfig -p 2>/dev/null | grep -q 'libze_loader.so.1'; then
        warn "  [FAIL] libze_loader.so.1 not in ldconfig cache (libze1 missing)"
        warn "         OpenVINO GPU/NPU plugins will fail to enumerate devices."
        warn "         fix: sudo apt-get install -y libze1   (from kobuk-team PPA)"
        ok=0
    else
        log "  [ OK ] libze_loader.so.1 present (oneAPI Level Zero loader)"
    fi

    if [[ -e /dev/dri/renderD128 ]]; then
        if sudo -u "$NEXUS_SERVICE_USER" \
                bash -c 'exec 9</dev/dri/renderD128 && exec 9<&-' 2>/dev/null; then
            log "  [ OK ] service user $NEXUS_SERVICE_USER can open /dev/dri/renderD128"
        else
            warn "  [FAIL] service user $NEXUS_SERVICE_USER cannot open /dev/dri/renderD128"
            warn "         (missing render-group membership or wrong device perms)"
            warn "         fix: sudo usermod -aG render $NEXUS_SERVICE_USER && sudo systemctl restart nexus-engine"
            ok=0
        fi
    else
        warn "  [WARN] /dev/dri/renderD128 missing \u2014 GPU driver may not be bound"
        warn "         (i915/xe kernel module didn't claim the device; check dmesg)"
        ok=0
    fi

    # VA-API hardware decode \u2014 bonus, doesn't affect return code.
    if command -v vainfo >/dev/null 2>&1 \
        && vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
             | grep -q 'Intel iHD'; then
        log "  [ OK ] VA-API iHD driver active (hardware decode available)"
    else
        log "  [info] VA-API iHD not active (hardware decode unavailable; engine still works on software decode)"
    fi
    _verify_va_gst_decoder

    return $(( ok ? 0 : 1 ))
}

# Verify the NPU userspace the engine will load. Same three classes
# of probe as the GPU path:
#   1. /dev/accel/accel0 exists (kernel driver bound)
#   2. libze_loader.so.1 is in ldconfig cache (OV NPU plugin needs L0)
#   3. The service user can open the NPU device node
_verify_intel_npu_userspace() {
    local ok=1

    log "verifying Intel NPU userspace..."

    if [[ ! -e /dev/accel/accel0 ]]; then
        warn "  [FAIL] /dev/accel/accel0 missing"
        warn "         NPU kernel driver (intel_vpu) not bound. Most common cause"
        warn "         is HWE kernel install pending a reboot."
        warn "         fix: sudo reboot, then re-run install.sh"
        return 1
    fi
    log "  [ OK ] /dev/accel/accel0 present"

    if ! ldconfig -p 2>/dev/null | grep -q 'libze_loader.so.1'; then
        warn "  [FAIL] libze_loader.so.1 not in ldconfig cache (libze1 missing)"
        warn "         OpenVINO NPU plugin cannot initialise without the L0 loader."
        warn "         fix: sudo apt-get install -y libze1   (from kobuk-team PPA)"
        ok=0
    else
        log "  [ OK ] libze_loader.so.1 present (oneAPI Level Zero loader)"
    fi

    if id -u "$NEXUS_SERVICE_USER" >/dev/null 2>&1 \
        && sudo -u "$NEXUS_SERVICE_USER" \
            bash -c 'exec 9</dev/accel/accel0 && exec 9<&-' 2>/dev/null; then
        log "  [ OK ] service user $NEXUS_SERVICE_USER can open /dev/accel/accel0"
    elif id -u "$NEXUS_SERVICE_USER" >/dev/null 2>&1; then
        warn "  [FAIL] service user $NEXUS_SERVICE_USER cannot open /dev/accel/accel0"
        warn "         (missing render-group membership or wrong device perms)"
        warn "         fix: sudo usermod -aG render $NEXUS_SERVICE_USER && sudo systemctl restart nexus-engine"
        ok=0
    else
        log "  [skip] service user $NEXUS_SERVICE_USER not yet created; device-open probe deferred"
    fi

    return $(( ok ? 0 : 1 ))
}

# Verify the AMD GPU userspace. Three probes mirroring the Intel
# path: VA-API enumerates Mesa Gallium, `/dev/dri/renderD128`
# exists, service user can open it. ROCm intentionally not probed
# here — it's classified + verified in `_drivers_amd_rocm` for the
# discrete GPUs it officially supports, and unsupported iGPUs run
# inference on the Vulkan(WebGPU) EP, not ROCm. On a Hailo box the
# AMD path here is decode-only (inference runs on the Hailo-8 via
# `nexus-hailo-backend`).
_verify_amd_gpu_userspace() {
    local ok=1

    log "verifying AMD GPU userspace..."

    if [[ -e /dev/dri/renderD128 ]]; then
        if sudo -u "$NEXUS_SERVICE_USER" \
                bash -c 'exec 9</dev/dri/renderD128 && exec 9<&-' 2>/dev/null; then
            log "  [ OK ] service user $NEXUS_SERVICE_USER can open /dev/dri/renderD128"
        else
            warn "  [FAIL] service user $NEXUS_SERVICE_USER cannot open /dev/dri/renderD128"
            warn "         (missing render-group membership or wrong device perms)"
            warn "         fix: sudo usermod -aG render $NEXUS_SERVICE_USER && sudo systemctl restart nexus-engine"
            ok=0
        fi
    else
        warn "  [FAIL] /dev/dri/renderD128 missing — amdgpu kernel module not bound"
        warn "         (check 'lsmod | grep amdgpu' and dmesg for binding errors)"
        ok=0
    fi

    if command -v vainfo >/dev/null 2>&1 \
        && vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
             | grep -q 'Mesa Gallium driver'; then
        log "  [ OK ] VA-API Mesa Gallium driver active (hardware decode available)"
    else
        log "  [info] VA-API Mesa Gallium not active (hardware decode unavailable; engine still works on software decode)"
    fi
    _verify_va_gst_decoder

    return $(( ok ? 0 : 1 ))
}

# Verify the Hailo-8 userspace. Probes mirror the Intel NPU path:
#   1. Driver bound: /dev/hailo0 exists.
#   2. Userspace works: `hailortcli scan` succeeds AND lists at
#      least one Hailo Device.
#   3. Service user can open the device node.
#
# Distinguishes four states explicitly so the operator-facing
# message matches reality:
#   - "no .debs staged"        : no apt entry, no .debs in vendor dir
#   - "staged, install failed" : .debs present but dpkg state != ii
#   - "installed, no /dev"     : packages ii but module not bound
#   - "fully working"          : everything green
_verify_hailo_userspace() {
    local ok=1
    log "verifying Hailo-8 userspace..."

    # Package-state probe — the canonical "did the .deb land cleanly?"
    # answer. dpkg-query exits non-zero if the package isn't even in
    # the database (treat as 'missing').
    local rt_state pcie_state
    rt_state=$(dpkg-query -W -f='${db:Status-Status}' hailort 2>/dev/null || echo missing)
    pcie_state=$(dpkg-query -W -f='${db:Status-Status}' hailort-pcie-driver 2>/dev/null || echo missing)

    if [[ "$rt_state" == "missing" && "$pcie_state" == "missing" ]]; then
        local hailo_local_dir="${NEXUS_HAILO_DEB_DIR:-/opt/nexus/vendor/hailo}"
        if compgen -G "$hailo_local_dir/hailort*.deb" >/dev/null 2>&1; then
            warn "  [FAIL] HailoRT .debs staged at $hailo_local_dir but no apt entry"
            warn "         (install.sh did not run the install step — re-run it)"
            return 1
        fi
        log "  [info] HailoRT not installed (no .debs staged); engine will use CPU EP"
        log "         see docs/INSTALL.md §5.5 to enable Hailo"
        return 0
    fi

    if [[ "$rt_state" != "installed" || "$pcie_state" != "installed" ]]; then
        warn "  [FAIL] HailoRT packages in incomplete state:"
        warn "           hailort=$rt_state  hailort-pcie-driver=$pcie_state"
        warn "         (postinst failed half-way — common cause is a kernel-API"
        warn "         rename. Inspect /var/log/hailort.deb.log,"
        warn "         /var/log/hailort-pcie-driver.deb.log, and"
        warn "         /var/lib/dkms/hailo_pci/*/build/make.log)"
        warn "         fix: re-run install.sh — it will retry the postinst"
        warn "         with the kernel-API compat patch applied."
        return 1
    fi
    log "  [ OK ] hailort + hailort-pcie-driver dpkg state = installed"

    if ! ldconfig -p 2>/dev/null | grep -q '\blibhailort\.so\b'; then
        warn "  [FAIL] libhailort.so not in ldconfig cache"
        warn "         fix: sudo ldconfig"
        ok=0
    else
        log "  [ OK ] libhailort.so present in ldconfig cache"
    fi

    if [[ ! -e /dev/hailo0 ]]; then
        warn "  [FAIL] /dev/hailo0 missing (packages installed but module not bound)"
        warn "         fix: sudo modprobe hailo_pci  (or reboot if DKMS just built)"
        return 1
    fi
    log "  [ OK ] /dev/hailo0 present"

    if ! command -v hailortcli >/dev/null 2>&1; then
        warn "  [FAIL] hailortcli not on PATH despite hailort=installed"
        warn "         (corrupted package — try: sudo apt-get install --reinstall hailort)"
        ok=0
    elif ! hailortcli scan 2>/dev/null | grep -q 'Hailo Device'; then
        warn "  [FAIL] hailortcli scan does not list any Hailo Device"
        warn "         (driver bound but userspace can't enumerate — version mismatch?)"
        ok=0
    else
        log "  [ OK ] hailortcli scan enumerates Hailo Device"
    fi

    if sudo -u "$NEXUS_SERVICE_USER" \
            bash -c 'exec 9</dev/hailo0 && exec 9<&-' 2>/dev/null; then
        log "  [ OK ] service user $NEXUS_SERVICE_USER can open /dev/hailo0"
    else
        warn "  [FAIL] service user $NEXUS_SERVICE_USER cannot open /dev/hailo0"
        warn "         (missing video-group membership or wrong device perms)"
        warn "         fix: sudo usermod -aG video $NEXUS_SERVICE_USER && sudo systemctl restart nexus-engine"
        ok=0
    fi

    return $(( ok ? 0 : 1 ))
}

# Resolve the staged nexus-probe binary. install.sh exports
# NEXUS_PROBE_BIN from the verified release dir; fall back to PATH for
# dev / standalone runs. Prints the path on success, returns 1 if none.
_nexus_probe_bin() {
    if [[ -n "${NEXUS_PROBE_BIN:-}" && -x "${NEXUS_PROBE_BIN}" ]]; then
        printf '%s' "${NEXUS_PROBE_BIN}"
        return 0
    fi
    local p
    if p="$(command -v nexus-probe 2>/dev/null)"; then
        printf '%s' "$p"
        return 0
    fi
    return 1
}

# Detect accelerator hardware via `nexus-probe accel-tags` (the single
# detector, shared with emit-config so drivers and the generated config
# can never disagree). Outputs one tag per line:
#   intel-igpu | intel-arc-dgpu | intel-npu | nvidia-gpu | amd-igpu | hailo-m2
# Empty output = nothing recognised (engine still installs CPU-only).
# The probe runs `lspci -nn`, so make sure pciutils is present first.
_detect_hardware() {
    local probe
    probe="$(_nexus_probe_bin)" || {
        warn "nexus-probe not available; cannot detect accelerator hardware"
        return 1
    }
    if ! command -v lspci >/dev/null 2>&1; then
        log "lspci missing; installing pciutils for hardware detection"
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq pciutils \
            >/dev/null 2>&1 || return 1
    fi
    # accel-tags emits the six hardware tags plus `amd-rocm-capable` (the
    # ROCm-vs-Vulkan verdict). _detect_hardware's contract is the hardware
    # tags only, so drop the rocm line here; the AMD driver branch reads
    # the verdict via _amd_gpu_rocm_supported. `|| true` keeps a CPU-only
    # box (no tags -> grep exit 1) from looking like a detection failure.
    "$probe" accel-tags 2>/dev/null | grep -Ev '^amd-rocm-capable$' || true
}

# Compare uname -r to a required major.minor pair. Returns 0 if
# current >= required, 1 otherwise.
_kernel_at_least() {
    local want_major="$1" want_minor="$2"
    local kver
    kver="$(uname -r | grep -oE '^[0-9]+\.[0-9]+')" || return 1
    local cur_major="${kver%.*}"
    local cur_minor="${kver#*.}"
    if (( cur_major > want_major )); then
        return 0
    fi
    if (( cur_major == want_major && cur_minor >= want_minor )); then
        return 0
    fi
    return 1
}

# Add ppa:kobuk-team/intel-graphics if it isn't already configured.
# Idempotent. Returns 0 on success, 1 on failure (caller decides
# whether the failure is fatal or warn-and-continue).
_ensure_kobuk_ppa() {
    if grep -rq 'kobuk-team/intel-graphics' /etc/apt/sources.list.d/ 2>/dev/null; then
        return 0
    fi
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        software-properties-common >/dev/null 2>&1 || {
            warn "software-properties-common install failed; cannot add kobuk-team PPA"
            return 1
        }
    log "adding ppa:kobuk-team/intel-graphics"
    add-apt-repository -y ppa:kobuk-team/intel-graphics || {
        warn "PPA add failed; libze1/iHD packages unavailable"
        return 1
    }
    DEBIAN_FRONTEND=noninteractive apt-get update -qq
}

# Ensure libze1 (oneAPI Level Zero loader, libze_loader.so.1) is
# installed FROM the kobuk-team PPA (NOT the Ubuntu archive). The
# Ubuntu noble archive ships libze1 1.16.1, which the OpenVINO
# 2025.4.x NPU plugin (bundled in onnxruntime-openvino ≥ 1.24.x)
# cannot drive: enumeration succeeds at the OV layer but device
# init fails with `[OpenVINO] Device NPU is not available`. The
# kobuk-team PPA ships a newer libze1 that matches the
# intel-level-zero-npu 1.32.x ABI.
#
# Idempotent: when libze1 is already at the PPA-pinned version
# apt-get install is a no-op. When it's the archive 1.16.1 the
# install upgrades it. Called from both `_drivers_intel_graphics`
# and `_drivers_intel_npu` so a box with only an NPU (no display
# iGPU path through OV) still gets the loader.
#
# Why we DON'T early-return when libze1 is already present: an
# earlier version of this helper checked `dpkg-query -W libze1`
# first and short-circuited, which left boxes provisioned with
# the Ubuntu archive 1.16.1 stuck at the wrong ABI version even
# after the PPA-add helper landed. The check still happens
# implicitly inside apt-get (it'll skip work when nothing to do).
_ensure_libze1() {
    _ensure_kobuk_ppa || return 1
    log "installing/upgrading libze1 (oneAPI Level Zero loader) from kobuk-team PPA"
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libze1; then
        warn "libze1 install failed; OpenVINO NPU/GPU plugins will fail to enumerate devices"
        return 1
    fi
    local ver
    ver=$(dpkg-query -W -f='${Version}' libze1 2>/dev/null || echo unknown)
    log "libze1 installed (version=$ver)"
}

# Install Intel iGPU / Arc dGPU stack from the kobuk-team PPA.
# Idempotent: rerunning checks for vainfo presence first.
#
# Why the PPA and not repositories.intel.com? — the Intel "client"
# repo was retired in 2025-Q3; intel-graphics now ships only the
# data-center channel which doesn't carry libigc1. The kobuk-team
# PPA is Ubuntu's blessed client-class staging area for the same
# packages (libze-intel-gpu1, iHD 25.x, etc).
# The canonical package set for the Intel iGPU / Arc dGPU stack.
# Kept in one place so the install path and the idempotency check
# can't drift apart. Mirrors docs/INSTALL.md §5.1.
#
# Why every entry matters at runtime (don't trim this list without
# proving the engine still attaches the OpenVINO GPU plugin):
#   intel-opencl-icd                 NEO compute runtime — required
#                                    by OpenVINO GPU plugin to JIT
#                                    kernels onto /dev/dri/renderD128.
#                                    Missing this is the bug we hit
#                                    on 192.168.1.99: vainfo reported
#                                    iHD (because intel-media-va-driver
#                                    was somehow pre-installed) and the
#                                    old early-return skipped the rest
#                                    of this package set. Engine then
#                                    silently fell back to CPU EP.
#   libze-intel-gpu1 / libze1        oneAPI Level Zero loader + Intel
#                                    backend. OV 2025.4 GPU plugin
#                                    enumerates via L0 first.
#   intel-media-va-driver-non-free   iHD VA-API driver (hardware decode).
#   intel-metrics-discovery          libmd_metrics — read by intel_gpu_top
#                                    AND by the engine's GPU PMU code path.
#   intel-gsc                        Graphics System Controller userspace
#                                    (firmware loader for Arc / Lunar Lake).
#   clinfo                           OpenCL probe — used by the post-install
#                                    verification below.
#   vainfo / intel-gpu-tools         operator-facing probes; intel_gpu_top
#                                    is what we tell operators to use to
#                                    confirm the engine is actually using
#                                    the iGPU.
_INTEL_GRAPHICS_PKGS=(
    libze-intel-gpu1 libze1
    intel-metrics-discovery intel-opencl-icd intel-gsc clinfo
    intel-media-va-driver-non-free
    libmfx-gen1 libvpl2 libvpl-tools
    libva-glx2 va-driver-all vainfo
    intel-gpu-tools
)

# Returns 0 if every package in the array argument is installed.
_all_dpkg_installed() {
    local pkg
    for pkg in "$@"; do
        if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null \
                | grep -q 'install ok installed'; then
            return 1
        fi
    done
    return 0
}

_drivers_intel_graphics() {
    # libze1 (oneAPI Level Zero loader) is required by both the
    # OpenVINO NPU and GPU plugins to enumerate devices. Ensure it
    # before the package-set early-return so a partially-installed
    # box (intel-opencl-icd present but libze1 still on the Ubuntu
    # archive 1.16.1) gets repaired rather than silently short-
    # circuited.
    _ensure_libze1

    # Idempotency: only skip the full install when every package in
    # the canonical list is already there AND vainfo confirms iHD.
    # The previous gate was vainfo-only, which let intel-opencl-icd
    # / libze-intel-gpu1 stay missing on boxes where iHD got pulled
    # in transitively (e.g. by a desktop-environment metapackage on
    # the base image).
    if _all_dpkg_installed "${_INTEL_GRAPHICS_PKGS[@]}" \
        && command -v vainfo >/dev/null 2>&1 \
        && vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
             | grep -q 'Intel iHD'; then
        log "Intel iGPU/dGPU stack already installed (all ${#_INTEL_GRAPHICS_PKGS[@]} packages present; vainfo: iHD)"
        return 0
    fi

    log "installing Intel iGPU/dGPU drivers (kobuk-team PPA)"
    _ensure_kobuk_ppa || return 0

    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        "${_INTEL_GRAPHICS_PKGS[@]}" \
        || {
            warn "Intel graphics package install failed; engine will fall back to CPU EP"
            return 0
        }

    # Sanity probe — non-fatal warn so a broken vainfo doesn't kill
    # the rest of the install.
    if vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
        | grep -q 'Intel iHD'; then
        log "Intel iHD driver active (vainfo confirmed)"
    else
        warn "Intel graphics installed but vainfo did not report iHD;"
        warn "a reboot may be required. Re-run install.sh after reboot."
    fi

    # GPU PMU exposure check — without one of these sysfs nodes
    # the engine can't report GPU utilization on the System page.
    # Most boxes will have `i915` (Alder Lake-N, Raptor Lake,
    # Iris Xe, Arc A-series) or `xe_<bdf>` (Lunar Lake / Battlemage
    # on kernel 6.8+). We only warn — a missing PMU does not break
    # detection or recording, it just hides one telemetry field.
    if ! ls /sys/bus/event_source/devices/i915 \
              /sys/bus/event_source/devices/xe_* \
              >/dev/null 2>&1; then
        warn "no Intel GPU PMU exposed (neither /sys/bus/event_source/devices/i915"
        warn "nor xe_<bdf> present); the System page will show 'utilization unavailable'."
        warn "Check 'lsmod | grep -E ^i915\\|^xe' and 'dmesg | grep -iE i915\\|xe'"
        warn "for binding errors. CAP_PERFMON is required by nexus-engine and is"
        warn "set by the systemd unit in v0.1.14+."
    fi
}

# Install Intel NPU driver from upstream GitHub release. Pinned to
# the latest version verified for Lunar Lake on kernel >= 6.10.
# Updating the pin is a one-line change here.
# Every .deb the NPU tarball ships. Each one is load-bearing for
# the OpenVINO NPU plugin's runtime path — checking only one of
# them (as the old early-return did) let a partial install slip
# through and present as `Device NPU is not available` from OV.
_INTEL_NPU_PKGS=(
    intel-driver-compiler-npu
    intel-fw-npu
    intel-level-zero-npu
)

_drivers_intel_npu() {
    # The OpenVINO NPU plugin (libopenvino_intel_npu_plugin.so) needs
    # the oneAPI Level Zero loader (libze1 → libze_loader.so.1) to
    # enumerate the NPU device. The kobuk-team PPA libze1 — NOT the
    # Ubuntu archive's 1.16.1 — is required for OV 2025.4.x NPU
    # plugin ABI. `_ensure_libze1` always runs (and is idempotent)
    # so an upgrade-from-archive happens BEFORE the early-return on
    # an already-installed NPU driver below.
    _ensure_libze1

    if [[ -e /dev/accel/accel0 ]] \
        && _all_dpkg_installed "${_INTEL_NPU_PKGS[@]}"; then
        log "Intel NPU driver already installed (/dev/accel/accel0 + all ${#_INTEL_NPU_PKGS[@]} packages present)"
        return 0
    fi

    local npu_ver="1.32.1"
    local npu_release="20260422-24767473183"
    local npu_tarball="linux-npu-driver-v${npu_ver}.${npu_release}-ubuntu2404.tar.gz"
    local npu_url="https://github.com/intel/linux-npu-driver/releases/download/v${npu_ver}/${npu_tarball}"

    log "installing Intel NPU driver v${npu_ver}"
    local tmpdir
    tmpdir="$(mktemp -d -t nexus-npu.XXXXXX)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" RETURN

    if ! curl -fsSL "$npu_url" -o "$tmpdir/${npu_tarball}"; then
        warn "NPU tarball download failed ($npu_url); skipping NPU driver"
        return 0
    fi
    if ! tar -xzf "$tmpdir/${npu_tarball}" -C "$tmpdir"; then
        warn "NPU tarball extract failed; skipping NPU driver"
        return 0
    fi

    # The tarball contains 4 .deb files at the top level. Install
    # them all in a single apt invocation so dpkg resolves the
    # intra-bundle deps in the right order.
    local debs=( "$tmpdir"/intel-*.deb )
    if (( ${#debs[@]} == 0 )); then
        warn "NPU tarball did not contain any intel-*.deb files; layout changed?"
        return 0
    fi
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${debs[@]}"; then
        warn "NPU driver install failed; engine will fall back to iGPU EP"
        return 0
    fi

    log "NPU driver installed; /dev/accel/accel0 should appear after reboot"
}

# Install the AMD graphics userspace stack from the Ubuntu archive.
# In-kernel `amdgpu` is already loaded on every supported tier (Ryzen
# 7000+ APU on kernel ≥ 6.8), so we only need the userspace bits:
#
#   mesa-va-drivers          radeonsi VA-API driver — engine clip
#                            recorder uses this for hardware H.264
#                            decode on the EQR7 Radeon 680M.
#   mesa-vulkan-drivers      RADV — pulls in libdrm + the Mesa shader
#                            cache; also unblocks any future GStreamer
#                            element that probes Vulkan.
#   libdrm-amdgpu1           AMDGPU DRM userspace; required by Mesa
#                            and by future ROCm experiments.
#   va-driver-all + vainfo   meta + probe — keep them in sync with
#                            the Intel install path so post-install
#                            verification looks identical to
#                            operators.
#
# We deliberately do NOT pull ROCm here. ROCm on Phoenix-class iGPUs
# (gfx1035, what the 680M reports) is unsupported upstream — those parts
# run inference through the Vulkan(WebGPU) EP instead (see
# `_drivers_amd_vulkan` / `_install_ort_webgpu`). The ROCm path is only
# taken for officially-supported discrete RDNA/CDNA GPUs, classified by
# `_amd_gpu_rocm_supported` from the PCI device ID.
#
# On a Hailo-equipped box the AMD path here is decode-only; inference runs
# on the Hailo-8 via `nexus-hailo-backend`. See docs/INSTALL.md §5.5.
_AMD_GRAPHICS_PKGS=(
    mesa-va-drivers
    mesa-vulkan-drivers
    libdrm-amdgpu1
    va-driver-all
    vainfo
)

_drivers_amd_graphics() {
    if _all_dpkg_installed "${_AMD_GRAPHICS_PKGS[@]}" \
        && command -v vainfo >/dev/null 2>&1 \
        && vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
             | grep -q 'Mesa Gallium driver'; then
        log "AMD GPU stack already installed (all ${#_AMD_GRAPHICS_PKGS[@]} packages present; vainfo: Mesa Gallium)"
        return 0
    fi

    log "installing AMD GPU userspace (Mesa VA-API + AMDGPU DRM)"
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
            "${_AMD_GRAPHICS_PKGS[@]}"; then
        warn "AMD graphics package install failed; engine will fall back to software decode"
        return 0
    fi

    if vainfo --display drm --device /dev/dri/renderD128 2>/dev/null \
        | grep -q 'Mesa Gallium driver'; then
        log "AMD Mesa Gallium VA-API driver active (vainfo confirmed)"
    else
        warn "AMD graphics installed but vainfo did not report Mesa Gallium;"
        warn "amdgpu may not be bound yet — check 'lsmod | grep amdgpu' and dmesg."
    fi
}

# Install AMD ROCm compute drivers for Radeon iGPU/dGPU (ONNX Runtime EP).
#
# Enables the ROCm execution provider in nexus-inference for AMD GPUs ROCm
# officially supports — discrete RDNA/CDNA parts (gfx1030+). The install
# driver branch only calls this after `_amd_gpu_rocm_supported` has
# classified the GPU from its PCI device ID; unsupported parts (Phoenix /
# Rembrandt iGPUs, gfx1035/gfx1103) take the Vulkan(WebGPU) path instead
# and never reach here. No HSA_OVERRIDE_GFX_VERSION force-fit is applied.
#
# Operator must already have the amdgpu kernel module bound (i.e.
# `_drivers_amd_graphics` has run first) so that /dev/dri/renderD12x
# and /dev/kfd exist. If either is missing, we emit a warn and return
# gracefully — the engine will fall back to CPU EP.
#
# Installs from the official AMD ROCm PPA (Ubuntu 24.04, 22.04).
# Adds rocminfo, rocm-smi, hip-runtime-amd, hip-dev, rocm-libs,
# migraphx, migraphx-dev, libamd-comgr-dev (headers for custom kernels).

# Resolve the rocminfo binary. ROCm installs it under /opt/rocm*/bin,
# which is NOT on the default PATH (no profile.d entry on a minimal
# server). Falls back to PATH for distros that do symlink it. Echoes
# the absolute path on success; empty + return 1 when not found.
_rocminfo_bin() {
    local p
    for p in /opt/rocm/bin/rocminfo /opt/rocm-*/bin/rocminfo; do
        [[ -x "$p" ]] && { echo "$p"; return 0; }
    done
    if command -v rocminfo >/dev/null 2>&1; then
        command -v rocminfo; return 0
    fi
    return 1
}

# Static allowlist of AMD PCI device IDs (the 0xXXXX device portion of
# `1002:XXXX`) that ROCm OFFICIALLY supports: discrete RDNA2 (gfx1030),
# RDNA3 (gfx1100/1101/1102), and CDNA (gfx908/gfx90a/gfx942). The
# allowlist itself now lives ONLY in the Rust nexus-probe crate
# (`ROCM_SUPPORTED_PCI_IDS`); `_amd_gpu_rocm_supported` queries it via
# `nexus-probe accel-tags` so the ROCm-vs-Vulkan decision has a single
# source of truth. Deliberately DEFAULT-DENY there: anything NOT on the
# list (Phoenix/Rembrandt iGPUs gfx1035/gfx1103, lower SKUs we have not
# vetted, brand-new parts) routes to the Vulkan(WebGPU) EP, never ROCm.
# Routing an unlisted-but-actually-supported part to Vulkan is a perf
# miss; routing an UNsupported part to ROCm risks the uncaught-C++ SIGABRT
# the engine's rocm_runtime_available() guard exists to avoid.

# True (0) when a discrete AMD GPU on the ROCm allowlist is present.
# Delegates to `nexus-probe accel-tags` — the device-ID allowlist now
# lives only in the Rust nexus-probe crate (the duplicate bash
# _ROCM_SUPPORTED_PCI_IDS array was removed), so the driver branch and
# the generated ep_priority decide ROCm-vs-Vulkan from one source.
# DEFAULT-DENY: a missing probe or no `amd-rocm-capable` tag returns 1,
# so the branch routes to Vulkan(WebGPU) rather than force-fitting the
# heavy ROCm stack onto a part it cannot drive.
#
# NOTE: the old bash helper also consulted a secondary `rocminfo` gfx
# probe when ROCm happened to be pre-installed. The Rust port omits that
# (the sysfs PCI-ID allowlist is authoritative); on a fresh install
# rocminfo isn't present yet, so that secondary probe was a no-op in the
# common case anyway.
_amd_gpu_rocm_supported() {
    local probe
    probe="$(_nexus_probe_bin)" || return 1
    "$probe" accel-tags 2>/dev/null | grep -qx 'amd-rocm-capable'
}

# True (0) when a usable ROCm GPU agent is present. Runs rocminfo and greps
# for a gfx target. Does NOT inject HSA_OVERRIDE_GFX_VERSION — this path is
# only reached for officially-supported discrete GPUs that enumerate a gfx
# agent without any override. If the operator has manually set the override
# (the unsupported escape hatch), it is honoured from the environment.
# Caller must be able to open /dev/kfd (root during install, or a
# render-group member at runtime).
_rocm_gpu_present() {
    local rocminfo
    rocminfo="$(_rocminfo_bin)" || return 1
    "$rocminfo" 2>/dev/null | grep -q 'gfx[0-9]'
}

_drivers_amd_rocm() {
    # Quick check: if /dev/kfd + /dev/dri/renderD128 are missing,
    # the amdgpu module is not bound yet. Defer gracefully.
    if [[ ! -c /dev/kfd ]]; then
        warn "AMD ROCm: /dev/kfd not present — amdgpu kernel module not bound"
        warn "         (reboot or 'modprobe amdgpu' and re-run install.sh)"
        return 0
    fi
    if [[ ! -e /dev/dri/renderD128 ]]; then
        warn "AMD ROCm: /dev/dri/renderD128 not present — amdgpu not yet bound to GPU"
        warn "         (reboot or 'modprobe amdgpu' and re-run install.sh)"
        return 0
    fi

    # Check if rocminfo is already installed + working (locate it under
    # /opt/rocm/bin, not just PATH).
    if _rocminfo_bin >/dev/null && _rocm_gpu_present; then
        log "AMD ROCm already installed (rocminfo confirmed)"
        return 0
    fi

    log "installing AMD ROCm compute drivers (HIP, MIGraphX, rocminfo)"

    # ROCm userspace (rocminfo, hip-runtime, rocm-libs) lives in AMD's
    # `rocm/apt/<ver>` repo — NOT the `amdgpu/` repo (that one only carries
    # the amdgpu-dkms kernel driver, which we don't need: the in-kernel
    # amdgpu is already bound, proven by /dev/kfd existing). The repo
    # codename MUST match the host release (noble=24.04, jammy=22.04); a
    # mismatched codename silently yields a repo with none of the packages.
    local rocm_apt_ver="${NEXUS_ROCM_APT_VERSION:-6.4.4}"
    local ubuntu_codename
    ubuntu_codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-noble}")"

    local rocm_keyring="/usr/share/keyrings/rocm-apt-keyring.gpg"
    local rocm_repo="deb [arch=amd64 signed-by=${rocm_keyring}] https://repo.radeon.com/rocm/apt/${rocm_apt_ver} ${ubuntu_codename} main"

    # Ensure the signing key exists (older amdgpu-install runs may have
    # left a stale rocm.list pointing at a different keyring path; we own
    # our own keyring so apt can always verify our repo line).
    if [[ ! -s "$rocm_keyring" ]]; then
        if ! curl -sS https://repo.radeon.com/rocm/rocm.gpg.key \
                | gpg --dearmor -o "$rocm_keyring" 2>/dev/null; then
            warn "AMD ROCm: could not fetch signing key from repo.radeon.com; skipping ROCm install"
            return 0
        fi
    fi

    # Idempotent but self-healing: only (re)write rocm.list when our EXACT
    # desired line isn't already the sole content. This deliberately
    # OVERWRITES any stale entry (e.g. an old `rocm/apt/6.1 ubuntu main`
    # that an earlier amdgpu-install left behind, which would otherwise
    # shadow the correct repo and make every package "unavailable").
    if [[ "$(cat /etc/apt/sources.list.d/rocm.list 2>/dev/null)" != "$rocm_repo" ]]; then
        printf '%s\n' "$rocm_repo" > /etc/apt/sources.list.d/rocm.list

        # Pin the ROCm repo above the Ubuntu archive so apt prefers AMD's
        # builds of any overlapping packages (per AMD's install guide).
        cat > /etc/apt/preferences.d/rocm-pin-600 <<'PIN'
Package: *
Pin: release o=repo.radeon.com
Pin-Priority: 600
PIN
    fi

    apt-get update -qq || true

    # Install ROCm userspace + HIP runtime + libs. Intentionally do NOT
    # install rocm-hip-sdk / rocm-language-runtime (heavy compilers) — the
    # engine only needs the runtime + libs to dlopen the ROCm ORT provider.
    local rocm_pkgs=(
        rocminfo                     # GPU enumeration + gfx version probe
        rocm-smi-lib                  # GPU utilization + temps (rocm-smi tool)
        hip-runtime-amd               # HIP runtime (libamdhip64.so)
        rocm-libs                     # core ROCm libraries (rocBLAS, MIOpen, …)
    )

    # Capture apt output so a failure surfaces the real reason instead of
    # the silent "package install failed" that hid a broken repo for a
    # whole debugging session.
    local apt_log
    apt_log="$(mktemp -t nexus-rocm-apt.XXXXXX)"
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
            "${rocm_pkgs[@]}" >"$apt_log" 2>&1; then
        warn "AMD ROCm package install failed; engine will fall back to CPU EP. apt said:"
        tail -n 15 "$apt_log" | while IFS= read -r line; do warn "  $line"; done
        rm -f "$apt_log"
        return 0
    fi
    rm -f "$apt_log"

    # Verify installation: rocminfo should show at least one gfx* device.
    # rocminfo lives under /opt/rocm/bin. No HSA_OVERRIDE force-fit — this
    # branch only runs for officially-supported discrete GPUs.
    if _rocm_gpu_present; then
        local rocminfo gfx_name
        rocminfo="$(_rocminfo_bin)"
        gfx_name=$("$rocminfo" 2>/dev/null | grep -o 'gfx[0-9]*' | head -1)
        log "AMD ROCm installed and GPU detected via rocminfo${gfx_name:+ ($gfx_name)}"
    else
        warn "AMD ROCm installed but rocminfo did not detect a GPU agent."
        warn "This is benign at install time if the running user is not in the"
        warn "'render' group (root sees it; the nexus service user is added to"
        warn "'render' by ensure_accelerator_groups)."
    fi

    # Verify /dev/kfd accessibility for the service user.
    # The nexus user will be added to the 'render' group by
    # ensure_accelerator_groups (called after install_drivers).
    if [[ -r /dev/kfd && -w /dev/kfd ]]; then
        log "/dev/kfd is accessible (ROCm compute ready)"
    else
        warn "/dev/kfd not accessible yet — will be gated by ensure_accelerator_groups"
    fi
}

# Fetch a ROCm-capable ONNX Runtime and wire the engine to load it.
#
# WHY this is a separate download (not bundled in the release tarball):
#   * The release tarball ships libonnxruntime.so.1.24.1 built with the
#     OpenVINO provider (Intel CPU/GPU/NPU). That build has NO ROCm EP.
#   * The newest prebuilt ONNX Runtime with a ROCm execution provider is
#     1.21.0, published by AMD at repo.radeon.com (~179 MB wheel; PyPI's
#     onnxruntime-rocm tops out at 1.22.2). Microsoft does not ship a
#     ROCm build on their mainline.
#   * The engine binary is compiled against the ort `api-21` feature
#     (see root Cargo.toml) precisely so ONE binary can dlopen BOTH the
#     1.24.1 OpenVINO .so (ABI 24 ≥ 21) AND this 1.21.0 ROCm .so
#     (ABI 21 == 21). GetApi(21) is satisfied by both runtimes.
#
# Layout: the ROCm runtime lands in a release-independent vendor dir
# (`/opt/nexus/vendor/onnxruntime-rocm`, mirroring the hailo vendor dir)
# so it survives `current` symlink flips on upgrade. A systemd drop-in
# (`10-ort-rocm.conf`) repoints ORT_DYLIB_PATH + LD_LIBRARY_PATH at it,
# so on an AMD box the engine loads the ROCm runtime instead of the
# bundled OpenVINO one. The ROCm runtime also carries a CPU EP, so the
# ["rocm","cpu"] fallback chain still works through the same .so.
#
# Idempotent: a version marker file short-circuits re-download. Curl /
# extract failures are warn-and-continue — the engine still installs and
# runs on the bundled OpenVINO/CPU runtime (just without GPU on AMD).
_install_ort_rocm() {
    # Only meaningful when ROCm compute is actually present. _drivers_amd_rocm
    # runs first; if /dev/kfd is absent (no GPU) or the ROCm runtime never
    # installed (no libamdhip64.so under /opt/rocm*/lib), there is nothing
    # for the ROCm ONNX Runtime provider to bind to. NOTE: do NOT gate on
    # `command -v rocminfo` here — ROCm installs rocminfo to /opt/rocm/bin,
    # which is not on PATH, so that check spuriously skipped the whole step.
    if [[ ! -c /dev/kfd ]]; then
        return 0
    fi
    if ! compgen -G "/opt/rocm*/lib/libamdhip64.so*" >/dev/null; then
        warn "ROCm runtime (libamdhip64.so) not found under /opt/rocm*/lib;"
        warn "skipping ROCm ONNX Runtime fetch (engine uses OpenVINO/CPU runtime)."
        return 0
    fi

    local ort_ver="1.21.0"
    local rocm_rel="6.4.4"          # repo.radeon.com directory carrying the 1.21.0 wheels
    local vendor_dir="${NEXUS_ORT_ROCM_DIR:-/opt/nexus/vendor/onnxruntime-rocm}"
    local marker="$vendor_dir/.nexus-ort-rocm-version"

    # Idempotent short-circuit: already installed at the target version.
    if [[ -f "$marker" ]] && grep -qx "$ort_ver" "$marker" 2>/dev/null \
        && [[ -e "$vendor_dir/libonnxruntime.so" ]]; then
        log "ROCm ONNX Runtime $ort_ver already installed ($vendor_dir)"
        # Re-resolve provider deps in case a prior install predated the
        # dep-linking step or ROCm libs were upgraded under it.
        _ort_rocm_link_missing_deps "$vendor_dir"
        _ort_rocm_write_dropin "$vendor_dir"
        return 0
    fi

    log "installing ROCm ONNX Runtime $ort_ver (provider for AMD Radeon inference)"

    local base="https://repo.radeon.com/rocm/manylinux/rocm-rel-${rocm_rel}"
    local mlx="manylinux_2_27_x86_64.manylinux_2_28_x86_64"
    # The wheel is just a zip; the bundled libonnxruntime.so is identical
    # across cp tags (the C API .so is not Python-version-specific). Try
    # cp312 (Ubuntu 24.04 default) first, then cp310 (22.04), then cp39.
    local cp_tags=(cp312 cp310 cp39)

    local tmpdir
    tmpdir="$(mktemp -d -t nexus-ort-rocm.XXXXXX)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" RETURN

    local whl="" tag
    for tag in "${cp_tags[@]}"; do
        local fn="onnxruntime_rocm-${ort_ver}-${tag}-${tag}-${mlx}.whl"
        log "  fetching $fn (~179 MB)…"
        if curl -fsSL "${base}/${fn}" -o "$tmpdir/ort.whl"; then
            whl="$tmpdir/ort.whl"
            break
        fi
        warn "  $fn not available; trying next cp tag"
    done

    if [[ -z "$whl" ]]; then
        warn "ROCm ONNX Runtime download failed (all cp tags); engine will use"
        warn "the bundled OpenVINO/CPU runtime — AMD GPU inference disabled."
        return 0
    fi

    # Wheels are zip archives. The runtime + provider .so files live under
    # onnxruntime/capi/. Need unzip (apt: unzip) — install if missing.
    if ! command -v unzip >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq unzip >/dev/null 2>&1 || {
            warn "unzip unavailable and install failed; cannot unpack ROCm ORT wheel"
            return 0
        }
    fi

    if ! unzip -q -o "$whl" 'onnxruntime/capi/*' -d "$tmpdir/extract"; then
        warn "ROCm ORT wheel extract failed; AMD GPU inference disabled"
        return 0
    fi

    local capi="$tmpdir/extract/onnxruntime/capi"
    if [[ ! -e "$capi/libonnxruntime.so" ]] \
        && ! compgen -G "$capi/libonnxruntime.so*" >/dev/null; then
        warn "ROCm ORT wheel layout unexpected (no libonnxruntime.so under capi/)"
        return 0
    fi

    # Stage into the vendor dir. Ship the versioned runtime + the
    # provider .so siblings ONNX Runtime dlopens at session-create time
    # (onnxruntime_providers_shared.so, onnxruntime_providers_rocm.so).
    install -d -m 0755 "$vendor_dir"
    cp -a "$capi"/libonnxruntime*.so* "$vendor_dir"/ 2>/dev/null || true
    cp -a "$capi"/libonnxruntime_providers_*.so* "$vendor_dir"/ 2>/dev/null || true

    # The ort crate dlopens the unversioned soname `libonnxruntime.so`.
    # The wheel ships it as libonnxruntime.so.<ver>; create the symlink.
    if [[ ! -e "$vendor_dir/libonnxruntime.so" ]]; then
        local versioned
        versioned="$(cd "$vendor_dir" && ls libonnxruntime.so.* 2>/dev/null | head -1)"
        if [[ -n "$versioned" ]]; then
            ln -sf "$versioned" "$vendor_dir/libonnxruntime.so"
        fi
    fi

    if [[ ! -e "$vendor_dir/libonnxruntime.so" ]]; then
        warn "ROCm ORT staged but libonnxruntime.so symlink missing; AMD GPU disabled"
        return 0
    fi

    # auditwheel mangles a handful of the provider's ROCm dependencies to
    # hash-suffixed sonames (e.g. librocm_smi64-a53a9aba.so.7.7.60404) but
    # does NOT bundle the .so itself into the capi/ dir — it expects the
    # matching system lib from the rocm-smi-lib / rocm-libs packages we
    # already install in `_drivers_amd_rocm`. ORT's provider loader matches
    # DT_NEEDED by exact filename string, so the version-suffixed system lib
    # under /opt/rocm/lib won't satisfy the hashed name. Bridge every still-
    # missing ROCm dependency to its system twin by symlinking the exact
    # name the provider asks for. Without this the provider .so fails to
    # dlopen, ORT silently drops the ROCm EP, and inference runs on CPU.
    _ort_rocm_link_missing_deps "$vendor_dir"

    printf '%s\n' "$ort_ver" > "$marker"
    log "ROCm ONNX Runtime $ort_ver staged at $vendor_dir"

    _ort_rocm_write_dropin "$vendor_dir"
}

# Resolve the ROCm provider's unsatisfied DT_NEEDED entries (auditwheel
# hash-named ROCm libs) to the matching system libraries under
# /opt/rocm/lib, by creating a symlink with the EXACT name the loader
# asks for. Idempotent. Only touches names that resolve to a real system
# lib whose un-hashed stem + version suffix match — never fabricates a lib.
_ort_rocm_link_missing_deps() {
    local vendor_dir="$1"
    local provider="$vendor_dir/libonnxruntime_providers_rocm.so"
    local rocm_lib="${NEXUS_ROCM_LIB_DIR:-/opt/rocm/lib}"
    [[ -e "$provider" ]] || return 0
    [[ -d "$rocm_lib" ]] || return 0

    local missing stem sys
    # `ldd` prints "<soname> => not found" for every unresolved dep when
    # the vendor + rocm dirs are on the search path.
    while read -r missing; do
        [[ -n "$missing" ]] || continue
        # Already satisfied (e.g. a prior install run)? skip.
        [[ -e "$vendor_dir/$missing" ]] && continue
        # Strip auditwheel's "-<8 hex>" hash that sits between the lib
        # stem and the ".so" suffix: librocm_smi64-a53a9aba.so.7.7.60404
        # -> librocm_smi64.so.7.7.60404
        stem="$(printf '%s' "$missing" | sed -E 's/-[0-9a-f]{6,}\.so/.so/')"
        if [[ -e "$rocm_lib/$stem" ]]; then
            ln -sfn "$rocm_lib/$stem" "$vendor_dir/$missing"
            log "ROCm ORT: linked provider dep $missing -> $rocm_lib/$stem"
        else
            warn "ROCm ORT: provider needs $missing but no system match at $rocm_lib/$stem"
        fi
    done < <(LD_LIBRARY_PATH="$vendor_dir:$rocm_lib" ldd "$provider" 2>/dev/null \
        | awk '/not found/ {print $1}')
}

# Write the systemd drop-in that repoints the engine's ORT loader at the
# ROCm runtime. Kept separate so the idempotent path can refresh it even
# when the .so is already present. The drop-in lives alongside the unit
# at /etc/systemd/system/nexus-engine.service.d/ and only OVERRIDES the
# two ORT env vars — everything else in the base unit (DeviceAllow,
# SupplementaryGroups) still applies.
_ort_rocm_write_dropin() {
    local vendor_dir="$1"
    local dropin_dir="/etc/systemd/system/nexus-engine.service.d"
    local dropin="$dropin_dir/10-ort-rocm.conf"

    # /opt/rocm/lib carries libamdhip64.so + the rocBLAS/MIOpen libs the
    # ROCm provider links against; the vendor dir carries libonnxruntime.so
    # and its provider siblings. Both must be on LD_LIBRARY_PATH.
    local rocm_lib="/opt/rocm/lib"
    [[ -d "$rocm_lib" ]] || rocm_lib="$(ls -d /opt/rocm*/lib 2>/dev/null | head -1)"
    [[ -n "$rocm_lib" ]] || rocm_lib="/opt/rocm/lib"

    install -d -m 0755 "$dropin_dir"
    cat > "$dropin" <<EOF
# Auto-generated by nexus install.sh (_install_ort_rocm). Repoints the
# ONNX Runtime loader at the ROCm-capable runtime fetched from AMD's
# repo.radeon.com (onnxruntime_rocm). The engine binary is built against
# ort api-21 so this 1.21.0 runtime loads cleanly. Overrides only the two
# ORT env vars from the base unit.
#
# Remove this file (and 'systemctl daemon-reload && systemctl restart
# nexus-engine') to fall back to the bundled OpenVINO/CPU runtime.
[Service]
Environment=ORT_DYLIB_PATH=${vendor_dir}/libonnxruntime.so
Environment=LD_LIBRARY_PATH=${vendor_dir}:${rocm_lib}
EOF

    log "wrote systemd drop-in $dropin (ORT_DYLIB_PATH -> ROCm runtime)"
    # daemon-reload so the override takes effect on next restart. install.sh
    # restarts the engine after install_drivers, so no explicit restart here.
    systemctl daemon-reload 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# AMD Vulkan (WebGPU EP) — runtime install for the `amd` tier.
#
# The `amd` tier targets AMD GPUs that ROCm does NOT officially support
# (Phoenix/Rembrandt iGPUs such as the Radeon 680M/780M, gfx1035/gfx1103).
# Instead of force-fitting them onto ROCm with HSA_OVERRIDE_GFX_VERSION,
# inference runs through ONNX Runtime's WebGPU execution provider on its
# Dawn->Vulkan backend (ONNX Runtime has no native Vulkan EP; WebGPU IS
# the Vulkan path on Linux). CPU is the terminal fallback.
#
# Validated end-to-end on a Radeon 680M (Rembrandt gfx1035): the engine
# registers ep="vulkan(webgpu)" and runs yolo26n_640 with the GPU at
# ~100% busy under sustained load (i.e. the RADV hardware device, not the
# llvmpipe software fallback).
# ---------------------------------------------------------------------------

# Install the Vulkan diagnostic userspace for the amd tier. The RADV ICD
# itself (mesa-vulkan-drivers) + libvulkan1 are already pulled in by
# `_drivers_amd_graphics`; this adds `vulkan-tools` so post-install
# verification (and the operator) can run `vulkaninfo --summary`.
# Idempotent + non-fatal.
_drivers_amd_vulkan() {
    if _all_dpkg_installed mesa-vulkan-drivers libvulkan1 vulkan-tools; then
        log "AMD Vulkan userspace already installed (RADV ICD + vulkan-tools present)"
    else
        log "installing AMD Vulkan userspace (RADV ICD + vulkan-tools)"
        if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
                mesa-vulkan-drivers libvulkan1 vulkan-tools; then
            warn "Vulkan userspace install failed; WebGPU EP will fall back to CPU."
            return 0
        fi
    fi

    # Sanity probe (non-fatal) — vulkaninfo enumerates the RADV device.
    if command -v vulkaninfo >/dev/null 2>&1 \
        && vulkaninfo --summary 2>/dev/null | grep -qiE 'RADV|AMD RADEON'; then
        log "Vulkan: RADV device enumerated (vulkaninfo confirmed)"
    else
        warn "Vulkan userspace installed but vulkaninfo did not enumerate a RADV"
        warn "device — amdgpu may not be bound yet; a reboot may be required."
    fi
}

# Fetch a WebGPU-capable ONNX Runtime and wire the engine to load it.
#
# WHY a separate download (not bundled in the release tarball):
#   * The release tarball ships libonnxruntime.so.1.24.x built with the
#     OpenVINO provider — it has NO WebGPU/Vulkan backend.
#   * The `onnxruntime-webgpu` PyPI wheel (1.27.0) ships a libonnxruntime.so
#     with Dawn STATICALLY linked in on its Vulkan backend. Stock PyPI
#     `onnxruntime` does NOT (its provider list is Azure+CPU only).
#   * The engine binary is built against ort `api-21`, so it dlopens the
#     1.27 runtime cleanly (GetApi(21) is satisfied by any ABI >= 21).
#
# Layout mirrors _install_ort_rocm: lands in a release-independent vendor
# dir (/opt/nexus/vendor/onnxruntime-webgpu) so it survives `current`
# symlink flips; a systemd drop-in (10-ort-webgpu.conf) repoints
# ORT_DYLIB_PATH + LD_LIBRARY_PATH at it. The WebGPU runtime also carries a
# CPU EP, so the ["vulkan","cpu"] fallback chain works through the same .so.
#
# Idempotent (version marker). Every failure is warn-and-continue — the
# engine still installs and runs on the bundled OpenVINO/CPU runtime (the
# amd tier's ep_priority falls through "vulkan" -> "cpu").
_install_ort_webgpu() {
    # WebGPU/Vulkan needs a DRM render node + a Vulkan ICD to attach to. If
    # neither is present there is nothing for Dawn to bind, so skip the
    # download and let the engine run CPU-only.
    if ! compgen -G "/dev/dri/renderD*" >/dev/null; then
        warn "no DRM render node (/dev/dri/renderD*); skipping WebGPU ONNX Runtime"
        warn "fetch (engine uses the bundled OpenVINO/CPU runtime)."
        return 0
    fi
    if ! compgen -G "/usr/share/vulkan/icd.d/*.json" >/dev/null \
        && ! compgen -G "/etc/vulkan/icd.d/*.json" >/dev/null; then
        warn "no Vulkan ICD found under /usr/share/vulkan/icd.d; skipping WebGPU"
        warn "ONNX Runtime fetch (engine uses the bundled OpenVINO/CPU runtime)."
        return 0
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        warn "python3 unavailable; cannot resolve onnxruntime-webgpu wheel URL — skipping."
        return 0
    fi

    local ort_ver="1.27.0"
    local vendor_dir="${NEXUS_ORT_WEBGPU_DIR:-/opt/nexus/vendor/onnxruntime-webgpu}"
    local marker="$vendor_dir/.nexus-ort-webgpu-version"

    # Idempotent short-circuit: already installed at the target version.
    if [[ -f "$marker" ]] && grep -qx "$ort_ver" "$marker" 2>/dev/null \
        && [[ -e "$vendor_dir/libonnxruntime.so" ]]; then
        log "WebGPU ONNX Runtime $ort_ver already installed ($vendor_dir)"
        _ort_webgpu_write_dropin "$vendor_dir"
        return 0
    fi

    log "installing WebGPU ONNX Runtime $ort_ver (Vulkan inference for AMD GPUs)"

    # Resolve a manylinux x86_64 wheel URL from the PyPI JSON API. The C
    # API .so is identical across cp tags (it is not Python-version
    # specific), so any manylinux x86_64 wheel works — take the first.
    local url
    url="$(curl -fsSL "https://pypi.org/pypi/onnxruntime-webgpu/${ort_ver}/json" 2>/dev/null \
        | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for u in d.get("urls", []):
    fn = u.get("filename", "")
    if fn.endswith(".whl") and "manylinux" in fn and "x86_64" in fn:
        print(u["url"])
        break
' 2>/dev/null)"

    if [[ -z "$url" ]]; then
        warn "could not resolve an onnxruntime-webgpu manylinux x86_64 wheel URL;"
        warn "engine will use the bundled OpenVINO/CPU runtime (Vulkan disabled)."
        return 0
    fi

    local tmpdir
    tmpdir="$(mktemp -d -t nexus-ort-webgpu.XXXXXX)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" RETURN

    log "  fetching $(basename "$url") (~24 MB)…"
    if ! curl -fsSL "$url" -o "$tmpdir/ort.whl"; then
        warn "WebGPU ONNX Runtime download failed; Vulkan inference disabled."
        return 0
    fi

    # Wheels are zip archives; the runtime + provider .so files live under
    # onnxruntime/capi/. Need unzip — install if missing.
    if ! command -v unzip >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq unzip >/dev/null 2>&1 || {
            warn "unzip unavailable and install failed; cannot unpack WebGPU ORT wheel"
            return 0
        }
    fi

    if ! unzip -q -o "$tmpdir/ort.whl" 'onnxruntime/capi/*' -d "$tmpdir/extract"; then
        warn "WebGPU ORT wheel extract failed; Vulkan inference disabled"
        return 0
    fi

    local capi="$tmpdir/extract/onnxruntime/capi"
    if [[ ! -e "$capi/libonnxruntime.so" ]] \
        && ! compgen -G "$capi/libonnxruntime.so*" >/dev/null; then
        warn "WebGPU ORT wheel layout unexpected (no libonnxruntime.so under capi/)"
        return 0
    fi

    # Stage into the vendor dir. Ship the versioned runtime + the provider
    # .so siblings ONNX Runtime dlopens at session-create time
    # (onnxruntime_providers_shared.so). Dawn is statically linked into
    # libonnxruntime.so itself, so there is no separate webgpu provider .so.
    install -d -m 0755 "$vendor_dir"
    cp -a "$capi"/libonnxruntime*.so* "$vendor_dir"/ 2>/dev/null || true
    cp -a "$capi"/libonnxruntime_providers_*.so* "$vendor_dir"/ 2>/dev/null || true

    # The ort crate dlopens the unversioned soname `libonnxruntime.so`. The
    # wheel ships it as libonnxruntime.so.<ver>; create the symlink.
    if [[ ! -e "$vendor_dir/libonnxruntime.so" ]]; then
        local versioned
        versioned="$(cd "$vendor_dir" && ls libonnxruntime.so.* 2>/dev/null | head -1)"
        [[ -n "$versioned" ]] && ln -sf "$versioned" "$vendor_dir/libonnxruntime.so"
    fi

    if [[ ! -e "$vendor_dir/libonnxruntime.so" ]]; then
        warn "WebGPU ORT staged but libonnxruntime.so symlink missing; Vulkan disabled"
        return 0
    fi

    printf '%s\n' "$ort_ver" > "$marker"
    log "WebGPU ONNX Runtime $ort_ver staged at $vendor_dir"

    _ort_webgpu_write_dropin "$vendor_dir"
}

# Write the systemd drop-in that repoints the engine's ORT loader at the
# WebGPU (Dawn->Vulkan) runtime. Mirrors _ort_rocm_write_dropin. The WebGPU
# .so links Dawn statically and dlopens libvulkan.so.1 from the default
# loader path (/usr/lib/x86_64-linux-gnu), so only the vendor dir needs to
# be on LD_LIBRARY_PATH (it AUGMENTS, not replaces, ld.so's default search).
_ort_webgpu_write_dropin() {
    local vendor_dir="$1"
    local dropin_dir="/etc/systemd/system/nexus-engine.service.d"
    local dropin="$dropin_dir/10-ort-webgpu.conf"

    install -d -m 0755 "$dropin_dir"
    # A box cannot run both the ROCm and the WebGPU runtime; if a stale
    # ROCm drop-in is present (e.g. a tier change) remove it so the two
    # ORT_DYLIB_PATH overrides do not stack.
    rm -f "$dropin_dir/10-ort-rocm.conf"
    cat > "$dropin" <<EOF
# Auto-generated by nexus install.sh (_install_ort_webgpu). Repoints the
# ONNX Runtime loader at the WebGPU-capable runtime (onnxruntime-webgpu,
# Dawn->Vulkan backend) for AMD GPUs ROCm does not support. The engine
# binary is built against ort api-21 so this 1.27 runtime loads cleanly.
# Overrides only the two ORT env vars from the base unit.
#
# Remove this file (and 'systemctl daemon-reload && systemctl restart
# nexus-engine') to fall back to the bundled OpenVINO/CPU runtime.
[Service]
Environment=ORT_DYLIB_PATH=${vendor_dir}/libonnxruntime.so
Environment=LD_LIBRARY_PATH=${vendor_dir}
EOF

    log "wrote systemd drop-in $dropin (ORT_DYLIB_PATH -> WebGPU/Vulkan runtime)"
    systemctl daemon-reload 2>/dev/null || true
}

# Install HailoRT for the Hailo-8 / Hailo-8L M.2 accelerator.
#
# HailoRT is distributed by Hailo Technologies under their developer
# agreement (https://hailo.ai/developer-zone/). We deliberately do
# NOT download it from a CDN inside install.sh — operators must
# register, accept the EULA, download the .deb pair, and stage them
# under `$NEXUS_HAILO_DEB_DIR` (default `/opt/nexus/vendor/hailo`).
# The installer picks them up from there.
#
# Two installable artifacts (versions track Hailo's release cadence):
#   hailort_<ver>_amd64.deb                    — userspace library
#                                                 + `hailortcli` probe
#   hailort-pcie-driver_<ver>_all.deb          — DKMS source for
#                                                 `hailo_pci.ko`
#
# Side-effect of the .deb postinst (HailoRT 4.23.x): installs a udev
# rule that leaves `/dev/hailo*` world-rw (`SUBSYSTEM=="hailo_chardev",
# MODE="0666"`) — no group, no chown. `install.sh` overrides that with
# `_hailo_install_udev_rule` (99-nexus-hailo.rules), which re-gates the
# node to `video` 0660. `ensure_accelerator_groups` runs after
# `install_drivers` in install.sh and adds the service user to `video`,
# so the engine can open /dev/hailo0 on the same install.
# (HailoRT ≤ 4.20 instead created a `hailo` group + `root:hailo 0660`
# rule; that path is still handled for back-compat.)
#
# When debs are absent we emit a multi-line warn pointing operators
# at the developer-zone register flow. The engine still installs and
# runs (CPU EP fallback) — staging the HailoRT debs and re-running
# install.sh enables the Hailo execution provider in `nexus-hailo-backend`.
# Fetch HailoRT .debs into the vendor dir.
#
# Two sources are tried in order:
#   1. URLs supplied non-interactively via env vars
#      `NEXUS_HAILO_DEB_URL` (runtime) and `NEXUS_HAILO_PCIE_DEB_URL`
#      (pcie driver). install.sh exports these from the matching
#      --hailo-deb-url / --hailo-pcie-deb-url flags. Either or both
#      may be set; the other one falls through to the prompt.
#   2. Interactive paste prompt — only when stdin is a TTY and
#      `NEXUS_UNATTENDED` is not set. Operator pastes the two
#      developer-zone presigned URLs (or hits Enter to skip).
#
# Returns 0 if it managed to land at least one new .deb (caller
# re-globs and decides whether both are now present); returns 1 if
# there is nothing useful to fetch (no URLs and no TTY, or the
# operator skipped). Curl failures are warn-and-continue so the
# install can still fall through to the register-here banner.
_hailo_fetch_debs() {
    local hailo_local_dir="$1"
    local rt_url="${NEXUS_HAILO_DEB_URL:-}"
    local pcie_url="${NEXUS_HAILO_PCIE_DEB_URL:-}"

    # Pick the prompt source. Stdin is the obvious one, but the
    # documented install path is `curl … | sudo bash`, where stdin
    # is the curl pipe (not a TTY) even when the operator is sitting
    # at an ssh -tt session. /dev/tty bypasses stdin and talks to the
    # controlling terminal directly, which is exactly what we want.
    # NEXUS_UNATTENDED=1 forces no-prompt (CI / scripted installs).
    local prompt_src=""
    if [[ "${NEXUS_UNATTENDED:-0}" != "1" ]]; then
        if [[ -r /dev/tty && -w /dev/tty ]]; then
            prompt_src="/dev/tty"
        elif [[ -t 0 ]]; then
            prompt_src="stdin"
        fi
    fi

    if [[ ( -z "$rt_url" || -z "$pcie_url" ) && -n "$prompt_src" ]]; then
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "Hailo-8 detected. To enable hardware inference acceleration"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "the installer needs the HailoRT + PCIe driver .debs from"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "your Hailo developer-zone account."
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            ""
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "On your laptop:"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "  1. Log in to https://hailo.ai/developer-zone/software-downloads/"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "  2. Right-click each link below -> Copy link address"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "     - HailoRT (Ubuntu 24.04, .deb)"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "     - HailoRT PCIe driver (.deb)"
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            "  3. Paste each URL below (or press Enter to skip)."
        printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" \
            ""

        if [[ -z "$rt_url" ]]; then
            printf '%s[nexus]%s HailoRT runtime URL: ' \
                "$(_color '1;36')" "$(_reset)"
            if [[ "$prompt_src" == "/dev/tty" ]]; then
                IFS= read -r rt_url </dev/tty || rt_url=""
            else
                IFS= read -r rt_url || rt_url=""
            fi
        fi
        if [[ -z "$pcie_url" ]]; then
            printf '%s[nexus]%s HailoRT PCIe driver URL: ' \
                "$(_color '1;36')" "$(_reset)"
            if [[ "$prompt_src" == "/dev/tty" ]]; then
                IFS= read -r pcie_url </dev/tty || pcie_url=""
            else
                IFS= read -r pcie_url || pcie_url=""
            fi
        fi
    fi

    # Trim surrounding whitespace that copy/paste sometimes drops in.
    rt_url="${rt_url#"${rt_url%%[![:space:]]*}"}"
    rt_url="${rt_url%"${rt_url##*[![:space:]]}"}"
    pcie_url="${pcie_url#"${pcie_url%%[![:space:]]*}"}"
    pcie_url="${pcie_url%"${pcie_url##*[![:space:]]}"}"

    if [[ -z "$rt_url" && -z "$pcie_url" ]]; then
        return 1
    fi

    install -d -o root -g root -m 0755 "$hailo_local_dir"

    local fetched=0
    if [[ -n "$rt_url" ]] \
        && ! compgen -G "$hailo_local_dir/hailort_*_amd64.deb" >/dev/null 2>&1; then
        if _hailo_curl_deb "$rt_url" "$hailo_local_dir" "hailort_runtime"; then
            fetched=1
        fi
    fi
    if [[ -n "$pcie_url" ]] \
        && ! compgen -G "$hailo_local_dir/hailort-pcie-driver_*.deb" >/dev/null 2>&1; then
        if _hailo_curl_deb "$pcie_url" "$hailo_local_dir" "hailort_pcie"; then
            fetched=1
        fi
    fi

    (( fetched )) && return 0 || return 1
}

# Download one .deb from a presigned URL into the vendor dir.
# Streams to a `.partial` first and atomically renames on success so
# a Ctrl-C never leaves a truncated .deb that fools the next install.
# Validates that the result is actually a Debian package (magic bytes
# `!<arch>`) before renaming — catches the case where the URL has
# expired and the server returned an HTML error page.
_hailo_curl_deb() {
    local url="$1" dest_dir="$2" label="$3"
    local tmp="$dest_dir/.${label}.partial"
    log "downloading HailoRT $label .deb from operator-supplied URL"
    if ! curl -fL --retry 3 --connect-timeout 15 -o "$tmp" "$url"; then
        warn "curl failed for $label URL (expired presigned URL?); install will fall back to CPU EP"
        rm -f "$tmp"
        return 1
    fi
    if ! head -c 8 "$tmp" | grep -q '^!<arch>'; then
        warn "$label URL did not return a .deb (got $(file -b "$tmp" 2>/dev/null || echo "unknown content"))"
        warn "  expired presigned URL? Re-copy from developer-zone and retry."
        rm -f "$tmp"
        return 1
    fi

    # Extract the package's own filename from the .deb control field
    # so the cached file matches what the operator would've staged
    # manually. Falls back to a generic name keyed off the label.
    local pkg_filename
    pkg_filename="$(dpkg-deb -f "$tmp" Package Version 2>/dev/null \
        | awk 'NR==1{p=$2} NR==2{v=$2} END{if(p&&v) printf "%s_%s_amd64.deb", p, v}')"
    if [[ -z "$pkg_filename" ]]; then
        pkg_filename="${label}.deb"
    fi

    mv "$tmp" "$dest_dir/$pkg_filename"
    chown root:root "$dest_dir/$pkg_filename"
    chmod 0644 "$dest_dir/$pkg_filename"
    log "cached HailoRT $label -> $dest_dir/$pkg_filename"
    return 0
}

# Install one HailoRT .deb, pre-answering its postinst's interactive
# prompt. Hailo's postinst scripts use `set -eEuo pipefail` together
# with `read -s -p ... -n 1 -t 10 reply || (( $? > 128 ))`, which has
# a known unset-var bug: when stdin is closed (`</dev/null`) or the
# 10s timeout fires with no input, `$reply` is never bound and the
# next `case $reply in` aborts on `set -u`. The `_drivers_hailo_pcie`
# call site used `</dev/null` for years and silently hit this bug
# whenever apt's noninteractive frontend wasn't enough — which is
# every time on HailoRT 4.23+.
#
# Fix: pipe a single-character answer plus newline. `read -n 1`
# consumes one byte, `$reply` is bound, the `case` dispatches,
# postinst exits 0.
#
# $1 = path to .deb
# $2 = answer to feed (Y for pcie-driver "use DKMS?", n for runtime
#      "activate hailort.service?")
# $3 = human label for log lines
_hailo_apt_install_with_answer() {
    local deb="$1" answer="$2" label="$3"
    if [[ ! -f "$deb" ]]; then
        warn "$label .deb missing at $deb"
        return 1
    fi
    log "installing $label (answer='$answer' to postinst prompt)"
    if printf '%s\n' "$answer" \
        | DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$deb"; then
        return 0
    fi
    # apt-get failed. Most common cause: the postinst's read-timeout
    # unset-var bug fired before our pipe reached it (apt buffers
    # stdin oddly under some configurations), or the DKMS build
    # itself failed. Try a direct dpkg --configure with the same
    # answer — that bypasses apt's stdin handling.
    warn "$label apt-get install returned non-zero; retrying via dpkg --configure"
    local pkg
    pkg=$(dpkg-deb -f "$deb" Package 2>/dev/null)
    if [[ -n "$pkg" ]] && printf '%s\n' "$answer" | dpkg --configure "$pkg" 2>&1; then
        log "$label finished via dpkg --configure"
        return 0
    fi
    warn "$label install failed after retry; package state may be 'iF'"
    return 1
}

# Patch known kernel-API renames in the Hailo DKMS source tree, then
# rebuild the module against the running kernel. Defense-in-depth for
# operators with older HailoRT cached .debs (4.20–4.22 don't have the
# `compact.h` shim that 4.23 ships). HailoRT 4.23 already handles
# `del_timer_sync` → `timer_delete_sync` (kernel 6.15) internally; if
# Hailo regresses or kernel adds another rename in 6.18+ we pick up
# the same patch hook here.
#
# Returns 0 if a patch was applied (caller should retry dkms build),
# 1 if no patches were needed.
_hailo_dkms_compat_patch() {
    # DKMS source dir from postinst — name varies across HailoRT
    # versions: 4.18 used hailort-pcie-driver-<ver>, 4.20+ uses
    # hailo_pci-<ver>, future versions could rename again. Glob both.
    local src_dirs=()
    local d
    for d in /usr/src/hailo_pci-* /usr/src/hailort-pcie-driver* /usr/src/hailo1x_pci-*; do
        [[ -d "$d" ]] && src_dirs+=("$d")
    done
    if (( ${#src_dirs[@]} == 0 )); then
        return 1
    fi

    # Map of removed-symbol -> replacement, kernel that did the removal.
    # Only adds: never remove a mapping (existing edge boxes pinned to
    # old kernels still need it).
    local -A renames=(
        [del_timer_sync]=timer_delete_sync   # removed in 6.15
        [del_timer]=timer_delete              # removed in 6.15
    )

    local patched=0 sym repl
    for sym in "${!renames[@]}"; do
        repl="${renames[$sym]}"
        for d in "${src_dirs[@]}"; do
            # Only patch if the old symbol actually appears (whole word)
            # AND the new one isn't already there. Avoids re-patching.
            if grep -rqE "\b${sym}\b" "$d" 2>/dev/null; then
                log "  patching kernel-API rename: $sym -> $repl in $d"
                find "$d" -type f \( -name '*.c' -o -name '*.h' \) -print0 \
                    | xargs -0 sed -i "s/\b${sym}\b/${repl}/g" 2>/dev/null || true
                patched=1
            fi
        done
    done

    (( patched )) || return 1

    # Rebuild every DKMS module under the patched trees against the
    # running kernel. dkms add is idempotent (we ignore the "already
    # exists" failure) but we MUST `dkms remove --all` first so the
    # cached prior-build artefact doesn't shadow our patched source.
    local mod ver
    for d in "${src_dirs[@]}"; do
        if [[ -f "$d/dkms.conf" ]]; then
            mod=$(awk -F'"' '/^PACKAGE_NAME=/ {print $2}' "$d/dkms.conf")
            ver=$(awk -F'"' '/^PACKAGE_VERSION=/ {print $2}' "$d/dkms.conf")
        else
            # Fall back to dirname pattern: hailo_pci-4.23.0
            local base; base=$(basename "$d")
            mod="${base%-*}"
            ver="${base##*-}"
        fi
        [[ -z "$mod" || -z "$ver" ]] && continue
        log "  rebuilding $mod/$ver against $(uname -r) with patched source"
        dkms remove -m "$mod" -v "$ver" --all 2>/dev/null || true
        dkms add -m "$mod" -v "$ver" 2>/dev/null || true
        if ! dkms build -m "$mod" -v "$ver" -k "$(uname -r)" 2>&1 | tail -3; then
            warn "  dkms build of $mod/$ver still failed after patch"
            return 1
        fi
        dkms install -m "$mod" -v "$ver" -k "$(uname -r)" 2>&1 | tail -3 || true
    done
    return 0
}

# Harden the Hailo character-device permissions. HailoRT 4.23.x ships
# a udev rule that sets `/dev/hailo*` world-rw (MODE="0666", no group),
# which is more permissive than necessary — any local user can talk to
# the NPU. Older HailoRT (≤ 4.20) created a `hailo` group and chowned
# the node `root:hailo 0660`; 4.23.x dropped that.
#
# We drop our own rule that re-group-gates the node to `video` (a group
# that always exists and that the `nexus` service user is already a
# member of via `ensure_accelerator_groups`) at MODE 0660. The filename
# (99-) sorts after Hailo's own rule, so our GROUP/MODE wins. Idempotent:
# rewrites the same content every run, then reloads + triggers udev so
# the live `/dev/hailo0` picks up the new perms without a reboot.
_hailo_install_udev_rule() {
    local rule_path="/etc/udev/rules.d/99-nexus-hailo.rules"
    log "installing Hailo device-permission hardening rule ($rule_path)"
    cat >"$rule_path" <<'EOF'
# Managed by nexus install.sh — group-gate the Hailo char device to
# `video` 0660 instead of HailoRT's default world-rw 0666. See
# deploy/udev/99-nexus-hailo.rules for the full rationale.
SUBSYSTEM=="hailo_chardev", GROUP="video", MODE="0660"
EOF
    chmod 0644 "$rule_path"
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=hailo_chardev 2>/dev/null || true
}

_drivers_hailo_pcie() {
    local hailo_local_dir="${NEXUS_HAILO_DEB_DIR:-/opt/nexus/vendor/hailo}"
    local rt_deb pcie_deb
    rt_deb=$(compgen -G "$hailo_local_dir/hailort_*_amd64.deb" 2>/dev/null | head -n1 || true)
    pcie_deb=$(compgen -G "$hailo_local_dir/hailort-pcie-driver_*.deb" 2>/dev/null | head -n1 || true)

    # If the operator passed download URLs (via flag/env) or we have
    # a TTY to prompt on, try to fetch the missing .debs into the
    # vendor dir before falling through to the register-here banner.
    # Either flow caches the .debs at $hailo_local_dir so subsequent
    # installs / rollbacks are no-network re-runs.
    if [[ -z "$rt_deb" || -z "$pcie_deb" ]]; then
        if _hailo_fetch_debs "$hailo_local_dir"; then
            rt_deb=$(compgen -G "$hailo_local_dir/hailort_*_amd64.deb" 2>/dev/null | head -n1 || true)
            pcie_deb=$(compgen -G "$hailo_local_dir/hailort-pcie-driver_*.deb" 2>/dev/null | head -n1 || true)
        fi
    fi

    if [[ -z "$rt_deb" || -z "$pcie_deb" ]]; then
        warn ""
        warn "================================================================"
        warn "Hailo-8 hardware detected (PCI vendor 1e60) but no HailoRT .debs"
        warn "found under $hailo_local_dir/."
        warn ""
        warn "HailoRT is a Hailo Technologies product distributed under their"
        warn "developer agreement. To enable the Hailo userspace + driver:"
        warn "  1. Register at https://hailo.ai/developer-zone/"
        warn "  2. Download hailort_<ver>_amd64.deb +"
        warn "     hailort-pcie-driver_<ver>_all.deb (Ubuntu 22.04 or 24.04)"
        warn "  3. Either:"
        warn "       (a) re-run install.sh and paste each download URL"
        warn "           when prompted (right-click the link on the"
        warn "           developer-zone download page -> Copy link), OR"
        warn "       (b) sudo mkdir -p $hailo_local_dir && sudo cp *.deb $hailo_local_dir/"
        warn "           then re-run install.sh, OR"
        warn "       (c) re-run install.sh with --hailo-deb-url <url>"
        warn "           --hailo-pcie-deb-url <url> (unattended)."
        warn ""
        warn "Engine will install and run, but inference will fall back to"
        warn "the CPU EP — stage the HailoRT debs and re-run install.sh to"
        warn "enable the Hailo execution provider."
        warn "================================================================"
        warn ""
        return 0
    fi

    # Skip if a usable HailoRT is already in place. `hailortcli scan`
    # is the canonical ground-truth probe; an exit code of 0 with
    # `Hailo Device` in stdout means the driver is bound AND the
    # userspace can open it.
    if command -v hailortcli >/dev/null 2>&1 \
        && hailortcli scan 2>/dev/null | grep -q 'Hailo Device'; then
        log "HailoRT already installed and hailortcli scan succeeds (driver + userspace healthy)"
        return 0
    fi

    log "installing HailoRT from $hailo_local_dir (pcie driver: $(basename "$pcie_deb"), runtime: $(basename "$rt_deb"))"

    # dkms + matching headers are required to build hailo_pci.ko
    # against the running kernel. Pull the exact `uname -r` to avoid
    # silently building against a stale linux-headers-generic.
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        dkms "linux-headers-$(uname -r)" </dev/null || {
            warn "dkms or linux-headers-$(uname -r) install failed; HailoRT install aborted"
            return 0
        }

    # Install pcie driver FIRST so the postinst dkms build sees the
    # headers we just pulled. Then the runtime userspace. We pipe an
    # explicit one-character answer instead of using `</dev/null`
    # because Hailo's postinsts use `set -u` together with a
    # `read -t 10` that leaves $reply unbound on EOF/timeout — see
    # _hailo_apt_install_with_answer for the full story.
    #   pcie postinst : "Do you wish to use DKMS? [Y/n]"  -> Y
    #   runtime postinst : "activate hailort.service? [y/N]" -> n
    if ! _hailo_apt_install_with_answer "$pcie_deb" Y "hailort-pcie-driver"; then
        # Most likely cause now is a kernel-API rename the HailoRT
        # source predates (e.g. del_timer_sync removed in 6.15+).
        # Try the compat patch in-place and re-finalise the postinst.
        if _hailo_dkms_compat_patch; then
            log "kernel-API patch applied; re-finalising hailort-pcie-driver"
            if ! printf 'Y\n' | dpkg --configure hailort-pcie-driver 2>&1; then
                warn "hailort-pcie-driver still failed after patch; engine will fall back to CPU EP"
                return 0
            fi
        else
            warn "hailort-pcie-driver install failed and no known kernel-API patch matched"
            warn "  see /var/log/hailort-pcie-driver.deb.log and"
            warn "  /var/lib/dkms/hailo_pci/*/build/make.log for details"
            return 0
        fi
    fi
    if ! _hailo_apt_install_with_answer "$rt_deb" n "hailort"; then
        warn "hailort runtime install failed; engine will fall back to CPU EP"
        warn "  see /var/log/hailort.deb.log for details"
        return 0
    fi

    # Refresh ldconfig cache. Hailo's runtime postinst symlinks
    # /usr/lib/libhailort.so but does NOT run ldconfig — and on a
    # newly-installed package some loaders only see the .so via the
    # cache. Cheap and idempotent.
    ldconfig 2>/dev/null || true

    # Confirm both packages landed in 'ii' state. Anything else
    # (iF / iU / iH) means a postinst aborted half-way and the
    # engine will hit a partial userspace at runtime.
    local bad
    bad=$(dpkg-query -W -f='${db:Status-Status} ${binary:Package}\n' \
        hailort hailort-pcie-driver 2>/dev/null \
        | awk '$1 != "installed" {print $2 "(" $1 ")"}' | xargs)
    if [[ -n "$bad" ]]; then
        warn "HailoRT packages not fully installed: $bad"
        warn "  fix: re-run install.sh after resolving the postinst error"
        return 0
    fi

    # Try to bind the module immediately so a fresh install doesn't
    # need a reboot. DKMS may have already done this via the postinst
    # — modprobe is idempotent either way.
    if modprobe hailo_pci 2>/dev/null; then
        log "hailo_pci module loaded"
    else
        warn "modprobe hailo_pci failed; a reboot is required before /dev/hailo0 appears"
    fi

    # Replace HailoRT's world-rw (0666) udev rule with a video-group
    # gated 0660 rule before the device node settles. Done after
    # modprobe so the trigger re-applies perms to the live node.
    _hailo_install_udev_rule

    if [[ -e /dev/hailo0 ]]; then
        log "HailoRT installed; /dev/hailo0 present"
    else
        warn "HailoRT installed but /dev/hailo0 not yet present — reboot and re-run install.sh to verify"
    fi
}

# --- Atomic-swap symlink ------------------------------------------------------

# Flip /opt/nexus/current -> releases/<version> with `ln -sfn` then a
# rename, both of which are atomic on POSIX.  The previous target (if
# any) is recorded into install-state.json as `previous_good_version`
# so the M-OTA updater (and `scripts/install.sh --rollback`) can
# revert without re-downloading anything.
swap_current_symlink() {
    local target_version="$1"
    local link="$NEXUS_PREFIX/current"
    local previous=""

    if [[ -L "$link" ]]; then
        previous="$(basename "$(readlink "$link")")"
    fi

    # ln -sfn is the canonical "atomic replace a symlink" recipe:
    # it creates a temp symlink with target_version then rename(2)s
    # it over the existing one.
    ln -sfn "releases/$target_version" "$link"

    if [[ -n "$previous" && "$previous" != "$target_version" ]]; then
        log "swapped current: $previous -> $target_version"
    else
        log "current -> $target_version"
    fi
    printf '%s' "$previous"
}

# --- Manifest verification ----------------------------------------------------

# Verify the tarball's accompanying .sha256 sidecar matches the
# tarball on disk.  Returns 0 on match, dies on mismatch.
verify_sha256() {
    local tarball="$1"
    local sha_file="$2"
    [[ -r "$tarball"  ]] || die "tarball not readable: $tarball"
    [[ -r "$sha_file" ]] || die "sha256 sidecar not readable: $sha_file"

    local expected actual
    # Sidecar format: "<sha256>  <filename>" (sha256sum's default).
    expected="$(awk '{print $1}' "$sha_file" | head -n1)"
    actual="$(sha256sum "$tarball" | awk '{print $1}')"

    [[ -n "$expected" ]] || die "sha256 sidecar empty: $sha_file"
    [[ "$expected" == "$actual" ]] \
        || die "sha256 mismatch:\n  expected $expected\n  actual   $actual"

    log "sha256 OK ($actual)"
}

# Walk every file listed in MANIFEST.json (written at build time by
# the release workflow) and verify each one's sha256.  Catches the
# case where someone hand-edited a file inside the extracted release
# dir between install and runtime.  Tolerates missing jq by falling
# back to a python one-liner.
verify_manifest_files() {
    local release_dir="$1"
    local manifest="$release_dir/MANIFEST.json"

    [[ -r "$manifest" ]] || die "release MANIFEST.json missing: $manifest"

    if command -v jq >/dev/null 2>&1; then
        jq -r '.files[] | "\(.sha256)  \(.path)"' "$manifest" \
            | ( cd "$release_dir" && sha256sum -c --quiet --strict - )
    else
        python3 - "$release_dir" "$manifest" <<'PY'
import hashlib, json, sys
release_dir, manifest_path = sys.argv[1], sys.argv[2]
with open(manifest_path) as f:
    manifest = json.load(f)
bad = []
for entry in manifest["files"]:
    p = f"{release_dir}/{entry['path']}"
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    if h.hexdigest() != entry["sha256"]:
        bad.append(entry["path"])
if bad:
    print("sha256 mismatch:", *bad, sep="\n  ", file=sys.stderr)
    sys.exit(1)
PY
    fi
    log "manifest sha256 OK ($(jq -r '.files | length' "$manifest" 2>/dev/null || echo '?') files)"
}

# --- Ed25519 signature verification ------------------------------------------

# Verify MANIFEST.json against MANIFEST.json.sig using the
# operator-onboarded public key committed at
# `scripts/lib/release-pubkey.pem`.
#
# Three outcomes:
#
#   1. Both .sig + pubkey present + signature valid     -> OK, log + return 0
#   2. Both .sig + pubkey present + signature INVALID   -> die (always fatal)
#   3. .sig OR pubkey absent
#        - NEXUS_REQUIRE_SIGNATURE=1 in env             -> die (paranoid mode)
#        - otherwise                                    -> warn + return 0
#
# Outcome 3's warning-without-die is intentional: it lets the very
# first release cut (before the GH signing secret is onboarded) ship
# a tarball without breaking install.sh. Once both halves are in
# place every subsequent release verifies strictly.
verify_signature() {
    local release_dir="$1"
    local manifest="$release_dir/MANIFEST.json"
    local sig="$release_dir/MANIFEST.json.sig"
    local pubkey="$release_dir/scripts/lib/release-pubkey.pem"

    [[ -r "$manifest" ]] || die "release MANIFEST.json missing: $manifest"

    if [[ ! -r "$pubkey" ]]; then
        if [[ "${NEXUS_REQUIRE_SIGNATURE:-0}" == "1" ]]; then
            die "NEXUS_REQUIRE_SIGNATURE=1 but no public key bundled in release: $pubkey"
        fi
        warn "release-pubkey.pem missing from release; cannot verify signature"
        return 0
    fi

    if [[ ! -r "$sig" ]]; then
        if [[ "${NEXUS_REQUIRE_SIGNATURE:-0}" == "1" ]]; then
            die "NEXUS_REQUIRE_SIGNATURE=1 but tarball is unsigned (no MANIFEST.json.sig)"
        fi
        warn "release is UNSIGNED (no MANIFEST.json.sig); skipping signature check"
        warn "  to enforce signatures, re-run with NEXUS_REQUIRE_SIGNATURE=1"
        return 0
    fi

    require_cmd openssl
    # Ed25519 raw-message verification (no pre-hash). Output goes
    # to /dev/null because openssl prints "Signature Verified
    # Successfully" on success which we duplicate in log() below.
    if ! openssl pkeyutl -verify -pubin -inkey "$pubkey" -rawin \
            -in "$manifest" -sigfile "$sig" >/dev/null 2>&1; then
        die "MANIFEST.json signature did NOT verify against bundled pubkey — refusing to install"
    fi
    log "MANIFEST.json signature OK (Ed25519, $(wc -c < "$sig") bytes)"
}

# --- Config generation (nexus-probe emit-config) ------------------------------

# Generate /etc/nexus/nexus.toml from the DETECTED hardware via
# `nexus-probe emit-config`, replacing the old per-tier template copy
# (`stage_tier_config`) and the `recommended_tier` lookup. nexus-probe
# builds a real, fully-defaulted `nexus_config::Config` from the box's
# capabilities (inference EP order, decode mode, worker/blocking
# threads, inference workers, model preset, bus capacity) and writes
# the bare-metal pack_path/ui_root directly — so the old sed
# path-rewrites and the Hailo ep_priority override are gone, decided in
# exactly one place (the generator).
#
# Runs on BOTH first install and every re-run. By DEFAULT it
# regenerates the config so an upgraded box picks up the
# capability-derived settings for its current hardware; any existing
# file is backed up to nexus.toml.bak.<ts> first. `--keep-config`
# (KEEP_CONFIG=1) opts out and preserves a hand-tuned file untouched.
#
# Arg 2 is the optional `--force-profile` value (intel-igpu | intel-npu
# | amd-vulkan | amd-rocm | hailo | nvidia | cpu); empty = auto-detect.
#
# ORDER CONTRACT: install_drivers() MUST have run before this so the VA
# GStreamer stack is present when emit-config probes for hardware decode.
emit_config() {
    local release_dir="$1"
    local force_profile="${2:-}"
    local target="$NEXUS_CONFIG_DIR/nexus.toml"
    local probe="$release_dir/bin/nexus-probe"

    [[ -x "$probe" ]] || die "nexus-probe not found or not executable: $probe"

    if [[ -e "$target" ]] && (( ${KEEP_CONFIG:-0} )); then
        log "--keep-config: preserving existing config: $target (skipping regenerate)"
        return 0
    fi

    install -d -o root -g root -m 0755 "$NEXUS_CONFIG_DIR"

    # Unconditionally back up before any overwrite so the operator can
    # always roll back to the previous config.
    if [[ -e "$target" ]]; then
        local backup
        backup="$target.bak.$(date +%Y%m%dT%H%M%S)"
        log "backing up existing config to $backup before regenerate"
        cp -a "$target" "$backup"
    fi

    local args=(emit-config --out "$target")
    if [[ -n "$force_profile" ]]; then
        args+=(--force-profile "$force_profile")
    fi

    log "generating $target from detected hardware (nexus-probe emit-config)"
    if ! "$probe" "${args[@]}"; then
        die "nexus-probe emit-config failed; refusing to start with an incomplete config"
    fi
    # nexus-probe writes the file as the invoking user (root); normalize
    # owner/mode to the old `install -m 0644` contract.
    chown root:root "$target"
    chmod 0644 "$target"
    log "generated config: $target"
}

# --- install-state.json -------------------------------------------------------

# Single-row state file the M-OTA updater (and rollback) read.  Path
# is canonical so the updater can find it without env wiring.  Format
# matches the cloud's `update_assignment_current` row shape so the
# updater can mirror state without translation.
write_install_state() {
    local version="$1"
    local previous="${2:-}"
    local state_file="$NEXUS_CONFIG_DIR/install-state.json"

    install -d -o root -g root -m 0755 "$NEXUS_CONFIG_DIR"

    python3 - "$state_file" "$version" "$previous" <<'PY'
import json, os, sys, time
state_file, version, previous = sys.argv[1], sys.argv[2], sys.argv[3]

state = {}
if os.path.exists(state_file):
    try:
        with open(state_file) as f:
            state = json.load(f)
    except Exception:
        state = {}

# Promote the prior current to previous_good so a future install.sh
# --rollback (or the M-OTA updater) can flip back without
# re-downloading.
state["channel"]               = state.get("channel", "stable")
state["previous_good_version"] = previous or state.get("current_version")
state["current_version"]       = version
state["installed_at"]          = int(time.time())
# Track the systemd unit hash so the M-OTA updater can refuse to
# auto-update on top of operator hand-edits (see M_OTA.md
# "Compose-file tamper detection" — same idea, different file).
unit = "/etc/systemd/system/nexus-engine.service"
if os.path.exists(unit):
    import hashlib
    h = hashlib.sha256()
    with open(unit, "rb") as f:
        for c in iter(lambda: f.read(1 << 16), b""):
            h.update(c)
    state["systemd_unit_sha256"] = h.hexdigest()

with open(state_file + ".tmp", "w") as f:
    json.dump(state, f, indent=2, sort_keys=True)
    f.write("\n")
os.replace(state_file + ".tmp", state_file)
os.chmod(state_file, 0o644)
PY

    log "wrote $state_file"
}

# --- systemd unit -------------------------------------------------------------

install_systemd_unit() {
    local release_dir="$1"
    local src="$release_dir/etc-templates/systemd/nexus-engine.service"
    local target="$NEXUS_SYSTEMD_DIR/nexus-engine.service"

    [[ -r "$src" ]] || die "systemd unit template not found: $src"

    install -o root -g root -m 0644 "$src" "$target"
    log "installed systemd unit: $target"

    systemctl daemon-reload
}

# --- OTA sudoers allowlist ----------------------------------------------------

# Phase 9 (M_OTA). Install the narrow sudoers rule that lets the
# unprivileged `nexus` service user run the three pinned OTA commands
# (tar extract, symlink flip, self restart) as root NOPASSWD. Validated
# with `visudo -cf` before it lands so a malformed file can never wedge
# sudo for the whole box.
install_update_sudoers() {
    local release_dir="$1"
    local src="$release_dir/etc-templates/sudoers.d/nexus-update"
    local target="/etc/sudoers.d/nexus-update"

    [[ -r "$src" ]] || die "OTA sudoers template not found: $src"

    # Stage to a temp file, validate, then atomically move into place so
    # a partial write can never break sudo parsing of /etc/sudoers.d.
    local tmp
    tmp="$(mktemp)"
    install -o root -g root -m 0440 "$src" "$tmp"
    if ! visudo -cf "$tmp" >/dev/null 2>&1; then
        rm -f "$tmp"
        die "OTA sudoers file failed visudo validation: $src"
    fi
    install -o root -g root -m 0440 "$tmp" "$target"
    rm -f "$tmp"
    log "installed OTA sudoers allowlist: $target"
}

# --- Health check -------------------------------------------------------------

# Poll the API until /api/v1/health returns 200 or the timeout elapses.
# The engine takes a few seconds to run migrations + open the model
# pack on first boot; 60s is generous but still bounded.
wait_for_health() {
    local timeout="${1:-60}"
    local port="${2:-8089}"
    local i=0
    log "waiting for engine /api/v1/health on :$port (up to ${timeout}s)..."
    while (( i < timeout )); do
        if curl -fsS -m 2 -o /dev/null "http://127.0.0.1:${port}/api/v1/health" 2>/dev/null; then
            log "engine healthy after ${i}s"
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

# Returns 0 if process $1 has any open fd whose symlink target
# starts with $2. Avoids `ls | grep` (SC2010) on /proc/PID/fd.
_proc_has_fd_to() {
    local pid="$1" prefix="$2" fd target
    for fd in /proc/"$pid"/fd/*; do
        target="$(readlink "$fd" 2>/dev/null)" || continue
        case "$target" in
            "$prefix"*) return 0 ;;
        esac
    done
    return 1
}

# Once the engine is up, prove via /proc/$PID/maps that the
# OpenVINO accelerator plugin .so actually loaded \u2014 i.e. that ORT
# successfully attached the OpenVINO EP to the GPU/NPU and not just
# the CPU fallback. This is the runtime complement to the install-
# time `verify_accelerators` probe.
#
# Why /proc/$PID/maps instead of grepping logs for `ep_registered`:
# `ep_registered` reports the EPs the engine *requested*, not the
# ones ORT actually bound. A box where OpenVINO falls back to CPU
# silently (e.g. libze1 ABI mismatch with bundled OV) still logs
# `ep_registered=[openvino(GPU)]` but never dlopens
# libopenvino_intel_gpu_plugin.so. Maps doesn't lie.
verify_engine_runtime_eps() {
    if ! command -v lspci >/dev/null 2>&1; then
        return 0
    fi

    local tags
    tags="$(_detect_hardware 2>/dev/null)" || return 0
    [[ -z "$tags" ]] && return 0

    local has_igpu=0 has_arc=0 has_npu=0
    while IFS= read -r tag; do
        case "$tag" in
            intel-igpu|intel-arc-dgpu) has_igpu=1 ;;
            intel-npu)                 has_npu=1 ;;
        esac
    done <<<"$tags"

    local pid
    pid="$(systemctl show -p MainPID --value nexus-engine 2>/dev/null)"
    if [[ -z "$pid" || "$pid" == "0" || ! -r "/proc/$pid/maps" ]]; then
        warn "verify_engine_runtime_eps: nexus-engine MainPID unreadable; skipping runtime EP probe"
        return 0
    fi

    log ""
    log "===== Engine runtime EP attachment ====="
    log "nexus-engine PID=$pid"

    local maps
    maps="$(cat /proc/"$pid"/maps 2>/dev/null)" || {
        warn "could not read /proc/$pid/maps (engine may have just restarted)"
        return 0
    }

    if (( has_igpu || has_arc )); then
        if grep -q 'libopenvino_intel_gpu_plugin\.so' <<<"$maps"; then
            log "  [ OK ] libopenvino_intel_gpu_plugin.so loaded \u2014 OpenVINO attached to GPU"
            if grep -q 'libigdrcl\.so' <<<"$maps"; then
                log "  [ OK ] libigdrcl.so (Intel NEO compute runtime) loaded"
            else
                warn "  [WARN] libigdrcl.so not in process map \u2014 GPU plugin may not have dlopened the OpenCL ICD yet"
            fi
            if _proc_has_fd_to "$pid" '/dev/dri/renderD'; then
                log "  [ OK ] engine has an open fd on /dev/dri/renderD* (GPU device in use)"
            else
                warn "  [WARN] engine has no /dev/dri/renderD* fd open \u2014 GPU plugin loaded but no device attached"
            fi
        else
            warn "  [FAIL] libopenvino_intel_gpu_plugin.so NOT loaded in engine process"
            warn "         OpenVINO failed to attach to the GPU; engine is running on CPU EP."
            warn "         Common causes:"
            warn "           - intel-opencl-icd missing (see install-time verification above)"
            warn "           - libze1 ABI mismatch (need kobuk-team PPA 1.30+, not Ubuntu archive 1.16.1)"
            warn "           - OV_DEVICE not set to GPU in nexus.toml or systemd drop-in"
            warn "         debug: sudo journalctl -u nexus-engine -n 200 | grep -iE 'openvino|gpu|ep_registered'"
        fi
    fi

    if (( has_npu )); then
        if grep -q 'libopenvino_intel_npu_plugin\.so' <<<"$maps"; then
            log "  [ OK ] libopenvino_intel_npu_plugin.so loaded \u2014 OpenVINO attached to NPU"
            if _proc_has_fd_to "$pid" '/dev/accel/accel'; then
                log "  [ OK ] engine has an open fd on /dev/accel/accel* (NPU device in use)"
            else
                warn "  [WARN] engine has no /dev/accel/accel* fd open \u2014 NPU plugin loaded but no device attached"
            fi
        else
            warn "  [FAIL] libopenvino_intel_npu_plugin.so NOT loaded in engine process"
            warn "         OpenVINO failed to attach to the NPU; engine is running on CPU EP for NPU work."
            warn "         Common causes:"
            warn "           - intel-driver-compiler-npu / intel-fw-npu / intel-level-zero-npu missing"
            warn "           - libze1 ABI mismatch (need kobuk-team PPA 1.30+)"
            warn "           - HWE kernel < 6.10 (NPU uAPI not present)"
            warn "         debug: sudo journalctl -u nexus-engine -n 200 | grep -iE 'openvino|npu|ep_registered'"
        fi
    fi
    log "========================================"
    log ""
}
