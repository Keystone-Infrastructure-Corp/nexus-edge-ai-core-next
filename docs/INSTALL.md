# Installation guide — `nexus-edge-ai-core-next`

> **Status: beta.** Cores M0–M4 + M-Install Checkpoints 1–3 + M-Admin
> Phases 0–6 are complete; the engine + admin UI are usable
> end-to-end on the reference hardware tiers. **Docker is no longer
> a supported install path** as of v0.1.10 — every install is a
> bare-metal release tarball driven by [scripts/install.sh](../scripts/install.sh).
> The bootstrap one-liner runs the host prep (apt prereqs, swap,
> firewall, render-group plumbing) automatically; only BIOS settings
> (§2), the Ubuntu install (§3), and the tier-specific GPU/NPU
> drivers (§5) still require operator hands. Follow the verification
> gate in §7 before declaring an install "done", and start with §8.0
> for the admin UI quickstart.
>
> **Audience:** an operator bringing up the engine on a fresh
> tier-target box. If you're contributing to the codebase, follow
> [DEV_NOTES.md](DEV_NOTES.md) instead — it covers the macOS dev
> toolchain and the per-change `cargo` loop.
>
> **Last reviewed:** 2026-05-24 (Docker install path removed —
> bare-metal tarball + install.sh is the only supported path on
> every tier). The kernel, driver, ORT, and CUDA versions cited
> here drift over time. Re-validate against the Appendix B / C
> transcripts on a fresh Multipass VM at every minor release before
> relying on the published commands.

---

## Table of contents

- [Installation guide — `nexus-edge-ai-core-next`](#installation-guide--nexus-edge-ai-core-next)
  - [Table of contents](#table-of-contents)
  - [1. Decide the hardware tier](#1-decide-the-hardware-tier)
  - [2. BIOS + firmware pre-install](#2-bios--firmware-pre-install)
    - [Universal (every tier)](#universal-every-tier)
    - [T10 / T24 (Intel mini PCs — Beelink, GMKtec)](#t10--t24-intel-mini-pcs--beelink-gmktec)
    - [T36 (Intel Arc A380 dGPU)](#t36-intel-arc-a380-dgpu)
    - [T36-S (Lunar Lake — GMKtec K13 / EVO-X1)](#t36-s-lunar-lake--gmktec-k13--evo-x1)
    - [T64 (NVIDIA RTX 4060)](#t64-nvidia-rtx-4060)
  - [3. Install Ubuntu 24.04 LTS Server](#3-install-ubuntu-2404-lts-server)
    - [3.1 Download + verify the ISO](#31-download--verify-the-iso)
    - [3.2 Write the ISO to USB](#32-write-the-iso-to-usb)
    - [3.3 Boot from USB](#33-boot-from-usb)
    - [3.4 Installer choices](#34-installer-choices)
    - [3.5 First boot housekeeping](#35-first-boot-housekeeping)
    - [3.6 HWE kernel — T36-S only](#36-hwe-kernel--t36-s-only)
  - [4. What `install.sh` does for you](#4-what-installsh-does-for-you)
  - [5. Tier-specific accelerator drivers](#5-tier-specific-accelerator-drivers)
    - [5.1 T10 / T24 — Intel UHD / Iris Xe iGPU](#51-t10--t24--intel-uhd--iris-xe-igpu)
    - [5.2 T36 — Intel Arc A380 dGPU](#52-t36--intel-arc-a380-dgpu)
    - [5.3 T36-S — Lunar Lake (Arc 140V iGPU + NPU 4)](#53-t36-s--lunar-lake-arc-140v-igpu--npu-4)
    - [5.4 T64 — NVIDIA RTX 4060](#54-t64--nvidia-rtx-4060)
  - [6. Install the engine](#6-install-the-engine)
    - [6.1 One-liner from GitHub Releases](#61-one-liner-from-github-releases)
    - [6.2 What the installer does, step by step](#62-what-the-installer-does-step-by-step)
    - [6.3 On-disk layout](#63-on-disk-layout)
    - [6.4 Configure cameras + first boot](#64-configure-cameras--first-boot)
      - [Confirm the tier the engine actually picked](#confirm-the-tier-the-engine-actually-picked)
    - [6.5 OS-level network manager (optional)](#65-os-level-network-manager-optional)
    - [6.6 Authentication](#66-authentication)
    - [6.7 Upgrades + rollback](#67-upgrades--rollback)
    - [6.8 Uninstall](#68-uninstall)
  - [7. Verification — smoke test](#7-verification--smoke-test)
    - [7.1 Engine answers HTTP](#71-engine-answers-http)
    - [7.2 UI loads in a browser](#72-ui-loads-in-a-browser)
    - [7.3 Cameras connect](#73-cameras-connect)
    - [7.4 Snapshot from each camera](#74-snapshot-from-each-camera)
    - [7.5 Inference backends are ready](#75-inference-backends-are-ready)
    - [7.6 Storage safety floor reports healthy](#76-storage-safety-floor-reports-healthy)
    - [7.7 Motion → clip → Timeline](#77-motion--clip--timeline)
    - [7.8 Alert end-to-end](#78-alert-end-to-end)
  - [8. Operating + day-2 essentials](#8-operating--day-2-essentials)
    - [8.0 Admin UI tour](#80-admin-ui-tour)
    - [8.1 Logs](#81-logs)
    - [8.2 Backups](#82-backups)
    - [8.3 Forward-looking](#83-forward-looking)
  - [9. Troubleshooting](#9-troubleshooting)
  - [10. Appendix A — End-to-end T24 transcript](#10-appendix-a--end-to-end-t24-transcript)
  - [11. Appendix B — End-to-end T36-S transcript](#11-appendix-b--end-to-end-t36-s-transcript)
  - [12. Appendix C — Build from source (developer-only)](#12-appendix-c--build-from-source-developer-only)
    - [12.1 Regenerate ONNX models with custom prompts](#121-regenerate-onnx-models-with-custom-prompts)
  - [13. Appendix D — Where to file bugs](#13-appendix-d--where-to-file-bugs)
  - [See also](#see-also)

---

## 1. Decide the hardware tier

The engine ships with five reference hardware tiers. Pick the row that
matches your box; everything else in this guide branches on the tier
you choose here. Full background:
[HARDWARE_TIERS.md](HARDWARE_TIERS.md).

| Tier        | Reference box                                       | Accelerator                | EP order                | Cameras (1080p / 15 fps) | Tier config                                       | Status        |
| ----------- | --------------------------------------------------- | -------------------------- | ----------------------- | ------------------------- | ------------------------------------------------- | ------------- |
| **T10**     | Beelink Mini S13 (N150, 16 GB)                      | UHD 24EU iGPU              | `openvino, cpu`         | 1–2                       | [config/tiers/t10.toml](../config/tiers/t10.toml)     | shipping      |
| **T24**     | GMKtec M3 Ultra (i7-12700H, 32 GB)                  | Iris Xe 96 EU              | `openvino, cpu`         | 4–6                       | [config/tiers/t24.toml](../config/tiers/t24.toml)     | shipping      |
| **T36**     | Lenovo P3 Tiny / HP Z2 Mini + Arc A380              | Intel Arc A380 6 GB dGPU   | `openvino, cpu`         | 8–12                      | [config/tiers/t36.toml](../config/tiers/t36.toml)     | shipping      |
| **T36-S**   | GMKtec K13 AI / EVO-X1 (Ultra 7 256V Lunar Lake)    | Arc 140V Xe2 + NPU 4       | `openvino, npu, cpu`    | 6–8                       | [config/tiers/t36s.toml](../config/tiers/t36s.toml)   | shipping      |
| **T64**     | Lenovo P3 Tower / HP Z2 G9 + RTX 4060               | NVIDIA RTX 4060 8 GB       | `tensorrt, cuda, cpu`   | 12–20                     | [config/tiers/t64.toml](../config/tiers/t64.toml)     | post-beta — CUDA/TensorRT EPs land in M5; until then T64 falls through to CPU and is **not** a meaningful deployment |

**Camera baseline (every tier):** 1080p H.264 over RTSP (or H.265 with
hardware decode), 15 fps capture, motion-gated to the detector. One
`nexus-engine` process per host. If your cameras don't fit this
profile (4K, JPEG snapshots, sub-1 fps), don't multiply the tier soak
ceiling by anything optimistic — open an issue (§13) so we can size
the box together.

**Box not in this list?** Skip `--tier` on `install.sh` and the
installer will run `nexus-probe` to pick the closest documented tier
for you. The mapping is advisory — you can override it later by
re-running with `--tier <name> --force-tier` — but it's the right
starting point.

---

## 2. BIOS + firmware pre-install

Knock these out *before* booting the Ubuntu installer. Each item is a
common foot-gun on the boxes we ship.

### Universal (every tier)

- **Update BIOS to the latest stable release** before doing anything
  else. Lunar Lake firmware in particular shipped without NPU
  exposed in early revisions.
- **VT-x / VT-d / IOMMU** — enabled. Required for device passthrough
  to accelerators on every tier.
- **Secure Boot** — disabled if you'll be installing the NVIDIA
  proprietary driver (§5.4) or the Intel NPU driver trio (§5.3).
  Otherwise leave it on.
- **SATA / NVMe** — SATA `AHCI` (no RAID), NVMe `PCIe`.
- **Boot mode** — UEFI only. CSM / Legacy boot off.
- **RAM XMP / EXPO** — enable the rated profile; the engine doesn't
  benefit from memory clock tuning beyond rated.

### T10 / T24 (Intel mini PCs — Beelink, GMKtec)

- **iGPU shared memory** — allocate **≥ 512 MB**. Beelink and GMKtec
  firmwares default to 64 MB on some SKUs, which starves the
  OpenVINO Execution Provider and quietly falls back to CPU. Look
  for "DVMT pre-allocated" / "Frame buffer size" / "iGPU memory" in
  the Advanced → Chipset menu.

### T36 (Intel Arc A380 dGPU)

- **Above 4G Decoding** — ON.
- **Resizable BAR** — ON. The Intel `i915` driver requires it for the
  A380.
- **PCIe slot speed** — Auto / Gen 4 (the A380 is x8 Gen 4).

### T36-S (Lunar Lake — GMKtec K13 / EVO-X1)

- **AI Acceleration / NPU** — ENABLED. On some K13 firmwares this is
  hidden under Advanced → AI Settings → "Intel AI Boost". If you
  don't see the option, the BIOS is too old; update first.
- **HWE kernel required** — see §3.6.

### T64 (NVIDIA RTX 4060)

- **Above 4G Decoding** — ON.
- **Resizable BAR** — ON.
- **CSM** — OFF (UEFI only; required by the NVIDIA driver and by
  Secure Boot if you choose to keep it on with signed modules).
- **IOMMU** — ON.

---

## 3. Install Ubuntu 24.04 LTS Server

We support exactly **Ubuntu 24.04 LTS Server (amd64)**. Other distros
will probably work but are unverified — open an issue if you're
shipping on something else and we'll prioritise based on demand.

### 3.1 Download + verify the ISO

```bash
# On any workstation
curl -fLO https://releases.ubuntu.com/24.04/ubuntu-24.04.2-live-server-amd64.iso
curl -fLO https://releases.ubuntu.com/24.04/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
# Expect:  ubuntu-24.04.2-live-server-amd64.iso: OK
```

Use whatever the current 24.04.x point release is — the SHA256 file
covers all of them.

### 3.2 Write the ISO to USB

**Linux:**

```bash
# Confirm the device first (DO NOT SKIP). Looking for the USB stick.
lsblk -d -o NAME,SIZE,MODEL,TRAN | grep -i usb
# Suppose it's /dev/sdb. Unmount any existing partitions:
sudo umount /dev/sdb*
sudo dd if=ubuntu-24.04.2-live-server-amd64.iso of=/dev/sdb bs=4M status=progress conv=fsync
sync
```

**macOS:**

```bash
diskutil list  # find the USB. e.g. /dev/disk6
diskutil unmountDisk /dev/disk6
sudo dd if=ubuntu-24.04.2-live-server-amd64.iso of=/dev/rdisk6 bs=1m
diskutil eject /dev/disk6
```

**Windows:** use [balenaEtcher](https://etcher.balena.io/) — point it
at the ISO and the USB. Don't bother with Rufus' DD-mode — Etcher
verifies after write, which catches the surprisingly common bad-USB
case.

### 3.3 Boot from USB

Vendor one-time-boot keys on the boxes we ship:

| Vendor   | One-time-boot key |
| -------- | ----------------- |
| Beelink  | F7                |
| GMKtec   | F7                |
| Lenovo   | F12               |
| HP       | F9                |

### 3.4 Installer choices

Walk the installer; the only screens that matter:

- **Language:** English. Keyboard: identify automatically.
- **Network:** DHCP for now. Static IP is set later through the
  admin UI (§6.5) — much friendlier than hand-editing netplan.
  If your camera VLAN requires VLAN tagging on the management
  interface, do that here — it's painful to retrofit.
- **Mirror:** the country default is fine.
- **Storage layout:** **Custom storage layout**. Build:
  - GPT partition table on the NVMe.
  - 1 GB EFI System Partition mounted at `/boot/efi`.
  - 2 GB ext4 mounted at `/boot`.
  - All remaining space as a single ext4 partition mounted at `/`.
  - **No LVM, no swap partition.** The M2.1 storage safety floor
    samples `statvfs(/var/lib/nexus/clips)` every 30 s and evicts
    when free space drops below 15 %; LVM thin pools and partition
    boundaries between `/` and `/var` make those numbers lie. Keep
    `/var/lib/nexus` on the same filesystem as `/`. `install.sh`
    adds an 8 GB swap **file** at `/swapfile` for you in §6.
- **Profile setup:**
  - Server name: your asset tag (e.g. `nx-t24-001`).
  - Pick a user name: `nexus-admin`.
  - Strong password.
- **SSH:** install OpenSSH server. Import SSH keys from GitHub /
  Launchpad if you have them.
- **Snaps:** **none**.

### 3.5 First boot housekeeping

```bash
sudo apt update
sudo apt full-upgrade -y
sudo reboot
```

### 3.6 HWE kernel — T36-S only

Lunar Lake's NPU driver requires kernel ≥ 6.10. Default 24.04
ships with 6.8; install the HWE kernel:

```bash
sudo apt install -y linux-generic-hwe-24.04
sudo reboot
uname -r            # expect 6.10.x or newer
```

T10 / T24 / T36 / T64: skip this — the GA kernel is what we test
against.

---

## 4. What `install.sh` does for you

In previous releases this section was a long manual checklist
(chrony, swap, ufw, the `nexus` user, render-group plumbing,
GStreamer runtime plugins, the static-IP netplan). All of it now
runs automatically as part of [scripts/install.sh](../scripts/install.sh)
on the same one-liner that fetches the release tarball. The
installer is idempotent — re-running it never duplicates state — so
you can rerun it after upgrading drivers, or pass `--skip-system-prep`
to bypass the prep on hosts that already have a hardened base image.

Concretely the installer:

| What                                       | Where it lives                                         | Default | Opt out                            |
| ------------------------------------------ | ------------------------------------------------------ | ------- | ---------------------------------- |
| `apt update` (if cache > 24 h old)         | Debian/Ubuntu cache `/var/cache/apt/pkgcache.bin`      | on      | `--no-deps`                        |
| Install GStreamer runtime plugins          | `gstreamer1.0-{tools,plugins-good,plugins-bad,libav,vaapi}` | on      | `--no-deps`                        |
| Install `chrony` + `ufw` + `jq` + `python3` (script + manifest helpers) | apt                                                   | on      | `--no-deps`                        |
| Enable + start `chrony`                    | systemd                                                 | on      | `--no-deps`                        |
| Create the `nexus` system user (uid auto)  | `useradd --system --home /var/lib/nexus`                 | on      | n/a (required)                      |
| Add `nexus` to `render` + `video` groups   | `usermod -aG`                                           | on      | n/a (required for accel access)     |
| Create `/etc/nexus` + `/var/lib/nexus/{clips,state}` | `install -d`                                      | on      | n/a                                |
| Allocate 8 GB `/swapfile` if no swap exists | `fallocate -l 8G`                                      | on      | `--no-swap`                        |
| Add ufw allow rules for `80/tcp` + `8089/tcp` (only when ufw is already active) | `ufw allow`                                      | on      | `--no-firewall`                    |
| **Detect accelerators via `lspci`** (Intel iGPU / Arc dGPU / NPU / NVIDIA) | `pciutils`                                          | on      | `--no-drivers`                     |
| **Install Intel iGPU + Arc dGPU stack** (kobuk-team PPA + iHD 25.x + Level Zero + media + compute — see §5.1 / §5.2) | apt + PPA                                          | on      | `--no-drivers`                     |
| **Install Intel NPU driver v1.32.1** (Lunar Lake + Meteor Lake; requires kernel ≥ 6.10 — see §5.3) | `wget` + `apt install ./intel-*.deb`              | on†    | `--no-drivers`                     |
| **Auto-install `linux-generic-hwe-24.04`** when NPU hardware is detected on a < 6.10 kernel, then exit asking for a reboot | apt                                          | on      | `--no-drivers`                     |
| Stage `/etc/nexus/nexus.toml` from tier template (first install only) | `install -m 0644`                                  | on      | n/a (preserved on upgrades)         |
| Install `/etc/systemd/system/nexus-engine.service` | from `etc-templates/systemd/`                       | on      | n/a                                |
| Atomically flip `/opt/nexus/current` → new release | `ln -sfn`                                          | on      | n/a                                |
| `systemctl enable --now nexus-engine`      | systemd                                                 | on      | `--no-start`                       |
| Install + enable `unattended-upgrades` (security patches only, auto-reboot OFF) | apt                                            | OFF     | `--enable-auto-updates` to opt in |

† NPU driver install only runs when both (a) Lunar Lake / Meteor Lake
hardware is detected and (b) the running kernel is ≥ 6.10. If
condition (a) holds but (b) doesn't, the installer stages the HWE
kernel and exits with a `REBOOT REQUIRED` banner; re-running the
same one-liner after reboot picks up where it left off.

What the installer **does NOT** do (the bits that still need you):

1. **BIOS settings** (§2) — physical access; never automated.
2. **Ubuntu install** (§3) — must complete before §4.
3. **NVIDIA driver install** (§5.4) — T64 is post-beta; the engine
   doesn't ship a CUDA or TensorRT execution provider yet (M5). The
   installer detects the card, warns, and leaves the host driver
   alone so you can manage it however you prefer.
4. **Static IP / VLAN config** — the engine's admin UI ships a
   Network page that drives netplan via a tiny privileged helper
   (§6.5). Configure cameras with DHCP first, then convert to
   static through the UI — much harder to lock yourself out of.

You **do not need to** apt-install Rust, Node, Cargo, ONNX Runtime,
or GStreamer dev headers. The release tarball is fully self-contained
for runtime; those tools are only relevant for the developer build
in [Appendix C](#12-appendix-c--build-from-source-developer-only).

---

## 5. Tier-specific accelerator drivers

> **§5 is automated by default.** The one-liner in §6.1 detects your
> accelerator hardware via `lspci` and installs the matching driver
> stack as part of the same run. The recipes below are kept for
> reference (hardened-image operators who passed `--no-drivers`,
> troubleshooting, or operators who want to understand what's
> happening). Read your tier's subsection, then continue at §6.
>
> **Exception:** NVIDIA (§5.4) is detect-only — the installer warns
> and skips because the engine has no GPU EP yet.

### 5.1 T10 / T24 — Intel UHD / Iris Xe iGPU

> **Use the Intel-graphics PPA (`ppa:kobuk-team/intel-graphics`), not
> the Ubuntu archive and not the old `repositories.intel.com/gpu`
> data-center repo.** Ubuntu 24.04 ships
> `intel-media-va-driver-non-free 24.1.0` (early-2024 vintage), which
> silently fails to init against the HWE kernel (≥ 6.11). Symptom:
> `vainfo` prints `iHD_drv_video.so init failed` with no further
> detail even though `dmesg` shows i915 bound, GuC authenticated, and
> `/dev/dri/renderD128` present. The PPA ships 25.x, which tracks
> the current i915 uAPI, plus a matched libva 1.22.x.
>
> **Why the PPA, not `repositories.intel.com/gpu`?** As of late 2025
> Intel split their package channels: client GPUs (UHD / Iris Xe /
> Arc / Lunar Lake / Battlemage / Panther Lake) live in the
> `kobuk-team` PPA, and `repositories.intel.com/gpu` is now
> data-center-only (Flex / Max). The PPA also renamed packages —
> `intel-level-zero-gpu` → `libze-intel-gpu1`, `level-zero` →
> `libze1` — so old install recipes fail with
> `intel-level-zero-gpu : Depends: libigc1 ... but it is not
> installable`.

```bash
# Add the Intel-graphics PPA (client GPUs).
sudo apt install -y software-properties-common
sudo add-apt-repository -y ppa:kobuk-team/intel-graphics
sudo apt update

# Compute stack. (intel-gsc = GPU firmware update tool, useful on
# T10 N100 boxes whose shipping firmware lags behind kernel.)
sudo apt install -y \
    libze-intel-gpu1 libze1 \
    intel-metrics-discovery intel-opencl-icd intel-gsc \
    clinfo

# Media stack.
sudo apt install -y \
    intel-media-va-driver-non-free \
    libmfx-gen1 libvpl2 libvpl-tools \
    libva-glx2 va-driver-all vainfo \
    intel-gpu-tools

sudo reboot
```

`install.sh` adds the `nexus` service user to the `render` + `video`
groups in §6 — you don't need to do that step manually anymore.
Optionally add your interactive login if you want to run `vainfo` /
`clinfo` as that user: `sudo usermod -aG render,video "$USER"` then
log out / in.

**Verify:**

```bash
# Use the DRM backend explicitly — on Ubuntu Server (no X) the
# default X11 backend prints "can't connect to X server!" and
# then misleadingly reports "iHD init failed".
vainfo --display drm --device /dev/dri/renderD128 | head -25
# Expect THREE things, in order:
#   1. libva info: VA-API version 1.22.x    ← proves you got the PPA
#      build; if it still reads 1.20.0 the libva packages came from
#      the Ubuntu archive — re-check that `apt policy libva2` shows
#      the candidate origin as `LP-PPA-kobuk-team-intel-graphics`,
#      and rerun `sudo apt install --reinstall libva2 libva-drm2`.
#   2. Driver version: Intel iHD driver ... - 25.x.x
#   3. VAProfileH264Main / VAProfileH264High / VAProfileHEVCMain /
#      VAProfileAV1Profile0 lines.
clinfo | grep -i 'platform name'
# Expect: "Intel(R) OpenCL Graphics" (or similar).
ls -l /dev/dri/render*
# Expect: crw-rw---- 1 root render ...
```

If `vainfo` exits with `Permission denied` opening `/dev/dri/renderD128`,
the user running it isn't in the `render` group. Run it as the service
user: `sudo -u nexus vainfo --display drm --device /dev/dri/renderD128`.

If `vainfo` prints `has no function __vaDriverInit_1_0`, libva is
from the Ubuntu archive (2.20.x) while iHD is the PPA build (needs
1.22.x). Confirm with `apt policy libva2` — the install candidate
should be from `LP-PPA-kobuk-team-intel-graphics`. Force-fix:
`sudo apt install --reinstall -y libva2 libva-drm2 libva-x11-2 libva-wayland2`.

If `apt install` fails with `intel-level-zero-gpu : Depends: libigc1
(>= ...) but it is not installable`, you're following an older recipe
that referenced the now-deprecated `repositories.intel.com/gpu/ubuntu
noble unified` data-center channel. Tear it down with `sudo rm -f
/etc/apt/sources.list.d/intel-gpu-noble.list
/etc/apt/preferences.d/intel-graphics && sudo apt update`, then
follow the PPA recipe above. The package names also changed:
`intel-level-zero-gpu` → `libze-intel-gpu1`, `level-zero` → `libze1`.

If `vainfo` still prints `iHD_drv_video.so init failed` after the
install, confirm in this order: (a) `lspci -nnk | grep -A3 -i vga`
shows `Kernel driver in use: i915`; (b) `dmesg | grep -iE 'guc|huc'`
shows `GuC firmware ... version` and `HuC: authenticated`; (c) `dpkg
-l intel-media-va-driver-non-free` shows a 25.x version. If (a) or
(b) is missing the iGPU isn't actually coming up — check the Beelink
BIOS for `Primary Display = IGFX` and `iGPU Multi-Monitor = Enabled`
so i915 binds even when running headless. The `i965_drv_video.so`
failure beneath the iHD one is expected and harmless — `i965` only
covers Gen8 and older; iHD is the right driver for Alder Lake-N.

### 5.2 T36 — Intel Arc A380 dGPU

> Same PPA as §5.1 — see that section for the background on why the
> old `repositories.intel.com/gpu` recipe no longer works.

```bash
# Add the Intel-graphics PPA (client GPUs, includes Arc dGPUs).
sudo apt install -y software-properties-common
sudo add-apt-repository -y ppa:kobuk-team/intel-graphics
sudo apt update

# Compute stack (the libze* pair is the new name for the old
# intel-level-zero-gpu + level-zero packages).
sudo apt install -y \
    libze-intel-gpu1 libze1 \
    intel-metrics-discovery intel-opencl-icd intel-gsc \
    clinfo

# Media stack.
sudo apt install -y \
    intel-media-va-driver-non-free \
    libmfx-gen1 libvpl2 libvpl-tools \
    libva-glx2 va-driver-all vainfo \
    intel-gpu-tools

sudo reboot
```

**Verify:**

```bash
vainfo --display drm --device /dev/dri/renderD128 | head -25
# Expect: "libva info: VA-API version 1.22.x" AND "Driver version:
# Intel iHD driver ... - 25.x.x" AND the full VAProfileH264* /
# VAProfileHEVC* / VAProfileAV1Profile0 list.
clinfo | grep -A2 'Platform Name'
# Expect "Intel(R) OpenCL Graphics" with the Arc A380 listed under
# Devices.
sudo intel_gpu_top -L          # lists the engines on the card
```

### 5.3 T36-S — Lunar Lake (Arc 140V iGPU + NPU 4)

```bash
# Step 1 — confirm HWE kernel is active (§3.6).
uname -r        # expect 6.10.x or later
```

```bash
# Step 2 — iGPU stack, same PPA recipe as §5.1 / §5.2.
sudo apt install -y software-properties-common
sudo add-apt-repository -y ppa:kobuk-team/intel-graphics
sudo apt update

sudo apt install -y \
    libze-intel-gpu1 libze1 \
    intel-metrics-discovery intel-opencl-icd intel-gsc \
    clinfo \
    intel-media-va-driver-non-free \
    libmfx-gen1 libvpl2 libvpl-tools \
    libva-glx2 va-driver-all vainfo
```

```bash
# Step 3 — NPU driver trio. We install from the upstream
# intel/linux-npu-driver release (Ubuntu has no apt package yet).
#
# Pin to a known-good tagged release rather than "latest" so a
# silently-broken upstream build can't take a fleet down. v1.32.1
# is verified for Lunar Lake on Ubuntu 24.04 / kernel >= 6.10.
#
# Since v1.32.x the release ships ONE tarball containing all three
# .debs (intel-fw-npu, intel-driver-compiler-npu,
# intel-level-zero-npu) instead of three separate downloads, and
# the install step is `apt install ./intel-*.deb` instead of dpkg
# so libtbb12 + libze1 deps resolve automatically. The libze1
# package comes from the kobuk-team PPA you added in Step 2.
NPU_VER=1.32.1
NPU_TARBALL=linux-npu-driver-v${NPU_VER}.20260422-24767473183-ubuntu2404.tar.gz
mkdir -p /tmp/npu && cd /tmp/npu
wget "https://github.com/intel/linux-npu-driver/releases/download/v${NPU_VER}/${NPU_TARBALL}"
tar -xf "${NPU_TARBALL}"
ls -1 *.deb     # expect 3 packages: intel-driver-compiler-npu,
                # intel-fw-npu, intel-level-zero-npu
sudo apt update
sudo apt install -y ./intel-*.deb
sudo reboot
```

**Verify:**

```bash
vainfo --display drm --device /dev/dri/renderD128 | head -25
# Expect: VA-API 1.22.x, "Intel iHD driver ... - 25.x".
ls -l /dev/accel/accel0
# Expect: crw-rw---- 1 root render ... /dev/accel/accel0
# If accel0 is missing, kernel < 6.10 OR NPU disabled in BIOS (§2).

sudo dmesg | grep -i 'intel_vpu\|intel_vpu0'
# Expect lines like "intel_vpu 0000:00:0b.0: Firmware: ..."
# Note: Ubuntu 24.04 sets `kernel.dmesg_restrict=1` by default, so a
# bare `dmesg` as a non-root user returns "read kernel buffer failed:
# Operation not permitted". Use `sudo dmesg` (or add yourself to the
# `adm` group). The `/dev/accel/accel0` check above is the
# authoritative "driver is up" signal — the engine only needs that.
```

The tier config [config/tiers/t36s.toml](../config/tiers/t36s.toml)
lists `npu` first in `ep_priority`, then `cpu`. If the NPU stack
is missing the engine **falls through to CPU automatically** — that's
the whole point of the EP priority list — so you can bring the box
up on the iGPU first, install the NPU later, and `systemctl restart
nexus-engine` to pick it up.

> **Why isn't `openvino` listed alongside `npu` for the iGPU?** ORT's
> `RegisterExecutionProviderLibrary` is one-shot per session: the
> OpenVINO EP library can only be registered once. Listing both
> `openvino` and `npu` trips the duplicate-registration guard, the
> yolo loader silently catches the error, and the camera falls back
> to the mock detector — `/api/backends` still reports
> `state: "ready"` so the failure is invisible from the UI. The
> single `npu` entry already dispatches through the OpenVINO EP with
> `device_type=NPU`, which covers both accelerators. See the inline
> comments in [config/tiers/t36s.toml](../config/tiers/t36s.toml#L43)
> for the full reasoning.

### 5.4 T64 — NVIDIA RTX 4060

> **Status:** T64 is post-beta. The CUDA + TensorRT EPs land in M5.
> Until then the engine compiles fine and exposes `cuda` /
> `tensorrt` in `ep_priority`, but the actual session opens against
> the CPU EP. T64 is **not** a meaningful production deployment yet.
> Set the box up to be ready for M5; verify with `nvidia-smi` only.

```bash
# Step 1 — blacklist nouveau, rebuild initramfs.
echo -e "blacklist nouveau\noptions nouveau modeset=0" \
  | sudo tee /etc/modprobe.d/blacklist-nouveau.conf
sudo update-initramfs -u
sudo reboot
```

```bash
# Step 2 — install the proprietary driver. ubuntu-drivers picks the
# best matching version for the card.
sudo apt install -y ubuntu-drivers-common
sudo ubuntu-drivers autoinstall
sudo reboot
nvidia-smi
# Expect a table listing the RTX 4060.
```

```bash
# Step 3 — CUDA 12.4 + cuDNN 9 from NVIDIA's keyring.
wget https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
sudo dpkg -i cuda-keyring_1.1-1_all.deb
sudo apt update
sudo apt install -y cuda-toolkit-12-4 cudnn9-cuda-12

echo 'export PATH=/usr/local/cuda-12.4/bin:$PATH' \
  | sudo tee /etc/profile.d/cuda.sh
echo '/usr/local/cuda-12.4/lib64' \
  | sudo tee /etc/ld.so.conf.d/cuda.conf
sudo ldconfig
```

```bash
# Step 4 — TensorRT 10 (when M5 lands; can install ahead of time).
sudo apt install -y tensorrt
```

`install.sh` (§6) adds the `nexus` service user to `render` and
`video` automatically. Run `nvidia-smi` to confirm the host driver
is healthy before proceeding.

---

## 6. Install the engine

### 6.1 One-liner from GitHub Releases

Same command does first install **and** every subsequent upgrade —
it's idempotent. On first install the script runs `nexus-probe`
from the staged release and auto-selects the matching tier from §1,
so the minimum invocation is zero flags:

```bash
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/latest/download/bootstrap.sh \
  | sudo bash
```

Pass `--tier <name>` only to override the probe — e.g. forcing
`t10` on a box that probes as `t24` for low-power soak testing, or
on hardware the probe doesn't recognise. `--tier` is consulted only
on first install, so re-running with the wrong tier on an existing
box won't clobber a tuned `/etc/nexus/nexus.toml` (use
`--force-tier` to opt in).

Useful flag combinations:

| Flags                                      | When                                                                                  |
| ------------------------------------------ | ------------------------------------------------------------------------------------- |
| (no flags)                                 | Fresh install — let `nexus-probe` auto-select the tier. **This is the default path.** |
| `--tier t24`                               | Fresh install on hardware the probe doesn't recognise, or forcing a non-default tier. |
| `--enable-auto-updates`                    | Plus any of the above — opts into apt's `unattended-upgrades` for security patches (auto-reboot stays OFF). |
| `--skip-system-prep`                       | Upgrade on a hardened base image where the operator manages apt prereqs themselves.   |
| `--no-firewall`                            | Cluster-edge box already behind a perimeter firewall; don't add ufw rules.            |
| `--no-swap`                                | Box already has a swap partition / dedicated swap LVM volume.                         |
| `--no-deps`                                | Apt prereqs already baked into the golden image.                                      |
| `--rollback`                               | Flip `/opt/nexus/current` back to the previous good release.                          |
| `--version vX.Y.Z`                         | Pin a specific version (instead of `latest`).                                         |
| `--force-tier --tier t36`                  | Re-templatize `/etc/nexus/nexus.toml` from a different tier (backs up to `.bak`).     |
| `--no-start`                               | Install everything but don't enable / start the systemd unit (useful for image bake). |

Expected runtime on a T24-class box: ~90 s end-to-end on a clean
network (most of which is the 250 MB tarball download). All flags
above are also available as environment variables (`NEXUS_PREP_DEPS=0`,
`NEXUS_PREP_SWAP=0`, etc.) for use in image-bake pipelines.

### 6.2 What the installer does, step by step

The bootstrap script:

1. Resolves the release tag (or pins to `--version vX.Y.Z`).
2. Downloads `nexus-edge-...-linux-x86_64.tar.gz` + its `.sha256` sidecar.
3. Hands off to the in-tarball `scripts/install.sh`, which:
   - Re-verifies the sha256 sidecar.
   - Extracts to `/opt/nexus/releases/<version>/`.
   - Verifies every file against `MANIFEST.json` (catches mid-flight corruption).
   - Verifies the Ed25519 signature on `MANIFEST.json` against the
     committed `scripts/lib/release-pubkey.pem` (or warns and
     continues if the release predates signing; set
     `NEXUS_REQUIRE_SIGNATURE=1` to enforce strictly).
   - **Runs `system_prep`** — apt-installs GStreamer runtime + chrony
     + ufw + jq + python3, enables chrony, allocates 8 GB `/swapfile`
     if needed, adds ufw rules if ufw is already enabled. See §4
     for the full opt-out matrix.
   - Creates the `nexus` system user + `/etc/nexus` + `/var/lib/nexus`.
   - Adds the `nexus` user to `render` + `video` groups (if those
     groups exist — they appear once you complete §5).
   - On first install with no `--tier`, runs `bin/nexus-probe` to
     auto-detect the tier from CPU + accelerator features.
   - Stages `/etc/nexus/nexus.toml` from the tier template **only if
     the file doesn't already exist** (operator edits survive
     upgrades forever).
   - Installs `/etc/systemd/system/nexus-engine.service`.
   - Atomically flips `/opt/nexus/current → releases/<version>` and
     records the previous version into `/etc/nexus/install-state.json`.
   - Enables + starts the unit and waits up to 60 s for
     `/api/health` to return 200.

### 6.3 On-disk layout

```text
/opt/nexus/
├── releases/
│   ├── v0.1.9/          # whatever you installed first
│   └── v0.2.0/          # the next release you upgrade to
├── current -> releases/v0.2.0   # atomic-swap symlink (rollback = flip)
└── (nothing else)

/etc/nexus/
├── nexus.toml               # operator-editable; survives every upgrade
└── install-state.json       # current_version, previous_good_version, systemd_unit_sha256

/var/lib/nexus/
├── nexus.db                 # SQLite (cameras, rules, events, motion)
├── clips/                   # recorded MP4s
└── state/
    ├── bootstrap-password.txt   # first-boot OTP for admin user (auth.mode = "local")
    ├── admin-secret             # HS256 session-signing key  (auth.mode = "local")
    └── dev-token                # bearer token              (auth.mode = "dev_token")
```

Each release directory contains:

```text
bin/
├── nexus-engine             # FEATURES=gstreamer,ort,ep-cpu,ep-openvino,ep-cuda,ep-tensorrt
├── nexus-probe
└── nexus-doctor
lib/onnxruntime/             # libonnxruntime.so + OpenVINO EP + intel CPU/GPU/NPU plugins
lib/nexus/
└── nexus-netd               # privileged netplan helper (§6.5)
share/
├── ui/                      # SPA bundle
└── models/                  # default 4-file model pack (~100 MB)
etc-templates/
├── nexus.example.toml
├── tiers/{t10,t24,t36,t36s,t64}.toml
└── systemd/nexus-engine.service
scripts/
├── install.sh               # idempotent; same script first install + upgrade
├── uninstall.sh
└── lib/install-common.sh
VERSION
MANIFEST.json                # every file + sha256 (manifest tamper check)
MANIFEST.json.sig            # Ed25519 signature (verified when public key is present)
```

The key idea: **immutable, versioned releases under
`/opt/nexus/releases/` and a single mutable symlink at
`/opt/nexus/current`**. Upgrades and rollbacks are symlink flips
followed by `systemctl restart nexus-engine`.

### 6.4 Configure cameras + first boot

The DB is the source of truth; the TOML file only seeds an empty DB
(`store.seed_from_config = true`). Two paths:

**A. Seed via TOML (good for first boot, easy to grep later):**

Edit `/etc/nexus/nexus.toml` and add one block per camera. Schema
mirrors [config/nexus.example.toml](../config/nexus.example.toml):

```toml
[[cameras]]
id = 1
name = "Front Door"
url = "rtsp://USER:PASS@10.0.20.11:554/Streaming/Channels/101"
enabled = true
prompts = ["person", "package", "vehicle"]
max_fps = 10

[[cameras.zones]]
id = "porch"
name = "Porch"
polygon = [[0.1, 0.5], [0.9, 0.5], [0.9, 1.0], [0.1, 1.0]]
kind = "inclusion"
```

> **RTSP credential gotcha:** if your password contains `!` and you
> ever paste the URL into a zsh / bash shell (e.g. testing with
> `curl -u`), wrap the whole string in single quotes after running
> `set +H`. The `!` triggers history expansion otherwise.

> **One-session-per-path IP cameras.** Some firmwares — confirmed on
> the InSight CS-series, also reported on a handful of low-end
> ONVIF-only re-badges — accept exactly **one** active RTSP session
> per stream URL. The engine sidesteps this by sharing the single
> RTSP session between detector and recorder through an internal
> `tee` (the `recorder = "gstreamer"` path). **Do not run two
> cameras in the engine with the same `url`** — they'll fight for
> the slot and one will silently stay at 0 fps after the first
> reconnect.

After editing, restart the engine:

```bash
sudo systemctl restart nexus-engine
```

**B. Add via the admin UI (recommended once the engine is up):**

Browse to `http://<box-ip>/`, log in with the bootstrap OTP from
`/var/lib/nexus/state/bootstrap-password.txt` (see §6.6 for first-boot
flow), then **Configuration → Cameras → + New camera**. The form
includes a `🔍 Discover` button that runs ONVIF WS-Discovery plus a
CIDR sweep, with per-row **Verify** that issues an RTSP `OPTIONS` /
`DESCRIBE` and shows the SDP streams.

**C. Add via the API (no engine restart, scriptable):**

```bash
curl -fsS -X PUT -H 'Content-Type: application/json' \
    http://localhost:8089/api/cameras/1 \
    -d '{
      "id": 1,
      "name": "Front Door",
      "url": "rtsp://USER:PASS@10.0.20.11:554/Streaming/Channels/101",
      "enabled": true,
      "prompts": ["person", "package", "vehicle"],
      "max_fps": 10
    }'
```

#### Confirm the tier the engine actually picked

```bash
sudo -u nexus /opt/nexus/current/bin/nexus-probe \
    --out /var/lib/nexus/device-manifest.json
jq '.recommended_tier, .accelerators' /var/lib/nexus/device-manifest.json
```

The `recommended_tier` field should match what's in
`/etc/nexus/nexus.toml`. If they disagree, the probe is right by
default — switch to its recommendation unless you have a deliberate
reason not to (e.g. you have both an Arc A380 and an RTX 4060 in the
same Lenovo P3 Tower and want `t64` over `t36`).

### 6.5 OS-level network manager (optional)

The admin UI's **Network** page (Admin → Network) needs a privileged
helper to write `/etc/netplan/*.yaml` and run `netplan apply`. The
engine itself runs unprivileged as `nexus`; only the tiny
`nexus-netd` binary is allowed `sudo`, and only against itself.

The release tarball stages the helper at
`/opt/nexus/current/lib/nexus/nexus-netd`. To enable it, install the
sudoers entry shipped under `etc-templates/sudoers.d/nexus-netd`:

```bash
sudo install -o root -g root -m 0440 \
    /opt/nexus/current/etc-templates/sudoers.d/nexus-netd \
    /etc/sudoers.d/nexus-netd
sudo visudo -cf /etc/sudoers.d/nexus-netd   # validate

# netplan + sudo (Ubuntu Server has both by default).
sudo apt install -y netplan.io sudo

# Smoke-test: should print 'platform: linux' and exit 1 (usage).
sudo -u nexus sudo -n /opt/nexus/current/lib/nexus/nexus-netd
```

Skip this step if you don't intend to manage NICs / VLANs through
the admin UI — the page degrades to read-only and `POST
/v1/admin/network/plan/apply` returns `501 platform_unsupported`.

The helper writes to `/etc/netplan/90-nexus.yaml` (and a sibling
`.90-nexus.yaml.bak` for rollback) — it deliberately does **not**
touch other files under `/etc/netplan/*.yaml`, so any
operator-managed `99-operator.yaml` co-exists with engine output via
netplan's standard file-merging rules.

Per-apply safety: after `POST /v1/admin/network/plan/apply` the
helper spawns a 120-second timer; if `POST .../confirm` doesn't
arrive (e.g. you locked yourself out of the new bind), the helper
restores the `.bak` and re-applies. This mirrors `netplan try`'s UX
without depending on its TTY-based confirm prompt.

### 6.6 Authentication

M-Install Checkpoint 3c made the engine secure-by-default. Full
identity model:
[ARCHITECTURE.md §11](../../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md#11-identity--authentication).

Short version: the customer-facing identity path is
**cloud-console-mediated** (one Entra app, one secret, held in the
cloud-console's Azure Key Vault, minting short-lived `actor_token`
JWTs that the edge verifies over the mTLS tunnel). Edge boxes
therefore do not ship with per-deployment IdP configuration. The
on-edge `auth.mode` exists for two reasons: the pre-enrollment
local-admin path, and the offline escape hatch.

| `auth.mode`   | When to use it                                                                  | Behaviour |
| ------------- | ------------------------------------------------------------------------------- | --------- |
| `"local"`     | **Customer-facing default.** Also the offline escape hatch when the cloud-console tunnel is unreachable. | First boot creates a single `admin` user with a one-time password printed at WARN and persisted to `/var/lib/nexus/state/bootstrap-password.txt`. Set a real password on first login; create operator/viewer users from the UI. The HS256 session-signing secret auto-provisions to `<state_dir>/admin-secret` (mode 0600). |
| `"dev_token"` | Single-box dev / closed-lab rig on a trusted LAN.                               | On first boot the engine generates a 32-byte URL-safe random token, persists it to `<state_dir>/dev-token` (mode 0600), and prints it once at WARN. Clients send `Authorization: Bearer <token>`. **Compile-removed under `--features prod-auth`** so a release binary cannot ship a shared-secret bearer. |
| `"none"`      | Closed-lab / regression rigs that bind only to loopback.                        | Engine **refuses to boot** unless `[server].api_bind` is `127.0.0.1:*`, `[::1]:*`, or `localhost:*`. |
| `"oidc"`      | **Expert mode.** Rare on-prem deployment pointed at a site-local IdP.            | OIDC auth-code + PKCE; bearer tokens validated against the issuer's JWKS at every request. Not the customer-facing default — ships unconfigured. |
| `"hybrid"`    | **Expert mode.** OIDC + a single local `breakglass` admin for IdP outages.       | Same as `oidc` plus the local-users login path. |

Tier configs in [config/tiers/](../config/tiers/) ship with
`mode = "local"`. Override with an explicit `[auth]` block in
`/etc/nexus/nexus.toml`:

```toml
# Recommended for any multi-operator install — auth.mode = "local".
[auth]
mode = "local"
# The HS256 session-signing secret auto-provisions to
# `<state_dir>/admin-secret` on first boot. Pin to override:
# admin_secret_path = "/run/secrets/nexus-admin-secret"

# One-box dev / closed lab — auth.mode = "dev_token".
[auth]
mode = "dev_token"

# Expert mode — on-prem IdP.
[auth]
mode = "oidc"
[auth.oidc]
issuer    = "https://auth.example.com/application/o/nexus"
audience  = "nexus-engine"
client_id = "nexus-engine"
# client_secret_file = "/run/secrets/oidc"   # confidential clients only
```

### 6.7 Upgrades + rollback

**Upgrade to latest** — same one-liner, just rerun. The existing
`/etc/nexus/nexus.toml` is preserved:

```bash
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/latest/download/bootstrap.sh \
  | sudo bash -s --
```

**Pin a specific version:**

```bash
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/download/v0.2.0/bootstrap.sh \
  | sudo bash -s -- --version v0.2.0
```

The previous release dir stays at `/opt/nexus/releases/<previous>/`
for rollback. Run `sudo /opt/nexus/current/scripts/uninstall.sh
--keep-releases` then re-install to garbage-collect.

**Rollback** — flip the symlink back to the previous good version:

```bash
sudo /opt/nexus/current/scripts/install.sh --rollback
```

This re-points `/opt/nexus/current` at the previous release dir
(no download needed) and restarts the engine. If you've upgraded
twice since the version you want, run `--rollback` twice.

### 6.8 Uninstall

```bash
# Default: stop + remove the unit and /opt/nexus, but preserve
# /etc/nexus + /var/lib/nexus so a re-install picks up where you
# left off.
sudo /opt/nexus/current/scripts/uninstall.sh

# Destructive: also wipes db, clips, configs, and the `nexus` user.
sudo /opt/nexus/current/scripts/uninstall.sh --purge
```

---

## 7. Verification — smoke test

Run these in order. Don't skip a step that fails. Each step has an
expected output; if you don't see it, drop into §9.

### 7.1 Engine answers HTTP

```bash
curl -fsS http://localhost:8089/api/health
# Expect: {"status":"ok"}  (or similar — non-empty 200)
curl -fsS http://localhost/api/health         # UI alias on :80
# Same body.
```

If the second command fails:
- `[server].ui_bind` is unset — the tier templates set it; confirm
  in `/etc/nexus/nexus.toml`.
- Port 80 is taken by another process — `sudo ss -ltnp | grep ':80 '`.
- The unit is missing `AmbientCapabilities=CAP_NET_BIND_SERVICE`
  (engine log will say `failed to bind server.ui_bind; … Permission
  denied`). The shipped unit has it. The same `AmbientCapabilities=`
  line also grants `CAP_PERFMON` so the engine can read Intel iGPU
  utilization via the `i915` PMU; if you removed it the system
  metrics endpoint will simply return `utilization_pct: null` and
  log a one-shot info line about `EACCES`.

### 7.2 UI loads in a browser

```
http://<box-ip>/         # preferred — served by ui_bind on :80
http://<box-ip>:8089/    # fallback — same UI, served by api_bind
```

You should see the Nexus dashboard. On first boot under
`auth.mode = "local"`, the engine bounces you to `/login`; sign in
with `admin` + the OTP from `/var/lib/nexus/state/bootstrap-password.txt`.

### 7.3 Cameras connect

In the UI, each enabled camera should transition to **`connected`**
within ~60 s.

```bash
curl -fsS http://localhost:8089/api/cameras | jq '.[] | {id, name, enabled}'
```

If a camera is stuck on `connecting` for > 2 min, jump to §9 (RTSP
entry).

### 7.4 Snapshot from each camera

Proves the GStreamer source is producing frames *and* the inference
pipeline is consuming them.

```bash
curl -fsS http://localhost:8089/api/cameras/1/frames/latest \
  -o /tmp/cam1.jpg
file /tmp/cam1.jpg
# Expect: JPEG image data, baseline, precision 8, 1920x1080 ...
```

### 7.5 Inference backends are ready

```bash
curl -fsS http://localhost:8089/api/backends | jq
# Expect: every slot in `state: "ready"` with the EP your tier expects:
#   T10/T24/T36/T36-S → "openvino" (or "npu" on T36-S if NPU stack present)
#   T64               → "cpu" today; "tensorrt" / "cuda" once M5 lands
#   anything          → "cpu" as last-resort fallback
```

If you see `state: "starting"` for more than 30 s the worker is
still loading the ONNX model (cold start can take 10–20 s). If you
see `state: "failed"`, check the engine logs — the most common
cause is a missing model file at `/var/lib/nexus/models/` or a
sha256 mismatch with `models-manifest.json`.

### 7.6 Storage safety floor reports healthy

```bash
curl -fsS http://localhost:8089/api/v1/storage/local \
  | jq '{recorder_kind, panic, free_pct, clips_dir, watermark_state}'
# Expect:
# { "recorder_kind": "gstreamer", "panic": false, "free_pct": <high>,
#   "clips_dir": "/var/lib/nexus/clips", "watermark_state": "ok" }
```

If `panic: true`, you're already below 5 % free on the clips
filesystem. `df -h /var/lib/nexus/clips` to see what's eating the
space.

### 7.7 Motion → clip → Timeline

Walk in front of one of the cameras for 5 s. Within 10 s:

- The camera card should turn yellow ("motion").
- The Timeline tab on that camera should show a new motion block.
- Click the block → it plays the recorded clip in-browser.

```bash
curl -fsS "http://localhost:8089/api/v1/cameras/1/motion?from=$(date -u -d '5 minutes ago' +%FT%TZ)&to=$(date -u +%FT%TZ)" \
  | jq 'length'
# Expect: > 0
```

> **If the motion block appears but clicking it shows "no playable
> data":** the recorder booted in `stub` mode. Confirm with
> `curl -fsS http://localhost:8089/api/v1/storage/local | jq -r .recorder_kind`.
> If it returns `"stub"`, add `[runtime.clips] recorder = "gstreamer"`
> to `/etc/nexus/nexus.toml` and restart.

### 7.8 Alert end-to-end

The example config seeds a `person_in_zone` CEL rule. Stand in
camera 1's frame for ≥ 2 s; an alert should appear in the Alerts
tab.

```bash
sudo -u nexus sqlite3 /var/lib/nexus/nexus.db \
    "SELECT count(*) FROM events;"
# Expect: > 0
```

If §7.7 worked but §7.8 didn't, the rule isn't matching — confirm
your camera has `prompts` containing `"person"`.

---

## 8. Operating + day-2 essentials

### 8.0 Admin UI tour

The operator console is a single-page web app served by the engine
binary itself at **`http://<engine-host>/`** (port 80) or
**`http://<engine-host>:8089/`** (port 8089 — same SPA, same auth) —
no separate admin process, no `/ui` path. Sidebar groups three
operational modes:

- **Operations** (read-only, live)
  - *Viewer* — live camera feed + tracked-object overlay.
  - *Timeline* — hourly motion clips with inline playback.
  - *Events* — alert history (filterable by rule / camera / severity).

- **Configuration** (CRUD; saves are inline, no full reload)
  - *Cameras* — `+ New camera` opens a form covering every
    `CameraConfig` field including parking-lot mode and an
    Advanced JSON editor for `model_override`. The `🔍 Discover`
    button opens a modal that runs ONVIF WS-Discovery and a CIDR
    sweep in parallel; results stream in live. Per-row **Verify**
    issues an RTSP `OPTIONS`/`DESCRIBE` (Digest auth supported) and
    shows the SDP streams.
  - *Rules* — `+ New rule` opens a form for id / name / severity /
    `camera_filter` / track-age / consecutive frames / cooldown /
    enabled. The `when` field has two modes: a row-based **visual
    CEL builder** and a **raw text** escape hatch.
  - *Zones* (per camera, inside the camera form) — polygon editor
    overlaid on the latest snapshot. Coords are stored normalised
    `[0..1]` so they survive resolution changes.

- **System**
  - *Storage* — local clips usage, cold-backend registry (LAN /
    Google Drive / OneDrive), watermark + throttle settings.
    OAuth for Drive/OneDrive is end-to-end in-engine; the refresh
    token never reaches the browser.
  - *Backends* — detector-pool slot/state/generation telemetry.
  - *Network* — netplan plan + lockout-safe apply (§6.5).
  - *Health* — engine vitals, version, uptime.

### 8.1 Logs

```bash
sudo journalctl -u nexus-engine -f
```

Bump verbosity by editing `RUST_LOG` in the systemd drop-in:

```bash
sudo systemctl edit nexus-engine
# Add:
#   [Service]
#   Environment=RUST_LOG=info,nexus=debug
sudo systemctl restart nexus-engine
```

> **Caveat:** the engine reads `log_level` from `[telemetry]` in
> `nexus.toml`, NOT from `RUST_LOG`. The `Environment=RUST_LOG=…`
> line above only works when the toml entry is left at its default.
> To force per-module verbosity that survives, edit `nexus.toml`:
>
> ```toml
> [telemetry]
> log_level = "warn,nexus_engine=info,nexus_inference=debug"
> ```

### 8.2 Backups

```bash
sudo systemctl stop nexus-engine
sudo tar -C /var/lib -czf /backup/nexus-$(date +%F).tgz nexus
sudo systemctl start nexus-engine
```

Restore is the inverse. There's no incremental clip backup story
yet — that's M2.2 (cold-mirror replication).

### 8.3 Forward-looking

- **M2.2 — Cold storage replication.** Operators will be able to
  point clip storage at a LAN folder, Google Drive, or OneDrive.
  [M2_STORAGE.md §M2.2](../../nexus-cloud-console/docs/edge-core/M2_STORAGE.md#m22--cold-storage-replication).
- **M3.1 — Visual prompts (YOLOE).** Operators upload a JPEG, attach
  it to a camera, write a CEL rule against the operator-supplied
  label.
  [M3_OPEN_VOCAB_VISUAL.md](../../nexus-cloud-console/docs/edge-core/M3_OPEN_VOCAB_VISUAL.md).

---

## 9. Troubleshooting

| Symptom | Likely cause | Fix |
| ------- | ------------ | --- |
| `curl /api/health` returns connection refused | Engine isn't up. | `systemctl status nexus-engine`; check logs (§8.1). |
| Engine refuses to start with `auth.mode = "none" is only allowed when server.api_bind is on loopback` | Since M-Install Checkpoint 2 the engine refuses to bind unauthenticated APIs onto a LAN. | Either change `[server].api_bind` to `127.0.0.1:8089` (LAN-only deployments), or set `[auth].mode = "local"`. The one-time admin password is at `/var/lib/nexus/state/bootstrap-password.txt` (mode 0600). |
| Engine logs `auth: bootstrap admin created` / `one_time_password=<value>` at boot | First boot under `mode = "local"`. | Copy the OTP from `/var/lib/nexus/state/bootstrap-password.txt`, log in once at `http://<host>/login`, finish the wizard. |
| UI loads but `/` returns 404 | `ui_root` mismatch — engine pointing at a path that doesn't exist. | `ls /opt/nexus/current/share/ui/index.html` should exist; `[server].ui_root` in `/etc/nexus/nexus.toml` should be `/opt/nexus/current/share/ui`. |
| Camera stuck on `connecting` for > 2 min | RTSP transport mismatch (camera serves UDP, engine asks TCP), bad credentials, blocked port. | Test with `gst-launch-1.0 -v rtspsrc location=<url> ! fakesink` from the host. If the password contains `!`, run `set +H` first and single-quote the URL. |
| `vainfo` succeeds for your login but engine logs say "no VAAPI device" | `nexus` user not in `render` group. | `sudo usermod -aG render nexus && sudo systemctl restart nexus-engine`. Re-running `install.sh` does this automatically. |
| `/dev/accel/accel0` missing on T36-S | Kernel < 6.10, NPU disabled in BIOS, or driver trio not installed. | `uname -r` ≥ 6.10 (§3.6); §2 BIOS; §5.3 driver install. |
| `nvidia-smi` works on the host but engine reports CPU EP only | T64 is post-beta; M5 hasn't landed. | Expected. The engine falls through to CPU until M5. |
| `/api/backends` shows all slots `state: "ready"` but every camera returns generic / mock-looking detection labels | `ep_priority` lists both `openvino` and `npu`. ORT's `RegisterExecutionProviderLibrary` is one-shot per session — the duplicate trips a "Provider OpenVINOExecutionProvider has already been registered" error and the yolo loader silently falls back to the mock detector. | Set `ep_priority = ["npu", "cpu"]` (T36-S) or `ep_priority = ["openvino", "cpu"]` (T10/T24/T36) — never both. See [config/tiers/t36s.toml](../config/tiers/t36s.toml#L43). |
| Camera reaches `streaming` once then stays at 0 fps after every subsequent reconnect, but VLC against the same URL works fine | IP-camera firmware (e.g. InSight CS-series) enforces one RTSP session per stream path. | Power-cycle the camera, confirm no other VMS / external probe is hitting the same `url`, and verify the engine is on `recorder = "gstreamer"` (§6.4). |
| Recorder writes 0-byte mp4 files | `recorder = "stub"` (the runtime default when `[runtime.clips]` is missing). | Add `[runtime.clips] recorder = "gstreamer"` to `/etc/nexus/nexus.toml` and restart. |
| Recorder writes ~864-byte mp4 files (`recorder_kind = "gstreamer"`) | mp4mux silently dropping buffers without PTS — should not happen on `main`. | Capture logs with `GST_DEBUG=qtmux:5,h264parse:4` and file an issue (§13). |
| Recorder refuses to open new clips ("storage panic") | Free space on `/var/lib/nexus/clips` ≤ `panic_watermark_pct` (default 5 %). | `df -h /var/lib/nexus/clips`; either grow the disk or lower `motion_clips_retention_days`. |
| Engine fails to load model at boot with "sha256 mismatch" | The .onnx file on disk doesn't match `models-manifest.json`. | Re-run the model-gen script (Appendix C) — the script re-stamps the manifest. |
| `intel-level-zero-gpu : Depends: libigc1 ... but it is not installable` | Old recipe referencing the deprecated `repositories.intel.com/gpu/ubuntu noble unified` data-center channel. | Tear down `/etc/apt/sources.list.d/intel-gpu-noble.list` and `/etc/apt/preferences.d/intel-graphics`, then follow §5.1's PPA recipe. |
| `vainfo` prints `has no function __vaDriverInit_1_0` | libva is from the Ubuntu archive (2.20.x) while iHD is the PPA build (needs 1.22.x). | `sudo apt install --reinstall -y libva2 libva-drm2 libva-x11-2 libva-wayland2` from the PPA. |
| `vainfo` exits with `iHD_drv_video.so init failed` and no further detail | Stock Ubuntu's `intel-media-va-driver-non-free 24.1.0` against kernel ≥ 6.11. | Add the kobuk-team PPA per §5.1 — its iHD 25.x tracks the current i915 uAPI. |
| Install fails because `ufw enable` would lock you out | The installer never enables ufw for you. If ufw is already active without an OpenSSH allow rule, the script adds engine port rules but the OpenSSH rule is on you. | Run `sudo ufw allow OpenSSH` BEFORE `sudo ufw enable`. |
| `apt-get install` fails with "Could not get lock /var/lib/dpkg/lock-frontend" | Another apt frontend (unattended-upgrades, packagekit) is holding the lock. | Wait 60 s and re-run `install.sh` — it's idempotent. |

If your symptom isn't here, file an issue (§13).

---

## 10. Appendix A — End-to-end T24 transcript

Copy-pasteable shell session that takes a fresh GMKtec M3 Ultra
from post-Ubuntu-install to "first alert visible in the UI". Use
this as the canonical regression test when making changes to this
document.

```bash
# ---- §3.5 First boot housekeeping --------------------------------
sudo timedatectl set-timezone America/New_York
sudo apt update && sudo apt full-upgrade -y
sudo reboot
# (reconnect)

# ---- §6.1 Install engine + drivers (one-liner) -----------------
# This single command:
#   - apt-installs GStreamer runtime + chrony + ufw + jq + python3
#   - enables chrony
#   - allocates 8 GB /swapfile
#   - adds ufw allow rules for 80 + 8089 (if ufw is active)
#   - lspci-probes the box and installs the Intel iGPU driver stack
#     (kobuk-team PPA + iHD 25.x + Level Zero + media + compute)
#   - creates the `nexus` system user + dirs
#   - adds `nexus` to render + video groups
#   - downloads + verifies the release tarball
#   - stages tier config, installs systemd unit, starts engine
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/latest/download/bootstrap.sh \
  | sudo bash -s -- --tier t24

# (Optional) verify the iGPU stack came up cleanly.
vainfo --display drm --device /dev/dri/renderD128 | head -5
# Expect "VA-API version 1.22.x" + "Intel iHD ... 25.x".

# ---- §6.4 First-boot login ---------------------------------------
# Grab the bootstrap OTP printed by the installer (also persisted):
sudo cat /var/lib/nexus/state/bootstrap-password.txt
# Browse to http://<box-ip>/ , log in as `admin` with the OTP,
# follow the setup wizard to set a real password.

# ---- §6.4 Add a camera (API) -------------------------------------
curl -fsS -X PUT -H 'Content-Type: application/json' \
    http://localhost:8089/api/cameras/1 \
    -d '{
      "id": 1,
      "name": "Front Door",
      "url": "rtsp://demo:demo@10.0.20.11:554/Streaming/Channels/101",
      "enabled": true,
      "prompts": ["person", "package", "vehicle"],
      "max_fps": 10
    }'

# ---- §7 Smoke test -----------------------------------------------
curl -fsS http://localhost:8089/api/health
curl -fsS http://localhost:8089/api/cameras | jq '.[] | {id, name}'
sleep 60
curl -fsS http://localhost:8089/api/cameras/1/frames/latest -o /tmp/cam1.jpg
file /tmp/cam1.jpg
curl -fsS http://localhost:8089/api/backends | jq
curl -fsS http://localhost:8089/api/v1/storage/local | jq
echo "Walk in front of camera 1 now..."
sleep 15
sudo -u nexus sqlite3 /var/lib/nexus/nexus.db "SELECT count(*) FROM events;"
```

---

## 11. Appendix B — End-to-end T36-S transcript

Copy-pasteable shell session that takes a fresh **GMKtec K13 AI**
(or EVO-X1, Intel Core Ultra 7 256V "Lunar Lake", Arc 140V iGPU
with NPU 4) from post-Ubuntu-install to "first alert visible in
the UI".

```bash
# ---- §3.5 First boot housekeeping --------------------------------
sudo timedatectl set-timezone America/New_York
sudo apt update && sudo apt full-upgrade -y
sudo reboot
# (reconnect)

# ---- §6.1 First pass — installer detects NPU + stages HWE kernel
# This single command lspci-probes the box, sees Lunar Lake silicon
# without the required >=6.10 kernel, apt-installs
# linux-generic-hwe-24.04, and exits with a REBOOT REQUIRED banner.
# Re-running the same one-liner after reboot does the rest.
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/latest/download/bootstrap.sh \
  | sudo bash -s -- --tier t36s

sudo reboot
# (reconnect)
uname -r                       # expect 6.10.x or newer

# ---- §6.1 Second pass — drivers + engine in one shot -------------
# After the reboot, re-run the same one-liner. It:
#   - apt-installs GStreamer runtime + chrony + ufw + jq + python3
#   - enables chrony, allocates /swapfile, adds ufw rules
#   - installs the Intel iGPU stack (kobuk-team PPA)
#   - downloads + installs the NPU driver v1.32.1 (4 .deb files)
#   - creates the `nexus` user, dirs, group memberships
#   - stages tier config, installs systemd unit, starts engine
curl -fsSL https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/releases/latest/download/bootstrap.sh \
  | sudo bash -s -- --tier t36s

# (Optional) verify both accelerators came up.
vainfo --display drm --device /dev/dri/renderD128 | head -25
# Expect: VA-API 1.22.x, "Intel iHD driver ... - 25.x".
ls -l /dev/accel/accel0
# Expect: crw-rw---- 1 root render ... /dev/accel/accel0
sudo dmesg | grep -i intel_vpu | head -5
# Expect: "intel_vpu 0000:00:0b.0: Firmware: ..."

# ---- §6.4 First-boot login ---------------------------------------
sudo cat /var/lib/nexus/state/bootstrap-password.txt
# Browse to http://<box-ip>/ , log in as `admin` with the OTP.

# ---- §6.4 Confirm probe sees BOTH accelerators -------------------
sudo -u nexus /opt/nexus/current/bin/nexus-probe \
    --out /var/lib/nexus/device-manifest.json
jq '.recommended_tier, .accelerators' /var/lib/nexus/device-manifest.json
# Expect: "t36s", and accelerators include both Arc 140V (iGPU)
#         and an NPU entry with provider "openvino" device "NPU".

# ---- §6.4 Add a camera -------------------------------------------
curl -fsS -X PUT -H 'Content-Type: application/json' \
    http://localhost:8089/api/cameras/1 \
    -d '{
      "id": 1,
      "name": "Front Door",
      "url": "rtsp://demo:demo@10.0.20.11:554/Streaming/Channels/101",
      "enabled": true,
      "prompts": ["person", "package", "vehicle"],
      "max_fps": 10
    }'

# ---- §7 Smoke test -----------------------------------------------
curl -fsS http://localhost/api/health        # UI alias on port 80
curl -fsS http://localhost:8089/api/health   # control-plane on 8089
curl -fsS http://localhost:8089/api/backends | jq
# Expect at least one backend with provider "openvino" and device
# "NPU" listed before the GPU/CPU fallbacks in ep_priority.
sleep 60
curl -fsS http://localhost:8089/api/cameras/1/frames/latest -o /tmp/cam1.jpg
file /tmp/cam1.jpg
echo "Walk in front of camera 1 now..."
sleep 15
sudo -u nexus sqlite3 /var/lib/nexus/nexus.db "SELECT count(*) FROM events;"
```

**T36-S-specific gotchas to watch for:**

1. `intel-level-zero-gpu : Depends: libigc1 ... but it is not
   installable` — you copy-pasted a pre-2025-Q3 recipe that
   referenced `repositories.intel.com/gpu`. Tear down
   `/etc/apt/sources.list.d/intel-gpu-noble.list` and
   `/etc/apt/preferences.d/intel-graphics`, then redo the PPA step.
2. `/dev/accel/accel0` missing after reboot — confirm `uname -r`
   is ≥ 6.10 **and** that the BIOS has "AI Acceleration / NPU" set
   to ENABLED. On some K13 firmwares this setting is under
   "Advanced > CPU Configuration" rather than the top-level device
   list.
3. `nexus-engine` boots but the OpenVINO NPU device isn't picked
   up — the engine **falls through to the iGPU automatically** per
   the EP priority list in [config/tiers/t36s.toml](../config/tiers/t36s.toml).
   Restart the engine after installing the NPU stack to pick it up
   (`sudo systemctl restart nexus-engine`).

---

## 12. Appendix C — Build from source (developer-only)

For contributors who need to compile the engine themselves
(patched branches, untagged commits, custom feature sets). For
shipping releases, use §6 — it lands the same on-disk layout
without a Rust + Node toolchain on the box, and is what the future
OTA updater operates against.

The toolchain pin lives in [rust-toolchain.toml](../rust-toolchain.toml)
(`channel = "stable"`).

```bash
# Toolchain.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable
echo 'source $HOME/.cargo/env' >> $HOME/.profile
. $HOME/.cargo/env

# Node 22 for the UI bundler.
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs

# GStreamer dev headers (plus runtime — install.sh installs runtime
# alone on production boxes; building from source needs the -dev
# packages too).
sudo apt install -y \
    pkg-config build-essential cmake git ca-certificates curl libssl-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-libav gstreamer1.0-tools gstreamer1.0-vaapi

# ONNX Runtime 1.22.0 (must match the workspace `ort` crate pin —
# the crate panics at session creation if it isn't 1.22.x).
ORT_VER=1.22.0
curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VER}/onnxruntime-linux-x64-${ORT_VER}.tgz" \
  | sudo tar -xz -C /opt
sudo mv "/opt/onnxruntime-linux-x64-${ORT_VER}" /opt/onnxruntime
echo '/opt/onnxruntime/lib' | sudo tee /etc/ld.so.conf.d/onnxruntime.conf
sudo ldconfig

# Build. Per-tier feature flags:
#   T10 / T24 / T36     →  gstreamer,ort,ep-cpu,ep-openvino
#   T36-S               →  gstreamer,ort,ep-cpu,ep-openvino   # NPU via OpenVINO
#   T64 (post-beta)     →  gstreamer,ort,ep-cpu,ep-cuda,ep-tensorrt
# `gstreamer` is mandatory — without it, RTSP support is `#[cfg]`'d out.
FEATURES="gstreamer,ort,ep-cpu,ep-openvino"
git clone https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next.git
cd nexus-edge-ai-core-next
cargo build --release -p nexus-engine --features "$FEATURES" --bin nexus-engine
cargo build --release -p nexus-probe  --bin nexus-probe
(cd ui && npm ci && npm run build)
```

To install your source build into the same on-disk layout as the
release tarball, run `tools/build-tarball.sh` (planned follow-up;
until it lands, you can either point a hand-written systemd unit
at `target/release/nexus-engine`, or stage your build into
`/opt/nexus/releases/dev-$(date +%s)/` and flip
`/opt/nexus/current` manually).

### 12.1 Regenerate ONNX models with custom prompts

The default models pack ships in the release tarball; this is only
for operators who want a custom prompt vocabulary or are
contributing new detectors.

```bash
sudo apt install -y python3.11 python3.11-venv python3.11-dev
python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install -r tools/models/requirements.txt

# Closed-vocab base detector.
python tools/models/gen_yolo26n.py \
    --output /var/lib/nexus/models/yolo26n_dynamic.onnx

# Open-vocab YOLO-World.
mkdir -p models/.cache
curl -sL --fail \
    -o models/.cache/yolov8s-worldv2.pt \
    https://github.com/ultralytics/assets/releases/download/v8.4.0/yolov8s-worldv2.pt
python tools/models/gen_yolo_world.py \
    --base-model models/.cache/yolov8s-worldv2.pt \
    --output /var/lib/nexus/models/yolo_world_v2_s.onnx

# Open-vocab YOLOE (M3.1 successor).
# If the ultralytics PyPI release lacks the `YOLOE` symbol, upgrade:
#   pip install -U 'git+https://github.com/ultralytics/ultralytics@main'
python tools/models/gen_yoloe.py \
    --prompts tools/models/yoloe_default_prompts.txt \
    --output /var/lib/nexus/models/yoloe26_s.onnx

# Refresh the manifest (the gen scripts each upsert their own row).
sudo install -o nexus -g nexus -m 0644 \
    models/models-manifest.json /var/lib/nexus/models/models-manifest.json
sudo systemctl restart nexus-engine
```

To change what YOLO-World can detect, edit
[tools/models/yolo_world_default_prompts.txt](../tools/models/yolo_world_default_prompts.txt)
and re-run. Each prompt becomes a class index baked into the ONNX
graph; the manifest captures the prompt list so the engine's
loader can map detections back to labels.

---

## 13. Appendix D — Where to file bugs

Open issues at
<https://github.com/Keystone-Infrastructure-Corp/nexus-edge-ai-core-next/issues>. Include:

1. **Tier + box** — e.g. "T36-S, GMKtec K13 AI, BIOS V1.07".
2. **OS + kernel** — `cat /etc/os-release; uname -r`.
3. **Engine version** — `sudo /opt/nexus/current/bin/nexus-engine --version`
   (or `cat /opt/nexus/current/VERSION`).
4. **Probe output** — attach `/var/lib/nexus/device-manifest.json`.
5. **Install state** — `sudo cat /etc/nexus/install-state.json`.
6. **Last 200 log lines** — `journalctl -u nexus-engine -n 200`.
7. **Watermark state** — `curl -fsS http://localhost:8089/api/v1/storage/local | jq`.
8. **Reproduction** — the smallest sequence that reliably reproduces
   the symptom.

Redact any RTSP credentials, OIDC issuer URLs, and customer-identifying
camera names before posting.

---

## See also

- [README.md](../README.md) — project overview, tier table, status banner.
- [docs/HARDWARE_TIERS.md](HARDWARE_TIERS.md) — full tier rationale + Lunar Lake driver caveat.
- [docs/ARCHITECTURE.md](../../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md) — trait + pool pattern, frame-lifecycle spans, side-channels.
- [docs/ROADMAP.md](../../nexus-cloud-console/docs/product/ROADMAP.md) — milestones M0 → M9.
- [docs/M2_STORAGE.md](../../nexus-cloud-console/docs/edge-core/M2_STORAGE.md) — M2.1 storage safety floor (shipped) + M2.2 cold-mirror (in progress).
- [docs/M3_OPEN_VOCAB_VISUAL.md](../../nexus-cloud-console/docs/edge-core/M3_OPEN_VOCAB_VISUAL.md) — visual-prompt detector design.
- [docs/M7_DELIVERY.md](../../nexus-cloud-console/docs/edge-core/M7_DELIVERY.md) — alert sinks + delivery policy.
- [docs/DEV_NOTES.md](DEV_NOTES.md) — developer setup, per-change cargo loop, model-gen smoke tests.
