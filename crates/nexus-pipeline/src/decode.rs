//! Decoder + post-process chain selection for the RTSP ingest path.
//!
//! Picks the GStreamer fragment that sits between the H.26x parser and the
//! RGB appsink. On Linux with a VA-capable GPU this routes decode onto the
//! GPU video block (Intel MFX / AMD VCN) via `vah26Xdec`; otherwise it falls
//! back to the software `avdec_h26X` chain. The historical hardcoded
//! `avdec_h26X` path pinned one CPU core per camera on any 1080p+ stream
//! while the iGPU sat at idle clock, so offloading the *decode* (the
//! expensive part) to silicon is what this module buys.
//!
//! **`vapostproc` is kept on Intel but bypassed on AMD.** The obvious VA
//! chain is `vah26Xdec ! vapostproc ! …` so the colour-convert/scale also
//! runs on the GPU, and that is exactly what we do on Intel. But on Mesa
//! `radeonsi` (AMD VCN) the new `va` plugin's `vapostproc` emits all-green
//! frames: its convert/download path is broken whether or not the caps pin
//! system memory (verified on a Radeon 680M, gfx1035, Mesa 25.2). So on an
//! AMD VA device we never use `vapostproc`. Instead, in preference order:
//!
//! 1. **Legacy `gstreamer1.0-vaapi` GPU path** — if `vaapih26Xdec` +
//!    `vaapipostproc` are registered: `vaapih26Xdec ! vaapipostproc !
//!    videoconvert ! videorate`. The OLD `vaapipostproc` does the NV12→RGB
//!    convert + downscale correctly on the GPU on the same Radeon 680M /
//!    Mesa 25.2 where the new `vapostproc` fails (verified with a live
//!    pipeline). `videoscale` is deliberately OMITTED so caps negotiation is
//!    forced to put the scale on `vaapipostproc` (the GPU) rather than let a
//!    downstream CPU `videoscale` claim it; the lone `videoconvert` is then
//!    only a cheap small-frame format bridge (the GPU has already downscaled
//!    to the target width/height). Keeps BOTH decode and convert/scale on
//!    the GPU. `vaapipostproc` runs a GBM/GL probe that needs
//!    `XDG_RUNTIME_DIR` set — the systemd unit provides it via
//!    `RuntimeDirectory=nexus`.
//! 2. **System-memory CPU-convert fallback** — otherwise `vah26Xdec !
//!    video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert`. The
//!    `video/x-raw,format=NV12` carries no `memory:VAMemory`/`memory:DMABuf`
//!    feature, so the decoder downloads each frame to system memory and the
//!    convert/scale runs on the CPU. The tail is ordered `videorate !
//!    videoscale ! videoconvert` so frames are dropped FIRST (cheap, still
//!    NV12), survivors are scaled while still subsampled NV12 (12 bpp,
//!    cheaper than RGB), and the costly NV12→RGB convert runs LAST — on the
//!    already-downscaled, rate-limited stream instead of at full resolution
//!    and full frame rate.
//!
//! GPU decode (the expensive part) is preserved in both. The split is keyed
//! on the DRM vendor via [`FactoryProbe::va_bypass_postproc`].
//!
//! Because a decoder is chosen on element *presence*, not on whether it
//! actually renders, the ingest path pairs this with a runtime guard
//! ([`rgb_frame_looks_degenerate`] plus `preroll_ingester`'s first-frames
//! check) that falls the camera back to the software chain if any hardware
//! decoder still renders garbage on the box.
//!
//! Selection is **fail-open**: if a requested hardware backend's elements are
//! not registered, it degrades to software (the caller logs the downgrade)
//! rather than failing the pipeline. Every chain ends with some ordering of
//! `videoconvert` / `videoscale` / `videorate` (the AMD legacy-`vaapipostproc`
//! chain omits `videoscale` on purpose — see above) so the downstream
//! `video/x-raw,format=RGB,width=..,height=..,framerate=../1` caps always
//! resolve.
//!
//! The selection logic is pure string-building over a [`FactoryProbe`]
//! abstraction so it is unit-testable on macOS without the `gstreamer`
//! feature or any real plugins.

pub use nexus_config::DecodeMode;

/// Which backend [`select_decode_chain`] actually chose after probing and
/// fail-open fallback. Distinct from the operator-requested [`DecodeMode`]:
/// a request of `Va` on a box without `vah264dec` yields a chain with
/// `backend == Software`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeBackend {
    /// libva (`vah26Xdec` + `vapostproc`).
    Va,
    /// Intel Media-SDK (`msdkh26Xdec` + `msdkvpp`).
    Msdk,
    /// NVIDIA NVDEC (`nvh26Xdec`, from the `nvcodec` plugin).
    Nvdec,
    /// Software (`avdec_h26X`).
    Software,
}

/// Abstraction over GStreamer element-factory presence so chain selection
/// is testable without real plugins. The production implementation,
/// [`GstFactoryProbe`], wraps `gstreamer::ElementFactory::find`.
pub trait FactoryProbe {
    /// Whether an element factory of the given name is registered.
    fn has(&self, factory_name: &str) -> bool;

    /// Whether the VA decode chain should bypass `vapostproc` on this host.
    /// `vapostproc` renders all-green frames on Mesa `radeonsi` (AMD) but
    /// works correctly on Intel, so the production probe returns `true` only
    /// on an AMD VA device. Default `false` keeps `vapostproc` (correct on
    /// Intel and in the pure unit tests).
    fn va_bypass_postproc(&self) -> bool {
        false
    }
}

/// The chosen decode + post-process fragment plus metadata for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeChain {
    /// GStreamer element fragment from the decoder through the
    /// convert/scale/rate tail, e.g.
    /// `vah264dec ! vapostproc ! videoconvert ! videoscale ! videorate`.
    /// The caller appends
    /// `! video/x-raw,format=RGB,width=..,height=..,framerate=../1 ! appsink`.
    pub elements: String,
    /// The backend that was actually selected (post fallback).
    pub backend: DecodeBackend,
    /// `true` when decode runs on the GPU video block.
    pub hwaccel: bool,
    /// Short human label for the boot log, e.g. `va (vah264dec+vapostproc)`.
    pub label: String,
}

impl DecodeChain {
    /// Whether the selected backend differs from a hardware request, i.e.
    /// the requested mode was `Va`/`Msdk`/`Nvdec` but selection fell open
    /// to software. The caller uses this to emit a one-line WARN.
    pub fn downgraded_from(&self, requested: DecodeMode) -> bool {
        matches!(
            requested,
            DecodeMode::Va | DecodeMode::Msdk | DecodeMode::Nvdec
        ) && self.backend == DecodeBackend::Software
    }
}

fn va_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "vah265dec",
        _ => "vah264dec",
    }
}

fn nvdec_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "nvh265dec",
        _ => "nvh264dec",
    }
}

/// Decoder factory name in the LEGACY `gstreamer1.0-vaapi` plugin (as opposed
/// to the new `va` plugin's [`va_decoder`]). Only used on AMD, where the old
/// `vaapipostproc` does GPU convert/scale correctly (see [`va_chain`]).
fn vaapi_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "vaapih265dec",
        _ => "vaapih264dec",
    }
}

fn msdk_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "msdkh265dec",
        _ => "msdkh264dec",
    }
}

fn sw_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "avdec_h265",
        _ => "avdec_h264",
    }
}

fn software_chain(codec_base: &str) -> DecodeChain {
    let dec = sw_decoder(codec_base);
    DecodeChain {
        elements: format!("{dec} ! videoconvert ! videoscale ! videorate"),
        backend: DecodeBackend::Software,
        hwaccel: false,
        label: format!("software ({dec})"),
    }
}

fn va_chain(codec_base: &str, bypass_postproc: bool, amd_vaapi_gpu: bool) -> DecodeChain {
    if !bypass_postproc {
        // Intel (and any other non-AMD VA device): `vapostproc` does GPU
        // colour-convert + scale correctly, so keep it. The trailing
        // videoconvert/videoscale are cheap no-ops when it already lands on
        // the requested RGB caps and a CPU safety net otherwise.
        let dec = va_decoder(codec_base);
        return DecodeChain {
            elements: format!("{dec} ! vapostproc ! videoconvert ! videoscale ! videorate"),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}+vapostproc)"),
        };
    }

    // AMD radeonsi: the new `vapostproc` renders all-green frames regardless
    // of its output caps (verified on a Radeon 680M / gfx1035, Mesa 25.2), so
    // it is never used below.
    if amd_vaapi_gpu {
        // Preferred AMD path: the LEGACY `gstreamer1.0-vaapi` plugin's
        // `vaapipostproc` does GPU convert + downscale correctly on the same
        // hardware where the new `vapostproc` fails (verified with a live
        // pipeline). `videoscale` is deliberately omitted so caps negotiation
        // is forced to put the scale on `vaapipostproc` (GPU) instead of a
        // downstream CPU `videoscale`; the lone `videoconvert` is then only a
        // cheap small-frame format bridge once the GPU has already downscaled
        // to the target width/height. Keeps BOTH decode and convert/scale on
        // the GPU. (`vaapipostproc` needs `XDG_RUNTIME_DIR`; the systemd unit
        // provides it via `RuntimeDirectory=nexus`.)
        let dec = vaapi_decoder(codec_base);
        return DecodeChain {
            elements: format!("{dec} ! vaapipostproc ! videoconvert ! videorate"),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}+vaapipostproc, gpu convert)"),
        };
    }

    // AMD fallback (no legacy vaapi plugin registered): decode on the GPU but
    // force the surface DOWN to system-memory NV12 (`video/x-raw,format=NV12`
    // carries no `memory:VAMemory`/`memory:DMABuf` feature, so the decoder
    // downloads each frame) and do the convert/scale on the CPU. Ordered
    // `videorate ! videoscale ! videoconvert` so frames are dropped first
    // (cheap, still NV12), survivors are scaled while still subsampled NV12
    // (12 bpp), and the costly NV12→RGB convert runs last on the
    // already-downscaled, rate-limited stream — not at full resolution and
    // full frame rate. GPU decode (the expensive part) is preserved.
    let dec = va_decoder(codec_base);
    DecodeChain {
        elements: format!(
            "{dec} ! video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert"
        ),
        backend: DecodeBackend::Va,
        hwaccel: true,
        label: format!("va ({dec}, sysmem NV12 + cpu convert)"),
    }
}

/// Build the VA chain for `codec_base` from a live probe: pick the Intel
/// (`vapostproc`) path, the AMD legacy-`vaapipostproc` GPU path, or the AMD
/// system-memory CPU-convert fallback. Keeps the AMD three-way decision in one
/// place so both [`select_decode_chain`] call sites stay in sync.
fn va_chain_for(codec_base: &str, probe: &impl FactoryProbe) -> DecodeChain {
    let bypass = probe.va_bypass_postproc();
    let amd_vaapi_gpu = bypass && amd_vaapi_gpu_available(probe, codec_base);
    va_chain(codec_base, bypass, amd_vaapi_gpu)
}

fn msdk_chain(codec_base: &str) -> DecodeChain {
    let dec = msdk_decoder(codec_base);
    DecodeChain {
        elements: format!("{dec} ! msdkvpp ! videoconvert ! videoscale ! videorate"),
        backend: DecodeBackend::Msdk,
        hwaccel: true,
        label: format!("msdk ({dec}+msdkvpp)"),
    }
}

/// NVDEC decode with CPU convert/scale.
///
/// `nvh26Xdec` can emit either `video/x-raw(memory:CUDAMemory)` or plain
/// system-memory `video/x-raw`; because the next element here is
/// `videoconvert` (system memory only), negotiation settles on the latter
/// and the decoder downloads NV12 itself. Decode runs on the GPU's NVDEC
/// block, convert/scale stay on the CPU — deliberately mirroring the AMD
/// "sysmem NV12 + cpu convert" chain, which is the shape already proven in
/// production here.
///
/// A fully GPU-resident tail (`cudaconvertscale ! cudadownload`) would
/// avoid the CPU colour conversion, but element availability varies across
/// GStreamer versions and it cannot be validated without the hardware, so
/// it is left as a future optimisation rather than an untested default.
fn nvdec_chain(codec_base: &str) -> DecodeChain {
    let dec = nvdec_decoder(codec_base);
    DecodeChain {
        elements: format!("{dec} ! videoconvert ! videoscale ! videorate"),
        backend: DecodeBackend::Nvdec,
        hwaccel: true,
        label: format!("nvdec ({dec}, sysmem NV12 + cpu convert)"),
    }
}

fn va_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(va_decoder(codec_base)) && probe.has("vapostproc")
}

/// Whether the LEGACY `gstreamer1.0-vaapi` GPU post-proc path is available for
/// `codec_base`, i.e. both `vaapih26Xdec` and `vaapipostproc` are registered.
/// Only consulted on AMD (see [`va_chain`]); on the Radeon 680M / Mesa 25.2
/// this old `vaapipostproc` does GPU convert/scale correctly where the new
/// `vapostproc` renders all-green.
fn amd_vaapi_gpu_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(vaapi_decoder(codec_base)) && probe.has("vaapipostproc")
}

fn msdk_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(msdk_decoder(codec_base)) && probe.has("msdkvpp")
}

/// Whether NVDEC decode is available for `codec_base`. Only the decoder
/// element is required — [`nvdec_chain`] converts on the CPU, so there is
/// no postproc counterpart to probe. The `nvcodec` plugin only registers
/// `nvh26Xdec` when it can dlopen `libnvcuvid.so`, so a registered factory
/// already implies a working driver-side NVDEC userspace.
fn nvdec_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(nvdec_decoder(codec_base))
}

/// Pick the decode + post-process fragment for one camera.
///
/// `codec_base` is the collapsed codec family (`"h264"` or `"h265"`); any
/// other value is treated as H.264 (the parser arms collapse `_plus` SVC
/// variants the same way). `mode` is the operator request; `probe` reports
/// which GStreamer element factories are registered on this host.
///
/// Fail-open semantics:
/// * `Software` — always the software chain.
/// * `Auto` — VA if available, else NVDEC, else software.
/// * `Va` — VA if available, else software (caller warns; see
///   [`DecodeChain::downgraded_from`]).
/// * `Msdk` — MSDK if available, else VA, else software.
/// * `Nvdec` — NVDEC if available, else software (caller warns).
///
/// `Auto` prefers VA over NVDEC so that a box with both an integrated
/// media engine and a discrete NVIDIA card decodes on the iGPU, leaving
/// the dGPU's budget entirely to inference.
///
/// Callers on macOS pass [`DecodeMode::Software`] (the only registered
/// decoders there are the `avdec_*` software ones; `vtdec` deadlocks a
/// headless engine), so this function never needs a `cfg(macos)` arm.
pub fn select_decode_chain(
    codec_base: &str,
    mode: DecodeMode,
    probe: &impl FactoryProbe,
) -> DecodeChain {
    match mode {
        DecodeMode::Software => software_chain(codec_base),
        DecodeMode::Auto => {
            if va_available(probe, codec_base) {
                va_chain_for(codec_base, probe)
            } else if nvdec_available(probe, codec_base) {
                nvdec_chain(codec_base)
            } else {
                software_chain(codec_base)
            }
        }
        DecodeMode::Va => {
            if va_available(probe, codec_base) {
                va_chain_for(codec_base, probe)
            } else {
                software_chain(codec_base)
            }
        }
        DecodeMode::Nvdec => {
            if nvdec_available(probe, codec_base) {
                nvdec_chain(codec_base)
            } else {
                software_chain(codec_base)
            }
        }
        DecodeMode::Msdk => {
            if msdk_available(probe, codec_base) {
                msdk_chain(codec_base)
            } else if va_available(probe, codec_base) {
                va_chain_for(codec_base, probe)
            } else {
                software_chain(codec_base)
            }
        }
    }
}

/// Number of consecutive decoded frames a hardware chain may emit that all
/// look degenerate (near-constant colour) before the ingest runtime guard
/// concludes the selected GPU decoder is not rendering correctly on this
/// hardware and falls the camera back to software decode. At the 15 fps
/// supervisor cap this is ~2 s — long enough that a working camera has shown
/// at least one real (non-flat) frame, short enough that a broken backend
/// self-heals almost immediately.
pub const DECODE_VALIDATION_FRAMES: u32 = 30;

/// Per-channel spread (max − min over sampled pixels) at or below which a
/// frame is considered "flat". A real sensor — even pointed at a dark or
/// blank wall — carries noise well above this; a broken decoder that emits a
/// constant colour (e.g. the radeonsi `vapostproc` all-green frame, RGB
/// `(0,128,0)` everywhere) has a spread of exactly 0.
const FLAT_CHANNEL_DELTA: u8 = 3;

/// Heuristic: does this tight-packed RGB24 frame look degenerate (a single
/// near-constant colour across the whole image)? Used by the ingest runtime
/// guard to detect a hardware decoder that is "registered but renders
/// garbage" on the local GPU/driver and trigger a software fallback.
///
/// Samples up to ~4096 pixels evenly across the frame and reports whether the
/// max − min spread on every channel is within [`FLAT_CHANNEL_DELTA`].
/// Returns `false` for an empty or sub-pixel slice — the guard must never
/// trip on malformed geometry (that has its own error path).
pub fn rgb_frame_looks_degenerate(rgb: &[u8]) -> bool {
    let pixels = rgb.len() / 3;
    if pixels == 0 {
        return false;
    }
    // Sample evenly so we inspect the whole frame, not just the first rows.
    let step = (pixels / 4096).max(1);
    let (mut rmin, mut gmin, mut bmin) = (255u8, 255u8, 255u8);
    let (mut rmax, mut gmax, mut bmax) = (0u8, 0u8, 0u8);
    let mut i = 0;
    while i < pixels {
        let o = i * 3;
        let (r, g, b) = (rgb[o], rgb[o + 1], rgb[o + 2]);
        rmin = rmin.min(r);
        rmax = rmax.max(r);
        gmin = gmin.min(g);
        gmax = gmax.max(g);
        bmin = bmin.min(b);
        bmax = bmax.max(b);
        i += step;
    }
    rmax - rmin <= FLAT_CHANNEL_DELTA
        && gmax - gmin <= FLAT_CHANNEL_DELTA
        && bmax - bmin <= FLAT_CHANNEL_DELTA
}

/// Production [`FactoryProbe`] backed by the live GStreamer registry.
#[cfg(feature = "gstreamer")]
pub struct GstFactoryProbe;

#[cfg(feature = "gstreamer")]
impl FactoryProbe for GstFactoryProbe {
    fn has(&self, factory_name: &str) -> bool {
        gstreamer::ElementFactory::find(factory_name).is_some()
    }

    fn va_bypass_postproc(&self) -> bool {
        va_device_is_amd()
    }
}

/// Whether any DRM render node is an AMD GPU (PCI vendor `0x1002`). AMD VA on
/// Linux always runs on Mesa `radeonsi`, whose `vapostproc` renders all-green
/// frames, so the VA chain bypasses it there (see [`va_chain`]). Reads the
/// world-readable `/sys/class/drm/renderD*/device/vendor` sysfs — no VA init,
/// no elevated caps. Any non-Linux or unreadable case returns `false` (keep
/// `vapostproc`); the preroll runtime guard is the backstop if that guess is
/// ever wrong on some box.
#[cfg(feature = "gstreamer")]
fn va_device_is_amd() -> bool {
    #[cfg(target_os = "linux")]
    {
        (128..136).any(|n| {
            std::fs::read_to_string(format!("/sys/class/drm/renderD{n}/device/vendor"))
                .map(|s| s.trim().eq_ignore_ascii_case("0x1002"))
                .unwrap_or(false)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Test probe: reports a fixed set of "registered" factories plus a
    /// controllable `va_bypass_postproc` (AMD-radeonsi) flag.
    struct SetProbe {
        factories: HashSet<&'static str>,
        bypass_postproc: bool,
    }

    impl FactoryProbe for SetProbe {
        fn has(&self, factory_name: &str) -> bool {
            self.factories.contains(factory_name)
        }
        fn va_bypass_postproc(&self) -> bool {
            self.bypass_postproc
        }
    }

    fn probe(names: &[&'static str]) -> SetProbe {
        SetProbe {
            factories: names.iter().copied().collect(),
            bypass_postproc: false,
        }
    }

    /// Like [`probe`] but simulates an AMD radeonsi VA device, i.e. one that
    /// must bypass the (broken) `vapostproc`.
    fn probe_amd(names: &[&'static str]) -> SetProbe {
        SetProbe {
            factories: names.iter().copied().collect(),
            bypass_postproc: true,
        }
    }

    fn va_full() -> SetProbe {
        probe(&["vah264dec", "vah265dec", "vapostproc"])
    }

    fn none() -> SetProbe {
        probe(&[])
    }

    #[test]
    fn software_mode_always_software_even_with_va_present() {
        let c = select_decode_chain("h264", DecodeMode::Software, &va_full());
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(!c.hwaccel);
        assert_eq!(
            c.elements,
            "avdec_h264 ! videoconvert ! videoscale ! videorate"
        );
        assert!(!c.downgraded_from(DecodeMode::Software));
    }

    #[test]
    fn software_mode_h265() {
        let c = select_decode_chain("h265", DecodeMode::Software, &va_full());
        assert!(c.elements.starts_with("avdec_h265 !"));
    }

    #[test]
    fn auto_picks_va_when_available() {
        // Non-AMD VA device (e.g. Intel): keep vapostproc (GPU postproc).
        let c = select_decode_chain("h264", DecodeMode::Auto, &va_full());
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.hwaccel);
        assert_eq!(
            c.elements,
            "vah264dec ! vapostproc ! videoconvert ! videoscale ! videorate"
        );
    }

    #[test]
    fn auto_picks_va_h265() {
        let c = select_decode_chain("h265", DecodeMode::Auto, &va_full());
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.elements.starts_with("vah265dec ! vapostproc !"));
    }

    #[test]
    fn amd_va_bypasses_vapostproc() {
        // AMD radeonsi WITHOUT the legacy gstreamer1.0-vaapi plugin: the new
        // `vapostproc` is broken (all-green), so the chain downloads
        // system-memory NV12 and converts on the CPU instead, keeping GPU
        // decode. Tail ordered videorate ! videoscale ! videoconvert so the
        // costly NV12→RGB convert runs last, on the downscaled/rate-limited
        // stream.
        let c = select_decode_chain(
            "h264",
            DecodeMode::Auto,
            &probe_amd(&["vah264dec", "vapostproc"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.hwaccel);
        assert!(!c.elements.contains("vapostproc"));
        assert_eq!(
            c.elements,
            "vah264dec ! video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert"
        );
    }

    #[test]
    fn amd_va_bypasses_vapostproc_h265() {
        let c = select_decode_chain(
            "h265",
            DecodeMode::Va,
            &probe_amd(&["vah265dec", "vapostproc"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c
            .elements
            .starts_with("vah265dec ! video/x-raw,format=NV12 !"));
    }

    #[test]
    fn amd_va_prefers_legacy_vaapi_gpu_postproc_when_present() {
        // AMD with the legacy gstreamer1.0-vaapi plugin ALSO registered:
        // prefer `vaapih264dec ! vaapipostproc` so convert+scale stay on the
        // GPU (verified correct on Radeon 680M / Mesa 25.2). `videoscale` is
        // omitted so the scale is forced onto vaapipostproc.
        let c = select_decode_chain(
            "h264",
            DecodeMode::Auto,
            &probe_amd(&["vah264dec", "vapostproc", "vaapih264dec", "vaapipostproc"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.hwaccel);
        assert!(!c.elements.contains("videoscale"));
        assert_eq!(
            c.elements,
            "vaapih264dec ! vaapipostproc ! videoconvert ! videorate"
        );
    }

    #[test]
    fn amd_va_legacy_vaapi_gpu_postproc_h265() {
        let c = select_decode_chain(
            "h265",
            DecodeMode::Va,
            &probe_amd(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
        assert_eq!(
            c.elements,
            "vaapih265dec ! vaapipostproc ! videoconvert ! videorate"
        );
    }

    #[test]
    fn auto_falls_back_to_software_without_va() {
        let c = select_decode_chain("h264", DecodeMode::Auto, &none());
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(!c.hwaccel);
        // Auto downgrade is silent (not a hardware *request*).
        assert!(!c.downgraded_from(DecodeMode::Auto));
    }

    #[test]
    fn va_request_falls_open_to_software_and_flags_downgrade() {
        let c = select_decode_chain("h264", DecodeMode::Va, &none());
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(c.downgraded_from(DecodeMode::Va));
    }

    #[test]
    fn nvdec_request_selects_nvcodec_h264() {
        let c = select_decode_chain("h264", DecodeMode::Nvdec, &probe(&["nvh264dec"]));
        assert_eq!(c.backend, DecodeBackend::Nvdec);
        assert!(c.hwaccel);
        assert_eq!(
            c.elements,
            "nvh264dec ! videoconvert ! videoscale ! videorate"
        );
        assert!(!c.downgraded_from(DecodeMode::Nvdec));
    }

    #[test]
    fn nvdec_request_selects_nvcodec_h265() {
        let c = select_decode_chain("h265", DecodeMode::Nvdec, &probe(&["nvh265dec"]));
        assert_eq!(c.backend, DecodeBackend::Nvdec);
        assert_eq!(
            c.elements,
            "nvh265dec ! videoconvert ! videoscale ! videorate"
        );
    }

    /// The nvcodec plugin registers per-codec decoders independently, so an
    /// H.265 stream on a card whose `nvh265dec` is absent must fall open to
    /// software rather than mis-selecting the H.264 element.
    #[test]
    fn nvdec_request_falls_open_to_software_and_flags_downgrade() {
        let c = select_decode_chain("h265", DecodeMode::Nvdec, &probe(&["nvh264dec"]));
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(c.downgraded_from(DecodeMode::Nvdec));
    }

    #[test]
    fn auto_uses_nvdec_when_va_is_absent() {
        let c = select_decode_chain("h264", DecodeMode::Auto, &probe(&["nvh264dec"]));
        assert_eq!(c.backend, DecodeBackend::Nvdec);
        assert!(c.hwaccel);
    }

    /// On a box carrying both an integrated media engine and a discrete
    /// NVIDIA card, `Auto` must decode on VA so the dGPU stays free for
    /// inference.
    #[test]
    fn auto_prefers_va_over_nvdec_when_both_are_present() {
        let c = select_decode_chain(
            "h264",
            DecodeMode::Auto,
            &probe(&["vah264dec", "vapostproc", "nvh264dec"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
    }

    #[test]
    fn va_request_needs_both_decoder_and_postproc() {
        // Decoder present but vapostproc missing → not usable.
        let c = select_decode_chain("h264", DecodeMode::Va, &probe(&["vah264dec"]));
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(c.downgraded_from(DecodeMode::Va));
    }

    #[test]
    fn msdk_prefers_msdk_then_va_then_software() {
        let msdk = probe(&["msdkh264dec", "msdkvpp", "vah264dec", "vapostproc"]);
        assert_eq!(
            select_decode_chain("h264", DecodeMode::Msdk, &msdk).backend,
            DecodeBackend::Msdk
        );

        let va_only = va_full();
        assert_eq!(
            select_decode_chain("h264", DecodeMode::Msdk, &va_only).backend,
            DecodeBackend::Va
        );

        let sw = none();
        let c = select_decode_chain("h264", DecodeMode::Msdk, &sw);
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(c.downgraded_from(DecodeMode::Msdk));
    }

    #[test]
    fn unknown_codec_base_treated_as_h264() {
        let c = select_decode_chain("av1", DecodeMode::Software, &none());
        assert!(c.elements.starts_with("avdec_h264 !"));
    }

    #[test]
    fn degenerate_detector_flags_constant_green() {
        // 640×360 all-green — the radeonsi vapostproc failure colour.
        let frame: Vec<u8> = std::iter::repeat_n([0u8, 128, 0], 640 * 360)
            .flatten()
            .collect();
        assert!(rgb_frame_looks_degenerate(&frame));
    }

    #[test]
    fn degenerate_detector_flags_solid_black_and_white() {
        assert!(rgb_frame_looks_degenerate(&vec![0u8; 320 * 240 * 3]));
        assert!(rgb_frame_looks_degenerate(&vec![255u8; 320 * 240 * 3]));
    }

    #[test]
    fn degenerate_detector_passes_gradient() {
        let mut frame = Vec::with_capacity(256 * 256 * 3);
        for y in 0..256u32 {
            for x in 0..256u32 {
                frame.push(x as u8);
                frame.push(y as u8);
                frame.push(((x + y) / 2) as u8);
            }
        }
        assert!(!rgb_frame_looks_degenerate(&frame));
    }

    #[test]
    fn degenerate_detector_passes_two_tone_frame() {
        // Mostly one colour, a quarter painted a very different one → the
        // spread far exceeds the flat delta.
        let mut frame = vec![40u8; 200 * 200 * 3];
        for p in 0..(200 * 200 / 4) {
            frame[p * 3] = 220;
            frame[p * 3 + 1] = 30;
            frame[p * 3 + 2] = 90;
        }
        assert!(!rgb_frame_looks_degenerate(&frame));
    }

    #[test]
    fn degenerate_detector_ignores_empty_or_subpixel() {
        assert!(!rgb_frame_looks_degenerate(&[]));
        assert!(!rgb_frame_looks_degenerate(&[1, 2]));
    }
}
