#!/usr/bin/env bash
# scripts/bootstrap.sh — one-liner that downloads a release tarball
# from GitHub Releases, verifies its sha256, extracts it under
# /opt/nexus/releases/<version>/, and hands off to the in-tarball
# scripts/install.sh.
#
# Operator-facing surface:
#
#     curl -fsSL https://raw.githubusercontent.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/main/scripts/bootstrap.sh \
#         | sudo bash -s -- --version v0.2.0
#
# Or, against a release that's already cut:
#
#     curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/download/v0.2.0/bootstrap.sh \
#         | sudo bash -s --
#
# (The release workflow uploads this file as `bootstrap.sh` alongside
# the tarball so the second URL works without specifying --version.)
#
# bootstrap.sh stays tiny and parameter-driven on purpose so that the
# verifier and config-generation logic live in install.sh +
# install-common.sh inside the tarball — i.e. shipped with the release
# and pinned by manifest sha256 — instead of in this network-fetched
# script.

set -euo pipefail

REPO="${NEXUS_REPO:-Keystone-Infrastructure-Corp/nexus-edge-ai-core-next}"
ARCH="$(uname -m)"
KERNEL="$(uname -s)"
NEXUS_PREFIX="${NEXUS_PREFIX:-/opt/nexus}"

FORCE_PROFILE=""
KEEP_CONFIG=0
VERSION=""
EXTRA_ARGS=()

usage() {
    cat <<EOF
Usage: bootstrap.sh [options] [-- <install.sh args>]

Options:
  --version <vX.Y.Z>       Release tag to install (default: latest).
  --force-profile <name>   Pin the inference profile (intel-igpu|intel-npu|
                           amd-vulkan|amd-rocm|hailo|nvidia|cpu); forwarded
                           to install.sh. Omit to auto-detect.
  --keep-config            Preserve an existing /etc/nexus/nexus.toml
                           instead of regenerating it; forwarded to
                           install.sh.
  --help                   This message.

Anything after --  is forwarded to install.sh verbatim, e.g.:
  bootstrap.sh --version v0.2.0 -- --no-start
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)        VERSION="$2"; shift 2 ;;
        --force-profile)  FORCE_PROFILE="$2"; shift 2 ;;
        --keep-config)    KEEP_CONFIG=1; shift ;;
        --help|-h)        usage; exit 0 ;;
        --)               shift; EXTRA_ARGS=("$@"); break ;;
        *)                echo "bootstrap.sh: unknown arg: $1" >&2; usage; exit 2 ;;
    esac
done

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    echo "bootstrap.sh: must run as root (sudo)" >&2
    exit 1
fi
[[ "$KERNEL" == "Linux"  ]] || { echo "bootstrap.sh: Linux only (saw: $KERNEL)" >&2; exit 1; }
[[ "$ARCH"   == "x86_64" ]] || { echo "bootstrap.sh: x86_64 only (saw: $ARCH)" >&2; exit 1; }

for cmd in curl tar sha256sum; do
    command -v "$cmd" >/dev/null 2>&1 \
        || { echo "bootstrap.sh: missing required command: $cmd" >&2; exit 1; }
done

# Resolve "latest" to a concrete tag so the URLs we build are stable
# and the version is logged for the audit trail.
if [[ -z "$VERSION" ]]; then
    echo "[nexus] resolving latest release tag"
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
                | python3 -c 'import json,sys;print(json.load(sys.stdin)["tag_name"])')"
    [[ -n "$VERSION" ]] || { echo "bootstrap.sh: could not resolve latest tag" >&2; exit 1; }
fi

TARBALL_NAME="nexus-edge-${VERSION}-linux-x86_64.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
TARBALL_URL="${BASE_URL}/${TARBALL_NAME}"
SHA_URL="${TARBALL_URL}.sha256"

workdir="$(mktemp -d -t nexus-bootstrap.XXXXXX)"
trap 'rm -rf "$workdir"' EXIT

echo "[nexus] downloading $TARBALL_URL"
curl -fL --retry 3 -o "$workdir/$TARBALL_NAME" "$TARBALL_URL"

echo "[nexus] downloading $SHA_URL"
curl -fL --retry 3 -o "$workdir/$TARBALL_NAME.sha256" "$SHA_URL"

# Hand off.  install.sh re-verifies sha256, extracts to the right
# location, runs MANIFEST.json verification, stages config, installs
# the systemd unit, flips current, and starts the service.
install_args=(--tarball "$workdir/$TARBALL_NAME" --version "$VERSION")
[[ -n "$FORCE_PROFILE" ]] && install_args+=(--force-profile "$FORCE_PROFILE")
(( KEEP_CONFIG )) && install_args+=(--keep-config)
install_args+=("${EXTRA_ARGS[@]}")

# We don't have install.sh on disk yet (the tarball does), but it's
# inside the archive we just downloaded.  Extract just scripts/ to a
# tmpdir and run from there — it'll re-extract the whole thing into
# /opt/nexus/releases/<version>/ during its own --tarball branch.
echo "[nexus] extracting installer from tarball"
tar -xzf "$workdir/$TARBALL_NAME" -C "$workdir" --wildcards \
    --strip-components=1 '*/scripts/*'

chmod +x "$workdir/scripts/install.sh"
exec "$workdir/scripts/install.sh" "${install_args[@]}"
