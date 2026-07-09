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
//! `radeonsi` (AMD VCN) `vapostproc` emits all-green frames: its
//! convert/download path is broken whether or not the caps pin system memory
//! (verified on a Radeon 680M, gfx1035, Mesa 25.2). So on an AMD VA device
//! the chain instead downloads the decoded surface to system-memory NV12 and
//! does the cheap convert/scale on the CPU (the same tail as the software
//! chain), still keeping GPU decode. The split is keyed on the DRM vendor via
//! [`FactoryProbe::va_bypass_postproc`].
//!
//! Because a decoder is chosen on element *presence*, not on whether it
//! actually renders, the ingest path pairs this with a runtime guard
//! ([`rgb_frame_looks_degenerate`] plus `preroll_ingester`'s first-frames
//! check) that falls the camera back to the software chain if any hardware
//! decoder still renders garbage on the box.
//!
//! Selection is **fail-open**: if a requested hardware backend's elements are
//! not registered, it degrades to software (the caller logs the downgrade)
//! rather than failing the pipeline. The chain always ends with
//! `videoconvert ! videoscale ! videorate` so the downstream
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
    /// GStreamer element fragment from the decoder through `videorate`,
    /// e.g. `vah264dec ! video/x-raw,format=NV12 ! videoconvert ! videoscale ! videorate`.
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
    /// the requested mode was `Va`/`Msdk` but selection fell open to
    /// software. The caller uses this to emit a one-line WARN.
    pub fn downgraded_from(&self, requested: DecodeMode) -> bool {
        matches!(requested, DecodeMode::Va | DecodeMode::Msdk)
            && self.backend == DecodeBackend::Software
    }
}

fn va_decoder(codec_base: &str) -> &'static str {
    match codec_base {
        "h265" => "vah265dec",
        _ => "vah264dec",
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

fn va_chain(codec_base: &str, bypass_postproc: bool) -> DecodeChain {
    let dec = va_decoder(codec_base);
    if bypass_postproc {
        // AMD radeonsi: `vapostproc` renders all-green frames regardless of
        // whether its output caps pin system memory — the decoder is fine,
        // its convert/download path is not (verified on a Radeon 680M /
        // gfx1035, Mesa 25.2; every `vapostproc` variant tested came back
        // byte-identically green). So decode on the GPU (`vah26Xdec`) but
        // force the surface DOWN to system-memory NV12 (`video/x-raw,
        // format=NV12` carries no `memory:VAMemory`/`memory:DMABuf` feature,
        // so the decoder downloads each frame) and do the cheap convert/scale
        // on the CPU, exactly like the software chain's tail. GPU decode (the
        // expensive part) is preserved.
        DecodeChain {
            elements: format!(
                "{dec} ! video/x-raw,format=NV12 ! videoconvert ! videoscale ! videorate"
            ),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}, sysmem NV12 + cpu convert)"),
        }
    } else {
        // Intel (and any other non-AMD VA device): `vapostproc` does GPU
        // colour-convert + scale correctly, so keep it. The trailing
        // videoconvert/videoscale are cheap no-ops when it already lands on
        // the requested RGB caps and a CPU safety net otherwise.
        DecodeChain {
            elements: format!("{dec} ! vapostproc ! videoconvert ! videoscale ! videorate"),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}+vapostproc)"),
        }
    }
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

fn va_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(va_decoder(codec_base)) && probe.has("vapostproc")
}

fn msdk_available(probe: &impl FactoryProbe, codec_base: &str) -> bool {
    probe.has(msdk_decoder(codec_base)) && probe.has("msdkvpp")
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
/// * `Auto` — VA if available, else software.
/// * `Va` — VA if available, else software (caller warns; see
///   [`DecodeChain::downgraded_from`]).
/// * `Msdk` — MSDK if available, else VA, else software.
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
        DecodeMode::Auto | DecodeMode::Va => {
            if va_available(probe, codec_base) {
                va_chain(codec_base, probe.va_bypass_postproc())
            } else {
                software_chain(codec_base)
            }
        }
        DecodeMode::Msdk => {
            if msdk_available(probe, codec_base) {
                msdk_chain(codec_base)
            } else if va_available(probe, codec_base) {
                va_chain(codec_base, probe.va_bypass_postproc())
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
        // AMD radeonsi: vapostproc is broken (all-green), so the chain
        // downloads system-memory NV12 and converts on the CPU instead,
        // keeping GPU decode.
        let c = select_decode_chain(
            "h264",
            DecodeMode::Auto,
            &probe_amd(&["vah264dec", "vapostproc"]),
        );
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.hwaccel);
        assert_eq!(
            c.elements,
            "vah264dec ! video/x-raw,format=NV12 ! videoconvert ! videoscale ! videorate"
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
