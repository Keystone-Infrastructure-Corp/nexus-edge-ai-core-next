#!/usr/bin/env bash
# scripts/uninstall.sh — symmetric removal of a bare-metal install
# done by scripts/install.sh.
#
# Default behaviour (SAFE — preserves customer data):
#   * stops + disables the systemd unit + removes the unit file
#   * removes the /opt/nexus tree (binaries, models, scripts)
#   * removes the one-time bootstrap-password sentinel file
#     ($NEXUS_STATE_DIR/bootstrap-password.txt) if it's still
#     lying around — it's an installer artifact, not customer data
#   * LEAVES /etc/nexus/, /var/lib/nexus/ (db, clips, admin secret),
#     and the `nexus` service user intact so a re-install picks up
#     where you left off
#
# Pass --purge to ALSO wipe customer config + state + service user.
# That nukes the SQLite db, every recorded motion clip, every
# operator-tuned config value, the admin secret, and removes the
# service user. There is no undo. Use --purge when decommissioning
# the box or selling/returning hardware.
#
# Flag aliases: `--all`, `--remove-everything`, and `--nuke` are
# accepted as synonyms for --purge for operator readability.

set -euo pipefail

SCRIPT_DIR="$( cd "$(dirname "${BASH_SOURCE[0]}")" && pwd )"
# shellcheck source=lib/install-common.sh
. "$SCRIPT_DIR/lib/install-common.sh"

PURGE=0
KEEP_RELEASES=0
ASSUME_YES=0

usage() {
    cat <<EOF
Usage: $0 [options]

Default: stop & remove the engine and its binaries under $NEXUS_PREFIX,
         AND remove the one-time bootstrap-password sentinel.
         Preserves customer data ($NEXUS_STATE_DIR), operator config
         ($NEXUS_CONFIG_DIR), and the '$NEXUS_SERVICE_USER' service user.

Options:
  --purge                    Remove EVERYTHING — $NEXUS_CONFIG_DIR,
                             $NEXUS_STATE_DIR (db + clips + admin secret),
                             and the '$NEXUS_SERVICE_USER' user. No undo.
  --all, --remove-everything, --nuke
                             Synonyms for --purge.
  --keep-releases            Don't remove $NEXUS_PREFIX/releases/* (default
                             is to remove the whole $NEXUS_PREFIX tree).
  -y, --yes                  Don't prompt for confirmation on --purge.
  -h, --help                 This message.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --purge|--all|--remove-everything|--nuke)
                         PURGE=1; shift ;;
        --keep-releases) KEEP_RELEASES=1; shift ;;
        -y|--yes)        ASSUME_YES=1; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               err "unknown option: $1"; usage; exit 2 ;;
    esac
done

require_root "$@"

# Purge is destructive + irreversible — make the operator confirm
# unless they passed -y. Stdin may not be a TTY (e.g. piped from
# bootstrap.sh-style invocations), in which case we require -y.
if (( PURGE )) && (( ! ASSUME_YES )); then
    if [[ ! -t 0 ]]; then
        die "--purge requires -y/--yes when stdin is not a TTY (destroys all customer data)"
    fi
    warn "--purge will DESTROY:"
    warn "    $NEXUS_CONFIG_DIR                (operator config, install-state)"
    warn "    $NEXUS_STATE_DIR                 (SQLite db, motion clips, admin secret)"
    warn "    service user: $NEXUS_SERVICE_USER"
    warn "This cannot be undone. Type 'PURGE' to proceed."
    read -r reply
    if [[ "$reply" != "PURGE" ]]; then
        log "aborted"
        exit 1
    fi
fi

# --- Stop + remove the systemd unit -------------------------------------------

unit="$NEXUS_SYSTEMD_DIR/nexus-engine.service"
if systemctl list-unit-files nexus-engine.service >/dev/null 2>&1; then
    if systemctl is-active --quiet nexus-engine; then
        log "stopping nexus-engine.service"
        systemctl stop nexus-engine
    fi
    if systemctl is-enabled --quiet nexus-engine 2>/dev/null; then
        log "disabling nexus-engine.service"
        systemctl disable nexus-engine
    fi
fi
if [[ -e "$unit" ]]; then
    log "removing $unit"
    rm -f "$unit"
    systemctl daemon-reload
fi

# --- Remove the OTA sudoers allowlist (Phase 9 M_OTA) -------------------------

if [[ -e /etc/sudoers.d/nexus-update ]]; then
    log "removing /etc/sudoers.d/nexus-update"
    rm -f /etc/sudoers.d/nexus-update
fi

# --- Remove the remote-support sudoers grant (ADR-066) ------------------------
# Written by nexus-apply-release during ssh-ca-install, not by the installer,
# so it has to be cleaned up explicitly here too.

if [[ -e /etc/sudoers.d/nexus-diag ]]; then
    log "removing /etc/sudoers.d/nexus-diag"
    rm -f /etc/sudoers.d/nexus-diag
fi

# --- Remove the root-owned OTA wrappers (outside $NEXUS_PREFIX) ----------------

for w in /usr/local/sbin/nexus-apply-release /usr/local/sbin/nexus-apply-deps /usr/local/sbin/nexus-diag; do
    if [[ -e "$w" ]]; then
        log "removing $w"
        rm -f "$w"
    fi
done

# --- Remove binaries under $NEXUS_PREFIX --------------------------------------

if (( KEEP_RELEASES )); then
    if [[ -L "$NEXUS_PREFIX/current" ]]; then
        log "removing $NEXUS_PREFIX/current symlink (keeping releases/)"
        rm -f "$NEXUS_PREFIX/current"
    fi
else
    if [[ -d "$NEXUS_PREFIX" ]]; then
        log "removing $NEXUS_PREFIX"
        rm -rf "$NEXUS_PREFIX"
    fi
fi

# --- Always clear the one-time bootstrap-password sentinel --------------------
# The engine writes this on first boot when it generates the admin OTP
# (see crates/nexus-engine/src/auth/bootstrap.rs). It's an installer
# artifact, NOT customer data — even a default "preserve data" uninstall
# should sweep it so a stale credential doesn't survive a reinstall.
sentinel="$NEXUS_STATE_DIR/bootstrap-password.txt"
if [[ -f "$sentinel" ]]; then
    log "removing one-time bootstrap-password sentinel ($sentinel)"
    rm -f "$sentinel"
fi

# --- Purge customer state (opt-in) --------------------------------------------

if (( PURGE )); then
    if [[ -d "$NEXUS_CONFIG_DIR" ]]; then
        warn "purging $NEXUS_CONFIG_DIR (operator config, install-state)"
        rm -rf "$NEXUS_CONFIG_DIR"
    fi
    if [[ -d "$NEXUS_STATE_DIR" ]]; then
        warn "purging $NEXUS_STATE_DIR (db, clips, admin secret)"
        rm -rf "$NEXUS_STATE_DIR"
    fi
    if id -u "$NEXUS_SERVICE_USER" >/dev/null 2>&1; then
        warn "removing service user: $NEXUS_SERVICE_USER"
        userdel "$NEXUS_SERVICE_USER" || true
    fi
fi

# --- Summary ------------------------------------------------------------------

log ""
log "================================================================"
log "  nexus-engine uninstall complete."
log ""
if (( PURGE )); then
    log "  Mode: --purge (removed everything)"
    log "    [x] binaries:     $NEXUS_PREFIX"
    log "    [x] config:       $NEXUS_CONFIG_DIR"
    log "    [x] state:        $NEXUS_STATE_DIR (db, clips, admin secret)"
    log "    [x] service user: $NEXUS_SERVICE_USER"
else
    log "  Mode: default (customer data preserved)"
    log "    [x] binaries:     $NEXUS_PREFIX"
    log "    [x] sentinel:     $sentinel (if present)"
    log "    [-] preserved:    $NEXUS_CONFIG_DIR (operator config)"
    log "    [-] preserved:    $NEXUS_STATE_DIR (db, clips, admin secret)"
    log "    [-] preserved:    service user '$NEXUS_SERVICE_USER'"
    log ""
    log "  To remove EVERYTHING (config + db + clips + admin secret + user):"
    log "    sudo $0 --purge"
fi
log "================================================================"
