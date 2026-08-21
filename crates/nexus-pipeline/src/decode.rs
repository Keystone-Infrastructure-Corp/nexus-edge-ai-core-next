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
//! `AMD_DEBUG=notiling` **is** now shipped, but by the engine rather than by
//! this module: `apply_amd_tiling_workaround` in `nexus-engine`'s `main.rs`
//! sets it before Mesa loads, gated on an AMD GPU being present and no
//! inference execution provider bound to that same iGPU (a Hailo-8 or NPU box
//! pays nothing for linear surfaces). Field-measured on a 53-camera Radeon
//! 680M, it cut green cameras 32% (9.27 → 6.30 mean over 829 samples). Nor is
//! this waiting on a GStreamer bump — GStreamer 1.26.6 / plugins-bad 1.26.5
//! was staged against the same Mesa and its `vapostproc` is byte-identically
//! broken *while tiled*. So on an AMD VA device, in preference order:
//!
//! 1. **Modern `vapostproc` GPU path, when surfaces are linear** — if
//!    `AMD_DEBUG=notiling` is in the environment (see
//!    [`FactoryProbe::amd_linear_surfaces`]): `vah26Xdec ! vapostproc !
//!    videorate ! videoconvert`. The tiling bug above is the *only* thing
//!    wrong with `vapostproc` here; with linear surfaces it is pixel-accurate,
//!    verified by capturing 490 consecutive frames through this exact chain on
//!    the Radeon 680M / Mesa 25.2.8 and finding real imagery throughout
//!    (per-channel spread 250–255) where the tiled configuration yields one
//!    flat colour. Preferred over the legacy tier below because it does not
//!    depend on the `gstreamer1.0-vaapi` plugin being installed at all, and
//!    because that legacy tier was field-measured tripping the frame-loop
//!    guard on every one of 53 cameras within minutes — which, while the
//!    escalation ladder still existed, pushed the whole fleet onto the
//!    CPU-convert fallback below. `videoscale` is OMITTED for the same
//!    reason as the legacy tier: keep the scale on the GPU.
//! 2. **Legacy `gstreamer1.0-vaapi` GPU path** — if `vaapih26Xdec` +
//!    `vaapipostproc` are registered: `vaapih26Xdec ! vaapipostproc !
//!    videorate ! videoconvert`. The OLD `vaapipostproc` does the NV12→RGB
//!    convert + downscale correctly on the GPU on the same Radeon 680M /
//!    Mesa 25.2 where the new `vapostproc` fails *while tiled* (verified with
//!    a live pipeline). `videoscale` is deliberately OMITTED so caps
//!    negotiation is forced to put the scale on `vaapipostproc` (the GPU)
//!    rather than let a downstream CPU `videoscale` claim it; the lone
//!    `videoconvert` is then only a cheap small-frame format bridge (the GPU
//!    has already downscaled to the target width/height). Keeps BOTH decode
//!    and convert/scale on the GPU. `vaapipostproc` runs a GBM/GL probe that
//!    needs `XDG_RUNTIME_DIR` set — the systemd unit provides it via
//!    `RuntimeDirectory=nexus`.
//! 3. **System-memory CPU-convert fallback** — otherwise `vah26Xdec !
//!    video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert`. The
//!    `video/x-raw,format=NV12` carries no `memory:VAMemory`/`memory:DMABuf`
//!    feature, so the decoder downloads each frame to system memory and the
//!    convert/scale runs on the CPU. The tail is ordered `videorate !
//!    videoscale ! videoconvert` so frames are dropped FIRST (cheap, still
//!    NV12), survivors are scaled while still subsampled NV12 (12 bpp,
//!    cheaper than RGB), and the costly NV12→RGB convert runs LAST — on the
//!    already-downscaled, rate-limited stream instead of at full resolution
//!    and full frame rate. This tier is where the whole fleet ended up before
//!    rung 1 existed, and its CPU convert is the bulk of the ~40% system time
//!    measured on the 53-camera box.
//!
//! GPU decode (the expensive part) is preserved in all three. The split is
//! keyed on the DRM vendor via [`FactoryProbe::va_bypass_postproc`] and, for
//! rung 1, on [`FactoryProbe::amd_linear_surfaces`].
//!
//! Because a decoder is chosen on element *presence*, not on whether it
//! actually renders, `preroll_ingester` arms two runtime guards on the RGB
//! tap. They differ in what they are allowed to do, and only one of them
//! acts:
//!
//! * [`FlatFrameDetector`] over [`rgb_frame_looks_degenerate`] — latches
//!   `force_software` and rebuilds **only** for a chain that has never once
//!   rendered a real frame, i.e. one that is simply wrong for this GPU. A
//!   chain that rendered fine and went flat later is reported and left up.
//! * [`FrameLoopDetector`] over [`frame_fingerprint`] — **reports only**.
//!
//! There is deliberately no escalation ladder off a bad decode rung. One
//! existed (`v0.1.189`–`v0.1.194`) and was reverted wholesale in `v0.1.195`:
//! its only remedy was a session rebuild, a rebuild reallocates the VA
//! surface pool, and on a VRAM-constrained box that reallocation is itself
//! what manufactures an unwritten (green) surface — which reads as a
//! duplicate and re-trips the guard. Field-measured at 1523 trips and 1523
//! rebuilds in 25 minutes; rate-limiting it to one rebuild per 5 minutes only
//! converted the storm into a stable limit cycle that never converged. Do not
//! re-introduce a rebuild-based remedy without new evidence that the pool
//! churn has stopped being the dominant cost. See BUG-039, BUG-065 and
//! BUG-071 in the engineering vault.
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

    /// Whether Mesa is allocating **linear** (untiled) surfaces this process,
    /// i.e. `AMD_DEBUG=notiling` is in the environment.
    ///
    /// The `vapostproc` breakage this module documents is a *tiling* bug: the
    /// VPP output surface is allocated tiled and read back as linear. With
    /// linear surfaces the element is pixel-accurate — verified on the same
    /// Radeon 680M / Mesa 25.2.8 that motivated the bypass, capturing 490
    /// consecutive frames through `vah265dec ! vapostproc` and finding real
    /// imagery throughout (per-channel spread 250–255), where the tiled
    /// configuration yields a single flat colour.
    fn amd_linear_surfaces(&self) -> bool {
        std::env::var("AMD_DEBUG").is_ok_and(|v| v.split(',').any(|tok| tok.trim() == "notiling"))
    }
}

/// The chosen decode + post-process fragment plus metadata for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeChain {
    /// GStreamer element fragment from the decoder through the
    /// convert/scale/rate tail, e.g.
    /// `vah264dec ! vapostproc ! videorate ! videoconvert`.
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
        elements: format!("{dec} name={DECODER_NAME} ! {CPU_TAIL}"),
        backend: DecodeBackend::Software,
        hwaccel: false,
        label: format!("software ({dec})"),
    }
}

fn va_chain(
    codec_base: &str,
    bypass_postproc: bool,
    amd_vaapi_gpu: bool,
    amd_linear_surfaces: bool,
) -> DecodeChain {
    if !bypass_postproc {
        // Intel (and any other non-AMD VA device): `vapostproc` does GPU
        // colour-convert + scale correctly, so keep it. `videoscale` is
        // omitted for the same reason as the AMD tiers — with it present,
        // negotiation lets a CPU `videoscale`/`videoconvert` claim the
        // convert+downscale instead of pushing them onto `vapostproc`, and
        // liborc then burns the box doing in software what the VPP block
        // does for free (BUG-122).
        let dec = va_decoder(codec_base);
        return DecodeChain {
            elements: format!("{dec} name={DECODER_NAME} ! vapostproc ! {GPU_SCALED_TAIL}"),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}+vapostproc)"),
        };
    }

    // AMD radeonsi: the new `vapostproc` renders all-green frames regardless
    // of its output caps (verified on a Radeon 680M / gfx1035, Mesa 25.2) --
    // but only while surfaces are TILED. Under `AMD_DEBUG=notiling` the same
    // element is pixel-accurate, so prefer it: it keeps convert + downscale on
    // the GPU without depending on the legacy `gstreamer1.0-vaapi` plugin,
    // whose `vaapipostproc` tier was field-measured tripping the frame-loop
    // guard on every camera within minutes and pushing the whole fleet onto
    // the CPU-convert fallback below. `videoscale` is omitted so caps
    // negotiation puts the scale on `vapostproc` (GPU) rather than a
    // downstream CPU `videoscale`.
    if amd_linear_surfaces {
        let dec = va_decoder(codec_base);
        return DecodeChain {
            elements: format!("{dec} name={DECODER_NAME} ! vapostproc ! {GPU_SCALED_TAIL}"),
            backend: DecodeBackend::Va,
            hwaccel: true,
            label: format!("va ({dec}+vapostproc, gpu convert, linear surfaces)"),
        };
    }

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
            elements: format!("{dec} name={DECODER_NAME} ! vaapipostproc ! {GPU_SCALED_TAIL}"),
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
        elements: format!("{dec} name={DECODER_NAME} ! video/x-raw,format=NV12 ! {CPU_TAIL}"),
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
    va_chain(
        codec_base,
        bypass,
        amd_vaapi_gpu,
        bypass && probe.amd_linear_surfaces(),
    )
}

/// Element name of the decoder, first element of every chain.
///
/// The ingester probes this element's src pad to count what the decoder
/// actually produced. That number cannot be recovered further downstream:
/// every chain ends in a `videorate` that pads a starved decoder back up to
/// the requested framerate by duplicating buffers, so a count taken at the
/// appsink reads a flat nominal rate no matter how little the hardware
/// managed (BUG-071).
pub const DECODER_NAME: &str = "vdec";

/// CPU tail for every chain that hands the supervisor a system-memory frame.
///
/// Ordered drop → scale → convert on purpose: `videorate` sheds frames while
/// they are still cheap, `videoscale` shrinks them while still subsampled
/// (12 bpp), and the per-pixel colour conversion runs last, on the
/// already-downscaled and rate-limited stream. Converting first — the shape
/// five of these six chains shipped with — costs `source_px / supervisor_px`
/// times more work per frame (6.25× at 1280×720 → 512×288) and allocates
/// full-size RGB buffers the kernel then has to zero. See BUG-109.
pub(crate) const CPU_TAIL: &str = "videorate ! videoscale ! videoconvert";

/// Tail for chains whose GPU post-processor has already scaled to the target
/// size. `videoscale` is deliberately absent so caps negotiation cannot pull
/// the downscale back onto the CPU; the trailing `videoconvert` is only a
/// small-frame format bridge. Rate-limiting still leads, so even that bridge
/// runs on the reduced frame rate.
const GPU_SCALED_TAIL: &str = "videorate ! videoconvert";

/// Element name of the RGB tap's decoder-input queue.
///
/// The ingester looks this element up to count its `overrun` signal, one
/// emission per leaked buffer. Because the queue sits between the parser and
/// the decode chain, its buffers are compressed access units — so a leak here
/// is a *bitstream* loss, not a dropped frame: the decoder never sees that
/// access unit and every picture until the next IDR carries the damage. The
/// counter is the only evidence that is happening, so [`rgb_tap_branch`] and
/// the lookup share this constant rather than two string literals.
pub const RGB_TAP_QUEUE_NAME: &str = "rgbq";

/// Time cap on the decoder-input queue, in nanoseconds.
///
/// Bounds how far the analysis branch's wall clock can drift behind the
/// recorder branch off the same tee. Must stay under the smallest alert-clip
/// pre-roll (`alert_clip.pre_secs`, default 3 s) or clips are cut past the
/// event they describe.
pub const RGB_TAP_QUEUE_MAX_TIME_NS: u64 = 2_000_000_000;

/// The RGB tap branch of the pre-roll ingest pipeline, from the `tee` to the
/// appsink.
///
/// `queue` is `leaky=downstream` so a slow decoder drops the oldest queued
/// access unit instead of stalling the shared upstream parser — which would
/// also stall the recorder's lossless branch off the same tee. The appsink
/// then drops *decoded* frames (`drop=true max-buffers=4`) when the
/// supervisor is the slow one; that drop is harmless, the queue's is not.
///
/// Depth is sized so ordinary jitter never reaches that leak. At 8 buffers it
/// did, and the leak was not survivable: `vah264dec`/`vapostproc` answer a
/// gapped bitstream with surfaces they never write, so six of ten cameras on an
/// Apollo Lake box served all-zero frames while the GPU stayed fully idle
/// (BUG-119). A full queue still leaks by design — the counter on
/// [`RGB_TAP_QUEUE_NAME`] is what reports it.
///
/// The binding cap is `max-size-time`, not the buffer count, because this queue
/// sits *upstream* of the decoder while `videorate` sits downstream in the tail:
/// it therefore fills at the camera's native rate, not the supervisor cap, so a
/// count means a different duration on every camera. Duration is what has to be
/// bounded, because [`crate::preroll_ingester`] stamps `Frame::captured_at` at
/// appsink delivery — queue latency lands in the analysis branch's wall clock
/// while the recorder branch off the same tee keeps stream time. Let that skew
/// exceed the alert clip's pre-roll and clips are cut past the event with boxes
/// drawn on unrelated frames. Two seconds keeps it inside both the 3 s
/// `alert_clip.pre_secs` and the 5 s `pre_roll_secs` default; the buffer count
/// is only a backstop for pathological frame rates.
#[must_use]
pub fn rgb_tap_branch(decode_chain: &str, width: u32, height: u32, framerate: u32) -> String {
    format!(
        "t. ! queue name={RGB_TAP_QUEUE_NAME} leaky=downstream max-size-buffers=200 \
            max-size-bytes=0 max-size-time={RGB_TAP_QUEUE_MAX_TIME_NS} \
         ! {decode_chain} \
         ! video/x-raw,format=RGB,width={width},height={height},framerate={framerate}/1 \
         ! appsink name=rgb emit-signals=true sync=false drop=true max-buffers=4"
    )
}

fn msdk_chain(codec_base: &str) -> DecodeChain {
    let dec = msdk_decoder(codec_base);
    DecodeChain {
        elements: format!("{dec} name={DECODER_NAME} ! msdkvpp ! {CPU_TAIL}"),
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
        elements: format!("{dec} name={DECODER_NAME} ! {CPU_TAIL}"),
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

/// Guard trips a session that has *already* rendered real video may
/// take before the ladder escalates it to software decode regardless.
///
/// Each trip is [`FLAT_FRAME_TRIP`] flat frames inside
/// [`FLAT_FRAME_EVAL_WINDOW`] and the detector resets after every one,
/// so five of them is roughly a minute of continuously wrong picture —
/// far past anything a transient surface-pool hiccup produces.
pub const FLAT_FRAME_TERMINAL_TRIPS: u32 = 5;

/// The last rung of the decode-health ladder.
///
/// [`FlatFrameDetector`] answers "is this session flat *right now*".
/// This answers the different question "has it been flat long enough
/// that leaving it up is worse than rebuilding it", and the two have
/// different costs: reporting is free, rebuilding churns the decoder's
/// surface pool. That asymmetry is why a validated session is left
/// alone at first — rebuilding a working chain under load manufactured
/// green frames and recovered nothing (BUG-065).
///
/// Leaving it alone *forever* is the opposite failure, and the more
/// expensive one: a camera that rendered one good frame at startup and
/// then went blank could never reach the software-decode remedy, and
/// stayed blind for two days while every liveness signal read healthy
/// (BUG-121). A ladder needs a last rung, not a dead end.
///
/// The rung is terminal because the caller stops arming it once the
/// camera-level software latch is set — not because software decode is
/// assumed to render. It sometimes does not: BUG-065 measured cameras
/// still serving degenerate frames on `avdec_h265` after escalating.
/// Since this counter is per session and the latch outlives it, a
/// session that stays flat on the software chain would otherwise re-arm
/// the rung on every rebuild and churn the decoder forever — the exact
/// BUG-065 loop.
#[derive(Debug, Default)]
pub struct TerminalRung {
    trips: u32,
}

impl TerminalRung {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one guard trip against an already-validated session.
    ///
    /// Returns `true` exactly once — on the trip that exhausts
    /// [`FLAT_FRAME_TERMINAL_TRIPS`] — so the caller escalates and logs
    /// a single time no matter how long the session lingers afterwards.
    ///
    /// `software_already_forced` is the camera-level latch. A session
    /// that is still flat *after* escalating has nowhere left to go, so
    /// re-escalating only rebuilds a decoder that is already on its last
    /// rung.
    pub fn trip(&mut self, software_already_forced: bool) -> bool {
        if software_already_forced {
            return false;
        }
        self.trips = self.trips.saturating_add(1);
        self.trips == FLAT_FRAME_TERMINAL_TRIPS
    }
}

/// How many recent frame fingerprints [`FrameLoopDetector`] keeps. Must
/// exceed the deepest surface/buffer pool we expect to cycle through;
/// observed depths in the field were 4–6, and the RGB branch's decoded side
/// only carries `appsink max-buffers=4`. The decoder-input queue ahead of it
/// holds compressed access units, not surfaces, so its depth does not bound
/// this window.
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
    /// controllable `va_bypass_postproc` (AMD-radeonsi) flag and an explicit
    /// linear-surfaces flag, so no test ever reads the ambient `AMD_DEBUG`.
    struct SetProbe {
        factories: HashSet<&'static str>,
        bypass_postproc: bool,
        linear_surfaces: bool,
    }

    impl FactoryProbe for SetProbe {
        fn has(&self, factory_name: &str) -> bool {
            self.factories.contains(factory_name)
        }
        fn va_bypass_postproc(&self) -> bool {
            self.bypass_postproc
        }
        fn amd_linear_surfaces(&self) -> bool {
            self.linear_surfaces
        }
    }

    fn probe(names: &[&'static str]) -> SetProbe {
        SetProbe {
            factories: names.iter().copied().collect(),
            bypass_postproc: false,
            linear_surfaces: false,
        }
    }

    /// Like [`probe`] but simulates an AMD radeonsi VA device, i.e. one that
    /// must bypass the (broken) `vapostproc`.
    fn probe_amd(names: &[&'static str]) -> SetProbe {
        SetProbe {
            factories: names.iter().copied().collect(),
            bypass_postproc: true,
            linear_surfaces: false,
        }
    }

    /// An AMD radeonsi device running with `AMD_DEBUG=notiling`, where the
    /// modern `vapostproc` is pixel-accurate.
    fn probe_amd_linear(names: &[&'static str]) -> SetProbe {
        SetProbe {
            factories: names.iter().copied().collect(),
            bypass_postproc: true,
            linear_surfaces: true,
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
            "avdec_h264 name=vdec ! videorate ! videoscale ! videoconvert"
        );
        assert!(!c.downgraded_from(DecodeMode::Software));
    }

    #[test]
    fn software_mode_h265() {
        let c = select_decode_chain("h265", DecodeMode::Software, &va_full());
        assert!(c.elements.starts_with("avdec_h265 name=vdec !"));
    }

    #[test]
    fn auto_picks_va_when_available() {
        // Non-AMD VA device (e.g. Intel): keep vapostproc (GPU postproc).
        let c = select_decode_chain("h264", DecodeMode::Auto, &va_full());
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.hwaccel);
        assert_eq!(
            c.elements,
            "vah264dec name=vdec ! vapostproc ! videorate ! videoconvert"
        );
    }

    #[test]
    fn auto_picks_va_h265() {
        let c = select_decode_chain("h265", DecodeMode::Auto, &va_full());
        assert_eq!(c.backend, DecodeBackend::Va);
        assert!(c.elements.starts_with("vah265dec name=vdec ! vapostproc !"));
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
            "vah264dec name=vdec ! video/x-raw,format=NV12 ! videorate ! videoscale ! videoconvert"
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
            .starts_with("vah265dec name=vdec ! video/x-raw,format=NV12 !"));
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
            "vaapih264dec name=vdec ! vaapipostproc ! videorate ! videoconvert"
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
            "vaapih265dec name=vdec ! vaapipostproc ! videorate ! videoconvert"
        );
    }

    /// The tiling bug is the only thing wrong with `vapostproc` on AMD, so
    /// with `AMD_DEBUG=notiling` it is preferred over the legacy tier — which
    /// was field-measured tripping the frame-loop guard on all 53 cameras and
    /// pushing the fleet onto CPU convert. Verified against real hardware by
    /// capturing 490 frames through this chain (spread 250–255 throughout).
    #[test]
    fn amd_with_linear_surfaces_prefers_modern_vapostproc() {
        let p = probe_amd_linear(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"]);
        let c = select_decode_chain("h265", DecodeMode::Auto, &p);
        assert_eq!(
            c.elements,
            "vah265dec name=vdec ! vapostproc ! videorate ! videoconvert"
        );
        assert!(c.hwaccel);
        assert!(
            !c.elements.contains("videoscale"),
            "videoscale is omitted so the GPU keeps the downscale"
        );
    }

    /// Without the flag the tiled `vapostproc` really is broken, so AMD must
    /// still fall to the legacy tier rather than rendering all-green.
    #[test]
    fn amd_without_linear_surfaces_still_avoids_modern_vapostproc() {
        let p = probe_amd(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"]);
        let c = select_decode_chain("h265", DecodeMode::Auto, &p);
        assert!(
            !c.elements.contains("! vapostproc"),
            "tiled vapostproc emits all-green and must not be selected: {}",
            c.elements
        );
        assert!(c.elements.contains("vaapipostproc"));
    }

    /// Intel is unaffected by the AMD-only rung.
    #[test]
    fn intel_keeps_its_vapostproc_chain_regardless_of_linear_surfaces() {
        let c = select_decode_chain("h264", DecodeMode::Auto, &va_full());
        assert_eq!(
            c.elements,
            "vah264dec name=vdec ! vapostproc ! videorate ! videoconvert"
        );
    }

    /// With `videoscale` present, caps negotiation let a CPU
    /// `videoscale`/`videoconvert` pair claim the convert+downscale instead
    /// of `vapostproc`, and liborc burned ~90% of the box doing in software
    /// what the VPP block does for free. The element's absence is the fix,
    /// so pin the absence (BUG-122).
    #[test]
    fn intel_va_chain_leaves_no_videoscale_for_the_cpu_to_claim() {
        for codec in ["h264", "h265"] {
            let c = select_decode_chain(codec, DecodeMode::Auto, &va_full());
            assert!(
                c.elements.contains("vapostproc"),
                "expected the GPU post-processor in {}",
                c.elements
            );
            assert!(
                !c.elements.contains("videoscale"),
                "videoscale lets the CPU claim the downscale off vapostproc: {}",
                c.elements
            );
        }
    }

    /// Colour conversion is the costliest element in every tail, so it must run
    /// LAST — after `videorate` has shed frames and `videoscale` has shrunk the
    /// survivors while they are still subsampled. Five of the six chains shipped
    /// the opposite order and converted at full source resolution, which on a
    /// 1280x720 source feeding a 512x288 supervisor frame is 6.25x the pixel
    /// work per frame and measured ~35% of all CPU on a J3455 (BUG-109).
    ///
    /// Asserted across every tier at once, so a newly added chain cannot
    /// reintroduce it in just one arm the way this defect originally spread.
    #[test]
    fn every_chain_converts_last() {
        let amd_sysmem = probe_amd(&["vah264dec", "vapostproc"]);
        let amd_legacy = probe_amd(&["vah264dec", "vapostproc", "vaapih264dec", "vaapipostproc"]);
        let amd_linear = probe_amd_linear(&["vah264dec", "vapostproc"]);
        let nvdec_only = probe(&["nvh264dec"]);
        let msdk_only = probe(&["msdkh264dec", "msdkvpp"]);

        let chains = [
            select_decode_chain("h264", DecodeMode::Software, &none()),
            select_decode_chain("h264", DecodeMode::Auto, &va_full()),
            select_decode_chain("h264", DecodeMode::Auto, &amd_sysmem),
            select_decode_chain("h264", DecodeMode::Auto, &amd_legacy),
            select_decode_chain("h264", DecodeMode::Auto, &amd_linear),
            select_decode_chain("h264", DecodeMode::Nvdec, &nvdec_only),
            select_decode_chain("h264", DecodeMode::Msdk, &msdk_only),
        ];

        for c in chains {
            let convert_at = c
                .elements
                .find("videoconvert")
                .unwrap_or_else(|| panic!("no videoconvert in {}", c.elements));
            for cheaper in ["videorate", "videoscale"] {
                if let Some(at) = c.elements.find(cheaper) {
                    assert!(
                        at < convert_at,
                        "{cheaper} must precede videoconvert in {}",
                        c.elements
                    );
                }
            }
        }
    }

    /// The leak counter finds its queue with [`RGB_TAP_QUEUE_NAME`], so the
    /// branch must actually name it. A `queue` with no `name=` is invisible
    /// to `by_name` and the counter silently reads zero forever — which is
    /// indistinguishable from a healthy decoder, and is exactly the blindness
    /// BUG-071 was opened against.
    #[test]
    fn rgb_tap_branch_names_the_queue_the_leak_counter_looks_up() {
        let d = rgb_tap_branch(
            "vah265dec name=vdec ! vapostproc ! videoconvert ! videorate",
            512,
            288,
            15,
        );
        assert!(
            d.contains(&format!("queue name={RGB_TAP_QUEUE_NAME} ")),
            "decoder-input queue must be named for by_name lookup: {d}"
        );
    }

    /// `decoder_input_drops` only means "compressed access units lost" while
    /// the queue is upstream of the decoder. Move it downstream and the same
    /// counter starts reporting harmless decoded-frame drops under a field
    /// name that says otherwise.
    #[test]
    fn rgb_tap_branch_keeps_the_counted_queue_ahead_of_the_decoder() {
        let chain = "vah265dec ! vapostproc ! videoconvert ! videorate";
        let d = rgb_tap_branch(chain, 512, 288, 15);
        let queue_at = d.find("queue name=").expect("named queue present");
        let decoder_at = d.find(chain).expect("decode chain present");
        assert!(
            queue_at < decoder_at,
            "the counted queue must sit between the parser and the decoder: {d}"
        );
    }

    /// A non-leaky queue never emits `overrun`, so the counter would read
    /// zero while the tee blocked instead — a different failure with the
    /// same silent telemetry.
    #[test]
    fn rgb_tap_branch_leaks_downstream() {
        let d = rgb_tap_branch(
            "avdec_h265 name=vdec ! videoconvert ! videoscale ! videorate",
            1024,
            576,
            8,
        );
        assert!(d.contains("leaky=downstream"), "{d}");
        assert!(
            d.contains("video/x-raw,format=RGB,width=1024,height=576,framerate=8/1"),
            "{d}"
        );
        assert!(d.starts_with("t. !"), "branch must hang off the tee: {d}");
    }

    /// The depth of the counted queue is not a tuning knob. At 8 buffers it
    /// leaked under ordinary jitter, and a leaked access unit is not a dropped
    /// frame — `vah264dec`/`vapostproc` answer the resulting gap with surfaces
    /// they never write, so the tap serves all-zero frames while the GPU sits
    /// idle and every log still reports `hwaccel=true` (BUG-119).
    #[test]
    fn rgb_tap_branch_queue_is_deep_enough_that_jitter_does_not_leak() {
        let d = rgb_tap_branch(
            "vah264dec name=vdec ! vapostproc ! videoconvert",
            512,
            288,
            15,
        );
        let depth: usize = d
            .split("max-size-buffers=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("decoder-input queue must declare max-size-buffers");
        assert!(
            depth >= 64,
            "decoder-input queue carries compressed access units; a shallow \
             leaky queue corrupts every picture to the next IDR: {d}"
        );
    }

    /// Count is the backstop; duration is the real bound. The queue fills at
    /// the camera's native rate (`videorate` is downstream of the decoder), and
    /// its latency lands in `Frame::captured_at`, which is stamped at appsink
    /// delivery. The recorder branch off the same tee keeps stream time, so
    /// this queue's depth *is* the skew between them — and a skew past the
    /// alert clip's pre-roll cuts clips on footage that postdates the event,
    /// with boxes drawn on unrelated frames.
    #[test]
    fn rgb_tap_branch_queue_latency_stays_inside_the_alert_pre_roll() {
        let d = rgb_tap_branch(
            "vah264dec name=vdec ! vapostproc ! videoconvert",
            512,
            288,
            15,
        );
        assert!(
            d.contains(&format!("max-size-time={RGB_TAP_QUEUE_MAX_TIME_NS}")),
            "decoder-input queue must be time-bounded, not count-bounded: {d}"
        );
        let ns: u64 = d
            .split("max-size-time=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("decoder-input queue must declare max-size-time");
        assert!(
            ns > 0 && ns < 3_000_000_000,
            "queue latency is the analysis/recorder branch skew and must stay \
             under the 3s alert_clip.pre_secs default: {d}"
        );
    }

    /// The decoder-output probe finds its element with [`DECODER_NAME`], so
    /// every chain has to name it. Without the name the probe silently never
    /// attaches and decode throughput reads zero, which is indistinguishable
    /// from a dead camera.
    #[test]
    fn every_chain_names_the_decoder_for_the_output_probe() {
        let probes: Vec<DecodeChain> = vec![
            select_decode_chain("h264", DecodeMode::Software, &none()),
            select_decode_chain("h265", DecodeMode::Auto, &va_full()),
            select_decode_chain(
                "h265",
                DecodeMode::Auto,
                &probe_amd_linear(&["vah265dec", "vapostproc"]),
            ),
            select_decode_chain(
                "h265",
                DecodeMode::Auto,
                &probe_amd(&["vah265dec", "vapostproc", "vaapih265dec", "vaapipostproc"]),
            ),
            select_decode_chain(
                "h265",
                DecodeMode::Auto,
                &probe_amd(&["vah265dec", "vapostproc"]),
            ),
            select_decode_chain(
                "h264",
                DecodeMode::Msdk,
                &probe(&["msdkh264dec", "msdkvpp"]),
            ),
            select_decode_chain("h264", DecodeMode::Nvdec, &probe(&["nvh264dec"])),
        ];
        for c in probes {
            assert!(
                c.elements.contains(&format!("name={DECODER_NAME} ")),
                "chain must name its decoder: {}",
                c.elements
            );
            assert!(
                c.elements
                    .starts_with(&c.elements[..c.elements.find(' ').unwrap_or(0)]),
                "decoder must lead the chain: {}",
                c.elements
            );
        }
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
            "nvh264dec name=vdec ! videorate ! videoscale ! videoconvert"
        );
        assert!(!c.downgraded_from(DecodeMode::Nvdec));
    }

    #[test]
    fn nvdec_request_selects_nvcodec_h265() {
        let c = select_decode_chain("h265", DecodeMode::Nvdec, &probe(&["nvh265dec"]));
        assert_eq!(c.backend, DecodeBackend::Nvdec);
        assert_eq!(
            c.elements,
            "nvh265dec name=vdec ! videorate ! videoscale ! videoconvert"
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
        assert!(c.elements.starts_with("avdec_h264 name=vdec !"));
    }

    #[test]
    fn terminal_rung_holds_until_the_blankness_is_sustained() {
        let mut rung = TerminalRung::new();
        for trip in 1..FLAT_FRAME_TERMINAL_TRIPS {
            assert!(
                !rung.trip(false),
                "escalated on trip {trip}, before the session had been flat long enough"
            );
        }
        assert!(
            rung.trip(false),
            "never escalated, so a validated session that goes blank stays blank forever"
        );
    }

    #[test]
    fn terminal_rung_escalates_exactly_once() {
        let mut rung = TerminalRung::new();
        for _ in 1..FLAT_FRAME_TERMINAL_TRIPS {
            rung.trip(false);
        }
        assert!(rung.trip(false), "expected the escalating trip");
        for _ in 0..10 {
            assert!(!rung.trip(false), "escalated more than once");
        }
    }

    /// The session-level counter resets on every rebuild while the
    /// camera-level software latch does not, so a session that stays
    /// flat on the software chain would re-arm a fresh rung and rebuild
    /// the decoder every time — the BUG-065 churn loop this rung is
    /// supposed to terminate.
    #[test]
    fn terminal_rung_never_re_arms_once_software_is_forced() {
        let mut rung = TerminalRung::new();
        for _ in 0..FLAT_FRAME_TERMINAL_TRIPS * 3 {
            assert!(
                !rung.trip(true),
                "escalated a session that is already on software decode, which rebuilds it forever"
            );
        }
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
