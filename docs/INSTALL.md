# Installation guide — `nexus-edge-ai-core-next`

> **Status: beta.** Cores M0–M4 + M-Install Checkpoints 1–2 + M-Admin
> Phases 0–6 are complete; the engine + admin UI are usable
> end-to-end on the reference hardware tiers. **M-Install Checkpoint
> 3a (image scope)** is also live: every `v*` git tag now publishes
> `ghcr.io/andboyer/nexus-engine:vX.Y.Z` via
> [.github/workflows/release.yml](../.github/workflows/release.yml),
> the default model pack is baked into the image, and per-tier
> Compose overlays under `deploy/` ship the right device + tier-config
> wiring out of the box (see §6). Production deployment is still
> blocked on M7 (alert delivery) + M8 (first customer trial) per
> [`ROADMAP.md`](ROADMAP.md). Follow the verification gate in §9
> before declaring an install "done", and start with §10.0 for the
> admin UI quickstart.
>
> **Audience:** an operator bringing up the engine on a fresh
> tier-target box. If you're contributing to the codebase, follow
> [DEV_NOTES.md](DEV_NOTES.md) instead — it covers the macOS dev
> toolchain and the per-change `cargo` loop.
>
> **Last reviewed:** 2026-05-17 (post M-Admin Phase 6 — full CRUD
> admin console shipped: cameras with ONVIF + CIDR discovery,
> rules with visual CEL builder + inline validation, polygon
> zones, storage backends). The kernel, driver, ORT, and CUDA
> versions cited here drift over time. Re-validate against the
> Appendix B transcript on a fresh Multipass VM at every minor
> release before relying on the published commands.

---

## Table of contents

1. [Decide the hardware tier](#1-decide-the-hardware-tier)
2. [BIOS + firmware pre-install](#2-bios--firmware-pre-install)
3. [Install Ubuntu 24.04 LTS Server](#3-install-ubuntu-2404-lts-server)
4. [Base system hardening + housekeeping](#4-base-system-hardening--housekeeping)
5. [Tier-specific accelerator drivers](#5-tier-specific-accelerator-drivers)
   - [5.1 T10 / T24 — Intel UHD / Iris Xe iGPU](#51-t10--t24--intel-uhd--iris-xe-igpu)
   - [5.2 T36 — Intel Arc A380 dGPU](#52-t36--intel-arc-a380-dgpu)
   - [5.3 T36-S — Lunar Lake (Arc 140V iGPU + NPU 4)](#53-t36-s--lunar-lake-arc-140v-igpu--npu-4)
   - [5.4 T64 — NVIDIA RTX 4060](#54-t64--nvidia-rtx-4060)
   - [5.5 GStreamer hardware decode (bare-metal only)](#55-gstreamer-hardware-decode-bare-metal-only)
6. [Install path A — Docker Compose (recommended)](#6-install-path-a--docker-compose-recommended)
7. [Install path B — Bare-metal systemd (advanced)](#7-install-path-b--bare-metal-systemd-advanced)
   - [7.0 Bare-metal from release tarball (recommended)](#70-bare-metal-from-release-tarball-recommended)
8. [Configure cameras + first boot](#8-configure-cameras--first-boot)
9. [Verification — smoke test](#9-verification--smoke-test)
10. [Operating + day-2 essentials](#10-operating--day-2-essentials)
11. [Troubleshooting](#11-troubleshooting)
12. [Appendix A — Reproducible model generation](#12-appendix-a--reproducible-model-generation)
13. [Appendix B — End-to-end T24 transcript](#13-appendix-b--end-to-end-t24-transcript)
14. [Appendix C — End-to-end T36-S transcript](#14-appendix-c--end-to-end-t36-s-transcript)
15. [Appendix D — Where to file bugs](#15-appendix-d--where-to-file-bugs)

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
| **T36-S**   | GMKtec K13 AI / EVO-X1 (Ultra 7 256V Lunar Lake)    | Arc 140V Xe2 + NPU 4       | `openvino, npu, cpu`    | 6–8                       | [config/tiers/t36s.toml](../config/tiers/t36s.toml)   | shipping (NPU requires bare-metal install — §5.3) |
| **T64**     | Lenovo P3 Tower / HP Z2 G9 + RTX 4060               | NVIDIA RTX 4060 8 GB       | `tensorrt, cuda, cpu`   | 12–20                     | [config/tiers/t64.toml](../config/tiers/t64.toml)     | post-beta — CUDA/TensorRT EPs land in M5; until then T64 falls through to CPU and is **not** a meaningful deployment |

**Camera baseline (every tier):** 1080p H.264 over RTSP (or H.265 with
hardware decode), 15 fps capture, motion-gated to the detector. One
`nexus-engine` process per host. If your cameras don't fit this
profile (4K, JPEG snapshots, sub-1 fps), don't multiply the tier soak
ceiling by anything optimistic — open an issue (§14) so we can size
the box together.

**Box not in this list?** Run `nexus-probe` after install (§8.2) and
read its `recommended_tier` field. The probe enumerates the host and
maps it onto the closest documented tier. The mapping is advisory —
you can override it — but it's the right starting point.

---

## 2. BIOS + firmware pre-install

Knock these out *before* booting the Ubuntu installer. Each item is a
common foot-gun on the boxes we ship.

### Universal (every tier)

- **Update BIOS to the latest stable release** before doing anything
  else. Lunar Lake firmware in particular shipped without NPU
  exposed in early revisions.
- **VT-x / VT-d / IOMMU** — enabled. Required for container device
  passthrough on every tier.
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
- **HWE kernel required** — see §3 step 6.

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
- **Network:** DHCP for now. Static IP comes later via netplan
  (§4.2). If your camera VLAN requires VLAN tagging on the
  management interface, do that here — it's painful to retrofit.
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
    `/var/lib/nexus` on the same filesystem as `/`. We add an 8 GB
    swap **file** at `/swapfile` post-install (step §4.6 below).
- **Profile setup:**
  - Server name: your asset tag (e.g. `nx-t24-001`).
  - Pick a user name: `nexus-admin`.
  - Strong password.
- **SSH:** install OpenSSH server. Import SSH keys from GitHub /
  Launchpad if you have them.
- **Snaps:** **none**. Don't install `docker` from the snap menu —
  we install Docker from the official `docker-ce` apt repo in §6.

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

## 4. Base system hardening + housekeeping

### 4.1 Time + timezone

```bash
sudo timedatectl set-timezone America/New_York   # or your TZ
timedatectl status
systemctl is-active chrony   # expect: active
```

If `chrony` isn't installed (some minimal images skip it):

```bash
sudo apt install -y chrony
sudo systemctl enable --now chrony
```

The clip recorder embeds wall-clock timestamps in clip filenames and
in `motion_clips.started_at`. Drift > 1 s makes the Timeline tab
ugly; drift > 5 s breaks alert correlation.

### 4.2 Static IP via netplan

```bash
sudo install -m 0600 -o root -g root /dev/null /etc/netplan/01-nexus.yaml
sudo tee /etc/netplan/01-nexus.yaml >/dev/null <<'EOF'
network:
  version: 2
  ethernets:
    enp1s0:                       # confirm with `ip -br link`
      dhcp4: false
      addresses:
        - 10.0.10.20/24
      routes:
        - to: default
          via: 10.0.10.1
      nameservers:
        addresses: [10.0.10.1, 1.1.1.1]
EOF
sudo netplan apply
ip -br addr show enp1s0
```

Pick whatever interface name `ip -br link` shows. On Beelink/GMKtec
boxes it's usually `enp1s0` or `enp2s0`; on Lenovo P3 it's `eno1`.

### 4.3 Engine user + state directories

The Docker image (and the bare-metal systemd unit in §7) both run as
**uid 1000 / gid 1000 named `nexus`**. Create that user on the host
so file ownership lines up between the container and the host
mountpoint:

```bash
sudo useradd --uid 1000 --create-home --shell /usr/sbin/nologin nexus
sudo mkdir -p /etc/nexus /var/lib/nexus/clips /var/lib/nexus/models
sudo chown -R nexus:nexus /var/lib/nexus
sudo chmod 755 /etc/nexus
```

`/var/lib/nexus` is the engine's state root:

| Path                          | Holds                                       |
| ----------------------------- | ------------------------------------------- |
| `/var/lib/nexus/nexus.db`     | SQLite DB (cameras, rules, events, motion)  |
| `/var/lib/nexus/clips/`       | Recorded mp4 clips (M2.1 watermark applies) |
| `/var/lib/nexus/models/`      | ONNX model files + `models-manifest.json`   |
| `/var/lib/nexus/state/`       | Per-camera static-object registries; auto-provisioned `dev-token` (mode 0600). Created on demand by the engine — `runtime.state_dir` in [config/nexus.example.toml](../config/nexus.example.toml) overrides the path. |
| `/var/lib/nexus/device-manifest.json` | Last `nexus-probe` output           |

### 4.4 Firewall (`ufw`)

The engine listens on **two TCP ports** out of the box:

| Port       | Bound by                  | Purpose                                                                 |
| ---------- | ------------------------- | ----------------------------------------------------------------------- |
| `80/tcp`   | `[server].ui_bind`        | Admin console alias — operators reach the UI at `http://<host>/`.       |
| `8089/tcp` | `[server].api_bind`       | Engine API (also serves the UI on this port — same router, both bindings). |

Both listeners share the same axum router, the same auth, and the
same TLS posture; the second bind is purely for operator UX so you
don't have to type `:8089`. Open both:

```bash
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp   comment 'nexus-engine UI alias'
sudo ufw allow 8089/tcp comment 'nexus-engine API + UI'
sudo ufw --force enable
sudo ufw status
```

The engine doesn't need inbound from anywhere else; cameras
initiate the RTSP push *to* the engine via outbound TCP only when
the URL is `rtsp-over-TCP`.

If port 80 is already taken on this host (some other web server),
comment out the `ui_bind` line in your tier config (see §6.5 /
§7.6) and drop the `ufw allow 80/tcp` step — operators will then
reach the UI at `http://<host>:8089/` only.

> **Bare-metal note (Install path B, §7):** binding any port
> `<1024` from a non-root user requires `CAP_NET_BIND_SERVICE`.
> The systemd unit in §7.7 sets `AmbientCapabilities=CAP_NET_BIND_SERVICE`
> for exactly this reason. Docker (§6) inherits the capability
> automatically.

### 4.5 Optional — unattended-upgrades

```bash
sudo apt install -y unattended-upgrades
sudo dpkg-reconfigure -plow unattended-upgrades   # answer "Yes"
# Edit /etc/apt/apt.conf.d/50unattended-upgrades and confirm:
#   Unattended-Upgrade::Automatic-Reboot "false";
# Auto-reboots will lose in-flight clips; schedule reboots manually.
```

### 4.6 8 GB swap file (no LVM swap partition, see §3.4)

```bash
sudo fallocate -l 8G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
free -h
```

---

## 5. Tier-specific accelerator drivers

Read **only the subsection for your tier**, then come back here and
continue at §6 (or §7 for T36-S).

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

sudo usermod -aG render,video nexus
sudo usermod -aG render,video nexus-admin
# Log out / in (or reboot) for group membership to take effect.
sudo reboot
```

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
the user running it isn't in the `render` group. Either run the verify
commands as the `nexus` service user (`sudo -u nexus vainfo --display drm
--device /dev/dri/renderD128`) or add your own login to the group with
`sudo usermod -aG render,video $USER` and log out / in.

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

sudo usermod -aG render,video nexus
sudo usermod -aG render,video nexus-admin
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

> **You need the bare-metal install path (§7) for T36-S.** Docker
> Compose with `--device /dev/accel/accel0` is fragile on
> the kernels available today; the systemd unit is reliable. The
> rest of this section sets up both the iGPU and the NPU.

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
```

```bash
# Step 4 — group + reboot.
sudo usermod -aG render,video nexus
sudo usermod -aG render,video nexus-admin
sudo reboot
```

**Verify:**

```bash
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

# Optional: install OpenVINO benchmark_app for end-to-end smoke.
# (See nexus-edge-deploy/scripts/openvino_smoke.sh if available.)
```

The tier config [config/tiers/t36s.toml](../config/tiers/t36s.toml)
lists `npu` first in `ep_priority`, then `cpu`. If the NPU stack
is missing the engine **falls through to CPU automatically** — that's
the whole point of the EP priority list — so you can bring the box
up on the iGPU first, install the NPU later, and restart the engine
to pick it up.

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

```bash
# Step 5 — NVIDIA Container Toolkit (Docker passthrough).
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
  | sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
  | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
  | sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list
sudo apt update
sudo apt install -y nvidia-container-toolkit
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker     # only if Docker is already installed
```

**Verify:**

```bash
nvidia-smi
# Host driver is fine.

# Container passthrough (after Docker install in §6):
docker run --rm --gpus all nvidia/cuda:12.4.0-base-ubuntu24.04 nvidia-smi
# Expect the same RTX 4060 table from inside the container.
```

### 5.5 GStreamer hardware decode (bare-metal only)

The container ships GStreamer + plugins. If you're going bare-metal
(§7), install the runtime now:

```bash
sudo apt install -y \
    gstreamer1.0-tools \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-libav \
    gstreamer1.0-vaapi
```

`gstreamer1.0-vaapi` lets the engine decode H.264 on the iGPU on
T10 / T24 / T36 / T36-S; on T64 the NVIDIA stack uses NVDEC via the
plugin already in `gstreamer1.0-plugins-bad`.

---

## 6. Install path A — Docker Compose (recommended)

Container is the default install for **T10 / T24 / T36 / T64**. T36-S
NPU users should follow §7 instead.

### 6.1 Install Docker Engine

Use the official `docker-ce` apt repo. **Do not install the snap** —
it sandboxes filesystem access in ways that break the
`/var/lib/nexus` bind mount.

```bash
# Remove any pre-existing Docker bits.
for pkg in docker.io docker-doc docker-compose podman-docker containerd runc; do
    sudo apt remove -y $pkg 2>/dev/null
done

# Install prerequisites + add Docker's GPG key.
sudo apt update
sudo apt install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
  -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc

# Add the repo and install.
echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/ubuntu \
$(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list

sudo apt update
sudo apt install -y \
    docker-ce docker-ce-cli containerd.io \
    docker-buildx-plugin docker-compose-plugin

# Add BOTH the service user (so the engine container can be managed
# by systemd-run / future automation) AND your interactive login
# user (so you can run `docker compose pull` / `up` directly during
# install). Replace $USER with your actual login name if you ran
# this block under sudo from a different account.
sudo usermod -aG docker nexus-admin
sudo usermod -aG docker "$USER"

# Activate the new group in THIS shell without logging out. Without
# this, `docker compose pull` in §6.7 fails with
# "permission denied while trying to connect to the docker API at
# unix:///var/run/docker.sock" \u2014 the group membership only loads
# at next login otherwise.
newgrp docker << 'EOF'
docker --version
docker compose version
docker info > /dev/null && echo "docker OK"
EOF
```

### 6.2 Clone the repo

> The repo is private, so HTTPS prompts for credentials and **GitHub
> no longer accepts passwords here** (you'll see `Invalid username
> or token. Password authentication is not supported for Git
> operations.`). Pick one of the two auth methods below before you
> run `git clone`. SSH-key is the only option that survives reboots
> for unattended OTA pulls, so prefer it on edge boxes.

**Option A — SSH deploy key (recommended for edge boxes):**

```bash
# Generate a key dedicated to this box (no passphrase so OTA pulls
# don't block waiting for unlock).
sudo -u nexus-admin ssh-keygen -t ed25519 -N "" \
    -C "nexus-edge-$(hostname)" \
    -f /home/nexus-admin/.ssh/id_ed25519_nexus
sudo -u nexus-admin cat /home/nexus-admin/.ssh/id_ed25519_nexus.pub
```

Copy the printed public key, then in the GitHub repo UI go to
**Settings → Deploy keys → Add deploy key**, paste it, check
**Allow write access** only if this box will push (it shouldn't —
leave it off). Then clone via SSH:

```bash
sudo -u nexus-admin tee -a /home/nexus-admin/.ssh/config > /dev/null << 'EOF'
Host github.com
  IdentityFile ~/.ssh/id_ed25519_nexus
  IdentitiesOnly yes
EOF

sudo mkdir -p /opt/nexus
sudo chown nexus-admin:nexus-admin /opt/nexus
cd /opt/nexus
sudo -u nexus-admin git clone \
    git@github.com:andboyer/nexus-edge-ai-core-next.git .
```

**Option B — Personal Access Token (interactive, one-off installs):**

Generate a fine-grained PAT at <https://github.com/settings/tokens?type=beta>
with **Repository access → Only select repositories →
`andboyer/nexus-edge-ai-core-next`** and **Repository permissions →
Contents: Read-only**. Then:

```bash
sudo mkdir -p /opt/nexus
sudo chown nexus-admin:nexus-admin /opt/nexus
cd /opt/nexus
git clone https://github.com/andboyer/nexus-edge-ai-core-next.git .
# Username: andboyer (your GitHub login)
# Password: <paste the PAT, NOT your account password>
```

### 6.3 Pick the tier config

Copy the tier file matching your hardware (from §1) into the
canonical engine config path. The per-tier overlays in §6.6 mount
`/etc/nexus/nexus.toml` into the container as the active config —
keeping the filename stable means switching tiers later, hand-editing
camera URLs, or pointing tooling at "the config" all just work.

```bash
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/config/tiers/t24.toml \
    /etc/nexus/nexus.toml            # ← swap t24.toml for your tier
```

> **Verify the recorder is in `gstreamer` mode (not `stub`).** Every
> tier config in `config/tiers/*.toml` now ships with
>
> ```toml
> [runtime.clips]
> recorder = "gstreamer"
> ```
>
> baked in. **If you're customising a config or porting an older one
> forward**, double-check the line is present — the runtime default
> when the key is missing is `recorder = "stub"`, which writes a
> 0-byte placeholder file per motion event and looks like a broken
> recorder to the operator. Verify after first boot with
>
> ```bash
> curl -fsS http://localhost:8089/api/v1/storage/local \
>     | jq -r .recorder_kind        # expect: "gstreamer"
> ```
>
> The `gstreamer` recorder shares the camera's single RTSP session
> with the detector via an internal `tee` (one connection per camera,
> not two), which is also what makes the engine compatible with the
> IP-camera firmwares that enforce a one-session-per-stream limit
> (e.g. InSight CS-series — see §8.1).

> **Optional once you're on the dogfooding kit.** Engine ≥ 0.1
> understands `--tier auto` (or `NEXUS_TIER=auto`), which calls
> `nexus-probe` in-process at startup and loads the matching
> `config/tiers/<tier>.toml` itself. The explicit copy above is
> still the right move for production deployments where you want
> to pin a version-controlled config and audit changes. See
> [`docs/ROADMAP.md` → M-Install Checkpoint 1](ROADMAP.md#checkpoint-1--dogfooding-kit-now-2-days).

### 6.4 Stage the models

> **When you need this section:**
>
> - **Pulling GHCR (§6.7 Option A):** skip — the published image bakes
>   the default pack (~100 MB) into `/usr/share/nexus/models/` and all
>   five tier configs already point `pack_path` there.
> - **Building from source (§6.7 Option B):** required, because
>   `models/` is gitignored — your fresh clone has an empty `models/`
>   directory (just a `.gitkeep`). Either (a) scp / generate the pack
>   into `/opt/nexus/models/` BEFORE running `docker compose build`
>   and the build will bake them into the image, OR (b) stage them at
>   `/var/lib/nexus/models/` and let the container bind-mount them at
>   runtime (steps below). Path (b) is preferred for fleet operators
>   because the same image works across boxes with different packs.
> - **Custom / fine-tuned pack on any install:** required.

```bash
# Stage the pack on the host where the per-tier overlay's
# /var/lib/nexus bind mount will surface it inside the container.
sudo mkdir -p /var/lib/nexus/models
sudo install -o nexus -g nexus -m 0644 \
    /path/to/yolo26n_dynamic.onnx \
    /var/lib/nexus/models/yolo26n_dynamic.onnx
sudo install -o nexus -g nexus -m 0644 \
    /path/to/yolo_world_v2_s.onnx \
    /var/lib/nexus/models/yolo_world_v2_s.onnx
sudo install -o nexus -g nexus -m 0644 \
    /path/to/yoloe26_s.onnx \
    /var/lib/nexus/models/yoloe26_s.onnx
sudo install -o nexus -g nexus -m 0644 \
    /path/to/models-manifest.json \
    /var/lib/nexus/models/models-manifest.json

# Sanity-check: the engine refuses to start if the manifest sha256
# doesn't match the file on disk.
sha256sum /var/lib/nexus/models/*.onnx
jq '.models[].sha256' /var/lib/nexus/models/models-manifest.json

# Tell the engine to read from /var/lib/nexus/models instead of
# /usr/share/nexus/models. Edit the active config:
sudo sed -i \
    's#/usr/share/nexus/models#/var/lib/nexus/models#g' \
    /etc/nexus/nexus.toml
```

> **Quick path from a dev machine that already has the pack:**
>
> ```bash
> # From the machine that ran `tools/models/gen_*.py`:
> ssh nexus 'sudo mkdir -p /var/lib/nexus/models && sudo chown -R nexus:nexus /var/lib/nexus/models'
> scp models/{yolo26n_dynamic.onnx,yolo_world_v2_s.onnx,yoloe26_s.onnx,models-manifest.json} \
>     nexus:/tmp/models-pack/
> ssh nexus 'sudo install -o nexus -g nexus -m 0644 /tmp/models-pack/* /var/lib/nexus/models/'
> ```

### 6.5 Where state lives

The per-tier overlays in §6.6 bind-mount `/var/lib/nexus` from the
host into the container. That's where the engine writes:

| Host path                                  | Holds                                             |
| ------------------------------------------ | ------------------------------------------------- |
| `/var/lib/nexus/nexus.db`                  | SQLite (cameras, rules, events, motion)           |
| `/var/lib/nexus/clips/`                    | Recorded mp4 clips                                |
| `/var/lib/nexus/state/dev-token`           | Auto-provisioned bearer token (mode 0600)         |
| `/var/lib/nexus/models/` *(if you ran §6.4)* | Custom model pack overriding the baked-in one  |

The baked-in image models at `/usr/share/nexus/models/` are **not**
shadowed by this bind mount — they live on a separate path inside
the container's read-only image layer.

### 6.6 Pick the per-tier compose overlay

The repo ships one overlay per supported tier under `deploy/`. Each one
sets the right `devices:` / `group_add:` / tier-pinned `NEXUS_CONFIG`
so the engine boots with the matching `ep_priority` list:

| Tier      | Overlay file                                                                        | What it adds                                      |
| --------- | ----------------------------------------------------------------------------------- | ------------------------------------------------- |
| **T10**   | [deploy/docker-compose.t10.yml](../deploy/docker-compose.t10.yml)     | `/dev/dri` iGPU, `t10.toml`                       |
| **T24**   | [deploy/docker-compose.t24.yml](../deploy/docker-compose.t24.yml)     | `/dev/dri` iGPU, `t24.toml`                       |
| **T36-S** | [deploy/docker-compose.t36s.yml](../deploy/docker-compose.t36s.yml)   | `/dev/dri` iGPU; **NPU passthrough commented** (see §5.3 — bare-metal still preferred for NPU today). Falls back to OpenVINO on the Arc 140V iGPU until you enable it. |

T36 (Arc A380 dGPU) and T64 (NVIDIA) overlays haven't been authored
yet because neither box is on the dogfooding desk; until they land,
fall back to a hand-written `docker-compose.override.yml` modeled on
the same shape — see §6.6.legacy below.

Symlink the right overlay so Compose auto-merges it on every command:

```bash
cd /opt/nexus/deploy
sudo ln -sf docker-compose.t24.yml docker-compose.override.yml   # ← T24 example; swap for your tier
ls -l docker-compose.override.yml                                # confirm symlink target
```

> **Why a symlink instead of `cp`:** subsequent `git pull` on
> `/opt/nexus` brings in upstream overlay tweaks automatically.
> Copying freezes the override at install-time and means you'll
> drift behind future bug-fixes.

**Then check the `render` / `video` group GIDs match the overlay's
defaults.** The overlays use numeric host GIDs (44 / 993) for
`/dev/dri` access because the container image has no `render` or
`video` group baked in. Verify on your box:

```bash
getent group render && getent group video
# Default expected: render:x:993:...  video:x:44:...
```

If either GID differs, drop an override into a `.env` file next to
the compose files (Compose auto-loads it):

```bash
sudo tee /opt/nexus/deploy/.env >/dev/null <<EOF
NEXUS_RENDER_GID=$(getent group render | cut -d: -f3)
NEXUS_VIDEO_GID=$(getent group video  | cut -d: -f3)
EOF
```

Without this, `docker compose up` fails with
`Unable to find group render: no matching entries in group file`
(if the host's GIDs differ from the 44/993 defaults).

#### 6.6.legacy — Hand-written override (T36, T64, or custom)

If your tier doesn't have a ready-made overlay yet, write your own
`docker-compose.override.yml` next to the base compose. Use one of
the canned overlays as a template; the device block is the only
tier-specific bit.

**T36 (Intel Arc A380 dGPU):**

```yaml
services:
  engine:
    image: ghcr.io/andboyer/nexus-engine:latest
    devices:
      - /dev/dri:/dev/dri
    group_add:
      - "video"
      - "render"
    volumes:
      - /etc/nexus:/etc/nexus:ro
      - /var/lib/nexus:/var/lib/nexus
    environment:
      - NEXUS_CONFIG=/etc/nexus/nexus.toml
      - NEXUS_TIER=t36
```

**T36-S** with NPU passthrough — see §7 (bare-metal). Container
NPU passthrough is unreliable on the kernels available today.

**T64 (NVIDIA):**

```yaml
services:
  engine:
    image: ghcr.io/andboyer/nexus-engine:latest
    runtime: nvidia
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: ["gpu"]
    volumes:
      - /etc/nexus:/etc/nexus:ro
      - /var/lib/nexus:/var/lib/nexus
    environment:
      - NEXUS_CONFIG=/etc/nexus/nexus.toml
      - NEXUS_TIER=t64
```

### 6.7 Pull (or build) + start

> The GHCR image `ghcr.io/andboyer/nexus-engine` is **private** and
> linked to this private repo. Tagged releases (`v*`) are published
> by [.github/workflows/release.yml](../.github/workflows/release.yml)
> with `:vX.Y.Z`, `:<sha>`, and `:latest` tags. **Pulling a release
> is the recommended path** — the image already includes the default
> model pack at `/usr/share/nexus/models/`, so §6.4 is skip-by-default
> and the engine starts in ~5 sec instead of a 15–25 min build.
>
> Build from source only if you need a commit between releases,
> are iterating on engine code, or want a custom feature set.

**Option A — Pull a published tag from GHCR (recommended):**

You need a GitHub Personal Access Token with `read:packages` scope to
pull. The token is created **once per edge box** and stored in Docker's
credential file under the user that runs `docker compose`.

1.  **Create the token in the GitHub UI:**

    > **Use a classic token.** Fine-grained PATs only expose the
    > `Packages` permission for **organization-owned** packages.
    > `andboyer/nexus-engine` is a user-owned package, so the
    > Packages permission **will not appear** in the fine-grained
    > token creation screen — pulls always 401 with such a token.
    > Open issue tracking this:
    > <https://github.com/orgs/community/discussions/24636>.
    > Switch to a classic token; if/when we move the package under
    > an org, fine-grained will become viable.

    Open <https://github.com/settings/tokens/new> ("Generate new
    token (classic)").

    - **Note:** `nexus-edge-<hostname>` (e.g. `nexus-edge-t10-01`)
    - **Expiration:** 1 year (rotate during scheduled maintenance),
      or "No expiration" if you'd rather rotate manually.
    - **Scopes:**
      - `read:packages` — **required**, lets `docker pull` fetch
        the image.
      - `repo` — optional, only if you also want this same token to
        `git pull` the private source for §6.8 updates.

    Click *Generate token* and copy it — it's shown **once**.

2.  **Log Docker in to GHCR on the edge box** (as the user that runs
    compose — `andboyer`, not root, because compose reads
    `~/.docker/config.json` of the calling user and forwards the
    credential to the daemon):

    ```bash
    # Paste the PAT when prompted — token IS the password.
    docker login ghcr.io -u andboyer
    # Or non-interactively:
    echo "<PAT>" | docker login ghcr.io -u andboyer --password-stdin
    # Stored at ~/.docker/config.json (mode 0600). Persists across
    # reboots until the PAT expires.
    ```

3.  **Pull + start:**

    ```bash
    cd /opt/nexus/deploy
    docker compose pull            # fetches ghcr.io/andboyer/nexus-engine:latest
    docker compose up -d
    docker compose logs -f         # Ctrl-C to detach
    ```

Pin to a specific `vX.Y.Z` in production by editing the `image:` line
of your tier overlay — `:latest` is fine for the dogfooding fleet but
auto-rolls forward on the next `pull`.

> **Troubleshooting `docker compose pull` → `denied`:**
>
> | Symptom in error | Cause | Fix |
> |---|---|---|
> | `denied: denied` with no auth attempt | not logged in | rerun `docker login ghcr.io -u andboyer` |
> | `denied: permission_denied: read_package` | PAT missing `read:packages` scope, OR you generated a fine-grained PAT (won't work for user-owned packages — see step 1 above) | regenerate as a **classic** token with `read:packages` |
> | `denied` after a recent PAT rotation | old creds cached | `docker logout ghcr.io` then `docker login` again |
> | `manifest unknown` for a `vX.Y.Z` tag | release workflow hasn't published that tag yet | check `gh run list --workflow=release.yml` on the publisher; or use `:latest` |
> | `denied` only as `root` / under `sudo` | login was under your user; root has its own `/root/.docker/config.json` | run compose without sudo (your user is in the `docker` group from §6.1), OR `sudo docker login ghcr.io -u andboyer` to give root its own creds |
>
> If you're not sure whether you're authenticated, run:
> `docker pull ghcr.io/andboyer/nexus-engine:latest` — a clean pull
> proves the credential is good independently of compose.

**Option B — Build from source (for non-tagged commits / local dev):**

> **Pre-flight:** complete [§6.4](#64-stage-the-models) first.
> `models/` is gitignored, so your clone has an empty models directory
> and the resulting image will contain no model files. You can still
> `docker compose build` successfully (the Dockerfile copies whatever
> is in `models/`, empty included), but the engine will fail at
> startup with `failed to open model file ... yolo26n_dynamic.onnx`.
> §6.4's bind-mount path (`/var/lib/nexus/models`) is the recommended
> workflow.

```bash
cd /opt/nexus/deploy
docker compose build engine    # first build ~15-25 min on T10/T24
docker compose up -d           # start the engine
docker compose logs -f         # Ctrl-C to detach; engine keeps running
```

First-boot tasks the engine performs automatically:

- Generates `/var/lib/nexus/state/dev-token` (mode 0600) — your bearer
  token. See §8.3.
- Creates `nexus.db` if absent and runs migrations.
- Loads the tier-pinned config, prints resolved `ep_priority` and
  worker counts.

Grab the dev-token straight out of the container:

```bash
docker exec nexus-engine cat /var/lib/nexus/state/dev-token
```

Then jump to §8 to add cameras.

### 6.8 Updating to a new release

The release workflow publishes a new immutable `vX.Y.Z` plus a
rolling `:latest` on every git tag. Choose the path that matches
how you installed in §6.7:

**If you pull from GHCR (§6.7 Option A — recommended):**

```bash
cd /opt/nexus
git pull --ff-only          # picks up overlay tweaks + new tier configs
cd deploy
docker compose pull         # pulls the new GHCR image
docker compose up -d        # restart-in-place with the new image
docker image prune -f       # reclaim disk
```

**If you built from source (§6.7 Option B):**

```bash
cd /opt/nexus
git pull --ff-only          # picks up overlay tweaks + new tier configs
cd deploy
docker compose build engine # rebuild against the new tree (cached)
docker compose up -d        # restart-in-place with the new image
docker image prune -f       # reclaim disk
```

Wrap that in `/usr/local/bin/nexus-update` and you have a one-word
update across the fleet:

```bash
sudo install -m 0755 /dev/null /usr/local/bin/nexus-update
sudo tee /usr/local/bin/nexus-update >/dev/null <<'EOF'
#!/bin/sh
set -e
cd /opt/nexus && git pull --ff-only
cd /opt/nexus/deploy
docker compose pull
docker compose up -d
docker image prune -f
EOF
```

Run by hand on each box after tagging a release, or cron-schedule it
nightly — your call. **Don't** point Watchtower at the container if
you care about staging releases: `:latest` rolls forward and a bad
tag breaks every box at once. Pin to `vX.Y.Z` in the overlay if you
want full release-gating.

---

## 7. Install path B — Bare-metal systemd (advanced)

This path is mandatory for **T36-S** (NPU passthrough is unreliable
in containers) and useful when you want a smaller attack surface or
need to install custom GStreamer plugins.

**Two sub-paths** below:

- **§7.0** — install from a pre-built release tarball. **Recommended
  for every bare-metal box.** No `git clone`, no `cargo build`, no
  Node toolchain. Drops a self-contained release into
  `/opt/nexus/releases/<version>/`, flips an atomic-swap symlink at
  `/opt/nexus/current`, and starts the systemd unit. The layout is
  the same one the future M-OTA updater uses to roll cohorts
  forward (see [M_OTA.md Step 8](M_OTA.md)) — so when the cloud
  control plane lands, this install is OTA-ready without any
  migration on your part.
- **§7.1–§7.7** — build from source. Use this if you've patched the
  engine locally or you're tracking `main` between releases. Same
  on-disk layout as §7.0 (so an `install.sh` run from a custom
  tarball drops into the same slot).

### 7.0 Bare-metal from release tarball (recommended)

#### 7.0.1 What you'll install

Every release published by [.github/workflows/release.yml](../.github/workflows/release.yml)
attaches a self-contained tarball:

```text
nexus-edge-vX.Y.Z-linux-x86_64.tar.gz   (~250 MB — engine, ORT-OpenVINO, models, UI)
nexus-edge-vX.Y.Z-linux-x86_64.tar.gz.sha256
install.sh                              (= scripts/bootstrap.sh, renamed for the curl-pipe one-liner)
```

The tarball contains:

```text
bin/
├── nexus-engine             # FEATURES=gstreamer,ort,ep-cpu,ep-openvino,ep-cuda,ep-tensorrt
├── nexus-probe
└── nexus-doctor
lib/onnxruntime/             # libonnxruntime.so + OpenVINO EP + intel CPU/GPU/NPU plugins
share/
├── ui/                      # SPA bundle
└── models/                  # default 4-file model pack (~100 MB)
etc-templates/
├── nexus.example.toml
├── tiers/{t10,t24,t36,t36s,t64}.toml
└── systemd/nexus-engine.service
scripts/
├── install.sh               # idempotent, handles upgrades + rollback
├── uninstall.sh
└── lib/install-common.sh
VERSION
MANIFEST.json                # every file + sha256 (used by install.sh + future OTA tamper check)
```

#### 7.0.2 Install (or upgrade)

Same command does first install AND every subsequent upgrade — it's
idempotent and the `--tier` flag is only consulted on first install
(so re-running with the wrong tier won't clobber a tuned
`/etc/nexus/nexus.toml`). If you omit `--tier` on a first install the
script runs `nexus-probe` from the staged release and auto-selects
the recommended tier; pass `--tier` explicitly to override.

```bash
curl -fsSL https://github.com/andboyer/nexus-edge-ai-core-next/releases/latest/download/install.sh \
  | sudo bash -s -- --tier t24      # swap t24 for your tier from §1, or omit for auto
```

The bootstrap script:

1. Resolves the release tag (or use `--version vX.Y.Z`).
2. Downloads `nexus-edge-...-linux-x86_64.tar.gz` + its `.sha256`.
3. Hands off to the in-tarball `scripts/install.sh`, which:
   - Re-verifies the sha256 sidecar.
   - Extracts to `/opt/nexus/releases/<version>/`.
   - Verifies every file against `MANIFEST.json` (catches mid-flight corruption).
   - Verifies the Ed25519 signature on `MANIFEST.json` against the
     committed `scripts/lib/release-pubkey.pem` (or warns and
     continues if the release predates signing; set
     `NEXUS_REQUIRE_SIGNATURE=1` to enforce strictly).
   - Creates the `nexus` system user + `/etc/nexus` + `/var/lib/nexus`.
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

Expected runtime on a T24-class box: ~90 s end-to-end on a clean
network (most of which is the 250 MB tarball download).

#### 7.0.3 Resulting on-disk layout

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
    ├── dev-token            # bearer token (auth.mode="none" only)
    └── admin-secret         # HS256 session key (auth.mode="local")
```

The key idea: **immutable, versioned releases under `/opt/nexus/releases/`
and a single mutable symlink at `/opt/nexus/current`**. Upgrades and
rollbacks are symlink flips followed by `systemctl restart
nexus-engine`. No `cargo build`, no `git pull`, no shared mutable
binary directory.

#### 7.0.4 Upgrade to a new release

```bash
# Idempotent — same one-liner as first install.
curl -fsSL https://github.com/andboyer/nexus-edge-ai-core-next/releases/latest/download/install.sh \
  | sudo bash -s --       # no --tier; existing /etc/nexus/nexus.toml is preserved
```

Or pin a specific version:

```bash
curl -fsSL https://github.com/andboyer/nexus-edge-ai-core-next/releases/download/v0.2.0/install.sh \
  | sudo bash -s -- --version v0.2.0
```

Keeps the old release dir at `/opt/nexus/releases/<previous>/` for
rollback (see §7.0.5). Run `sudo /opt/nexus/current/scripts/uninstall.sh
--keep-releases` then re-install to garbage-collect.

#### 7.0.5 Rollback

The installer records the previous good version on every upgrade.
Flip back with one command:

```bash
sudo /opt/nexus/current/scripts/install.sh --rollback
```

This re-points `/opt/nexus/current` at the previous release dir
(no download needed) and restarts the engine. If you've upgraded
twice since the version you want, run `--rollback` twice.

#### 7.0.6 Uninstall

```bash
# Default: stop + remove the unit and /opt/nexus, but preserve
# /etc/nexus + /var/lib/nexus so a re-install picks up where you
# left off.
sudo /opt/nexus/current/scripts/uninstall.sh

# Destructive: also wipes db, clips, configs, and the `nexus` user.
sudo /opt/nexus/current/scripts/uninstall.sh --purge
```

#### 7.0.7 What the tarball does NOT include

Operator must still install these manually before §7.0.2:

- **GStreamer runtime plugins** (§7.3 below — `apt install` is fine,
  same package set as the source-build path).
- **GPU drivers** per tier (§5.1–§5.4) — Intel iHD / Compute Runtime,
  NVIDIA driver + CUDA / TensorRT for T64, Lunar Lake NPU userspace
  for T36-S. The tarball binary is compiled with every EP enabled,
  but each EP only activates if its system libs are present.
- **Ubuntu prerequisites** from §3 and §4 (`tini`, firewall, etc.).

If you're missing any of the above, the engine will start but
specific cameras or codecs will fail at runtime. Run
`sudo /opt/nexus/current/bin/nexus-doctor` to see exactly what's
missing — it's the same smoke-test runner the verify step in §9
uses.

---

### 7.1 Install Rust _(build-from-source path; skip if you used §7.0)_

§§7.1–7.7 are only for operators who want to compile the engine
themselves — patched branches, untagged commits, custom feature
sets. For shipping releases, **prefer §7.0** — it lands the same
on-disk layout (`/opt/nexus/releases/<v>/` + atomic-swap
`/opt/nexus/current` symlink) without a Rust + Node toolchain on
the box, and is what the future OTA updater (M_OTA.md Step 8)
will operate against. If you build from source, you can install
into the same layout by running `sudo scripts/install.sh --tier
t24` against a tarball you build yourself with `tools/build-tarball.sh`
(planned follow-up; for now, follow §§7.1–7.7).

The toolchain pin lives in [rust-toolchain.toml](../rust-toolchain.toml)
(`channel = "stable"`).

```bash
sudo -u nexus-admin bash <<'EOF'
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable
EOF

# Make ~/.cargo/env sourced for every login shell, including future
# `sudo -u nexus-admin -i` invocations:
echo 'source $HOME/.cargo/env' \
  | sudo tee -a /home/nexus-admin/.profile
```

> **Foot-gun (from user-memory):** `command -v rustup` returning empty
> in a fresh shell does NOT mean rustup is missing — zsh/bash often
> just hasn't sourced `~/.cargo/env`. Check
> `[ -d ~/.rustup ] || ls ~/.cargo/bin/rustup` first.

### 7.2 Install Node 22

```bash
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs
node -v       # expect v22.x
npm -v
```

### 7.3 Install GStreamer + dev headers

The same package list as the Dockerfile uses
([deploy/Dockerfile](../deploy/Dockerfile) stages 2 + 3):

```bash
sudo apt install -y \
    pkg-config build-essential cmake git ca-certificates curl \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-libav gstreamer1.0-tools \
    libssl-dev
```

Add the runtime decode plugin from §5.5 if you skipped it.

### 7.4 Install ONNX Runtime 1.22.0

Pinned to 1.22.0 to match the Dockerfile + CI. The engine loads it
at runtime via `load-dynamic`. The `ort 2.0.0-rc.10` crate calls
`GetVersionString()` at session creation and panics with exit code 134
(`ort … is not compatible with the ONNX Runtime binary found at …`)
if the version isn't in the 1.22.x line — bump in lock-step with the
workspace `ort` dependency in `Cargo.toml`.

```bash
ORT_VER=1.22.0
curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VER}/onnxruntime-linux-x64-${ORT_VER}.tgz" \
  | sudo tar -xz -C /opt
sudo mv "/opt/onnxruntime-linux-x64-${ORT_VER}" /opt/onnxruntime
echo '/opt/onnxruntime/lib' | sudo tee /etc/ld.so.conf.d/onnxruntime.conf
sudo ldconfig
ls -l /opt/onnxruntime/lib/libonnxruntime.so*
```

### 7.5 Build the engine + UI

```bash
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
source $HOME/.cargo/env

# Pick the cargo features for your tier. The proxy features on
# `nexus-engine` (M-Install Checkpoint 2) forward into
# `nexus-inference`; the ones you don't pick stay zero-cost.
# `gstreamer` is mandatory on every tier — without it the engine
# compiles but the RTSP source is `#[cfg]`'d out, so every camera
# fails to connect with `requires the gstreamer feature` and the
# supervisor stops the pipeline before any frame arrives.
#   T10 / T24 / T36     →  gstreamer,ort,ep-cpu,ep-openvino
#   T36-S               →  gstreamer,ort,ep-cpu,ep-openvino   # NPU dispatched via OpenVINO; no separate ep-npu feature
#   T64 (post-beta)     →  gstreamer,ort,ep-cpu,ep-cuda,ep-tensorrt
FEATURES="gstreamer,ort,ep-cpu,ep-openvino"   # T24 example

# Two cargo invocations because workspace-level `--features` requires
# `-p` — same pattern the Dockerfile uses (deploy/Dockerfile, stage 2).
# `nexus-probe` carries no EP-relevant features so it builds with
# workspace defaults.
cargo build --release -p nexus-engine --features "$FEATURES" --bin nexus-engine
cargo build --release -p nexus-probe  --bin nexus-probe

(cd ui && npm ci && npm run build)
EOF

sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-engine /usr/local/bin/nexus-engine
sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-probe  /usr/local/bin/nexus-probe

sudo mkdir -p /usr/share/nexus/ui
sudo cp -r /opt/nexus/ui/dist/. /usr/share/nexus/ui/
sudo chown -R root:root /usr/share/nexus
```

### 7.6 Stage tier config + models

Identical to §6.3 + §6.4. The bare-metal engine reads from the same
`/etc/nexus/nexus.toml` and `/var/lib/nexus/models/` paths.

### 7.7 systemd unit

The canonical unit file is checked in at
[deploy/systemd/nexus-engine.service](../deploy/systemd/nexus-engine.service).
For the build-from-source path, install it and adjust the
`ExecStart` path to point at the `/usr/local/bin/` binaries you
just installed in §7.5 (the unit file as shipped assumes the §7.0
atomic-swap layout under `/opt/nexus/current/bin/`):

```bash
sudo install -o root -g root -m 0644 \
    /opt/nexus/deploy/systemd/nexus-engine.service \
    /etc/systemd/system/nexus-engine.service

# Build-from-source layout: binaries live in /usr/local/bin/, not
# /opt/nexus/current/bin/. ORT was installed under /opt/onnxruntime
# in §7.4, not /opt/nexus/current/lib/onnxruntime.
sudo sed -i \
    -e 's#/opt/nexus/current/bin/nexus-engine#/usr/local/bin/nexus-engine#' \
    -e 's#/opt/nexus/current/lib/onnxruntime#/opt/onnxruntime/lib#g' \
    /etc/systemd/system/nexus-engine.service

sudo systemctl daemon-reload
sudo systemctl enable --now nexus-engine
sudo journalctl -u nexus-engine -f
# Ctrl-C to detach.
```

The `DeviceAllow` lines in the unit are a no-op when the device
doesn't exist on this host; leave them in so the unit is portable
across tiers. The two `sed` rewrites above are the *only*
difference between the build-from-source layout and the §7.0
tarball layout — everything else (config path, state dir, hardening
stanza, `CAP_NET_BIND_SERVICE` for the :80 UI alias) is identical.

---

## 8. Configure cameras + first boot

### 8.1 Add cameras

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

> **RTSP credential gotcha (from user-memory):** if your password
> contains `!` and you ever paste the URL into a zsh / bash shell
> (e.g. testing with `curl -u`), wrap the whole string in single
> quotes after running `set +H`. The `!` triggers history expansion
> otherwise.

> **One-session-per-path IP cameras.** Some firmwares — confirmed on
> the InSight CS-series, also reported on a handful of low-end
> ONVIF-only re-badges — accept exactly **one** active RTSP session
> per stream URL. The second connection either gets a `503 Service
> Unavailable` or quietly starves the first. The engine sidesteps
> this by sharing the single RTSP session between detector and
> recorder through an internal `tee` (the `recorder = "gstreamer"`
> path; see §6.3). If you're configuring an external probe / VMS
> against the same camera stream URL while the engine is running,
> expect one of the two to lose the session. Workarounds:
> (a) point the external tool at the camera's `stream2` (most
> firmwares expose a separate sub-stream URL with its own session
> slot), or (b) stop the engine briefly. **Do not run two cameras
> in the engine with the same `url`** — they'll fight for the slot
> and one will silently stay at 0 fps after the first reconnect.

After editing, restart the engine to pick up the seed:

- Container: `docker compose restart engine`
- Bare-metal: `sudo systemctl restart nexus-engine`

**B. Add via API (no engine restart, recommended once running):**

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

### 8.2 Run `nexus-probe` to confirm the tier

```bash
# Container:
docker compose exec engine \
    /usr/local/bin/nexus-probe --out /var/lib/nexus/device-manifest.json

# Bare-metal:
sudo -u nexus /usr/local/bin/nexus-probe \
    --out /var/lib/nexus/device-manifest.json

jq '.recommended_tier, .accelerators' /var/lib/nexus/device-manifest.json
```

The `recommended_tier` field should match the tier file you copied
into `/etc/nexus/nexus.toml` in §6.3 / §7.6. If they disagree, the
probe is right by default — switch to its recommendation unless you
have a deliberate reason not to (one common case: you have an Arc
A380 *and* an RTX 4060 in the same Lenovo P3 Tower; pick `t64.toml`
manually).

### 8.3 Authentication

M-Install Checkpoint 2 made the engine secure-by-default. Read
[docs/ARCHITECTURE.md §11](ARCHITECTURE.md#11-identity--authentication)
for the full identity model. Short version: the customer-facing
identity path is **cloud-console-mediated** (one Entra app, one
secret, held in the cloud-console's Azure Key Vault, minting
short-lived `actor_token` JWTs that the edge verifies over the
mTLS tunnel). Edge boxes therefore do not ship with per-deployment
IdP configuration. The on-edge `auth.mode` exists for two reasons:
the pre-enrollment local-admin path, and the offline escape hatch.

| `auth.mode`   | When to use it                                                                  | Behaviour |
| ------------- | ------------------------------------------------------------------------------- | --------- |
| `"local"`     | **Customer-facing default.** Also the offline escape hatch when the cloud-console tunnel is unreachable. | First boot creates a single `admin` user with a one-time password printed at WARN. Set a real password on first login; create operator/viewer users from the UI. The HS256 session-signing secret auto-provisions to `<state_dir>/admin-secret` (mode 0600) — pin `auth.admin_secret_path = ...` to override with a k8s/Docker/systemd-managed file. |
| `"dev_token"` | Single-box dev / closed-lab rig on a trusted LAN.                               | On first boot the engine generates a 32-byte URL-safe random token, persists it to `<state_dir>/dev-token` (mode 0600), and prints it once at WARN. Clients send `Authorization: Bearer <token>`. **Compile-removed under `--features prod-auth`** so a release binary cannot ship a shared-secret bearer. |
| `"none"`      | Closed-lab / regression rigs that bind only to loopback.                        | Engine **refuses to boot** unless `[server].api_bind` is `127.0.0.1:*`, `[::1]:*`, or `localhost:*`. Use this only when an upstream reverse proxy / SSH tunnel is doing the auth. |
| `"oidc"`      | **Expert mode.** Rare on-prem deployment pointed at a site-local IdP (Authentik / Keycloak / AD FS).         | OIDC auth-code + PKCE; bearer tokens validated against the issuer's JWKS at every request. Not the customer-facing default — ships unconfigured. |
| `"hybrid"`    | **Expert mode.** OIDC + a single local `breakglass` admin for IdP outages.       | Same as `oidc` plus the local-users login path. Same expert-mode caveat. |

Tier configs in [config/tiers/](../config/tiers/) currently default
to `mode = "dev_token"` for installer convenience; flip to `"local"`
as soon as the box has more than one operator. Add an explicit
`[auth]` block to `/etc/nexus/nexus.toml`:

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
# Auto-provisioned at <state_dir>/dev-token unless pinned here.

# Expert mode — on-prem IdP. See docs/M6_IDENTITY.md before enabling.
[auth]
mode = "oidc"
[auth.oidc]
issuer    = "https://auth.example.com/application/o/nexus"
audience  = "nexus-engine"
client_id = "nexus-engine"
# client_secret_file = "/run/secrets/oidc"   # confidential clients only
```

`dev_token = "..."` pinned in TOML is acceptable as a shared secret
in a closed lab but never in production. Always prefer `mode = "local"`
for any deployment with more than one human operator.

---

## 9. Verification — smoke test

Run these in order. Don't skip a step that fails. Each step has an
expected output; if you don't see it, drop into §11 (Troubleshooting).

### 9.1 Engine answers HTTP

```bash
curl -fsS http://localhost:8089/api/health
# Expect: {"status":"ok"}  (or similar — non-empty 200)
```

Also verify the UI alias is listening on port 80:

```bash
curl -fsS http://localhost/api/health
# Same body as above. Same router on both ports.
```

If the second command fails, either:
- `[server].ui_bind` is unset in your nexus.toml — see §6.5 / §7.6.
- Port 80 is taken by another process — `sudo ss -ltnp | grep ':80 '`.
- Bare-metal: the systemd unit is missing `AmbientCapabilities=CAP_NET_BIND_SERVICE`
  (engine log will say `failed to bind server.ui_bind; … Permission denied`).

### 9.2 UI loads in a browser

```
http://<box-ip>/         # preferred — served by the ui_bind alias on :80
http://<box-ip>:8089/    # fallback — same UI, served by api_bind
```

You should see the Nexus dashboard. The Cameras tab should list
every `[[cameras]]` block / API-added camera from §8.1.

### 9.3 Cameras connect

In the UI, each enabled camera should transition to **`connected`**
within ~60 s. The `/api/cameras` endpoint returns the configured
rows (it does not include runtime state — the UI subscribes to
`/api/stream/metadata` for that):

```bash
curl -fsS http://localhost:8089/api/cameras | jq '.[] | {id, name, enabled}'
```

If a camera is stuck on `connecting` for > 2 min, jump to §11
(RTSP entry).

### 9.4 Snapshot from each camera

Proves the GStreamer source is producing frames *and* the inference
pipeline is consuming them.

```bash
curl -fsS http://localhost:8089/api/cameras/1/frames/latest \
  -o /tmp/cam1.jpg
file /tmp/cam1.jpg
# Expect: JPEG image data, baseline, precision 8, 1920x1080 ...
```

If the curl succeeds with a 0-byte file, the camera is connected
but no frame has reached the cache yet — wait 5 s and retry.

### 9.5 Inference backends are ready

```bash
curl -fsS http://localhost:8089/api/backends | jq
# Expect: every slot in `state: "ready"` with the EP your tier expects:
#   T10/T24/T36/T36-S → "openvino" (or "npu" on T36-S if NPU stack present)
#   T64               → "cpu" today; "tensorrt" / "cuda" once M5 lands
#   anything          → "cpu" as last-resort fallback
```

If you see `state: "starting"` for more than 30 s, the worker is
still loading the ONNX model (cold start can take 10–20 s for the
first session). If you see `state: "failed"`, check the engine
logs — the most common cause is a missing model file at
`/var/lib/nexus/models/` or a sha256 mismatch with
`models-manifest.json`.

### 9.6 Storage safety floor reports healthy

```bash
curl -fsS http://localhost:8089/api/v1/storage/local | jq '{recorder_kind, panic, free_pct, clips_dir, watermark_state}'
# Expect (subset — full body also includes fs_total_bytes,
# fs_used_bytes, fs_free_bytes, watermark_low_pct,
# watermark_panic_pct, per_camera[] — all sourced from statvfs +
# the watermark FSM):
# {
#   "recorder_kind": "gstreamer" | "stub",
#   "panic": false,
#   "free_pct": <high number>,
#   "clips_dir": "/var/lib/nexus/clips",
#   "watermark_state": "ok"
# }
```

If `panic: true` (or `watermark_state == "panic"`) you're already
below 5 % free on the clips filesystem. `df -h /var/lib/nexus/clips`
to see what's eating the space and drop into §11.

### 9.7 Motion → clip → Timeline

Walk in front of one of the cameras for 5 s. Within 10 s:

- The camera card should turn yellow ("motion").
- The Timeline tab on that camera should show a new motion block in
  the current hour.
- Click the block → it plays the recorded clip in-browser.

CLI cross-check:

```bash
curl -fsS "http://localhost:8089/api/v1/cameras/1/motion?from=$(date -u -d '5 minutes ago' +%FT%TZ)&to=$(date -u +%FT%TZ)" \
  | jq 'length'
# Expect: > 0
```

> **If the motion block appears but clicking it shows "no playable
> data" (or the file size is 0 bytes on disk in `clips_dir`):** the
> recorder booted in `stub` mode. Run
>
> ```bash
> curl -fsS http://localhost:8089/api/v1/storage/local | jq -r .recorder_kind
> ```
>
> If it returns `"stub"`, add `[runtime.clips] recorder = "gstreamer"`
> to `/etc/nexus/nexus.toml` (see §6.3 callout) and restart the
> engine. If it returns `"gstreamer"` but clips still look broken,
> see §11 (recorder writes ~864-byte mp4 files).

### 9.8 Alert end-to-end

The example config seeds a `person_in_zone` CEL rule. Stand in
camera 1's frame for ≥ 2 s; an alert should appear in the Alerts
tab and an `events` row should land in the DB:

```bash
sqlite3 /var/lib/nexus/nexus.db \
    "SELECT count(*) FROM events;"
# Expect: > 0
```

If §9.7 worked but §9.8 didn't, the rule isn't matching — confirm
your camera has `prompts` containing `"person"` (the closed-vocab
detector won't return person detections without the label being in
the prompt list).

---

## 10. Operating + day-2 essentials

### 10.0 Admin UI quickstart

The operator console is a single-page web app served by the engine
binary itself at **`http://<engine-host>/`** (port 80, served by
`[server].ui_bind`) or **`http://<engine-host>:8089/`** (port 8089,
served by `[server].api_bind` — same SPA, same auth) — no separate
admin process, no `/ui` path. Sidebar groups three operational
modes:

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
    shows the SDP streams; **Add** prefills the camera form with
    the guessed RTSP URL.
  - *Rules* — `+ New rule` opens a form for id / name / severity /
    `camera_filter` (multi-select chips) / track-age / consecutive
    frames / cooldown / enabled. The `when` field has two modes:
    a row-based **visual CEL builder** (subject ▸ operator ▸ value,
    AND/OR joiner) and a **raw text** escape hatch. Raw mode calls
    `POST /api/rules/validate` on blur and shows the parser error
    inline before save.
  - *Zones* (per camera, inside the camera form) — polygon editor
    overlaid on the latest snapshot. Click to add vertices, drag
    to move, right-click to delete. Green polygons are inclusion
    zones, red are exclusion; coords are stored normalized
    `[0..1]` so they survive resolution changes.

- **System**
  - *Storage* — local clips usage, cold-backend registry (LAN /
    Google Drive / OneDrive), watermark + throttle settings,
    replication stats. OAuth for Drive/OneDrive is end-to-end
    in-engine; the refresh token never reaches the browser.
  - *Backends* — detector-pool slot/state/generation telemetry.
  - *Health* — engine vitals, version, uptime.

#### Bearer-token auth (LAN / Tailscale access)

Loopback (`127.0.0.1`) requires no token. Over LAN or Tailscale,
the login path depends on the `[auth]` config block:

- `mode = "local"` (recommended per §8.3) — sign in via the
  `/login` page with the username + password you set on first
  boot. The SPA stores a session JWT in `localStorage`; no
  bearer-token paste field is shown.
- `mode = "dev_token"` — paste the auto-generated token from
  `<state_dir>/dev-token` into the topbar field. The engine
  prints it once at WARN on first boot.
- `mode = "oidc"` / `"hybrid"` — click the "Sign in with $provider"
  button on `/login`. The engine completes the auth-code + PKCE
  exchange and issues a session JWT. (Expert mode — see §8.3.)

Any 401 from a gated write surfaces as a toast and bounces the
SPA back to the login page.

### 10.1 Logs

```bash
# Container
docker compose -f deploy/docker-compose.yml \
              -f deploy/docker-compose.override.yml logs -f engine
# Bare-metal
sudo journalctl -u nexus-engine -f
```

Bump verbosity by editing `RUST_LOG` (compose: in the
`environment:` block; systemd: in the unit file under `Environment=`)
then restart. Useful filters:

```
RUST_LOG=info,nexus=debug                 # default
RUST_LOG=info,nexus=trace                 # verbose
RUST_LOG=info,nexus_pipeline=trace        # camera + recorder only
RUST_LOG=info,nexus_inference=trace       # detector pool only
```

### 10.2 Backups

The engine state is three things:

```bash
sudo systemctl stop nexus-engine          # or: docker compose stop engine
sudo tar -C /var/lib -czf /backup/nexus-$(date +%F).tgz nexus
sudo systemctl start nexus-engine
```

Restore is the inverse. There's no incremental clip backup story
yet — that's M2.2 (cold-mirror replication, see
[ROADMAP.md](ROADMAP.md#m22--cold-storage-replication-cold-mirror-not-tiering)).

### 10.3 Upgrades

**Container:**

```bash
cd /opt/nexus
git pull
docker compose -f deploy/docker-compose.yml \
              -f deploy/docker-compose.override.yml build
docker compose -f deploy/docker-compose.yml \
              -f deploy/docker-compose.override.yml up -d
```

**Bare-metal:**

```bash
cd /opt/nexus
sudo -u nexus-admin git pull
# Same two-step build as §7.5 (workspace-level --features needs -p).
# `gstreamer` must be in the feature list or the rebuilt binary will
# silently lose RTSP support and every camera will fail to connect.
sudo -u nexus-admin bash -c '. $HOME/.cargo/env && \
    cargo build --release -p nexus-engine --features gstreamer,ort,ep-cpu,ep-openvino --bin nexus-engine && \
    cargo build --release -p nexus-probe  --bin nexus-probe'
sudo cp /usr/local/bin/nexus-engine /usr/local/bin/nexus-engine.bak
sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-engine /usr/local/bin/nexus-engine
sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-probe  /usr/local/bin/nexus-probe
sudo systemctl restart nexus-engine
```

**Roll back (bare-metal):**

```bash
sudo cp /usr/local/bin/nexus-engine.bak /usr/local/bin/nexus-engine
sudo systemctl restart nexus-engine
```

**Roll back (container):** redeploy a prior image tag (`docker
compose pull <tag>` once we ship versioned tags; `git checkout` the
prior commit + rebuild today).

### 10.4 Forward-looking

- **M2.2 — Cold storage replication.** Operators will be able to
  point clip storage at a LAN folder, Google Drive, or OneDrive.
  The watermark sweeper soft-evicts cold-replicated clips before
  cascade-deleting non-replicated ones. Design lives in
  [M2_STORAGE.md §M2.2](M2_STORAGE.md#m22--cold-storage-replication).
- **M3.1 — Visual prompts (YOLOE).** Operators upload a JPEG, attach
  it to a camera, write a CEL rule against the operator-supplied
  label. Design lives in
  [M3_OPEN_VOCAB_VISUAL.md](M3_OPEN_VOCAB_VISUAL.md).

This guide will grow §6.7 / §10 sections for both once they ship.

---

## 11. Troubleshooting

| Symptom | Likely cause | Fix |
| ------- | ------------ | --- |
| `curl /api/health` returns connection refused | Engine isn't up. | `systemctl status nexus-engine` or `docker compose ps`; check logs (§10.1). |
| Engine refuses to start with `auth.mode = "none" is only allowed when server.api_bind is on loopback` | Since M-Install Checkpoint 2 the engine refuses to bind unauthenticated APIs onto a LAN. | Either change `[server].api_bind` to `127.0.0.1:8089` (LAN-only deployments), or set `[auth].mode = "dev_token"` and read the auto-generated token from `/var/lib/nexus/state/dev-token` (mode 0600). |
| Engine logs `auth: generated new dev token` at boot | First boot under `mode = "dev_token"` (the default since M-Install Checkpoint 2). The token is the credential clients send as `Authorization: Bearer <token>`. | Copy the token from the WARN line *or* from `/var/lib/nexus/state/dev-token`. To rotate: stop the engine, delete the file, restart. The path follows `runtime.state_dir` from `nexus.toml` (default `/var/lib/nexus/state`). |
| Engine logs `nexus.toml has no [auth] section` at boot | Pre-Checkpoint-2 config; the engine grandfathers it to `mode = "none"` for 7 days. | Add an explicit `[auth]` block (see [config/nexus.example.toml](../config/nexus.example.toml)) before the deadline printed in the WARN line. |
| UI loads but `/` returns 404 | `ui_root` mismatch — engine pointing at a path that doesn't exist. | Container: image build incomplete; rebuild. Bare-metal: `ls /usr/share/nexus/ui` should list `index.html`. Update `[server].ui_root` in `/etc/nexus/nexus.toml` to match. |
| Camera stuck on `connecting` for > 2 min | RTSP transport mismatch (camera serves UDP, engine asks TCP), bad credentials, blocked port. | Test with `gst-launch-1.0 -v rtspsrc location=<url> ! fakesink` from the host. If the password contains `!`, run `set +H` first and single-quote the URL (zsh history expansion). |
| `vainfo` succeeds for `nexus-admin` but engine logs say "no VAAPI device" | `nexus` user not in `render` group. | `sudo usermod -aG render nexus` then restart the engine + reload the systemd-cgroup view (`systemctl daemon-reload && systemctl restart nexus-engine`). |
| `/dev/accel/accel0` missing on T36-S | Kernel < 6.10, NPU disabled in BIOS, or driver trio not installed. | `uname -r` must be ≥ 6.10 (§3.6); §2 BIOS section; §5.3 driver install. |
| `nvidia-smi` works on the host but engine reports CPU EP only | NVIDIA Container Toolkit not installed, or compose doesn't have the `runtime: nvidia` block. | §5.4 step 5; §6.6 T64 snippet. |
| `/api/backends` shows all slots `state: "ready"` but every camera returns generic / mock-looking detection labels | `ep_priority` lists both `openvino` and `npu`. ORT's `RegisterExecutionProviderLibrary` is one-shot per session — the duplicate trips a "Provider OpenVINOExecutionProvider has already been registered" error that the yolo loader catches and silently falls back to the mock detector. | Set `ep_priority = ["npu", "cpu"]` (T36-S) or `ep_priority = ["openvino", "cpu"]` (T10/T24/T36) — never both. See [config/tiers/t36s.toml](../config/tiers/t36s.toml#L43) for the inline rationale. |
| Camera reaches `streaming` once then stays at 0 fps after every subsequent reconnect, but VLC against the same URL works fine | IP-camera firmware (e.g. InSight CS-series) enforces one RTSP session per stream path; some other client — or a stale engine session lingering after a crash — is holding the slot. | Power-cycle the camera (clears any stuck server-side session), confirm no other VMS / external probe is hitting the same `url`, and verify the engine is on `recorder = "gstreamer"` so detector + recorder share a single session via the internal `tee` (§6.3, §8.1). |
| Recorder writes 0-byte mp4 files | `recorder = "stub"` (the runtime default when `[runtime.clips]` is missing). | Add `[runtime.clips] recorder = "gstreamer"` to `/etc/nexus/nexus.toml` (§6.3 callout) and `sudo systemctl restart nexus-engine`. Confirm with `curl /api/v1/storage/local \| jq -r .recorder_kind`. |
| Recorder writes ~864-byte mp4 files (`recorder_kind = "gstreamer"`) | mp4mux silently dropping buffers without PTS. The shipping recorder synthesises PTS, coalesces same-PTS NAL pairs, and forces `h264parse config-interval=0`, so this should not happen on `main`; it indicates a regression. | Capture logs with `GST_DEBUG=qtmux:5,h264parse:4` and file an issue (§14). |
| Recorder refuses to open new clips ("storage panic") | Free space on `/var/lib/nexus/clips` ≤ `panic_watermark_pct` (default 5 %). | `df -h /var/lib/nexus/clips`; either grow the disk or lower `motion_clips_retention_days` and wait for the eviction round (default 30 s sample interval). |
| Engine fails to load model at boot with "sha256 mismatch" | The .onnx file on disk doesn't match `models-manifest.json`. Common after regenerating with custom prompts but forgetting to refresh the manifest. | Re-run `python tools/models/gen_yolo_world.py …` (Appendix A) — the script re-stamps the manifest. |
| `command not found: cargo` after a fresh ssh login | Rust toolchain is installed, but `~/.cargo/env` not sourced for non-interactive shells. | Confirm with `[ -d ~/.rustup ] || ls ~/.cargo/bin/rustup`. Add `source $HOME/.cargo/env` to `~/.profile` (§7.1). |
| `apt install docker-ce` fails: "package not found" | Docker apt repo not added or `apt update` not re-run. | §6.1 from the top — the `tee /etc/apt/sources.list.d/docker.list` step is the one most often skipped. |

If your symptom isn't here, file an issue (§14) with the data points
listed there.

---

## 12. Appendix A — Reproducible model generation

The repo doesn't ship pre-built ONNX files (they're in `.gitignore`).
Until a signed model pack lands on GitHub Releases, this is the
authoritative way to populate `/var/lib/nexus/models/`. Skip if you
have prebuilt models from another machine.

### 12.1 Install Python 3.11

The model-gen scripts depend on PyTorch + ultralytics which only
publish wheels for 3.11 on noble.

```bash
sudo apt install -y python3.11 python3.11-venv python3.11-dev
python3.11 --version    # expect 3.11.x
```

### 12.2 Create the model-gen venv

```bash
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install --upgrade pip
pip install -r tools/models/requirements.txt
EOF
```

### 12.3 Generate `yolo26n_dynamic.onnx` (closed-vocab)

```bash
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
source .venv-modelgen/bin/activate
python tools/models/gen_yolo26n.py \
    --output /opt/nexus/models/yolo26n_dynamic.onnx
EOF

# Move into place + fix ownership.
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/yolo26n_dynamic.onnx \
    /var/lib/nexus/models/yolo26n_dynamic.onnx
```

### 12.4 Generate `yolo_world_v2_s.onnx` (open-vocab)

The ultralytics auto-downloader for the YOLO-World base checkpoint
is flaky on some networks. Fetch the .pt with curl first, then point
the gen script at it:

```bash
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
source .venv-modelgen/bin/activate
mkdir -p models/.cache
curl -sL --fail \
    -o models/.cache/yolov8s-worldv2.pt \
    https://github.com/ultralytics/assets/releases/download/v8.4.0/yolov8s-worldv2.pt
python tools/models/gen_yolo_world.py \
    --base-model models/.cache/yolov8s-worldv2.pt \
    --output /opt/nexus/models/yolo_world_v2_s.onnx
EOF

sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/yolo_world_v2_s.onnx \
    /var/lib/nexus/models/yolo_world_v2_s.onnx
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/models-manifest.json \
    /var/lib/nexus/models/models-manifest.json
```

The gen script also writes `models-manifest.json` with refreshed
sha256 entries — copy it across alongside the `.onnx` file.

### 12.5 Generate `yoloe26_s.onnx` (open-vocab, M3.1 successor)

YOLOE is the open-vocabulary detector line that supersedes
YOLO-World in the ultralytics ecosystem. Same prompt-baking flow as
§12.4; smaller artifact (~42 MB vs ~48 MB).

```bash
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
source .venv-modelgen/bin/activate
# If the ultralytics PyPI release lacks the `YOLOE` symbol, upgrade:
#   pip install -U 'git+https://github.com/ultralytics/ultralytics@main'
python tools/models/gen_yoloe.py \
    --prompts tools/models/yoloe_default_prompts.txt
EOF

sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/yoloe26_s.onnx \
    /var/lib/nexus/models/yoloe26_s.onnx
# Manifest was upserted by the gen script — re-copy:
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/models-manifest.json \
    /var/lib/nexus/models/models-manifest.json
```

The per-camera prompt subset is configured at runtime via
`[[cameras]].prompts` in `nexus.toml`; every prompt in that list
MUST also appear in `tools/models/yoloe_default_prompts.txt`
before regeneration.

### 12.6 Custom prompt vocabulary

To change what YOLO-World can detect, edit
[tools/models/yolo_world_default_prompts.txt](../tools/models/yolo_world_default_prompts.txt)
and re-run §12.4. Each prompt becomes a class index baked into the
ONNX graph; the manifest captures the prompt list so the engine's
loader can map detections back to labels.

After regenerating, restart the engine — the loader checks
`models-manifest.json` against the on-disk file's sha256 and refuses
to start if they disagree.

---

## 13. Appendix B — End-to-end T24 transcript

Copy-pasteable shell session that takes a fresh GMKtec M3 Ultra from
post-Ubuntu-install to "first alert visible in the UI". Use this as
the canonical regression test when making changes to this document.

```bash
# ---- §4 Base hygiene ---------------------------------------------
sudo timedatectl set-timezone America/New_York
sudo apt update && sudo apt full-upgrade -y
sudo useradd --uid 1000 --create-home --shell /usr/sbin/nologin nexus
sudo mkdir -p /etc/nexus /var/lib/nexus/clips /var/lib/nexus/models
sudo chown -R nexus:nexus /var/lib/nexus
sudo ufw default deny incoming && sudo ufw default allow outgoing
sudo ufw allow OpenSSH && sudo ufw allow 8089/tcp
sudo ufw --force enable
sudo fallocate -l 8G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab

# ---- §5.1 Intel iGPU drivers (T24) -------------------------------
sudo apt install -y software-properties-common
sudo add-apt-repository -y ppa:kobuk-team/intel-graphics
sudo apt update
sudo apt install -y \
    libze-intel-gpu1 libze1 \
    intel-metrics-discovery intel-opencl-icd intel-gsc clinfo \
    intel-media-va-driver-non-free \
    libmfx-gen1 libvpl2 libvpl-tools \
    libva-glx2 va-driver-all vainfo \
    intel-gpu-tools
sudo usermod -aG render,video nexus
sudo usermod -aG render,video $USER
sudo reboot
# (reconnect)

# ---- §6.1 Docker -------------------------------------------------
sudo apt install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo $VERSION_CODENAME) stable" \
    | sudo tee /etc/apt/sources.list.d/docker.list
sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io \
    docker-buildx-plugin docker-compose-plugin
sudo usermod -aG docker $USER
exec sudo -u $USER bash -i        # re-login for docker group

# ---- §6.2 Clone + tier config + override -------------------------
# Repo is private — uses an SSH deploy key per §6.2 Option A.
# Generate the key + add it as a deploy key in the GitHub UI BEFORE
# running this block. PAT route (Option B) works too if you have one.
sudo mkdir -p /opt/nexus && sudo chown $USER:$USER /opt/nexus
git clone git@github.com:andboyer/nexus-edge-ai-core-next.git /opt/nexus
sudo install -o nexus -g nexus -m 0600 \
    /opt/nexus/config/tiers/t24.toml /etc/nexus/nexus.toml
cat <<'EOF' | sudo tee /opt/nexus/deploy/docker-compose.override.yml
services:
  engine:
    volumes:
      - /etc/nexus:/etc/nexus:ro
      - /var/lib/nexus:/var/lib/nexus
    devices:
      - /dev/dri:/dev/dri
    group_add:
      - "render"
EOF

# ---- §12 Generate models (skip if you have prebuilt) -------------
sudo apt install -y python3.11 python3.11-venv python3.11-dev
cd /opt/nexus
python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install -r tools/models/requirements.txt
python tools/models/gen_yolo26n.py --output /opt/nexus/models/yolo26n_dynamic.onnx
mkdir -p models/.cache
curl -sL --fail -o models/.cache/yolov8s-worldv2.pt \
    https://github.com/ultralytics/assets/releases/download/v8.4.0/yolov8s-worldv2.pt
python tools/models/gen_yolo_world.py \
    --base-model models/.cache/yolov8s-worldv2.pt \
    --output /opt/nexus/models/yolo_world_v2_s.onnx
sudo install -o nexus -g nexus -m 0644 /opt/nexus/models/*.onnx /var/lib/nexus/models/
sudo install -o nexus -g nexus -m 0644 /opt/nexus/models/models-manifest.json /var/lib/nexus/models/

# ---- §6.7 Build + start ------------------------------------------
cd /opt/nexus
docker compose -f deploy/docker-compose.yml \
              -f deploy/docker-compose.override.yml build
docker compose -f deploy/docker-compose.yml \
              -f deploy/docker-compose.override.yml up -d

# ---- §8.1 Add a camera -------------------------------------------
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

# ---- §9 Smoke test -----------------------------------------------
curl -fsS http://localhost:8089/api/health
curl -fsS http://localhost:8089/api/cameras | jq '.[] | {id, name, state}'
sleep 60
curl -fsS http://localhost:8089/api/cameras/1/frames/latest -o /tmp/cam1.jpg
file /tmp/cam1.jpg
curl -fsS http://localhost:8089/api/backends | jq
curl -fsS http://localhost:8089/api/v1/storage/local | jq
echo "Walk in front of camera 1 now..."
sleep 15
sqlite3 /var/lib/nexus/nexus.db "SELECT count(*) FROM events;"
```

---

## 14. Appendix C — End-to-end T36-S transcript

Copy-pasteable shell session that takes a fresh **GMKtec K13 AI**
(or EVO-X1, Intel Core Ultra 7 256V "Lunar Lake", Arc 140V iGPU
with NPU 4) from post-Ubuntu-install to "first alert visible in
the UI". T36-S is **bare-metal-only** — NPU passthrough via Docker
`--device /dev/accel/accel0` is fragile on shipping kernels, so the
systemd path in §7 is the canonical install. Use this as the
canonical regression test when making changes to this document.

```bash
# ---- §3.6 HWE kernel (Lunar Lake NPU needs >= 6.10) --------------
sudo apt update && sudo apt full-upgrade -y
sudo apt install -y linux-generic-hwe-24.04
sudo reboot
# (reconnect)
uname -r                       # expect 6.10.x or newer

# ---- §4 Base hygiene ---------------------------------------------
sudo timedatectl set-timezone America/New_York
sudo useradd --uid 1000 --create-home --shell /usr/sbin/nologin nexus
sudo useradd --create-home --shell /bin/bash nexus-admin
sudo usermod -aG sudo nexus-admin
sudo mkdir -p /etc/nexus /var/lib/nexus/clips /var/lib/nexus/models
sudo chown -R nexus:nexus /var/lib/nexus
sudo ufw default deny incoming && sudo ufw default allow outgoing
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp          # UI alias (skip if port 80 is taken)
sudo ufw allow 8089/tcp        # control-plane / API
sudo ufw --force enable
sudo fallocate -l 8G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab

# ---- §5.3 Step 2 — iGPU stack (PPA, NOT repositories.intel.com) -
sudo apt install -y software-properties-common
sudo add-apt-repository -y ppa:kobuk-team/intel-graphics
sudo apt update
sudo apt install -y \
    libze-intel-gpu1 libze1 \
    intel-metrics-discovery intel-opencl-icd intel-gsc clinfo \
    intel-media-va-driver-non-free \
    libmfx-gen1 libvpl2 libvpl-tools \
    libva-glx2 va-driver-all vainfo \
    intel-gpu-tools

# ---- §5.3 Step 3 — NPU driver trio (upstream GitHub release) -----
# Pinned to v1.32.1 (latest verified for Lunar Lake on kernel >=6.10).
# Single-tarball + `apt install ./intel-*.deb` lets apt resolve
# libtbb12 and libze1 from the kobuk-team PPA added in Step 2.
NPU_VER=1.32.1
NPU_TARBALL=linux-npu-driver-v${NPU_VER}.20260422-24767473183-ubuntu2404.tar.gz
mkdir -p /tmp/npu && cd /tmp/npu
wget "https://github.com/intel/linux-npu-driver/releases/download/v${NPU_VER}/${NPU_TARBALL}"
tar -xf "${NPU_TARBALL}"
sudo apt update
sudo apt install -y ./intel-*.deb

sudo usermod -aG render,video nexus
sudo usermod -aG render,video nexus-admin
sudo reboot
# (reconnect)

# Verify both accelerators came up before continuing.
vainfo --display drm --device /dev/dri/renderD128 | head -25
# Expect: VA-API 1.22.x, "Intel iHD driver ... - 25.x",
#         VAProfileH264* / VAProfileHEVC* / VAProfileAV1Profile0.
ls -l /dev/accel/accel0
# Expect: crw-rw---- 1 root render ... /dev/accel/accel0
sudo dmesg | grep -i intel_vpu | head -5
# Expect: "intel_vpu 0000:00:0b.0: Firmware: ..."
# (Ubuntu 24.04 defaults to kernel.dmesg_restrict=1 — hence sudo.)

# ---- §5.5 GStreamer runtime + dev headers (bare-metal needs both) -
sudo apt install -y \
    pkg-config build-essential cmake git ca-certificates curl libssl-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-libav gstreamer1.0-tools gstreamer1.0-vaapi

# ---- §7.1 Rust toolchain (as nexus-admin) ------------------------
sudo -u nexus-admin bash <<'EOF'
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable
EOF
echo 'source $HOME/.cargo/env' \
  | sudo tee -a /home/nexus-admin/.profile

# ---- §7.2 Node 22 ------------------------------------------------
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt install -y nodejs
node -v && npm -v

# ---- §7.4 ONNX Runtime 1.22.0 ------------------------------------
ORT_VER=1.22.0
curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VER}/onnxruntime-linux-x64-${ORT_VER}.tgz" \
  | sudo tar -xz -C /opt
sudo mv "/opt/onnxruntime-linux-x64-${ORT_VER}" /opt/onnxruntime
echo '/opt/onnxruntime/lib' | sudo tee /etc/ld.so.conf.d/onnxruntime.conf
sudo ldconfig
ls -l /opt/onnxruntime/lib/libonnxruntime.so*

# ---- §6.2 Clone (SSH deploy key per §6.2 Option A) ---------------
sudo mkdir -p /opt/nexus && sudo chown nexus-admin:nexus-admin /opt/nexus
sudo -u nexus-admin git clone \
    git@github.com:andboyer/nexus-edge-ai-core-next.git /opt/nexus

# ---- §7.5 Build engine + UI -------------------------------------
# T36-S features: gstreamer + ort + ep-cpu + ep-openvino. The NPU is
# dispatched via the OpenVINO EP (device NPU), NOT a separate ep-npu
# feature. `gstreamer` is REQUIRED — without it the engine compiles
# but the RTSP source is `#[cfg]`'d out and every camera fails to
# connect with `requires the gstreamer feature`.
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
source $HOME/.cargo/env
FEATURES="gstreamer,ort,ep-cpu,ep-openvino"
cargo build --release -p nexus-engine --features "$FEATURES" --bin nexus-engine
cargo build --release -p nexus-probe  --bin nexus-probe
(cd ui && npm ci && npm run build)
EOF

sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-engine /usr/local/bin/nexus-engine
sudo install -o root -g root -m 0755 \
    /opt/nexus/target/release/nexus-probe  /usr/local/bin/nexus-probe
sudo mkdir -p /usr/share/nexus/ui
sudo cp -r /opt/nexus/ui/dist/. /usr/share/nexus/ui/
sudo chown -R root:root /usr/share/nexus

# ---- §12 Generate models (skip if you have prebuilt) -------------
# T36-S uses YOLOE (open-vocab successor to YOLO-World). The base
# yolo26n model is still used for the fast-path detector.
sudo apt install -y python3.11 python3.11-venv python3.11-dev
sudo -u nexus-admin bash <<'EOF'
cd /opt/nexus
python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install -r tools/models/requirements.txt
python tools/models/gen_yolo26n.py \
    --output /opt/nexus/models/yolo26n_dynamic.onnx
python tools/models/gen_yoloe.py \
    --prompts tools/models/yoloe_default_prompts.txt \
    --output /opt/nexus/models/yoloe26_s.onnx
EOF
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/yolo26n_dynamic.onnx \
    /opt/nexus/models/yoloe26_s.onnx \
    /var/lib/nexus/models/
sudo install -o nexus -g nexus -m 0644 \
    /opt/nexus/models/models-manifest.json \
    /var/lib/nexus/models/

# ---- §7.6 Stage tier config -------------------------------------
sudo install -o nexus -g nexus -m 0600 \
    /opt/nexus/config/tiers/t36s.toml /etc/nexus/nexus.toml

# ---- §7.7 systemd unit ------------------------------------------
sudo tee /etc/systemd/system/nexus-engine.service >/dev/null <<'EOF'
[Unit]
Description=Nexus Edge AI engine (T36-S, Lunar Lake)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=nexus
Group=nexus
WorkingDirectory=/var/lib/nexus
Environment=ORT_DYLIB_PATH=/opt/onnxruntime/lib/libonnxruntime.so
Environment=RUST_LOG=info,nexus=debug
ExecStart=/usr/local/bin/nexus-engine --config /etc/nexus/nexus.toml
Restart=on-failure
RestartSec=5s
LimitNOFILE=65535

AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/nexus /tmp
PrivateTmp=true
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictSUIDSGID=true
DevicePolicy=closed
DeviceAllow=/dev/dri rw
DeviceAllow=/dev/accel/accel0 rw   # Lunar Lake NPU

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now nexus-engine
sudo journalctl -u nexus-engine -n 50 --no-pager

# ---- §8.2 Probe — confirms iGPU + NPU + decoder are wired -------
sudo -u nexus /usr/local/bin/nexus-probe \
    --output /var/lib/nexus/device-manifest.json
sudo cat /var/lib/nexus/device-manifest.json | jq '.tier, .accelerators'
# Expect: tier == "t36s", accelerators include both Arc 140V (iGPU)
#         and an NPU entry with provider "openvino" device "NPU".

# ---- §8.1 Add a camera -------------------------------------------
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

# ---- §9 Smoke test -----------------------------------------------
curl -fsS http://localhost/api/health        # UI alias on port 80
curl -fsS http://localhost:8089/api/health   # control-plane on 8089
curl -fsS http://localhost:8089/api/cameras | jq '.[] | {id, name, state}'
sleep 60
curl -fsS http://localhost:8089/api/cameras/1/frames/latest -o /tmp/cam1.jpg
file /tmp/cam1.jpg
curl -fsS http://localhost:8089/api/backends | jq
# Expect at least one backend with provider "openvino" and device
# "NPU" listed before the GPU/CPU fallbacks in ep_priority.
curl -fsS http://localhost:8089/api/v1/storage/local | jq
echo "Walk in front of camera 1 now..."
sleep 15
sqlite3 /var/lib/nexus/nexus.db "SELECT count(*) FROM events;"
```

**T36-S-specific gotchas to watch for:**

1. `vainfo` complains `intel-level-zero-gpu : Depends: libigc1 ...
   but it is not installable` — you copy-pasted a pre-2025-Q3
   recipe that referenced `repositories.intel.com/gpu`. Tear down
   `/etc/apt/sources.list.d/intel-gpu-noble.list` and
   `/etc/apt/preferences.d/intel-graphics`, then redo the PPA step
   above. See §5.1 troubleshooting.
2. `/dev/accel/accel0` missing after reboot — confirm `uname -r`
   is ≥ 6.10 (HWE step above) **and** that the BIOS has "AI
   Acceleration / NPU" set to ENABLED. On some K13 firmwares this
   setting is under "Advanced > CPU Configuration" rather than the
   top-level device list.
3. `nexus-engine` boots but the OpenVINO NPU device isn't picked
   up — the engine **falls through to the iGPU automatically** per
   the EP priority list in [config/tiers/t36s.toml](../config/tiers/t36s.toml).
   You'll still get frames + detections, just on the iGPU. Restart
   the engine after installing the NPU stack to pick it up
   (`sudo systemctl restart nexus-engine`).
4. Apple-silicon-style `vtdec` worry doesn't apply here — on Linux
   the GStreamer pipeline uses `vaapidecodebin`, not the macOS
   VideoToolbox path.

---

## 15. Appendix D — Where to file bugs

Open issues at
<https://github.com/andboyer/nexus-edge-ai-core-next/issues>. Include:

1. **Tier + box** — e.g. "T36-S, GMKtec K13 AI, BIOS V1.07".
2. **OS + kernel** — `cat /etc/os-release; uname -r`.
3. **Engine version** — `nexus-engine --version` (or `git rev-parse
   HEAD` from the build tree).
4. **Install path** — container or bare-metal. If container: `docker
   --version; docker compose version`. If bare-metal: `cargo
   --version; rustc --version`.
5. **Probe output** — attach
   `/var/lib/nexus/device-manifest.json`.
6. **Last 200 log lines** —
   `docker compose logs engine | tail -200` or
   `journalctl -u nexus-engine -n 200`.
7. **Watermark state** —
   `curl -fsS http://localhost:8089/api/v1/storage/local | jq`.
8. **Reproduction** — the smallest sequence that reliably reproduces
   the symptom.

Redact any RTSP credentials, OIDC issuer URLs, and customer-identifying
camera names before posting.

---

## See also

- [README.md](../README.md) — project overview, tier table, status banner.
- [docs/HARDWARE_TIERS.md](HARDWARE_TIERS.md) — full tier rationale + Lunar Lake driver caveat.
- [docs/ARCHITECTURE.md](ARCHITECTURE.md) — trait + pool pattern, frame-lifecycle spans, side-channels.
- [docs/ROADMAP.md](ROADMAP.md) — milestones M0 → M9.
- [docs/M2_STORAGE.md](M2_STORAGE.md) — M2.1 storage safety floor (shipped) + M2.2 cold-mirror (in progress).
- [docs/M3_OPEN_VOCAB_VISUAL.md](M3_OPEN_VOCAB_VISUAL.md) — visual-prompt detector design.
- [docs/M7_DELIVERY.md](M7_DELIVERY.md) — alert sinks + delivery policy.
- [docs/DEV_NOTES.md](DEV_NOTES.md) — developer setup, per-change cargo loop, model-gen smoke tests.
