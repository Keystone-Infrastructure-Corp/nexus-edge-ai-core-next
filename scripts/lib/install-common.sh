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
}

# Add the service user to `render` and `video` groups so the engine can
# open `/dev/dri/renderD128` (every iGPU/dGPU tier) and `/dev/accel/accel0`
# (T36-S NPU). Idempotent: usermod -aG is a no-op if the user is already
# a member. Groups that don't exist on the host are silently skipped —
# `render` only exists once the GPU userspace from §5 is installed, but
# the install script may run before that on a freshly-flashed box.
ensure_accelerator_groups() {
    local group
    for group in render video; do
        if getent group "$group" >/dev/null 2>&1; then
            if id -nG "$NEXUS_SERVICE_USER" | tr ' ' '\n' | grep -qx "$group"; then
                log "service user $NEXUS_SERVICE_USER already in $group"
            else
                usermod -aG "$group" "$NEXUS_SERVICE_USER"
                log "added $NEXUS_SERVICE_USER to $group group"
            fi
        else
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

    local has_igpu=0 has_arc=0 has_npu=0 has_nvidia=0
    while IFS= read -r tag; do
        case "$tag" in
            intel-igpu)     has_igpu=1 ;;
            intel-arc-dgpu) has_arc=1 ;;
            intel-npu)      has_npu=1 ;;
            nvidia-gpu)     has_nvidia=1 ;;
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

    local has_igpu=0 has_arc=0 has_npu=0
    while IFS= read -r tag; do
        case "$tag" in
            intel-igpu)     has_igpu=1 ;;
            intel-arc-dgpu) has_arc=1 ;;
            intel-npu)      has_npu=1 ;;
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
    log "===================================="
    log ""
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

# Detect accelerator hardware via lspci. Outputs one tag per line:
#   intel-igpu | intel-arc-dgpu | intel-npu | nvidia-gpu
# Empty output = nothing recognised (engine still installs CPU-only).
_detect_hardware() {
    if ! command -v lspci >/dev/null 2>&1; then
        log "lspci missing; installing pciutils for hardware detection"
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq pciutils \
            >/dev/null 2>&1 || return 1
    fi

    local pci
    pci="$(lspci -nn 2>/dev/null)" || return 1

    # Intel iGPU (display-class VGA with vendor 8086). Covers UHD,
    # Iris Xe, Arc 140V (Lunar Lake), Arc Graphics (Meteor Lake).
    # NB: dGPU Arc cards ALSO match this regex — we filter them out
    # of the iGPU tag below by checking for the discrete device IDs.
    local has_intel_vga=0 has_arc_dgpu=0
    if echo "$pci" | grep -qE 'VGA[^[]*\[8086:'; then
        has_intel_vga=1
    fi
    # Intel Arc A-series discrete (DG2 silicon): device IDs 56a0..56af.
    if echo "$pci" | grep -qE '\[8086:56[a-f][0-9a-f]\]'; then
        has_arc_dgpu=1
    fi
    if (( has_intel_vga && ! has_arc_dgpu )); then
        echo intel-igpu
    elif (( has_intel_vga && has_arc_dgpu )); then
        # Box has both (e.g. Lenovo P3 Tower with iGPU + A380).
        echo intel-igpu
    fi
    if (( has_arc_dgpu )); then
        echo intel-arc-dgpu
    fi

    # Intel NPU (Versatile Processing Unit / Neural Processing Unit).
    # Device IDs: 7d1d (Meteor Lake VPU), 643e (Arrow Lake NPU),
    # 7e4e (Lunar Lake NPU). Also matches the "Processing
    # accelerators" PCI class (1200) when the device-id list above
    # misses a future SKU.
    if echo "$pci" | grep -qE '\[8086:(7d1d|643e|7e4e)\]' \
        || echo "$pci" | grep -qiE 'processing accelerator.*\[8086:'; then
        echo intel-npu
    fi

    # NVIDIA discrete (vendor 10de).
    if echo "$pci" | grep -qE '(VGA|3D controller)[^[]*\[10de:'; then
        echo nvidia-gpu
    fi
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

# --- nexus-probe auto-tier ----------------------------------------------------

# Run the staged `nexus-probe` binary, parse its JSON manifest, and
# echo the `recommended_tier` (e.g. "t24"). Returns empty string on
# any failure (missing binary, non-zero exit, malformed JSON, tier
# not in the known set) so the caller can fall back to demanding an
# explicit `--tier`.
auto_detect_tier() {
    local release_dir="$1"
    local probe="$release_dir/bin/nexus-probe"

    if [[ ! -x "$probe" ]]; then
        return 0
    fi
    require_cmd python3

    local json
    if ! json="$("$probe" --out - 2>/dev/null)"; then
        return 0
    fi
    local tier
    # Prefer `recommended_tier_config` (e.g. "config/tiers/t24.toml")
    # over `recommended_tier` because the latter is upper-case with
    # punctuation ("T10", "T36-S") while the file stem is the exact
    # lower-case CLI tier we want ("t10", "t36s"). Falls through
    # silently for the "dev" / "config/single-camera.toml" case
    # which has no production tier mapping.
    tier="$(printf '%s' "$json" | python3 -c '
import json, os, sys
try:
    m = json.load(sys.stdin)
    cfg = m.get("recommended_tier_config", "")
    stem = os.path.splitext(os.path.basename(cfg))[0]
    if stem in ("t10","t24","t36","t36s","t64"):
        print(stem)
except Exception:
    pass
' 2>/dev/null || true)"
    printf '%s' "$tier"
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

# --- Tier config staging ------------------------------------------------------

# Copy etc-templates/tiers/<tier>.toml -> /etc/nexus/nexus.toml on
# FIRST install only.  Rewrites pack_path + ui_root to point at the
# atomic-swap symlink so upgrades that change either don't require
# editing nexus.toml.  Preserves operator edits on every subsequent
# install (the file lives in /etc, which is the contract).
stage_tier_config() {
    local tier="$1"
    local release_dir="$2"
    local target="$NEXUS_CONFIG_DIR/nexus.toml"
    local src="$release_dir/etc-templates/tiers/${tier}.toml"

    [[ -r "$src" ]] || die "tier template not found: $src"

    if [[ -e "$target" ]]; then
        log "preserving existing config: $target (tier template skipped)"
        return 0
    fi

    install -o root -g root -m 0644 "$src" "$target"
    # Tier templates use the Docker paths /usr/share/nexus/{models,ui}
    # because the same templates are bind-mounted into the container.
    # On bare-metal the equivalent lives under the atomic-swap root.
    sed -i \
        -e 's#/usr/share/nexus/models#/opt/nexus/current/share/models#g' \
        -e 's#/usr/share/nexus/ui#/opt/nexus/current/share/ui#g' \
        "$target"
    log "staged tier config: $tier -> $target"
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

# --- Health check -------------------------------------------------------------

# Poll the API until /api/health returns 200 or the timeout elapses.
# The engine takes a few seconds to run migrations + open the model
# pack on first boot; 60s is generous but still bounded.
wait_for_health() {
    local timeout="${1:-60}"
    local port="${2:-8089}"
    local i=0
    log "waiting for engine /api/health on :$port (up to ${timeout}s)..."
    while (( i < timeout )); do
        if curl -fsS -m 2 -o /dev/null "http://127.0.0.1:${port}/api/health" 2>/dev/null; then
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
