#!/usr/bin/env bash
# Audit every network interface on an edge box, then bring any wired NIC
# that no renderer owns into service with DHCP.
#
# Ubuntu Server's installer writes a netplan config naming ONLY the port
# that was cabled at install time.  A second physical NIC therefore stays
# `unmanaged` and administratively DOWN forever: it is present in lspci
# and in /sys/class/net, but holds no address, so it is invisible to
# `if_addrs::get_if_addrs()` -- which is what the engine's
# GET /v1/admin/network/interfaces enumerates from.  The port looks
# missing when nothing is actually wrong with it.
#
# Two traps this script exists to avoid:
#   * carrier and `ethtool` are meaningless while a link is admin-DOWN
#     (the PHY is powered off), so a perfectly good cabled port reports
#     "Link detected: no".  We bring candidates up before judging them.
#   * cloud-init can re-render /etc/netplan/50-cloud-init.yaml on boot and
#     silently discard edits to it, so we write our own 60-nexus-*.yaml
#     files instead and never touch the installer's.
#
# Idempotent: safe to re-run after fitting a NIC or moving a cable.

set -euo pipefail

NETPLAN_DIR="${NETPLAN_DIR:-/etc/netplan}"
FILE_PREFIX="60-nexus-"
METRIC_BASE="${METRIC_BASE:-200}"
REVERT_AFTER="${REVERT_AFTER:-180}"
REVERT_UNIT="nexus-nic-revert"

DRY_RUN=0
LINK_ONLY=0
DO_REVERT=0

# --- Logging ------------------------------------------------------------------

_color() { [[ -t 1 ]] && printf '\033[%sm' "$1" || true; }
_reset() { [[ -t 1 ]] && printf '\033[0m' || true; }

log()   { printf '%s[nexus]%s %s\n' "$(_color '1;36')" "$(_reset)" "$*"; }
warn()  { printf '%s[nexus]%s %s\n' "$(_color '1;33')" "$(_reset)" "$*" >&2; }
err()   { printf '%s[nexus]%s %s\n' "$(_color '1;31')" "$(_reset)" "$*" >&2; }
die()   { err "$*"; exit 1; }
head1() { printf '\n%s== %s%s\n' "$(_color '1;37')" "$*" "$(_reset)"; }

usage() {
    cat <<'EOF'
Audit every NIC on this box and enable any that no renderer owns.

  sudo scripts/enable-nics.sh              audit, then enable with DHCP
  sudo scripts/enable-nics.sh --dry-run    audit only, change nothing
  sudo scripts/enable-nics.sh --link-only  bring up, but assign no address
  sudo scripts/enable-nics.sh --revert     remove configs this script wrote

Environment: NETPLAN_DIR, METRIC_BASE (200), REVERT_AFTER (180s)
EOF
    exit 0
}

# --- Pre-flight ---------------------------------------------------------------

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run|--audit) DRY_RUN=1 ;;
        --link-only)       LINK_ONLY=1 ;;
        --revert)          DO_REVERT=1 ;;
        -h|--help)         usage ;;
        *)                 die "unknown argument '$1' (try --help)" ;;
    esac
    shift
done

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    die "must run as root (try: sudo $0)"
fi

have() { command -v "$1" >/dev/null 2>&1; }

for c in ip netplan networkctl; do
    have "$c" || die "required command '$c' not found on PATH"
done

# --- Interface classification -------------------------------------------------

# A NIC we are willing to manage: real hardware behind a bus, ARPHRD_ETHER,
# not wireless, not an SR-IOV virtual function.  Virtual devices (veth,
# bridges, bonds, VLAN sub-interfaces, docker0, tun) have no `device`
# symlink and are excluded by that test alone.
is_wired_nic() {
    local p="/sys/class/net/$1"
    if [[ "$1" == "lo" ]]; then return 1; fi
    if [[ ! -e "$p/device" ]]; then return 1; fi
    if [[ "$(cat "$p/type" 2>/dev/null || echo 0)" != "1" ]]; then return 1; fi
    if [[ -e "$p/wireless" || -e "$p/phy80211" ]]; then return 1; fi
    if [[ -e "$p/device/physfn" ]]; then return 1; fi
    return 0
}

# basename of a driver symlink, or "none".
link_driver() {
    local d
    d="$(readlink -f "$1/driver" 2>/dev/null || true)"
    if [[ -n "$d" ]]; then basename "$d"; else echo none; fi
}

# True when any netplan document already mentions this interface -- as an
# ethernet, or as a member of a bond/bridge/VLAN.  Deliberately greedy: a
# false positive skips the NIC, which is the safe direction.
in_netplan() {
    netplan get 2>/dev/null | grep -qF "$1:"
}

# Carrier is unreadable while a link is admin-DOWN, so bring it up first.
# Restores the original admin state when only auditing.
probe_carrier() {
    local n="$1" was_up=1 carrier
    if [[ "$(cat "/sys/class/net/$n/operstate" 2>/dev/null || echo down)" == "down" ]]; then
        was_up=0
    fi
    ip link set "$n" up 2>/dev/null || true
    for _ in $(seq 1 16); do
        if [[ "$(cat "/sys/class/net/$n/carrier" 2>/dev/null || echo 0)" == "1" ]]; then
            break
        fi
        sleep 0.5
    done
    carrier="$(cat "/sys/class/net/$n/carrier" 2>/dev/null || echo 0)"
    if [[ "$DRY_RUN" -eq 1 && "$was_up" -eq 0 ]]; then
        ip link set "$n" down 2>/dev/null || true
    fi
    [[ "$carrier" == "1" ]]
}

# --- Reporting ----------------------------------------------------------------

report_board() {
    head1 "Board"
    local f
    for f in sys_vendor product_name board_name bios_version; do
        printf '  %-14s %s\n' "$f" "$(cat "/sys/class/dmi/id/$f" 2>/dev/null || echo '?')"
    done
    printf '  %-14s %s\n' kernel "$(uname -r)"
}

# The one check that can prove a genuine hardware/driver fault: a PCI
# network-class device that never produced a netdev.
report_pci() {
    head1 "PCI network devices"
    if ! have lspci; then
        warn "pciutils not installed -- skipping PCI audit (apt install pciutils)"
        return 0
    fi
    local slot drv nets desc faults=0
    for slot in $(lspci -D -n -d ::0200 2>/dev/null | awk '{print $1}'); do
        drv="$(link_driver "/sys/bus/pci/devices/$slot")"
        nets="$(ls "/sys/bus/pci/devices/$slot/net" 2>/dev/null | tr '\n' ' ' || true)"
        desc="$(lspci -s "$slot" 2>/dev/null | cut -d' ' -f2- || true)"
        printf '  %-12s driver=%-10s netdev=%s\n' "$slot" "$drv" "${nets:-<NONE>}"
        printf '  %-12s %s\n' '' "$desc"
        if [[ -z "$nets" ]]; then
            err "  ^ PCI device present but produced NO netdev -- driver missing or failed to bind"
            faults=$((faults + 1))
        fi
    done
    if [[ -n "$(lspci -n -d ::0280 2>/dev/null || true)" ]]; then
        log "  (wireless controllers present; this script does not manage them)"
    fi
    if [[ "$faults" -gt 0 ]]; then
        warn "$faults PCI NIC(s) have no driver bound -- check dmesg for that chipset's module"
    fi
}

report_links() {
    head1 "Interfaces"
    local n p note
    for p in /sys/class/net/*; do
        n="$(basename "$p")"
        if [[ "$n" == "lo" ]]; then continue; fi
        note=""
        if ! is_wired_nic "$n"; then note="  [not a managed wired NIC]"; fi
        printf '  %-10s drv=%-10s state=%-8s mac=%s%s\n' \
            "$n" "$(link_driver "$p/device")" \
            "$(cat "$p/operstate" 2>/dev/null || echo '?')" \
            "$(cat "$p/address" 2>/dev/null || echo '?')" "$note"
    done
    printf '\n'
    ip -br addr | sed 's/^/  /'
}

report_renderer() {
    head1 "Renderer state"
    networkctl list --no-legend 2>/dev/null | sed 's/^/  /' || true
    local ci="/etc/cloud/cloud.cfg.d/90-installer-network.cfg"
    if [[ -f "$ci" ]] && ! grep -qs 'config: *disabled' /etc/cloud/cloud.cfg.d/*.cfg; then
        warn "cloud-init still owns netplan ($ci) -- it may re-render 50-cloud-init.yaml on boot,"
        warn "which is why this script writes its own ${FILE_PREFIX}*.yaml rather than editing that file"
    fi
}

# --- Revert -------------------------------------------------------------------

do_revert() {
    local files=()
    mapfile -t files < <(ls -1 "$NETPLAN_DIR/$FILE_PREFIX"*.yaml 2>/dev/null || true)
    if [[ "${#files[@]}" -eq 0 ]]; then
        log "nothing to revert -- no $FILE_PREFIX*.yaml present"
        exit 0
    fi
    log "removing:"
    printf '  %s\n' "${files[@]}"
    rm -f "${files[@]}"
    netplan apply
    log "reverted."
    exit 0
}

if [[ "$DO_REVERT" -eq 1 ]]; then
    do_revert
fi

# --- Audit --------------------------------------------------------------------

report_board
report_pci
report_links
report_renderer

# --- Select candidates --------------------------------------------------------

head1 "Candidates"

primary="$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev") {print $(i+1); exit}}' || true)"
if [[ -n "$primary" ]]; then
    log "default route lives on '$primary' -- it will not be touched"
else
    warn "no default route on this box -- no interface is being treated as primary"
fi

candidates=()
for p in /sys/class/net/*; do
    n="$(basename "$p")"
    if ! is_wired_nic "$n"; then continue; fi
    if [[ "$n" == "$primary" ]]; then
        log "  $n: holds the default route, skipping"
        continue
    fi
    if in_netplan "$n"; then
        log "  $n: already present in netplan, skipping"
        continue
    fi
    if probe_carrier "$n"; then
        log "  $n: unmanaged, cable detected ($(cat "/sys/class/net/$n/speed" 2>/dev/null || echo '?') Mb/s) -> ENABLE"
    else
        warn "  $n: unmanaged, NO cable detected -> ENABLE anyway (optional: true, so boot won't block)"
    fi
    candidates+=("$n")
done

if [[ "${#candidates[@]}" -eq 0 ]]; then
    log "no unmanaged wired NICs found -- nothing to do."
    exit 0
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    head1 "Dry run"
    log "would enable: ${candidates[*]}"
    log "re-run without --dry-run to apply"
    exit 0
fi

# --- Write config -------------------------------------------------------------

head1 "Enabling"

metric="$METRIC_BASE"
new_files=()
for n in "${candidates[@]}"; do
    f="$NETPLAN_DIR/$FILE_PREFIX$n.yaml"
    {
        printf 'network:\n  version: 2\n  ethernets:\n    %s:\n' "$n"
        if [[ "$LINK_ONLY" -eq 1 ]]; then
            printf '      dhcp4: false\n      dhcp6: false\n'
            printf '      # No address; the link still comes up so the console can see the NIC.\n'
            printf '      link-local: [ ipv6 ]\n'
        else
            printf '      dhcp4: true\n'
        fi
        printf '      # Never block boot on a port that has no DHCP server.\n'
        printf '      optional: true\n'
        if [[ "$LINK_ONLY" -eq 0 ]]; then
            printf '      dhcp4-overrides:\n'
            printf '        # Above the primary NIC, so it keeps the default route.\n'
            printf '        route-metric: %s\n' "$metric"
        fi
    } > "$f"
    chmod 600 "$f"
    new_files+=("$f")
    log "wrote $f"
    metric=$((metric + 100))
done

if ! netplan generate; then
    err "netplan generate rejected the config -- removing what we wrote and aborting"
    rm -f "${new_files[@]}"
    netplan generate || true
    exit 1
fi

# Arm the undo BEFORE applying: if the apply strands the box, or this script
# dies with its SSH session, the timer restores the previous config
# unattended.  Cancelled below once connectivity is confirmed.
revert_armed=0
if have systemd-run; then
    systemctl stop "$REVERT_UNIT.timer" 2>/dev/null || true
    if systemd-run --unit="$REVERT_UNIT" --on-active="$REVERT_AFTER" --collect \
        /bin/sh -c "rm -f ${new_files[*]} && netplan apply" >/dev/null 2>&1; then
        revert_armed=1
        log "auto-revert armed (${REVERT_AFTER}s) in case this goes wrong"
    fi
else
    warn "systemd-run unavailable -- applying without an auto-revert safety net"
fi

# Detached, so losing the SSH session cannot kill netplan mid-apply.
if have systemd-run; then
    systemd-run --unit=nexus-nic-apply --collect netplan apply >/dev/null 2>&1 || netplan apply
else
    netplan apply
fi

for _ in $(seq 1 40); do
    if [[ "$LINK_ONLY" -eq 1 ]]; then
        if ip -br addr show "${candidates[0]}" 2>/dev/null | grep -q 'fe80:'; then break; fi
    else
        if ip -4 -br addr show "${candidates[0]}" 2>/dev/null | grep -qE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+'; then break; fi
    fi
    sleep 0.5
done

# --- Verify -------------------------------------------------------------------

head1 "Result"
networkctl list --no-legend 2>/dev/null | sed 's/^/  /' || true
printf '\n'
ip -br addr | sed 's/^/  /'
printf '\n  default route(s):\n'
ip -4 route show default | sed 's/^/    /'

healthy=1
if [[ -n "$primary" ]]; then
    if ! ip -4 addr show "$primary" | grep -q 'inet '; then healthy=0; fi
    if ! ip -4 route show default | grep -q "dev $primary"; then healthy=0; fi
fi

printf '\n'
if [[ "$healthy" -eq 0 ]]; then
    err "'$primary' lost its address or the default route!"
    if [[ "$revert_armed" -eq 1 ]]; then
        err "leaving auto-revert armed -- it fires within ${REVERT_AFTER}s and restores the previous config"
    else
        err "run '$0 --revert' to undo"
    fi
    exit 1
fi

if [[ "$revert_armed" -eq 1 ]]; then
    systemctl stop "$REVERT_UNIT.timer" 2>/dev/null || true
    log "auto-revert cancelled"
fi

# Two NICs on one L2 is legal but messy: duplicate on-link routes, ARP flux
# (either NIC answers ARP for the other's IP, flapping the switch MAC table),
# and asymmetric replies.  Worth saying out loud rather than leaving to bite.
dupes="$(ip -4 route show scope link proto kernel 2>/dev/null | awk '{print $1}' | sort | uniq -d || true)"
if [[ -n "$dupes" ]]; then
    warn "these subnets are now reachable via more than one NIC:"
    for d in $dupes; do
        warn "  $d via:$(ip -4 route show scope link proto kernel | awk -v n="$d" '$1==n {printf " %s", $3}')"
    done
    warn "Expect ARP flux and asymmetric routing. Either move the second port onto its own"
    warn "VLAN/subnet (the intended camera-LAN topology), or blunt it with:"
    warn "  sysctl -w net.ipv4.conf.all.arp_ignore=1 net.ipv4.conf.all.arp_announce=2"
    printf '\n'
fi

log "done. Re-run any time; it only touches NICs that no renderer owns."
