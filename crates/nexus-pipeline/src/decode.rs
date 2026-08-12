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
//! frames: **its output buffer is never written**. A green frame decodes to
//! exactly one distinct colour covering 100% of the pixels — `(0,134,0)` is
//! simply what an all-zero NV12 buffer becomes under a BT.601-limited
//! convert. VPP reports success; `GST_DEBUG=va*:4` logs no VA error.
//!
//! Root cause is **surface tiling**, measured on a Radeon 680M (gfx1035,
//! Mesa 25.2.8): `AMD_DEBUG=notiling` makes `vapostproc` pixel-accurate
//! against the CPU reference, while `AMD_DEBUG=nodcc` changes nothing.
//! radeonsi allocates the VPP *output* surface tiled and the readback path
//! consumes it as linear, so downstream sees an unwritten buffer. The
//! decoder's own readback handles tiling correctly (the bypass chain below
//! is pixel-accurate with tiling on), and legacy `vaapipostproc` uses a
//! copy-based readback and is unaffected — so this is specific to
//! `vapostproc`'s output surface, not to VA readback in general.
//!
//! `AMD_DEBUG=notiling` is **not** shipped as a workaround: it is a
//! process-global Mesa flag that makes every surface linear, and the engine
//! runs ORT inference on the same iGPU. Nor is this waiting on a GStreamer
//! bump — GStreamer 1.26.6 / plugins-bad 1.26.5 was staged against the same
//! Mesa and its `vapostproc` is byte-identically broken. So on an AMD VA
//! device we never use `vapostproc`. Instead, in preference order:
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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

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
    /// `true` iff this chain is the AMD legacy-`gstreamer1.0-vaapi`
    /// `vaapipostproc` GPU-convert tier. Lets the ingester's runtime
    /// frame-loop guard tell whether a repeating-frame trip happened on the
    /// tier it can escalate away from (see
    /// [`VAAPIPOSTPROC_LOOP_ESCALATION_LIMIT`]) without string-matching
    /// `label`.
    pub legacy_vaapipostproc: bool,
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
        legacy_vaapipostproc: false,
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
            legacy_vaapipostproc: false,
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
            legacy_vaapipostproc: true,
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
        legacy_vaapipostproc: false,
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

/// Number of runtime frame-loop-guard trips (see [`FrameLoopDetector`]) a
/// camera may accumulate on the AMD legacy-`vaapipostproc` GPU-convert tier
/// before the ingester stops trusting that tier for the rest of this
/// camera's session lifetime and re-selects with
/// [`AvoidLegacyVaapiPostproc`], landing on the system-memory NV12 +
/// CPU-convert tier instead (GPU decode is kept; only the buggy GPU
/// post-process is dropped).
///
/// A plain session rebuild is not a fix here: `vaapipostproc`'s surface pool
/// recycling that the loop guard catches is a property of this box's
/// concurrent camera count against the driver's surface allocator, not a
/// one-off — observed in the field to retrip within minutes of every
/// rebuild on the same camera. The limit is kept above 1 so a single
/// coincidental trip (the guard's own bar is already ~2s of provably stale
/// video within a 90-frame window) doesn't move a camera off GPU
/// post-process on a fluke.
pub const VAAPIPOSTPROC_LOOP_ESCALATION_LIMIT: u32 = 3;

/// Wraps a [`FactoryProbe`] and reports the legacy `gstreamer1.0-vaapi`
/// decoder + `vaapipostproc` elements as unregistered regardless of what is
/// actually installed, forcing [`va_chain_for`] past the AMD GPU-convert
/// tier onto the system-memory NV12 + CPU-convert fallback. Element
/// presence alone can't capture "registered but its surface pool recycles
/// under load on this box", so this is how the runtime loop guard
/// (see [`VAAPIPOSTPROC_LOOP_ESCALATION_LIMIT`]) expresses that verdict
/// back into chain selection on the next session rebuild.
pub struct AvoidLegacyVaapiPostproc<'a, P>(pub &'a P);

impl<P: FactoryProbe> FactoryProbe for AvoidLegacyVaapiPostproc<'_, P> {
    fn has(&self, factory_name: &str) -> bool {
        match factory_name {
            "vaapih264dec" | "vaapih265dec" | "vaapipostproc" => false,
            _ => self.0.has(factory_name),
        }
    }

    fn va_bypass_postproc(&self) -> bool {
        self.0.va_bypass_postproc()
    }
}

fn msdk_chain(codec_base: &str) -> DecodeChain {
    let dec = msdk_decoder(codec_base);
    DecodeChain {
        elements: format!("{dec} ! msdkvpp ! videoconvert ! videoscale ! videorate"),
        backend: DecodeBackend::Msdk,
        hwaccel: true,
        label: format!("msdk ({dec}+msdkvpp)"),
        legacy_vaapipostproc: false,
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
        legacy_vaapipostproc: false,
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

/// How many recent frames [`FlatFrameDetector`] evaluates its trip decision
/// over. Matches [`FRAME_LOOP_EVAL_WINDOW`] — ~6 s at the 15 fps supervisor
/// cap — so the two runtime decode-health guards report on the same
/// timescale.
pub const FLAT_FRAME_EVAL_WINDOW: usize = 90;

/// Degenerate frames within [`FLAT_FRAME_EVAL_WINDOW`] before
/// [`FlatFrameDetector`] trips. At the 15 fps supervisor cap 30 flat frames
/// is ~2 s of provably wrong picture, and a chain that is broken from its
/// very first frame still trips on exactly the evidence the superseded
/// consecutive-run check needed — 30 flat frames in a row is also 30 flat
/// frames within 90.
pub const FLAT_FRAME_TRIP: u32 = 30;

/// Detects a hardware decode path that is emitting degenerate
/// (near-constant colour) frames — the all-zero NV12 surface an AMD VA pool
/// hands back when it recycles a slot it never wrote, which renders as a
/// solid green picture.
///
/// Counts flat frames *within a rolling window* rather than requiring an
/// unbroken run, and never disarms. Both properties are load-bearing, and
/// the startup-only validation this replaces had neither:
///
/// * Validating only the first frames of a session stops evaluating for the
///   rest of it. Since practically every session renders correctly for at
///   least a frame or two before a pool goes bad, that is a *startup check*,
///   not a runtime guard — a camera that turned green minutes later was
///   never re-examined and stayed green indefinitely.
/// * The frame-loop guard cannot cover the gap either: a frozen green
///   picture repeats at distance 1, which [`FrameLoopDetector`] excludes by
///   design so that static night scenes and `videorate` padding do not tear
///   down healthy sessions.
///
/// Measured on a 53-camera Radeon 680M box running 0.1.190: eleven cameras
/// sat on a byte-identical 4661-byte all-green frame across three
/// consecutive sweeps while [`FrameLoopDetector`] logged zero trips in the
/// same window — exactly the blind spot above.
#[derive(Debug, Default)]
pub struct FlatFrameDetector {
    /// Per-observation flat/not-flat outcomes over the trip window.
    outcomes: std::collections::VecDeque<bool>,
    hits: u32,
    observed: u64,
    flat: u64,
}

impl FlatFrameDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames observed and, of those, how many looked degenerate. Reported
    /// even when the detector never trips, so a flat rate below
    /// [`FLAT_FRAME_TRIP`] is still visible to telemetry instead of silent.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.observed, self.flat)
    }

    /// Record one frame. Returns `true` once [`FLAT_FRAME_TRIP`] of the last
    /// [`FLAT_FRAME_EVAL_WINDOW`] frames looked degenerate, then resets so a
    /// caller that chooses not to act still gets a bounded log rate.
    pub fn observe(&mut self, flat: bool) -> bool {
        self.observed = self.observed.saturating_add(1);
        if flat {
            self.flat = self.flat.saturating_add(1);
            self.hits += 1;
        }
        self.outcomes.push_back(flat);
        if self.outcomes.len() > FLAT_FRAME_EVAL_WINDOW && self.outcomes.pop_front() == Some(true) {
            self.hits = self.hits.saturating_sub(1);
        }

        if self.hits >= FLAT_FRAME_TRIP {
            self.hits = 0;
            self.outcomes.clear();
            return true;
        }
        false
    }
}

/// How many recent frame fingerprints [`FrameLoopDetector`] keeps. Must
/// exceed the deepest surface/buffer pool we expect to cycle through;
/// observed depths in the field were 4–6, and the RGB branch itself only
/// carries `queue max-size-buffers=8` + `appsink max-buffers=4`.
pub const FRAME_LOOP_WINDOW: usize = 12;

/// How many recent observations the trip decision is evaluated over.
///
/// The detector counts looping frames *within this window* rather than
/// requiring an unbroken run. A recycled surface pool does not always hand
/// back a stale surface — a genuinely fresh frame slips through whenever
/// the race resolves the other way — and under a consecutive-run rule a
/// single such frame reset the counter to zero, so an intermittent loop
/// could run indefinitely without ever tripping.
///
/// At the 15 fps supervisor cap this is ~6 s of video.
pub const FRAME_LOOP_EVAL_WINDOW: usize = 90;

/// Looping frames within [`FRAME_LOOP_EVAL_WINDOW`] before the detector
/// trips. One in three frames provably recycled is far outside anything a
/// healthy decoder produces — repeats at distance 1 (static scenes,
/// `videorate` padding) are excluded before they ever reach this count —
/// while still being slack enough that a short coincidental burst cannot
/// tear down a working session.
pub const FRAME_LOOP_TRIP: u32 = 30;

/// Minimum interval between two frame-loop-guard session rebuilds on one
/// camera.
///
/// The guard's remedy is a full teardown + rebuild, which reallocates the
/// camera's VA decoder and its surface pool. On a VRAM-constrained box that
/// reallocation is itself the thing that produces an unwritten (solid green)
/// surface — and a green frame is pixel-identical to its predecessor, so it
/// reads as a duplicate and trips the guard again. Field-measured on a
/// 53-camera Radeon 680M at 90% VRAM: 1523 trips and 1523 rebuilds in 25
/// minutes, ~61 rebuilds a minute across the fleet, indefinitely.
///
/// Throttling the remedy breaks that loop without blinding the detector: a
/// genuinely wedged decoder is still rebuilt, just at most once per window,
/// and the trips in between are reported rather than acted on.
pub const FRAME_LOOP_REBUILD_COOLDOWN: Duration = Duration::from_secs(300);

/// Per-camera rate limiter for the frame-loop guard's rebuild remedy.
///
/// Lives outside the session (like `vaapipostproc_loop_trips`) because the
/// thing being limited is how often sessions are torn down — state that a
/// per-session detector cannot hold, since the rebuild is what destroys it.
#[derive(Debug, Default)]
pub struct LoopRebuildThrottle {
    last_rebuild: Mutex<Option<Instant>>,
    suppressed: AtomicU32,
}

impl LoopRebuildThrottle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a rebuild may proceed at `now`. Records the rebuild when it
    /// returns `true`; counts the trip as suppressed when it returns `false`.
    pub fn allow_rebuild_at(&self, now: Instant) -> bool {
        let mut last = self.last_rebuild.lock();
        match *last {
            Some(prev) if now.duration_since(prev) < FRAME_LOOP_REBUILD_COOLDOWN => {
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    /// Trips observed while inside the cooldown, for the suppression log.
    #[must_use]
    pub fn suppressed(&self) -> u32 {
        self.suppressed.load(Ordering::Relaxed)
    }
}

/// Guard-driven rebuilds one camera may spend on a single decode tier before
/// that tier is judged wrong *for that camera*.
///
/// [`LoopRebuildThrottle`] bounds how *often* the rebuild remedy runs; it does
/// not give the remedy anywhere to go. Field-measured on a 53-camera Radeon
/// 680M: twelve cameras each tripped exactly once per
/// [`FRAME_LOOP_REBUILD_COOLDOWN`] for the whole observation window — every
/// permitted rebuild was spent and every one of them came back green. Two
/// strikes is deliberately small because a rebuild that was going to help
/// helps immediately; a third is just another 5 minutes of green wall.
pub const DECODE_ESCALATION_LIMIT: u32 = 2;

/// Trip-free interval after which a camera's escalation strikes lapse.
///
/// Without this a camera accumulates a strike every few hours and eventually
/// escalates itself onto software decode over nothing but uptime.
pub const DECODE_ESCALATION_FORGIVE: Duration = Duration::from_secs(1800);

/// Per-camera striker that decides when a decode tier has had its chance.
///
/// Lives outside the session for the same reason [`LoopRebuildThrottle`] does:
/// the rebuild it is counting is the thing that destroys per-session state.
#[derive(Debug, Default)]
pub struct DecodeEscalation {
    state: Mutex<EscalationState>,
}

#[derive(Debug, Default)]
struct EscalationState {
    strikes: u32,
    last_trip: Option<Instant>,
}

impl DecodeEscalation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a guard-driven rebuild at `now`. Returns `true` when this tier
    /// has burned its allowance and the caller should step down one rung.
    ///
    /// Resets on escalation so the next tier down starts with a full
    /// allowance rather than escalating again on its first trip.
    pub fn record_rebuild_at(&self, now: Instant) -> bool {
        let mut st = self.state.lock();
        if let Some(prev) = st.last_trip {
            if now.duration_since(prev) >= DECODE_ESCALATION_FORGIVE {
                st.strikes = 0;
            }
        }
        st.last_trip = Some(now);
        st.strikes += 1;
        if st.strikes >= DECODE_ESCALATION_LIMIT {
            st.strikes = 0;
            true
        } else {
            false
        }
    }

    /// Strikes accumulated against the current tier, for logging.
    #[must_use]
    pub fn strikes(&self) -> u32 {
        self.state.lock().strikes
    }
}

/// Host-wide ceiling on how many cameras may sit on software decode at once.
///
/// Escalating a broken camera to CPU decode is correct; escalating an
/// unbounded number of them is how a box dies. The ceiling is keyed on CPU
/// cores rather than fleet size because cores are the thing being spent: a
/// 1080p15 software H.265 decode costs roughly half a core, and the same
/// Radeon 680M box that motivated this already runs the engine at ~11 of 16
/// cores with every camera doing CPU colour-convert. A camera refused the
/// budget stops rebuilding and is reported degraded instead — an honest
/// degraded camera is worth more than a green one that also eats the cores
/// the healthy cameras need.
#[derive(Debug)]
pub struct SoftwareFallbackBudget {
    cap: u32,
    claimed: AtomicU32,
}

impl SoftwareFallbackBudget {
    #[must_use]
    pub fn new(cap: u32) -> Self {
        Self {
            cap,
            claimed: AtomicU32::new(0),
        }
    }

    /// Budget for a host with `cores` CPUs: a quarter of them, always at
    /// least one so a small box can still fall back.
    #[must_use]
    pub fn for_cores(cores: usize) -> Self {
        let cap = u32::try_from(cores / 4).unwrap_or(u32::MAX);
        Self::new(cap.max(1))
    }

    /// Claims one software-decode slot. `false` means the host is already at
    /// its ceiling and the caller must not latch software decode.
    pub fn try_claim(&self) -> bool {
        self.claimed
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                (c < self.cap).then_some(c + 1)
            })
            .is_ok()
    }

    #[must_use]
    pub fn claimed(&self) -> u32 {
        self.claimed.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn cap(&self) -> u32 {
        self.cap
    }
}

/// Process-wide software-decode budget, sized from the host's CPU count.
pub fn software_fallback_budget() -> &'static SoftwareFallbackBudget {
    static BUDGET: OnceLock<SoftwareFallbackBudget> = OnceLock::new();
    BUDGET.get_or_init(|| {
        SoftwareFallbackBudget::for_cores(
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(4),
        )
    })
}

/// Byte stride at which [`frame_fingerprint`] samples the frame. Prime, so
/// consecutive samples rotate through the R/G/B channels instead of reading
/// one channel forever.
const FINGERPRINT_STRIDE: usize = 31;

/// FNV-1a over a strided sample of a tight-packed RGB24 frame.
///
/// Cheap enough to run on the streaming thread (~1/31 of the bytes we
/// already memcpy) and sensitive enough that a person moving anywhere in
/// frame changes it, which is what the loop detector needs: it must never
/// call two genuinely different frames identical.
pub fn frame_fingerprint(rgb: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < rgb.len() {
        h ^= u64::from(rgb[i]);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += FINGERPRINT_STRIDE;
    }
    // Fold the length in so a truncated frame can't collide with a full one.
    h ^= rgb.len() as u64;
    h.wrapping_mul(0x0000_0100_0000_01b3)
}

/// Detects a decode path that has stopped advancing and is instead
/// re-serving a small set of already-delivered frames in a fixed cycle.
///
/// This is a distinct failure from a stall: `frame_id`, `last_frame_at` and
/// the wire timestamp all keep advancing normally, so every liveness signal
/// the engine has reports the camera as healthy while the picture on the
/// wall is seconds old. The only observable is the pixel content itself.
///
/// A **period of 1** (this frame identical to the one before it) is
/// explicitly *not* a loop — that is what a genuinely static night scene or
/// a `videorate` duplicating up to the configured framerate looks like, and
/// tearing those sessions down would be a self-inflicted outage. Only a
/// repeat at distance ≥ 2, i.e. `… A B C D A B C D …`, counts.
#[derive(Debug, Default)]
pub struct FrameLoopDetector {
    recent: std::collections::VecDeque<u64>,
    /// Per-observation loop/no-loop outcomes over the trip window.
    outcomes: std::collections::VecDeque<bool>,
    hits: u32,
    /// Period of the most recent looping frame, reported on trip so the
    /// operator log names the cycle depth actually seen.
    last_period: usize,
    observed: u64,
    duplicates: u64,
}

impl FrameLoopDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames observed and, of those, how many repeated at distance ≥ 2.
    ///
    /// Reported even when the detector never trips: without it a loop that
    /// stays under [`FRAME_LOOP_TRIP`] is completely invisible, which is
    /// the only reason its magnitude in the field is unknown.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.observed, self.duplicates)
    }

    /// Record one frame. Returns `Some(period)` once [`FRAME_LOOP_TRIP`]
    /// of the last [`FRAME_LOOP_EVAL_WINDOW`] frames looped, and resets so
    /// a caller that chooses not to act still gets a bounded log rate.
    pub fn observe(&mut self, fingerprint: u64) -> Option<usize> {
        let identical_to_previous = self.recent.back() == Some(&fingerprint);
        // Distance back to the most recent frame with this content.
        let period = if identical_to_previous {
            None
        } else {
            self.recent
                .iter()
                .rev()
                .position(|f| *f == fingerprint)
                .map(|back| back + 1)
        };

        self.recent.push_back(fingerprint);
        if self.recent.len() > FRAME_LOOP_WINDOW {
            self.recent.pop_front();
        }

        self.observed = self.observed.saturating_add(1);
        let looped = period.is_some();
        if let Some(p) = period {
            self.duplicates = self.duplicates.saturating_add(1);
            self.last_period = p;
            self.hits += 1;
        }
        self.outcomes.push_back(looped);
        if self.outcomes.len() > FRAME_LOOP_EVAL_WINDOW && self.outcomes.pop_front() == Some(true) {
            self.hits = self.hits.saturating_sub(1);
        }

        if self.hits >= FRAME_LOOP_TRIP {
            self.hits = 0;
            self.outcomes.clear();
            return Some(self.last_period);
        }
        None
    }
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

/// Context types that are safe to hand from one pipeline to another.
///
/// A *display* is a connection to the driver (VADisplay / EGLDisplay / GBM
/// device). It is refcounted, internally locked, and explicitly designed by
/// upstream GStreamer to be shared process-wide — and it is the object that
/// owns the Mesa driver instance, its `util_queue` worker pool and the
/// `gldisplay-event` thread, so sharing it captures essentially all of the
/// thread/VRAM win this module exists for.
///
/// A GL **context** is not in this list, deliberately. `GstGLContext` is
/// bound to the thread that made it current; the sanctioned way for a second
/// pipeline to use the same GPU state is to create its own context that
/// *shares* with the first (`gst_gl_display_create_context`), which the
/// elements do for themselves once they are handed the display. Handing the
/// same `GstGLContext` object to N pipelines running on N streaming threads
/// puts N threads on one context concurrently, which in Mesa surfaces as
/// stale texture/PBO readback — decoded frames that cycle through the last
/// few pool entries instead of advancing. Same reasoning for the per-element
/// `gst.gl.app_context` / `gst.gl.local_context` handles.
///
/// # Why the VA *decoder* displays are not in this list either
///
/// The same "cycle through the last few pool entries" failure was measured
/// on a 29-camera Intel box sharing `gst.va.display.handle` across 29
/// `vah265dec` pipelines: 4–5 of 8 sampled cameras served byte-identical
/// JPEGs on a short rotating cycle (period 2–6) while `frame_id` and the
/// wire timestamp advanced normally — i.e. every liveness signal reported
/// the camera healthy while the wall showed footage seconds stale. The
/// effect toggled cleanly with `NEXUS_SHARED_GL_CONTEXT`:
///
/// | sharing | runs | cameras looping (of 8) |
/// |---------|------|------------------------|
/// | on      | 3    | 4, 4, 5                |
/// | off     | 2    | 0, 0                   |
///
/// Cross-camera payload identity was 0 in every run, so the corruption is
/// *within* each decoder's surface pool, not a mix-up between pipelines:
/// N decoders driving one `VADisplay` race on that display's internal
/// surface bookkeeping and re-hand a previously-decoded surface back to
/// the sink.
///
/// Dropping these two costs very little of what this module exists for.
/// The thread/VRAM win is owned by `gst.gl.GLDisplay` — that is the object
/// holding the Mesa driver instance and its `util_queue` pool, created by
/// the GBM/GL probe `vaapipostproc` runs on realize. `vapostproc` (the new
/// `va` plugin, our Intel path) never runs that probe, so sharing its VA
/// display bought no threads and only ever carried the risk.
///
/// `gst.vaapi.Display` (legacy AMD path) has since been measured directly on
/// a Radeon 680M box (3 cameras on the legacy `vaapipostproc` chain, 638
/// poll rounds per phase, toggled with `NEXUS_SHARED_GL_CONTEXT`):
///
/// | sharing | threads (min/med/max of 33) | cameras looping | cross-camera identical |
/// |---------|-----------------------------|-----------------|------------------------|
/// | on      | 131 / 131 / 131             | 0 of 3          | 0                      |
/// | off     | 138 / 138 / 138             | 0 of 3          | 0                      |
///
/// So on AMD the removal is safe but not free: **+7 threads for 3 cameras**,
/// ~2.3 per camera, with `gst.gl.GLDisplay` still shared. (Scaling that to a
/// 29-camera box is extrapolation — only 3 were measured.) AMD never
/// reproduced the looping itself, so here the removal is *preventative*: the
/// shared-display pattern is the measured root cause on Intel and keeping it
/// live on one vendor would leave a latent hazard plus a second code path. A
/// visible thread/VRAM regression is recoverable; silently stale video on a
/// surveillance wall is not.
#[cfg(feature = "gstreamer")]
const SHAREABLE_CONTEXT_TYPES: &[&str] = &[
    // Created by the GBM/GL probe `vaapipostproc` runs on realize. This is
    // the one that owns the Mesa driver instance + `util_queue` workers,
    // and the only one this module needs to share.
    "gst.gl.GLDisplay",
];

#[cfg(feature = "gstreamer")]
fn context_is_shareable(ctx_type: &str) -> bool {
    SHAREABLE_CONTEXT_TYPES.contains(&ctx_type)
}

/// Process-wide cache of the display contexts negotiated by the first
/// pipeline that needed each type. Keyed by context type; only types in
/// [`SHAREABLE_CONTEXT_TYPES`] are ever stored or served.
#[cfg(feature = "gstreamer")]
static SHARED_CONTEXTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, gstreamer::Context>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Whether context sharing is enabled. Opt out with
/// `NEXUS_SHARED_GL_CONTEXT=0` if a box turns out to serialise decode
/// behind one VA display.
#[cfg(feature = "gstreamer")]
fn shared_contexts_enabled() -> bool {
    !matches!(
        std::env::var("NEXUS_SHARED_GL_CONTEXT").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Make `pipeline` join the process-wide VA/GL display instead of
/// standing up its own.
///
/// `vaapipostproc` (the legacy `gstreamer1.0-vaapi` path we prefer on
/// AMD, see the module docs) runs a GBM/GL probe on realize, and by
/// default every pipeline that does so creates its own `GstGLDisplay`
/// and its own Mesa driver instance. With one ingest pipeline per
/// camera that scales linearly: on a 50-camera box it accounted for
/// roughly 1,200 of the engine's 1,331 threads (Mesa `util_queue`
/// workers named `<proc>:sh*` / `:disk$*`) and drove VRAM to 91% of a
/// 4 GB carve-out.
///
/// The fix is the standard GStreamer context dance: answer
/// `need-context` from a cache that `have-context` fills, so pipeline
/// #2..#N adopt pipeline #1's display. It has to run on a **sync**
/// bus handler — `need-context` is posted during state change and the
/// element has already fallen back to creating its own display by the
/// time an async watch would see it.
///
/// Only the display-type contexts in [`SHAREABLE_CONTEXT_TYPES`] are
/// cached and served; every other type (notably `gst.gl.GLContext`)
/// falls through so each pipeline negotiates its own. See that
/// constant for why sharing a GL *context* object is not safe.
///
/// Returns `BusSyncReply::Pass` so any existing async bus watch on the
/// same pipeline keeps receiving every message unchanged. Call at most
/// once per pipeline: `Bus::set_sync_handler` panics if a handler is
/// already installed.
#[cfg(feature = "gstreamer")]
pub fn install_shared_display_context(pipeline: &gstreamer::Pipeline) {
    use gstreamer::prelude::*;

    if !shared_contexts_enabled() {
        return;
    }
    let Some(bus) = pipeline.bus() else {
        return;
    };
    bus.set_sync_handler(move |_bus, msg| {
        match msg.view() {
            gstreamer::MessageView::NeedContext(need) => {
                let ctx_type = need.context_type();
                let cached = if context_is_shareable(ctx_type) {
                    SHARED_CONTEXTS
                        .lock()
                        .ok()
                        .and_then(|m| m.get(ctx_type).cloned())
                } else {
                    None
                };
                if let (Some(ctx), Some(src)) = (cached, msg.src()) {
                    if let Some(el) = src.downcast_ref::<gstreamer::Element>() {
                        el.set_context(&ctx);
                    }
                }
            }
            gstreamer::MessageView::HaveContext(have) => {
                let ctx = have.context();
                let ctx_type = ctx.context_type().to_string();
                if !context_is_shareable(&ctx_type) {
                    return gstreamer::BusSyncReply::Pass;
                }
                if let Ok(mut m) = SHARED_CONTEXTS.lock() {
                    // First pipeline to negotiate a given type wins;
                    // later ones are handed this one instead.
                    m.entry(ctx_type).or_insert(ctx);
                }
            }
            _ => {}
        }
        gstreamer::BusSyncReply::Pass
    });
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

    /// Everything the runtime escalation keys off `legacy_vaapipostproc`, so
    /// a stray `true` on any other tier would silently arm the escalation for
    /// hardware that has no `vaapipostproc` problem.
    #[test]
    fn legacy_vaapipostproc_flag_marks_that_tier_and_no_other() {
        let amd_legacy = probe_amd(&["vah264dec", "vapostproc", "vaapih264dec", "vaapipostproc"]);
        assert!(select_decode_chain("h264", DecodeMode::Auto, &amd_legacy).legacy_vaapipostproc);
        assert!(
            select_decode_chain("h265", DecodeMode::Auto, &{
                probe_amd(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"])
            })
            .legacy_vaapipostproc
        );

        for c in [
            // Intel / other non-AMD VA device.
            select_decode_chain("h264", DecodeMode::Auto, &va_full()),
            // AMD system-memory NV12 + CPU-convert fallback.
            select_decode_chain(
                "h264",
                DecodeMode::Auto,
                &probe_amd(&["vah264dec", "vapostproc"]),
            ),
            // MSDK (Intel), NVDEC (CUDA), software.
            select_decode_chain(
                "h264",
                DecodeMode::Msdk,
                &probe(&["msdkh264dec", "msdkvpp"]),
            ),
            select_decode_chain("h264", DecodeMode::Nvdec, &probe(&["nvh264dec"])),
            select_decode_chain("h264", DecodeMode::Software, &va_full()),
        ] {
            assert!(
                !c.legacy_vaapipostproc,
                "non-legacy tier wrongly flagged: {}",
                c.label
            );
        }
    }

    /// The escalation target: same box, same probe, only the wrapper differs.
    /// GPU decode must survive (`hwaccel` stays true and the chain still
    /// starts with a hardware decoder) — this is a post-process demotion, not
    /// a fall to software.
    #[test]
    fn avoid_legacy_vaapipostproc_drops_to_sysmem_cpu_convert_tier() {
        // h265 mirrors the field box (all 53 cameras selected
        // `vaapih265dec+vaapipostproc`).
        let p = probe_amd(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"]);
        let before = select_decode_chain("h265", DecodeMode::Auto, &p);
        assert!(before.legacy_vaapipostproc);

        let after = select_decode_chain("h265", DecodeMode::Auto, &AvoidLegacyVaapiPostproc(&p));
        assert_eq!(after.backend, DecodeBackend::Va);
        assert!(after.hwaccel);
        assert!(!after.legacy_vaapipostproc);
        assert!(!after.elements.contains("vaapipostproc"));
        assert!(!after.elements.contains("vapostproc"));
        assert_eq!(
            after.elements,
            "vah265dec ! video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert"
        );
    }

    #[test]
    fn avoid_legacy_vaapipostproc_drops_to_sysmem_cpu_convert_tier_h264() {
        let p = probe_amd(&["vah264dec", "vapostproc", "vaapih264dec", "vaapipostproc"]);
        let after = select_decode_chain("h264", DecodeMode::Auto, &AvoidLegacyVaapiPostproc(&p));
        assert!(after.hwaccel);
        assert_eq!(
            after.elements,
            "vah264dec ! video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert"
        );
    }

    /// Intel keeps GPU post-process. `vapostproc` is a different plugin from
    /// the shadowed legacy `vaapipostproc`, and an Intel box never latches the
    /// escalation anyway — but if it ever did, nothing may move.
    #[test]
    fn avoid_legacy_vaapipostproc_leaves_intel_untouched() {
        let p = va_full();
        for mode in [DecodeMode::Auto, DecodeMode::Va] {
            let c = select_decode_chain("h264", mode, &AvoidLegacyVaapiPostproc(&p));
            assert_eq!(c.backend, DecodeBackend::Va);
            assert_eq!(
                c.elements,
                "vah264dec ! vapostproc ! videoconvert ! videoscale ! videorate"
            );
        }
    }

    #[test]
    fn avoid_legacy_vaapipostproc_leaves_msdk_and_nvdec_untouched() {
        let msdk = probe(&["msdkh264dec", "msdkvpp", "vah264dec", "vapostproc"]);
        assert_eq!(
            select_decode_chain("h264", DecodeMode::Msdk, &AvoidLegacyVaapiPostproc(&msdk)).backend,
            DecodeBackend::Msdk
        );

        let nv = probe(&["nvh264dec", "nvh265dec"]);
        for (codec, mode) in [("h264", DecodeMode::Nvdec), ("h265", DecodeMode::Auto)] {
            let c = select_decode_chain(codec, mode, &AvoidLegacyVaapiPostproc(&nv));
            assert_eq!(c.backend, DecodeBackend::Nvdec);
            assert!(c.hwaccel);
        }
    }

    /// The wrapper must not invent capabilities either: with no VA plugin at
    /// all it still lands on software rather than emitting a chain whose
    /// elements are not registered.
    #[test]
    fn avoid_legacy_vaapipostproc_still_falls_to_software_when_nothing_present() {
        let p = none();
        let c = select_decode_chain("h264", DecodeMode::Auto, &AvoidLegacyVaapiPostproc(&p));
        assert_eq!(c.backend, DecodeBackend::Software);
        assert!(!c.hwaccel);
    }

    /// A box that has ONLY the legacy vaapi plugin (no `va` plugin) can never
    /// reach the legacy tier in the first place, so escalating there must not
    /// produce a chain referencing absent `vah26Xdec`.
    #[test]
    fn avoid_legacy_vaapipostproc_legacy_only_box_lands_on_software() {
        let p = probe_amd(&["vaapih264dec", "vaapipostproc"]);
        assert_eq!(
            select_decode_chain("h264", DecodeMode::Auto, &p).backend,
            DecodeBackend::Software,
            "legacy-only box never selects the legacy tier (va_available gates it)"
        );
        let c = select_decode_chain("h264", DecodeMode::Auto, &AvoidLegacyVaapiPostproc(&p));
        assert_eq!(c.backend, DecodeBackend::Software);
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

    /// The gap this detector closes: startup-only validation disarms for the
    /// rest of the session on its first non-flat frame, so a decode path
    /// that started healthy and went green later was never re-examined.
    /// Field evidence: eleven cameras sat on a byte-identical all-green
    /// frame indefinitely while both existing guards reported nothing.
    #[test]
    fn flat_detector_still_trips_long_after_a_healthy_start() {
        let mut d = FlatFrameDetector::new();
        for _ in 0..5_000 {
            assert!(!d.observe(false), "healthy video must never trip");
        }
        let trips = (0..FLAT_FRAME_TRIP).any(|_| d.observe(true));
        assert!(trips, "must trip once the picture goes flat, however late");
    }

    #[test]
    fn flat_detector_trips_on_a_broken_from_boot_chain() {
        let mut d = FlatFrameDetector::new();
        let at = (1..=FLAT_FRAME_TRIP).position(|_| d.observe(true));
        assert_eq!(
            at,
            Some(FLAT_FRAME_TRIP as usize - 1),
            "a chain flat from frame 1 must trip on exactly FLAT_FRAME_TRIP frames"
        );
    }

    /// An intermittently-recycled pool lets real frames through between the
    /// unwritten ones. Under a consecutive-run rule any single good frame
    /// reset the count to zero and the fault ran forever.
    #[test]
    fn flat_detector_trips_on_an_intermittent_fault() {
        let mut d = FlatFrameDetector::new();
        let mut tripped = false;
        // 2 flat : 1 good — well inside the window's 30-in-90 bar, and never
        // more than two flat frames in a row.
        for i in 0..FLAT_FRAME_EVAL_WINDOW {
            if d.observe(i % 3 != 2) {
                tripped = true;
                break;
            }
        }
        assert!(tripped, "intermittent flat frames must still trip");
    }

    /// The counterweight: a sparse flat rate must not tear down a working
    /// session, or a dark scene costs the camera its session.
    #[test]
    fn flat_detector_ignores_a_sparse_flat_rate() {
        let mut d = FlatFrameDetector::new();
        for i in 0..10_000 {
            assert!(
                !d.observe(i % 5 == 0),
                "20% flat is under the {FLAT_FRAME_TRIP}-in-{FLAT_FRAME_EVAL_WINDOW} bar"
            );
        }
    }

    #[test]
    fn flat_detector_reports_stats_even_without_tripping() {
        let mut d = FlatFrameDetector::new();
        for i in 0..100 {
            d.observe(i % 10 == 0);
        }
        assert_eq!(d.stats(), (100, 10));
    }

    /// Trips reset, so a caller that declines to act gets a bounded log
    /// rate rather than one error per frame.
    #[test]
    fn flat_detector_resets_after_tripping() {
        let mut d = FlatFrameDetector::new();
        let mut trips = 0;
        for _ in 0..(FLAT_FRAME_TRIP * 3) {
            if d.observe(true) {
                trips += 1;
            }
        }
        assert_eq!(
            trips, 3,
            "one trip per FLAT_FRAME_TRIP flat frames, not per frame"
        );
    }

    /// A permanently-green camera trips the flat guard every
    /// `FLAT_FRAME_TRIP` frames forever. Sharing the loop guard's throttle
    /// is what keeps that from becoming the very rebuild storm BUG-039
    /// fixed: at 15 fps a 100%-flat camera trips every ~2 s, which without
    /// the cooldown is ~150 rebuilds per camera per 5 minutes.
    #[test]
    fn flat_detector_trip_rate_is_survivable_only_with_the_throttle() {
        let mut d = FlatFrameDetector::new();
        let throttle = LoopRebuildThrottle::new();
        let start = Instant::now();
        let mut trips = 0u32;
        let mut rebuilds = 0u32;
        // 5 minutes of a 100%-flat camera at the 15 fps supervisor cap.
        for frame in 0..(15 * 300) {
            if d.observe(true) {
                trips += 1;
                let now = start + Duration::from_millis(frame * 1000 / 15);
                if throttle.allow_rebuild_at(now) {
                    rebuilds += 1;
                }
            }
        }
        assert_eq!(trips, 150, "a fully-green camera trips ~every 2 s");
        assert_eq!(rebuilds, 1, "but the cooldown lets exactly one through");
    }

    /// The field failure this ladder exists for. Measured on San Marcos 1
    /// (`0.1.192`): twelve cameras each tripped exactly once per
    /// `FRAME_LOOP_REBUILD_COOLDOWN` for the whole window and stayed green
    /// throughout — every permitted rebuild was spent on a tier that had
    /// already proved it could not render. Throttling alone turns an
    /// unbounded rebuild storm into a bounded rebuild *loop*, which still
    /// never converges. The ladder must step down instead.
    #[test]
    fn escalation_stops_a_camera_rebuilding_the_same_tier_forever() {
        let esc = DecodeEscalation::new();
        let throttle = LoopRebuildThrottle::new();
        let start = Instant::now();
        let mut rebuilds = 0u32;
        let mut step_downs = 0u32;
        // 25 minutes of a camera that re-trips the moment the cooldown ends.
        for minute in 0..25u64 {
            let now = start + Duration::from_secs(minute * 60);
            if throttle.allow_rebuild_at(now) {
                rebuilds += 1;
                if esc.record_rebuild_at(now) {
                    step_downs += 1;
                }
            }
        }
        assert_eq!(
            rebuilds, 5,
            "one rebuild per 5-minute cooldown, as measured"
        );
        assert_eq!(
            step_downs, 2,
            "the camera steps down a tier instead of spending every rebuild on \
             one that has already failed"
        );
    }

    #[test]
    fn escalation_gives_each_tier_a_full_allowance() {
        let esc = DecodeEscalation::new();
        let start = Instant::now();
        let mut tick = 0u64;
        for tier in 0..3 {
            for _ in 1..DECODE_ESCALATION_LIMIT {
                tick += 60;
                assert!(
                    !esc.record_rebuild_at(start + Duration::from_secs(tick)),
                    "tier {tier} escalated before spending its allowance"
                );
            }
            tick += 60;
            assert!(
                esc.record_rebuild_at(start + Duration::from_secs(tick)),
                "tier {tier} never escalated"
            );
        }
    }

    /// Without lapsing, a camera that trips once every few hours escalates
    /// itself onto software decode over nothing but uptime.
    #[test]
    fn escalation_strikes_lapse_after_a_trip_free_window() {
        let esc = DecodeEscalation::new();
        let t0 = Instant::now();
        assert!(!esc.record_rebuild_at(t0), "one strike must not escalate");
        assert_eq!(esc.strikes(), 1);
        let later = t0 + DECODE_ESCALATION_FORGIVE + Duration::from_secs(1);
        assert!(
            !esc.record_rebuild_at(later),
            "a camera that behaved for the whole window starts clean"
        );
        assert_eq!(esc.strikes(), 1, "the lapsed strike is not still counted");
    }

    /// The box this was measured on runs the engine at ~11 of 16 cores with
    /// every camera already doing CPU colour-convert. Letting all twelve
    /// chronic cameras latch software H.265 would take cores that do not
    /// exist and drag the healthy cameras down too.
    #[test]
    fn software_budget_caps_how_much_of_the_host_goes_to_cpu() {
        let b = SoftwareFallbackBudget::for_cores(16);
        assert_eq!(b.cap(), 4, "a quarter of the cores");
        for _ in 0..4 {
            assert!(b.try_claim());
        }
        assert!(
            !b.try_claim(),
            "the next camera stays on hardware and is reported degraded \
             rather than taking cores the box does not have"
        );
        assert_eq!(b.claimed(), 4);
    }

    #[test]
    fn software_budget_always_lets_one_camera_fall_back() {
        let b = SoftwareFallbackBudget::for_cores(2);
        assert_eq!(b.cap(), 1, "a small box can still fall back");
        assert!(b.try_claim());
        assert!(!b.try_claim());
    }

    #[test]
    fn loop_detector_trips_on_a_fixed_cycle() {
        let mut d = FrameLoopDetector::new();
        let mut tripped = None;
        // `… A B C D E F A B C D E F …` — a 6-deep recycled pool.
        for i in 0..(FRAME_LOOP_TRIP as usize + 12) {
            if let Some(p) = d.observe((i % 6) as u64 + 1) {
                tripped = Some(p);
                break;
            }
        }
        assert_eq!(tripped, Some(6));
    }

    /// The regression this throttle exists for: on a VRAM-constrained box
    /// the guard's rebuild is what manufactures the unwritten (green)
    /// surface, the green frame reads as a duplicate, and the guard trips
    /// again — 1523 trips and 1523 rebuilds in 25 minutes were measured in
    /// the field. The remedy must be rate-limited even when the detector
    /// keeps firing.
    #[test]
    fn rebuild_throttle_allows_one_rebuild_per_cooldown() {
        let t = LoopRebuildThrottle::new();
        let t0 = Instant::now();

        assert!(t.allow_rebuild_at(t0), "first trip must be allowed to act");
        assert!(
            !t.allow_rebuild_at(t0 + Duration::from_secs(1)),
            "a trip one second later must not tear the session down again"
        );
        assert!(
            !t.allow_rebuild_at(t0 + FRAME_LOOP_REBUILD_COOLDOWN - Duration::from_millis(1)),
            "still inside the cooldown"
        );
        assert!(
            t.allow_rebuild_at(t0 + FRAME_LOOP_REBUILD_COOLDOWN),
            "a genuinely wedged decoder must still be rebuilt once the window passes"
        );
        assert_eq!(t.suppressed(), 2, "suppressed trips are counted, not lost");
    }

    /// A storm at the field-measured rate collapses to the cooldown rate.
    #[test]
    fn rebuild_throttle_collapses_a_field_rate_storm() {
        let t = LoopRebuildThrottle::new();
        let t0 = Instant::now();
        // One camera tripped roughly every 52 s for 25 minutes.
        let mut rebuilds = 0;
        for i in 0..29 {
            if t.allow_rebuild_at(t0 + Duration::from_secs(i * 52)) {
                rebuilds += 1;
            }
        }
        // Each grant restarts the window from the granting trip, so grants
        // land on the first trip at or past +300 s: t = 0, 312, 624, 936,
        // 1248. Five rebuilds where the guard asked for 29.
        assert_eq!(
            rebuilds, 5,
            "25 min of trips must collapse to one rebuild per cooldown, not 29"
        );
        assert_eq!(t.suppressed(), 24, "the other 24 trips are counted");
    }

    #[test]
    fn loop_detector_ignores_a_static_scene() {
        // Every frame identical to the one before it: an empty room at
        // night, or `videorate` padding up to the configured framerate.
        // Never a loop, however long it runs.
        let mut d = FrameLoopDetector::new();
        for _ in 0..1000 {
            assert_eq!(d.observe(42), None);
        }
    }

    #[test]
    fn loop_detector_ignores_advancing_video() {
        let mut d = FrameLoopDetector::new();
        for i in 0..1000u64 {
            assert_eq!(d.observe(i), None);
        }
    }

    #[test]
    fn loop_detector_trips_on_an_intermittently_broken_cycle() {
        // A recycled surface pool does not hand back a stale surface every
        // single time — a genuinely fresh frame slips through whenever the
        // race resolves the other way. Under the old consecutive-run rule
        // one such frame reset the counter to zero, so this pattern ran
        // forever without ever tripping.
        let mut d = FrameLoopDetector::new();
        let mut tripped = None;
        'outer: for round in 0..20u64 {
            for i in 0..6u64 {
                if let Some(p) = d.observe(i + 1) {
                    tripped = Some(p);
                    break 'outer;
                }
            }
            if let Some(p) = d.observe(1_000 + round) {
                tripped = Some(p);
                break;
            }
        }
        assert_eq!(tripped, Some(7));
    }

    #[test]
    fn loop_detector_tolerates_sparse_duplicates() {
        // A few repeats scattered through otherwise advancing video — the
        // residual measured on healthy cameras — must never tear a working
        // session down.
        let mut d = FrameLoopDetector::new();
        for i in 0..1_000u64 {
            let fp = if i % 25 == 24 { i - 3 } else { i };
            assert_eq!(d.observe(fp), None);
        }
        let (observed, duplicates) = d.stats();
        assert_eq!(observed, 1_000);
        assert_eq!(duplicates, 40);
    }

    #[test]
    fn fingerprint_distinguishes_single_byte_changes() {
        let a = vec![7u8; 4096];
        let mut b = a.clone();
        b[FINGERPRINT_STRIDE * 3] = 8;
        assert_eq!(frame_fingerprint(&a), frame_fingerprint(&a));
        assert_ne!(frame_fingerprint(&a), frame_fingerprint(&b));
        assert_ne!(frame_fingerprint(&a), frame_fingerprint(&a[..2048]));
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn only_the_gl_display_is_shared() {
        // The GBM/GL display owns the Mesa driver instance + `util_queue`
        // workers — the whole reason this module shares anything.
        assert!(context_is_shareable("gst.gl.GLDisplay"));

        // VA *decoder* displays are NOT shared: N decoders on one
        // `VADisplay` race on its surface bookkeeping and re-serve
        // already-delivered frames on a short rotating cycle while
        // `frame_id` keeps advancing (measured on a 29-camera Intel box;
        // see SHAREABLE_CONTEXT_TYPES).
        assert!(!context_is_shareable("gst.va.display.handle"));
        assert!(!context_is_shareable("gst.vaapi.Display"));

        // A GL context is per-thread; pipeline #2 must make its own.
        assert!(!context_is_shareable("gst.gl.GLContext"));
        assert!(!context_is_shareable("gst.gl.app_context"));
        assert!(!context_is_shareable("gst.gl.local_context"));
    }
}
