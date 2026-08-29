#!/usr/bin/env bash
# scripts/bootstrap.sh — one-liner that downloads a release tarball
# from GitHub Releases, verifies its sha256, extracts it under
# /opt/nexus/releases/<version>/, and hands off to the in-tarball
# scripts/install.sh.
#
# Operator-facing surface. With no flags this installs whatever is
# currently on the `stable` channel:
#
#     curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/download/stable/bootstrap.sh \
#         | sudo bash -s --
#
# `--channel beta` opts a box into the pre-release channel. `--version
# vX.Y.Z` pins one exact build and skips channel resolution entirely.
#
# Channel resolution reads the VERSION asset off the floating `stable` /
# `beta` / `dev` release, which .github/workflows/release.yml re-points
# using the same channel it registers the build under with the cloud
# orchestrator. It deliberately does NOT use GitHub's own "latest"
# release: that tracks the prerelease flag alone, so a full release the
# workflow routed to `beta` would still be served here as stable.
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
CHANNEL=""
EXTRA_ARGS=()

usage() {
    cat <<EOF
Usage: bootstrap.sh [options] [-- <install.sh args>]

Options:
  --channel <name>         Release channel to install from: stable
                           (default), beta, or dev.
  --version <vX.Y.Z>       Install this exact release tag instead of
                           whatever the channel currently points at.
                           Mutually exclusive with --channel.
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
        --channel)        CHANNEL="$2"; shift 2 ;;
        --version)        VERSION="$2"; shift 2 ;;
        --force-profile)  FORCE_PROFILE="$2"; shift 2 ;;
        --keep-config)    KEEP_CONFIG=1; shift ;;
        --help|-h)        usage; exit 0 ;;
        --)               shift; EXTRA_ARGS=("$@"); break ;;
        *)                echo "bootstrap.sh: unknown arg: $1" >&2; usage; exit 2 ;;
    esac
done

if [[ -n "$VERSION" && -n "$CHANNEL" ]]; then
    echo "bootstrap.sh: --version and --channel are mutually exclusive" >&2
    exit 2
fi

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

# Resolve the channel to a concrete tag so that from here down both
# modes take the identical, version-pinned download path — and so the
# version that got installed is logged for the audit trail.
if [[ -z "$VERSION" ]]; then
    CHANNEL="${CHANNEL:-stable}"
    case "$CHANNEL" in
        dev|beta|stable) ;;
        *) echo "bootstrap.sh: unknown channel: $CHANNEL (expected dev, beta, or stable)" >&2; exit 2 ;;
    esac
    echo "[nexus] resolving the '$CHANNEL' channel"
    channel_url="https://github.com/${REPO}/releases/download/${CHANNEL}/VERSION"
    # Without this, an unreachable pointer surfaces as a bare
    # `curl: (56) ... 404` with nothing saying what to do about it.
    if ! VERSION="$(curl -fsSL --retry 3 "$channel_url")"; then
        echo "bootstrap.sh: could not read the '$CHANNEL' channel pointer at $channel_url" >&2
        echo "bootstrap.sh: the channel may not have published a release yet; pass --version vX.Y.Z to install a specific one" >&2
        exit 1
    fi
    VERSION="$(printf '%s' "$VERSION" | tr -d '[:space:]')"
    # This is about to be interpolated into a download URL, and an empty
    # or HTML body would otherwise fail much later as a 404.
    [[ "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]] \
        || { echo "bootstrap.sh: '$CHANNEL' channel returned an unusable version: '$VERSION'" >&2; exit 1; }
    echo "[nexus] '$CHANNEL' channel is $VERSION"
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
