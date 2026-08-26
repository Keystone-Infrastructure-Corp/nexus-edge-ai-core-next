//! Phase 10 (Phase E) — WebRTC HD sub-pipeline (`webrtcbin`).
//!
//! The live wall paints every cell from the always-on LBR JPEG pump
//! (Phase B/C/D). When an operator expands one camera to the solo (1×1)
//! view, the cloud negotiates a **WebRTC** peer connection so that single
//! camera streams at full HD. This module owns the edge half of that
//! connection: a per-session GStreamer pipeline that **passes the camera's
//! already-compressed stream straight through** to a browser — no decode,
//! no re-encode in the common path.
//!
//! ```text
//!   PreRollIngester (compressed H.264/H.265 NAL broadcast)
//!        │  subscribe()  →  broadcast::Receiver<NalSample>
//!        ▼
//!   appsrc ! h264parse ! rtph264pay ! application/x-rtp,… ! webrtcbin
//!            (or h265parse / rtph265pay for HEVC cameras)
//! ```
//!
//! The edge is the **answerer**: the browser mints the SDP offer (relayed
//! by the cloud over the tunnel — wired in Phase F), the edge answers, and
//! ICE candidates trickle both ways. The m-line the edge sends is
//! send-only (the browser is `recvonly`); audio is deferred to Phase I.
//!
//! **Keyframe on join.** Camera GOPs are 2–4 s, so a fresh subscriber that
//! started mid-GOP would show a decode-artefact smear until the next IDR.
//! The feed **splices in at the next keyframe** — it drops delta frames until
//! it sees an IDR, then starts pushing. On the transcode path, browser PLI/FIR
//! keyframe requests are additionally forwarded to the encoder (see
//! [`wire_keyframe_on_pli`]) so a mid-stream reference loss recovers on demand
//! rather than waiting a full GOP. That guarantees the browser's decoder begins
//! on a clean intra frame.
//!
//! **Transcode fallback** (HEVC camera + non-HEVC browser) is an exception
//! path documented in the plan; Phase E ships passthrough only and returns
//! [`WebRtcError::Unsupported`] for [`WebRtcMode::Transcode`].
//!
//! This module is gated behind the `gstreamer-webrtc` Cargo feature, which
//! pulls in the `-webrtc` (gst-plugins-bad) and `-sdp` (gst-plugins-base)
//! bindings. It builds only where those dev headers exist (the Linux CI
//! `system-libs` job or a local GStreamer install), never on the default
//! macOS build.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use gstreamer_rtp as gst_rtp;
use gstreamer_rtp::prelude::RTPHeaderExtensionExt;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;

use nexus_types::{CameraId, CodecKind};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use std::sync::Arc;
use std::time::Duration;

use crate::preroll::NalSample;

/// Errors from building or driving a [`WebRtcSession`].
#[derive(Debug, Error)]
pub enum WebRtcError {
    /// `gst::init()` failed (GStreamer not initialised / missing plugins).
    #[error("gstreamer init: {0}")]
    Init(String),
    /// The sub-pipeline could not be constructed (missing `webrtcbin`, a
    /// bad launch string, a failed downcast, …).
    #[error("build webrtc pipeline: {0}")]
    Build(String),
    /// SDP parse / serialize failure.
    #[error("sdp: {0}")]
    Sdp(String),
    /// A pipeline state transition failed.
    #[error("state: {0}")]
    State(String),
    /// The requested mode is not implemented in this phase.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// How the edge should encode the outbound stream for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRtcMode {
    /// Send the camera's native codec straight through (no transcode).
    /// The only mode implemented in Phase E.
    Passthrough,
    /// Transcode to H.264 for browsers that can't decode the native
    /// codec (HEVC camera + non-HEVC Chromium). Exception path — not
    /// implemented in Phase E.
    Transcode,
}

/// One ICE server (STUN or TURN) the session should offer to the peer.
/// Mirrors the wire `IceServer` the cloud injects on `webrtc_offer`
/// (STUN + HMAC-credentialed TURN from Phase G).
#[derive(Debug, Clone)]
pub struct IceServerCfg {
    /// One or more `stun:` / `turn:` / `turns:` URLs.
    pub urls: Vec<String>,
    /// TURN long-term-credential username (ignored for STUN).
    pub username: Option<String>,
    /// TURN long-term-credential secret (ignored for STUN).
    pub credential: Option<String>,
}

/// An outbound signalling artefact produced by the edge publisher. The caller
/// (the signalling bridge) forwards these to the cloud: `Offer` →
/// `live_hd_offer`, `Connected` → `live_hd_publishing`.
#[derive(Debug, Clone)]
pub enum WebRtcEvent {
    /// The local SDP **offer** is ready (publisher / offerer role), with the
    /// gathered ICE candidates already baked into the SDP. The caller
    /// forwards it to the SFU as `live_hd_offer`.
    Offer {
        /// The offer SDP text (ICE candidates baked in).
        sdp: String,
        /// Negotiated video codec label (`"h264"` / `"h265"`).
        codec: &'static str,
    },
    /// The peer connection reached `connected` (publisher role) — the edge
    /// is now streaming media to the SFU. The caller forwards this as
    /// `live_hd_publishing`.
    Connected,
    /// The session failed while negotiating; the caller should tear down.
    Failed(String),
}

/// A single live WebRTC HD session for one `(session, camera)`.
///
/// Dropping the session tears the pipeline down and stops the NAL feed.
pub struct WebRtcSession {
    session_id: String,
    camera_id: CameraId,
    codec: CodecKind,
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// Kept alive for the session's lifetime; the feed task holds a clone.
    _appsrc: AppSrc,
    events: mpsc::UnboundedSender<WebRtcEvent>,
    feed: JoinHandle<()>,
    /// Adaptive-bitrate control loop (transcode sessions only); aborted on drop.
    cc: Option<JoinHandle<()>>,
    /// Weak handle to the `rtpgccbwe` pacer once negotiation splices it in
    /// (transcode + plugin present only). Held so a cloud-relayed
    /// `live_hd_bitrate` can clamp its `max-bitrate` at runtime to the slowest
    /// browser viewer's measured downlink — the raw SFU never relays that
    /// estimate back to the publisher, so this is the only end-to-end feedback
    /// path. `None`/empty for passthrough, the AIMD fallback, or before the
    /// aux-sender request fires.
    gcc: Arc<parking_lot::Mutex<Option<gst::glib::WeakRef<gst::Element>>>>,
}

impl WebRtcSession {
    /// Build a **publisher** (offerer) session that streams the camera
    /// send-only to an SFU.
    ///
    /// Unlike [`WebRtcSession::new`], the edge creates the SDP offer, waits
    /// for ICE gathering to complete (the SFU's `tracks/new` is a single
    /// request — no trickle), and emits [`WebRtcEvent::Offer`] with the
    /// candidates baked into the SDP. The SFU's answer arrives via
    /// [`WebRtcSession::set_answer`], and once the peer connection is up the
    /// session emits [`WebRtcEvent::Connected`].
    pub fn new_publisher(
        session_id: String,
        camera_id: CameraId,
        codec: CodecKind,
        mode: WebRtcMode,
        ice_servers: &[IceServerCfg],
        nal_rx: broadcast::Receiver<NalSample>,
        events: mpsc::UnboundedSender<WebRtcEvent>,
    ) -> Result<Self, WebRtcError> {
        let sess = Self::build_common(
            session_id,
            camera_id,
            codec,
            mode,
            ice_servers,
            nal_rx,
            events,
        )?;
        sess.wire_offerer();
        // Bring the pipeline up so `webrtcbin` opens its peer-connection,
        // fires `on-negotiation-needed`, and begins gathering ICE.
        sess.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| WebRtcError::State(format!("set Playing: {e}")))?;
        Ok(sess)
    }

    /// Shared pipeline construction for both roles. Builds the passthrough
    /// pipeline, applies ICE servers, and starts the NAL feed. The caller
    /// wires the role-specific signalling afterwards.
    fn build_common(
        session_id: String,
        camera_id: CameraId,
        codec: CodecKind,
        mode: WebRtcMode,
        ice_servers: &[IceServerCfg],
        nal_rx: broadcast::Receiver<NalSample>,
        events: mpsc::UnboundedSender<WebRtcEvent>,
    ) -> Result<Self, WebRtcError> {
        gst::init().map_err(|e| WebRtcError::Init(e.to_string()))?;

        // Choose the media pipeline. Prefer a short-GOP hardware **transcode**
        // (HW-decode the camera codec → re-encode H.264 with a ~2s keyframe
        // interval, CBR, no B-frames): a long-GOP camera stream stalls for a
        // full GOP after any packet loss on the UDP WebRTC path, whereas a
        // short GOP recovers within ~1-2s. Falls back to raw passthrough on
        // boxes without a HW encoder + matching decoder (e.g. macOS dev), or if
        // the transcode pipeline fails to build. `mode` is advisory; the choice
        // is driven by hardware availability.
        let _ = mode;
        let transcode = hw_h264_encoder().zip(hw_decoder(codec));
        let (mut desc, mut transcoding) = match transcode {
            Some((enc, dec)) => (transcode_pipeline_desc(codec, enc, dec), true),
            None => (passthrough_pipeline_desc(codec), false),
        };
        let pipeline = match gst::parse::launch(&desc) {
            Ok(p) => p,
            Err(e) if transcoding => {
                warn!(
                    camera_id,
                    error = %e,
                    "webrtc transcode pipeline failed to build; falling back to passthrough"
                );
                transcoding = false;
                desc = passthrough_pipeline_desc(codec);
                gst::parse::launch(&desc)
                    .map_err(|e| WebRtcError::Build(format!("parse::launch: {e}")))?
            }
            Err(e) => return Err(WebRtcError::Build(format!("parse::launch: {e}"))),
        }
        .downcast::<gst::Pipeline>()
        .map_err(|_| WebRtcError::Build("downcast Pipeline".to_string()))?;
        debug!(camera_id, transcoding, "webrtc HD pipeline built");

        // Per-camera transcode framerate. `videorate` normalises to a constant
        // rate so the browser plays out smoothly on even RTP timestamps, but
        // cameras run at different rates — so rather than hardcode one value,
        // start the `ratecaps` filter at DEFAULT_TRANSCODE_FPS and adopt the
        // camera's own declared framerate once the decoder negotiates it. A
        // framerate-only caps change does not realloc VA surfaces, so it is
        // safe even if `videorate` has already begun. Cameras that declare no
        // rate (`framerate=0/1`) keep the default. Passthrough builds have no
        // `ratecaps` element and skip this entirely.
        if transcoding {
            if let Some(ratecaps) = pipeline.by_name("ratecaps") {
                ratecaps.set_property("caps", va_transcode_caps(DEFAULT_TRANSCODE_FPS, None));
                if let Some(dec_src) = pipeline.by_name("dec").and_then(|d| d.static_pad("src")) {
                    let ratecaps = ratecaps.clone();
                    let _ = dec_src.add_probe(
                        gst::PadProbeType::EVENT_DOWNSTREAM,
                        move |_pad, info| {
                            let Some(gst::PadProbeData::Event(ev)) = &info.data else {
                                return gst::PadProbeReturn::Ok;
                            };
                            let gst::EventView::Caps(caps_ev) = ev.view() else {
                                return gst::PadProbeReturn::Ok;
                            };
                            // Act on the first raw-video caps event (always
                            // carries width/height). Adopt the camera's declared
                            // framerate (default if it declares none), and cap
                            // the output resolution to <=1080p so a 4MP source
                            // is scaled down: a full-native re-encode makes
                            // oversized keyframes that starve a shared uplink.
                            let Some(s) = caps_ev.caps().structure(0) else {
                                return gst::PadProbeReturn::Ok;
                            };
                            let (Ok(src_w), Ok(src_h)) =
                                (s.get::<i32>("width"), s.get::<i32>("height"))
                            else {
                                return gst::PadProbeReturn::Ok;
                            };
                            let fps = s
                                .get::<gst::Fraction>("framerate")
                                .ok()
                                .filter(|fr| fr.numer() > 0 && fr.denom() > 0)
                                .map(|fr| (fr.numer() / fr.denom()).clamp(1, MAX_TRANSCODE_FPS))
                                .unwrap_or(DEFAULT_TRANSCODE_FPS);
                            let dims = capped_transcode_dims(src_w, src_h);
                            ratecaps.set_property("caps", va_transcode_caps(fps, dims));
                            match dims {
                                Some((w, h)) => debug!(
                                    camera_id,
                                    src_w,
                                    src_h,
                                    out_w = w,
                                    out_h = h,
                                    fps,
                                    "webrtc transcode capped resolution"
                                ),
                                None => debug!(
                                    camera_id,
                                    src_w, src_h, fps, "webrtc transcode framerate"
                                ),
                            }
                            gst::PadProbeReturn::Remove
                        },
                    );
                }
            }
        }

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| WebRtcError::Build("appsrc 'src' missing".to_string()))?
            .downcast::<AppSrc>()
            .map_err(|_| WebRtcError::Build("downcast AppSrc".to_string()))?;
        let webrtc = pipeline
            .by_name("webrtc")
            .ok_or_else(|| WebRtcError::Build("webrtcbin 'webrtc' missing".to_string()))?;

        // (Phase b probe) Add the transport-wide-cc RTP header extension to the
        // payloader so the offer advertises TWCC. Combined with the twcc-stats
        // logging in the congestion-control loop, this tells us whether the
        // Cloudflare SFU returns TWCC feedback — the signal `rtpgccbwe` needs.
        if let Some(pay) = pipeline.by_name("pay") {
            match gst_rtp::RTPHeaderExtension::create_from_uri(RTP_TWCC_URI) {
                Some(ext) => {
                    ext.set_id(1);
                    pay.emit_by_name::<()>("add-extension", &[&ext]);
                    debug!(
                        camera_id,
                        "webrtc: added transport-wide-cc extension (id=1)"
                    );
                }
                None => warn!(
                    camera_id,
                    "webrtc: could not create TWCC extension (rtpmanager missing?)"
                ),
            }
        }

        // Enable RED/ULPFEC (+ keep NACK) on the send transceiver so the viewer
        // leg recovers bursty downlink loss from parity instead of round-trip
        // retransmits. The transceiver exists now that `pay ! webrtc` linked the
        // request sink pad during parse.
        configure_fec(&webrtc, camera_id);

        // Tell appsrc the exact byte-stream codec so h264parse/rtppay
        // negotiate without a probe.
        let caps_name = if codec.base() == "h265" {
            "video/x-h265"
        } else {
            "video/x-h264"
        };
        let caps = gst::Caps::builder(caps_name)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        appsrc.set_caps(Some(&caps));
        appsrc.set_stream_type(AppStreamType::Stream);

        // Apply ICE servers: the first STUN URL → `stun-server`; every
        // TURN URL → `add-turn-server` (webrtcbin supports several).
        for server in ice_servers {
            for url in &server.urls {
                if let Some(stun) = stun_url_for(url) {
                    webrtc.set_property("stun-server", stun.as_str());
                } else if let Some(turn) = turn_url_for(
                    url,
                    server.username.as_deref(),
                    server.credential.as_deref(),
                ) {
                    let _added: bool = webrtc.emit_by_name::<bool>("add-turn-server", &[&turn]);
                }
            }
        }

        // Pump compressed NALs into appsrc, splicing in at the next IDR.
        let feed = tokio::spawn(feed_loop(appsrc.clone(), nal_rx, camera_id));

        // Congestion control for the transcode path. Prefer `rtpgccbwe`
        // (Google Congestion Control + pacing) when the bundled plugin is
        // present: it spreads keyframe bursts under the uplink and drives the
        // encoder off a delay-based estimate, so a higher sustained bitrate no
        // longer triggers the loss spikes that plague pure loss-based AIMD.
        // Fall back to the AIMD loop when the plugin is absent/unloadable.
        // Passthrough builds have no `enc` element and skip both.
        // Forward browser-driven PLI/FIR keyframe requests to the encoder so a
        // viewer that lost reference frames recovers on the next RTP frame
        // instead of waiting up to the scheduled GOP (`key-int-max`, ~4s) — or,
        // when a delta-only gap outlasts the GOP, never recovering (black).
        if transcoding {
            wire_keyframe_on_pli(&pipeline, camera_id);
        }

        let gcc: Arc<parking_lot::Mutex<Option<gst::glib::WeakRef<gst::Element>>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let cc = if transcoding {
            match pipeline.by_name("enc") {
                Some(enc) if gst::ElementFactory::find("rtpgccbwe").is_some() => {
                    wire_gcc_congestion_control(&webrtc, &enc, camera_id, Arc::clone(&gcc));
                    info!(
                        camera_id,
                        "webrtc congestion control: rtpgccbwe (paced GCC)"
                    );
                    None
                }
                Some(enc) => {
                    info!(
                        camera_id,
                        "webrtc congestion control: AIMD (rtpgccbwe plugin unavailable)"
                    );
                    Some(spawn_congestion_control(webrtc.clone(), enc, camera_id))
                }
                None => None,
            }
        } else {
            None
        };

        Ok(Self {
            session_id,
            camera_id,
            codec,
            pipeline,
            webrtc,
            _appsrc: appsrc,
            events,
            feed,
            cc,
            gcc,
        })
    }

    /// Wire the publisher/offerer negotiation: on `on-negotiation-needed`
    /// create an offer, adopt it locally, and — once ICE gathering completes
    /// — emit [`WebRtcEvent::Offer`] with the candidates baked into the SDP.
    /// Also emit [`WebRtcEvent::Connected`] when the peer connection is up.
    fn wire_offerer(&self) {
        let codec_label = self.codec.base();

        connect_negotiation_needed(
            &self.webrtc,
            self.events.clone(),
            self.session_id.clone(),
            codec_label,
        );
        connect_connection_state(&self.webrtc, self.events.clone(), self.session_id.clone());
    }

    /// Apply the SFU's SDP **answer** (publisher role). Media starts flowing
    /// to the SFU once the DTLS/ICE connection is up (which fires
    /// [`WebRtcEvent::Connected`]). The pipeline is already `Playing` from
    /// [`WebRtcSession::new_publisher`], so no state change is needed here.
    pub fn set_answer(&self, sdp_answer: &str) -> Result<(), WebRtcError> {
        let msg = gst_sdp::SDPMessage::parse_buffer(sdp_answer.as_bytes())
            .map_err(|e| WebRtcError::Sdp(format!("parse answer: {e}")))?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, msg);
        let events_err = self.events.clone();
        let remote_promise = gst::Promise::with_change_func(move |reply| {
            if let Err(e) = reply {
                let _ = events_err.send(WebRtcEvent::Failed(format!(
                    "set-remote-description(answer): {e:?}"
                )));
            }
        });
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&answer, &remote_promise]);
        Ok(())
    }

    /// The session's stable id (for logging / manager bookkeeping).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The camera this session streams.
    pub fn camera_id(&self) -> CameraId {
        self.camera_id
    }

    /// Clamp the outbound HD bitrate to the cloud-computed downlink target
    /// (kbps), the slowest browser viewer's measured receive path.
    ///
    /// Over the raw Cloudflare SFU the local `rtpgccbwe` estimator only sees
    /// the fat edge→CF publish leg, so it ramps to the 4 Mbps ceiling
    /// regardless of the operator's real CF→browser downlink; when that
    /// downlink is slower the edge over-sends, loss spikes, the browser floods
    /// PLIs, and the stream blacks out with no feedback loop. This is the
    /// missing feedback: we lower the pacer's `max-bitrate` so GCC settles at
    /// or below the true downlink, and clamp the encoder immediately so the
    /// drop takes effect on the next frame rather than waiting for the next
    /// estimate.
    ///
    /// Bounds mirror the pacer's own limits (`GCC_MIN_BPS`..`GCC_MAX_BPS`); the
    /// cloud already clamps to [600, 4000] kbps. No-op on passthrough / AIMD
    /// sessions or before the aux-sender has been requested.
    pub fn set_max_bitrate_kbps(&self, kbps: u32) {
        let bps = kbps.saturating_mul(1000).clamp(GCC_MIN_BPS, GCC_MAX_BPS);
        let capped_kbps = bps / 1000;

        let Some(gcc) = self
            .gcc
            .lock()
            .as_ref()
            .and_then(gst::glib::WeakRef::upgrade)
        else {
            debug!(
                camera_id = self.camera_id,
                kbps = capped_kbps,
                "set_max_bitrate_kbps: no rtpgccbwe pacer (passthrough/AIMD or not yet negotiated); ignoring"
            );
            return;
        };
        gcc.set_property("max-bitrate", bps);

        // Clamp the encoder down immediately if it is currently above the new
        // ceiling; let GCC drive it back up when the estimate recovers.
        if let Some(enc) = self.pipeline.by_name("enc") {
            let current: u32 = enc.property("bitrate");
            if current > capped_kbps {
                enc.set_property("bitrate", capped_kbps);
            }
        }
        debug!(
            camera_id = self.camera_id,
            kbps = capped_kbps,
            "set_max_bitrate_kbps: clamped rtpgccbwe max-bitrate to browser downlink"
        );
    }

    /// True once the NAL feed task has ended — either because the consumer
    /// stalled past [`FEED_PUSH_STALL_TIMEOUT`] (the browser tab closed / the
    /// PeerConnection died without a `live_hd_stop`), the broadcast closed, or
    /// the feed hit EOS. A finished feed means the session is producing nothing
    /// and is only holding a still-parked `push_buffer` blocking-pool thread
    /// alive; the manager-side reaper uses this to drop such sessions promptly
    /// (drop → pipeline Null → the parked push unblocks and the thread frees)
    /// instead of waiting for a client disconnect that may never come.
    pub fn feed_ended(&self) -> bool {
        self.feed.is_finished()
    }
}

impl Drop for WebRtcSession {
    fn drop(&mut self) {
        self.feed.abort();
        if let Some(cc) = &self.cc {
            cc.abort();
        }
        // Sessions are dropped by the manager-side reaper on a tokio
        // worker, and `webrtcbin` can sit in NULL for as long as the
        // ICE/DTLS transport takes to unwind.
        crate::teardown::null_pipeline_detached(
            self.pipeline.clone(),
            "webrtc::WebRtcSession::drop",
            Some(self.camera_id),
        );
    }
}

/// Nominal frame spacing (~30 fps) used to keep the appsrc timeline strictly
/// advancing when a sample lacks a usable PTS or repeats one.
const FEED_FALLBACK_STEP: Duration = Duration::from_millis(33);

/// Builds a clean, 0-based, **strictly-monotonic** presentation timeline from
/// the camera's own per-AU timestamps so `rtph264pay` emits exactly one
/// distinct RTP timestamp per access unit.
///
/// Why this exists: the pipeline used to run `appsrc do-timestamp=true`, which
/// stamps each buffer with the pipeline running-time *at push*. Under the
/// `block=true` back-pressure from `webrtcbin`, the feed pushes access units in
/// bursts, so consecutive AUs got near-identical timestamps. `rtph264pay` then
/// emitted colliding RTP timestamps and the browser's H.264 depacketiser could
/// not tell one frame from the next — it only ever completed the occasional
/// keyframe, so HD rendered frozen/black (confirmed via `chrome://webrtc-internals`:
/// tens of MB received, `framesReceived` stuck at a handful, all keyframes).
///
/// The pre-roll ingester already hands us a monotonic `NalSample.pts` per AU
/// (synthesised for cameras that drop it), so we stamp buffers with that
/// instead — every frame gets a distinct, correctly-spaced RTP timestamp
/// regardless of push cadence.
struct FeedClock {
    base: Option<Duration>,
    last: Option<Duration>,
}

impl FeedClock {
    fn new() -> Self {
        Self {
            base: None,
            last: None,
        }
    }

    /// Resolve the session-relative, strictly-advancing PTS for a sample.
    /// Rebases the first seen timestamp to zero; a missing, duplicate, or
    /// backward source timestamp is nudged forward by one nominal frame so two
    /// AUs never share an RTP timestamp (the exact failure this guards against).
    fn resolve(&mut self, sample: &NalSample) -> Duration {
        let rel = match sample.pts.or(sample.dts) {
            Some(raw) => {
                let base = *self.base.get_or_insert(raw);
                raw.saturating_sub(base)
            }
            None => self.last.map_or(Duration::ZERO, |l| l + FEED_FALLBACK_STEP),
        };
        let rel = match self.last {
            Some(last) if rel <= last => last + FEED_FALLBACK_STEP,
            _ => rel,
        };
        self.last = Some(rel);
        rel
    }
}

/// Pump compressed NAL samples from the ingester broadcast into `appsrc`.
///
/// Splices in at the next keyframe (drops delta frames until an IDR) so a
/// mid-GOP subscribe never feeds the browser a broken reference frame. A
/// broadcast lag re-arms the splice (we may have dropped the frames between
/// the last IDR and now). The [`FeedClock`] persists across re-arms so the
/// outbound RTP timeline stays monotonic even across a dropped-frame gap.
/// Pump compressed NALs from a camera ingester into `appsrc`, splicing in at
/// the next keyframe and re-arming the splice after a broadcast lag. Shared by
/// the SFU ([`WebRtcSession`]) and MoQ ([`crate::moq_publish::MoqSession`])
/// publishers — both feed the same per-camera NAL stream.
pub(crate) async fn feed_loop(
    appsrc: AppSrc,
    mut rx: broadcast::Receiver<NalSample>,
    camera_id: CameraId,
) {
    let mut started = false;
    let mut clock = FeedClock::new();
    loop {
        match rx.recv().await {
            Ok(sample) => {
                if !started {
                    if !sample.is_keyframe {
                        continue;
                    }
                    started = true;
                    debug!(camera_id, "webrtc feed: spliced in at keyframe");
                }
                // Resolve the monotonic PTS on the async side so the
                // `FeedClock` state stays here and is never moved into the
                // blocking task below.
                let ts_ns = clock.resolve(&sample).as_nanos() as u64;
                // `appsrc` is configured `block=true`: when the consumer
                // (webrtcbin / the browser PeerConnection, or webmux for MoQ)
                // stalls, its queue fills and `push_buffer` blocks in
                // libgstapp's GCond until the consumer drains. Running that on
                // a core tokio worker is what wedged the whole engine — enough
                // stalled live sessions pinned every async worker in
                // `g_cond_wait`, starving the runtime so detection, the LBR
                // live pump, and the cloud tunnel all froze. Push off the
                // runtime instead, and bound it with a timeout so a dead
                // consumer tears THIS session down rather than leaking a
                // blocking-pool thread forever (session teardown sets the
                // pipeline to Null, which flushes the appsrc and releases the
                // still-blocked push).
                let push_appsrc = appsrc.clone();
                let is_keyframe = sample.is_keyframe;
                let data = sample.data;
                let push = tokio::task::spawn_blocking(move || {
                    push_nal_bytes(&push_appsrc, &data, ts_ns, is_keyframe)
                });
                match tokio::time::timeout(FEED_PUSH_STALL_TIMEOUT, push).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(e))) => {
                        warn!(camera_id, error = %e, "webrtc feed: push failed; ending");
                        break;
                    }
                    Ok(Err(join_err)) => {
                        warn!(camera_id, error = %join_err, "webrtc feed: push task panicked; ending");
                        break;
                    }
                    Err(_elapsed) => {
                        warn!(
                            camera_id,
                            timeout_ms = FEED_PUSH_STALL_TIMEOUT.as_millis() as u64,
                            "webrtc feed: consumer stalled (push_buffer blocked past timeout); \
                             tearing down session"
                        );
                        // The blocking push is still parked on the appsrc
                        // GCond; it unblocks when the session drop nulls the
                        // pipeline. Do NOT send EOS here — that would race the
                        // still-in-flight push on the same appsrc.
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                debug!(
                    camera_id,
                    dropped, "webrtc feed: broadcast lagged; re-arming splice"
                );
                started = false;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = appsrc.end_of_stream();
}

/// Upper bound on how long a single `push_buffer` may block on a stalled
/// consumer before the feed gives up and tears the session down. A healthy
/// browser/webrtcbin drains within milliseconds; several seconds of block
/// means the PeerConnection is dead (tab closed, network dropped, ICE gone).
const FEED_PUSH_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Copy one NAL sample into a `gst::Buffer`, stamp it with the [`FeedClock`]'s
/// monotonic PTS/DTS, and push it into `appsrc`. `appsrc` runs
/// `do-timestamp=false` so these explicit timestamps drive `rtph264pay`'s RTP
/// clock — see [`FeedClock`] for why auto-timestamping breaks frame assembly.
///
/// The PTS is resolved by the caller ([`feed_loop`]) via [`FeedClock`] and
/// passed in as `ts_ns`, so this function is pure blocking GStreamer work and
/// can run under `spawn_blocking` without moving the clock off the async task.
/// `push_buffer` here may block on a `block=true` appsrc when the consumer
/// stalls — which is exactly why the caller runs it off the runtime.
fn push_nal_bytes(
    appsrc: &AppSrc,
    data: &[u8],
    ts_ns: u64,
    is_keyframe: bool,
) -> Result<(), String> {
    let ts = gst::ClockTime::from_nseconds(ts_ns);
    let mut buf = gst::Buffer::with_size(data.len()).map_err(|e| format!("alloc: {e}"))?;
    {
        let bm = buf.get_mut().ok_or("buffer not unique")?;
        let mut map = bm.map_writable().map_err(|e| format!("map: {e}"))?;
        map.copy_from_slice(data);
        drop(map);
        // Baseline/main-profile IP-camera live streams carry no B-frames, so
        // DTS == PTS; setting both keeps `rtph264pay` from inferring a bogus
        // reorder delay.
        bm.set_pts(ts);
        bm.set_dts(ts);
        if !is_keyframe {
            bm.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
    }
    appsrc
        .push_buffer(buf)
        .map(|_| ())
        .map_err(|e| format!("push_buffer: {e:?}"))
}

/// Emit the local offer (with ICE candidates baked in) once `webrtcbin` has
/// finished gathering, guarding against a double emit. If gathering is
/// already complete, emit immediately; otherwise subscribe to
/// `notify::ice-gathering-state` and emit on the first `Complete`.
fn emit_offer_when_gathered(
    webrtc: &gst::Element,
    events: &mpsc::UnboundedSender<WebRtcEvent>,
    session: &str,
    codec: &'static str,
    emitted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let state = webrtc.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
    if state == gst_webrtc::WebRTCICEGatheringState::Complete {
        emit_local_offer(webrtc, events, session, codec, emitted);
        return;
    }
    let events = events.clone();
    let session = session.to_string();
    let emitted = std::sync::Arc::clone(emitted);
    webrtc.connect("notify::ice-gathering-state", false, move |vals| {
        let wb = vals.first().and_then(|v| v.get::<gst::Element>().ok())?;
        let st = wb.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        if st == gst_webrtc::WebRTCICEGatheringState::Complete {
            emit_local_offer(&wb, &events, &session, codec, &emitted);
        }
        None
    });
}

/// Wire `on-negotiation-needed` → `create-offer` → `set-local-description` →
/// emit [`WebRtcEvent::Offer`] once ICE gathering completes.
///
/// A free function rather than a method so the no-self-reference invariant is
/// testable without standing up a whole session — see
/// `negotiation_wiring_does_not_leak_the_webrtcbin`.
///
/// The emitting `webrtcbin` is taken from the signal's first argument and must
/// never be captured instead: a GObject owns its signal closures, so a captured
/// strong ref makes the element own itself and it is never finalized — not even
/// after the pipeline reaches NULL. That leaked one `webrtcbin`, with its ICE
/// agent, DTLS transport and RTP session, per HD session (BUG-136).
fn connect_negotiation_needed(
    webrtc: &gst::Element,
    events: mpsc::UnboundedSender<WebRtcEvent>,
    session_id: String,
    codec_label: &'static str,
) {
    let emitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    webrtc.connect("on-negotiation-needed", false, move |vals| {
        let webrtc = vals.first().and_then(|v| v.get::<gst::Element>().ok())?;
        let events = events.clone();
        let session = session_id.clone();
        let emitted = std::sync::Arc::clone(&emitted);
        let webrtc_for_local = webrtc.clone();
        let offer_promise = gst::Promise::with_change_func(move |reply| {
            let offer = reply
                .ok()
                .flatten()
                .and_then(|s| s.get::<gst_webrtc::WebRTCSessionDescription>("offer").ok());
            let Some(offer) = offer else {
                let _ = events.send(WebRtcEvent::Failed(
                    "create-offer: no offer in reply".to_string(),
                ));
                return;
            };
            webrtc_for_local
                .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            emit_offer_when_gathered(&webrtc_for_local, &events, &session, codec_label, &emitted);
        });
        webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &offer_promise]);
        None
    });
}

/// Emit [`WebRtcEvent::Connected`] once the peer connection is up, and
/// [`WebRtcEvent::Failed`] if it fails.
///
/// Free function for the same reason as [`connect_negotiation_needed`]: it is a
/// signal connected *on* the `webrtcbin`, so it shares that function's
/// self-reference hazard and is covered by the same test.
fn connect_connection_state(
    webrtc: &gst::Element,
    events: mpsc::UnboundedSender<WebRtcEvent>,
    session_id: String,
) {
    let signalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    webrtc.connect("notify::connection-state", false, move |vals| {
        let wb = vals.first().and_then(|v| v.get::<gst::Element>().ok())?;
        let state = wb.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        match state {
            gst_webrtc::WebRTCPeerConnectionState::Connected
                if !signalled.swap(true, std::sync::atomic::Ordering::SeqCst) =>
            {
                debug!(session = %session_id, "webrtc publisher connected");
                let _ = events.send(WebRtcEvent::Connected);
            }
            gst_webrtc::WebRTCPeerConnectionState::Failed => {
                let _ = events.send(WebRtcEvent::Failed("peer connection failed".to_string()));
            }
            _ => {}
        }
        None
    });
}

/// Read `webrtcbin`'s `local-description` (now including gathered candidates)
/// and emit it as [`WebRtcEvent::Offer`], at most once.
fn emit_local_offer(
    webrtc: &gst::Element,
    events: &mpsc::UnboundedSender<WebRtcEvent>,
    session: &str,
    codec: &'static str,
    emitted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(local) =
        webrtc.property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")
    else {
        let _ = events.send(WebRtcEvent::Failed(
            "local-description is empty at ICE-complete".to_string(),
        ));
        return;
    };
    match local.sdp().as_text() {
        Ok(sdp) => {
            debug!(session = %session, "webrtc publisher offer ready (ICE gathered)");
            let _ = events.send(WebRtcEvent::Offer { sdp, codec });
        }
        Err(e) => {
            let _ = events.send(WebRtcEvent::Failed(format!("offer as_text: {e}")));
        }
    }
}

/// Default constant framerate for the HD transcode, used up front and kept for
/// cameras that do not declare a source rate (e.g. an RTSP stream negotiating
/// `framerate=0/1`). Both current reference cameras deliver ~24fps.
const DEFAULT_TRANSCODE_FPS: i32 = 24;
/// Upper bound on the transcode output framerate. A security live view does not
/// need more, and it caps re-encode load for high-rate (e.g. 60fps) cameras.
const MAX_TRANSCODE_FPS: i32 = 30;
/// Upper bound on the transcode output resolution (1080p). A security HD live
/// view does not need more, and re-encoding a camera's full native resolution
/// (e.g. 4MP, 2688x1520) yields oversized keyframes that starve a shared
/// uplink. Larger sources are scaled down aspect-preserving; smaller ones pass
/// through unchanged (never upscaled).
const MAX_TRANSCODE_WIDTH: i32 = 1920;
const MAX_TRANSCODE_HEIGHT: i32 = 1080;

/// Scale `(src_w, src_h)` down to fit within
/// [`MAX_TRANSCODE_WIDTH`] x [`MAX_TRANSCODE_HEIGHT`], preserving aspect ratio
/// and rounding to even dimensions (H.264 requires even). Returns `None` when
/// the source already fits (no scaling, and never an upscale).
fn capped_transcode_dims(src_w: i32, src_h: i32) -> Option<(i32, i32)> {
    if src_w <= 0 || src_h <= 0 {
        return None;
    }
    if src_w <= MAX_TRANSCODE_WIDTH && src_h <= MAX_TRANSCODE_HEIGHT {
        return None;
    }
    let scale = f64::min(
        f64::from(MAX_TRANSCODE_WIDTH) / f64::from(src_w),
        f64::from(MAX_TRANSCODE_HEIGHT) / f64::from(src_h),
    );
    let even = |v: f64| ((v.round() as i32) & !1).max(2);
    let mut w = even(f64::from(src_w) * scale);
    let mut h = even(f64::from(src_h) * scale);
    // Snap to the exact cap when within ~2% so a near-16:9 source (e.g. a 4MP
    // 2688x1520 sensor at 1.768:1) lands on a clean 1920x1080 instead of
    // 1910x1080. Genuinely different ratios (e.g. 4:3) stay far from the box
    // edge and keep their true aspect (letterboxed by the browser).
    if w >= MAX_TRANSCODE_WIDTH * 98 / 100 {
        w = MAX_TRANSCODE_WIDTH;
    }
    if h >= MAX_TRANSCODE_HEIGHT * 98 / 100 {
        h = MAX_TRANSCODE_HEIGHT;
    }
    Some((w, h))
}

/// A VAMemory raw-video caps fixing the output `framerate` and, when `dims` is
/// `Some`, the output `width`/`height` — used to drive `videorate` +
/// `vapostproc` via the `ratecaps` capsfilter.
fn va_transcode_caps(fps: i32, dims: Option<(i32, i32)>) -> gst::Caps {
    let mut builder = gst::Caps::builder("video/x-raw")
        .features(["memory:VAMemory"])
        .field("framerate", gst::Fraction::new(fps, 1));
    if let Some((w, h)) = dims {
        builder = builder.field("width", w).field("height", h);
    }
    builder.build()
}

/// URI of the transport-wide-cc RTP header extension (RMCAT draft). Added to
/// the payloader so `webrtcbin` negotiates TWCC in the SDP. `rtpgccbwe` (the
/// future GCC estimator) needs the receiver — Cloudflare's SFU — to return
/// TWCC feedback; adding this + logging `twcc-stats` confirms whether it does,
/// before we invest in cross-building the native `gst-plugins-rs` estimator.
const RTP_TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

/// Bounds and step sizes for the manual AIMD (additive-increase,
/// multiplicative-decrease) bitrate controller. The reference edge's uplink to
/// Cloudflare is ~5.3 Mbps shared with the LBR wall + detection, so the ceiling
/// stays modest; the floor keeps HD watchable.
const CC_MIN_KBPS: u32 = 400;
const CC_MAX_KBPS: u32 = 1800;
const CC_START_KBPS: u32 = 1000;
/// Additive increase (kbps) per control tick when the path is clean.
const CC_INCREASE_KBPS: u32 = 100;
/// Multiplicative decrease factor applied when loss is high.
const CC_DECREASE_FACTOR: f64 = 0.8;
/// RTCP `fraction-lost` (0..1) above which we back off, and below which we
/// probe the bitrate upward. Between the two we hold (dead-band).
const CC_LOSS_HIGH: f64 = 0.10;
const CC_LOSS_LOW: f64 = 0.02;

/// Bounds for `rtpgccbwe`, in BITS per second (its properties are bits/s, not
/// kbps). The ceiling is capped at 4 Mbps rather than the gst-plugins-rs 8 Mbps
/// default: over a Cloudflare TURN **relay** the delay-based estimate has been
/// observed to run away to the 8 Mbps ceiling, saturate the relayed path, and
/// collapse the return TWCC feedback — after which media flow silently dies and
/// the viewer goes black with no recovery. 4 Mbps keeps 1080p watchable while
/// staying inside the reference edge's ~5.3 Mbps shared uplink even when the
/// media is relayed rather than sent peer-to-peer.
const GCC_MIN_BPS: u32 = 400_000;
const GCC_START_BPS: u32 = 1_200_000;
const GCC_MAX_BPS: u32 = 4_000_000;

/// Manual AIMD bitrate controller for the WebRTC transcode. `rtpgccbwe` (the
/// stock GStreamer Google-Congestion-Control estimator) is not installed on the
/// edge, so this is a lightweight stand-in: it reacts to the RTCP
/// `fraction-lost` the browser reports back and nudges the encoder bitrate to
/// track the available uplink — slow up, fast down. Pure + unit-testable.
struct BitrateController {
    kbps: u32,
}

impl BitrateController {
    fn new() -> Self {
        Self {
            kbps: CC_START_KBPS,
        }
    }

    /// Fold one loss observation (`fraction_lost` in 0..1) into the target
    /// bitrate and return the new value (kbps), clamped to [MIN, MAX].
    fn update(&mut self, fraction_lost: f64) -> u32 {
        if fraction_lost > CC_LOSS_HIGH {
            let reduced = (self.kbps as f64 * CC_DECREASE_FACTOR) as u32;
            self.kbps = reduced.max(CC_MIN_KBPS);
        } else if fraction_lost < CC_LOSS_LOW {
            self.kbps = (self.kbps + CC_INCREASE_KBPS).min(CC_MAX_KBPS);
        }
        self.kbps
    }
}

/// Minimum spacing between encoder-forced IDRs driven by inbound PLI/FIR. A
/// struggling viewer floods PLIs (one per lost reference), and each forced IDR
/// is a full 20–40 KB frame with `all-headers`. Honouring every PLI at 1080p
/// emits keyframes far faster than the GCC-collapsed bitrate budget can carry
/// (~8 Mbps of IDR against a ~1.3 Mbps estimate), which congests the send path,
/// causes more loss, provokes still more PLIs, and spirals the stream to black.
/// One forced keyframe per second is enough to recover a genuine reference loss
/// while a PLI storm is coalesced into a single IDR.
const FORCED_KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(1000);

/// Forward browser keyframe requests (RTCP PLI/FIR) to the encoder. `webrtcbin`
/// translates an inbound PLI/FIR into an upstream `GstForceKeyUnit` custom
/// event; we intercept it on the payloader's src pad and re-issue a
/// force-key-unit directly to the encoder's sink pad so `vah264enc` emits a
/// fresh IDR immediately. Without this, a viewer that loses reference frames
/// (e.g. after a relay hiccup) has to wait up to the scheduled GOP
/// (`key-int-max`, ~4s) for the next keyframe — and if the gap outlasts the GOP
/// it stays black forever. Only meaningful on the transcode path (the `enc`/`pay` elements
/// exist); passthrough has no encoder to retarget.
///
/// Forced IDRs are debounced to [`FORCED_KEYFRAME_MIN_INTERVAL`]: within the
/// cooldown the redundant upstream FKU is dropped (`PadProbeReturn::Drop`) so
/// the encoder is not driven into a keyframe storm that outruns the congestion
/// budget and blacks the stream out.
fn wire_keyframe_on_pli(pipeline: &gst::Pipeline, camera_id: CameraId) {
    let Some(enc) = pipeline.by_name("enc") else {
        return;
    };
    let Some(pay) = pipeline.by_name("pay") else {
        return;
    };
    let Some(pay_src) = pay.static_pad("src") else {
        return;
    };
    let enc_weak = enc.downgrade();
    let last_forced: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);
    let _ = pay_src.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
        let Some(gst::PadProbeData::Event(ev)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let is_fku = matches!(ev.view(), gst::EventView::CustomUpstream(_))
            && ev
                .structure()
                .is_some_and(|s| s.name() == "GstForceKeyUnit");
        if !is_fku {
            return gst::PadProbeReturn::Ok;
        }
        // Debounce: swallow PLIs that arrive inside the cooldown so a viewer's
        // PLI flood collapses to a single IDR instead of a keyframe storm.
        let now = std::time::Instant::now();
        {
            let mut last = last_forced.lock();
            if last.is_some_and(|t| now.duration_since(t) < FORCED_KEYFRAME_MIN_INTERVAL) {
                return gst::PadProbeReturn::Drop;
            }
            *last = Some(now);
        }
        if let Some(enc) = enc_weak.upgrade() {
            if let Some(sink) = enc.static_pad("sink") {
                let s = gst::Structure::builder("GstForceKeyUnit")
                    .field("all-headers", true)
                    .build();
                let _ = sink.send_event(gst::event::CustomUpstream::new(s));
                debug!(camera_id, "webrtc: PLI/FIR -> forced encoder keyframe");
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// FEC redundancy the send transceiver advertises, in percent of media rate.
///
/// The HD viewer leg (SFU → browser) suffers bursty downlink loss at the
/// 1080p bitrate that the low-bitrate grid never sees — measured ~8 % RTP loss
/// with repeated freezes while the same workstation renders every LBR tile
/// cleanly. Pure NACK/RTX cannot keep up across Cloudflare RTT: a lost
/// reference packet costs a full round-trip to retransmit, and by the time it
/// arrives the jitter buffer has already frozen. Forward error correction
/// (RED + ULPFEC) sends redundant parity so the browser reconstructs the
/// missing packets locally, with no round-trip. 25 % covers an ~8 % loss rate
/// with burst headroom while staying well under the parity overhead that would
/// itself congest the link (webrtcbin caps the useful range at 100 %).
const FEC_PERCENTAGE: u32 = 25;

/// Enable RED/ULPFEC (and keep NACK/RTX) on webrtcbin's send transceiver.
///
/// By default webrtcbin negotiates the FEC payload types as `pt -1` — i.e. the
/// RED and ULPFEC encoders are created but disabled, so the stream is protected
/// by retransmission only. Setting `fec-type = ulp-red` on the transceiver
/// makes the offer advertise real RED/ULPFEC payload types and emit parity
/// packets at [`FEC_PERCENTAGE`]; `do-nack = true` keeps retransmission wired as
/// the second layer. The transceiver already exists once the payloader's src
/// pad is linked to webrtcbin's request sink pad at parse time, so we fetch
/// index 0 and configure it before the offer is generated.
///
/// `fec-type` is set via its GEnum value nick (`ulp-red`) rather than the Rust
/// `WebRTCFECType` enum because that binding is gated behind the `v1_14_1`
/// gstreamer-webrtc feature, which this build does not enable; the nick form
/// carries no such feature dependency.
fn configure_fec(webrtc: &gst::Element, camera_id: CameraId) {
    let transceiver = webrtc
        .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32]);
    let Some(transceiver) = transceiver else {
        warn!(
            camera_id,
            "webrtc: no send transceiver to configure FEC (offer not yet built?)"
        );
        return;
    };
    transceiver.set_property_from_str("fec-type", "ulp-red");
    transceiver.set_property("fec-percentage", FEC_PERCENTAGE);
    transceiver.set_property("do-nack", true);
    info!(
        camera_id,
        fec_percentage = FEC_PERCENTAGE,
        "webrtc: enabled RED/ULPFEC + NACK on send transceiver"
    );
}

/// Minimum spacing between encoder bitrate retargets.
///
/// `vah264enc` accepts `bitrate` while PLAYING, but it reads `key-int-max` at
/// *configure* time — so every write reconfigures the encoder and restarts the
/// GOP with a fresh IDR. `rtpgccbwe` is a per-packet-group estimator that
/// re-estimates several times a second, so retargeting on each notify emitted
/// **6-7 IDR/s** (measured on an Alder Lake-N core against the browser's
/// `keyFramesDecoded`). At 1080p that is ~4.3 Mbps out of an encoder pinned to
/// `bitrate=1000` CBR: it saturated the uplink, collapsed the GCC estimate, and
/// drove the stream to 0 fps — the operator-visible "HD live view drops".
///
/// Damping costs nothing in responsiveness: `rtpgccbwe` still *paces* the send
/// path continuously, so short-term congestion is absorbed by the pacer. The
/// encoder target only has to track the trend.
const BITRATE_RETARGET_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Minimum relative *rise* worth paying an IDR for, in percent.
const BITRATE_RETARGET_MIN_RISE_PCT: u32 = 15;

/// Minimum relative *fall* worth paying an IDR for, in percent.
///
/// Deliberately lower than the rise threshold. Holding a stale *high* target
/// overshoots the link and grows the pacer's queue, which shows up as latency;
/// holding a stale *low* one only costs picture quality. Congestion control is
/// conventionally asymmetric for that reason. A smaller fall threshold cannot
/// reintroduce the keyframe storm, because [`BITRATE_RETARGET_MIN_INTERVAL`]
/// bounds the write rate regardless of which threshold applies.
const BITRATE_RETARGET_MIN_FALL_PCT: u32 = 5;

/// Whether a new GCC estimate justifies reconfiguring the encoder. Pure.
fn should_retarget_bitrate(current_kbps: u32, new_kbps: u32, since_last: Option<Duration>) -> bool {
    if since_last.is_some_and(|d| d < BITRATE_RETARGET_MIN_INTERVAL) {
        return false;
    }
    if current_kbps == 0 {
        return new_kbps != 0;
    }
    let min_pct = if new_kbps < current_kbps {
        BITRATE_RETARGET_MIN_FALL_PCT
    } else {
        BITRATE_RETARGET_MIN_RISE_PCT
    };
    current_kbps.abs_diff(new_kbps) * 100 >= current_kbps * min_pct
}

/// Wire `rtpgccbwe` as webrtcbin's aux-sender for the transcode path. The
/// element paces the outbound RTP and produces a delay-based bandwidth
/// estimate; we retarget the `vah264enc` `bitrate` (kbps = bits/1000) when that
/// estimate moves materially, subject to [`should_retarget_bitrate`] — never on
/// every notify, because each write costs an IDR. This is the proactive
/// replacement for the reactive [`spawn_congestion_control`] AIMD loop and is
/// only wired when the bundled `libgstrsrtp.so` plugin registered `rtpgccbwe`.
fn wire_gcc_congestion_control(
    webrtc: &gst::Element,
    enc: &gst::Element,
    camera_id: CameraId,
    gcc_slot: Arc<parking_lot::Mutex<Option<gst::glib::WeakRef<gst::Element>>>>,
) {
    let enc_weak = enc.downgrade();
    // `request-aux-sender` fires during negotiation with (webrtcbin, session_id);
    // it must return the element to splice into the send path (the pacer). We
    // ignore the session id (single sendonly video) and return `None` (no aux
    // sender) only if the element unexpectedly fails to build.
    webrtc.connect("request-aux-sender", false, move |_values| {
        let cc = gst::ElementFactory::make("rtpgccbwe").build().ok()?;
        cc.set_property("min-bitrate", GCC_MIN_BPS);
        cc.set_property("estimated-bitrate", GCC_START_BPS);
        cc.set_property("max-bitrate", GCC_MAX_BPS);
        // Publish a weak handle so a cloud `live_hd_bitrate` can clamp
        // `max-bitrate` at runtime to the slowest viewer's real downlink.
        *gcc_slot.lock() = Some(cc.downgrade());
        let enc_weak = enc_weak.clone();
        // Per-session; `request-aux-sender` can fire again on renegotiation.
        let last_retarget: parking_lot::Mutex<Option<std::time::Instant>> =
            parking_lot::Mutex::new(None);
        cc.connect_notify(Some("estimated-bitrate"), move |bwe, _pspec| {
            let bits: u32 = bwe.property("estimated-bitrate");
            let kbps = (bits / 1000).max(GCC_MIN_BPS / 1000);
            let Some(enc) = enc_weak.upgrade() else {
                return;
            };
            let current: u32 = enc.property("bitrate");
            let now = std::time::Instant::now();
            let mut last = last_retarget.lock();
            if !should_retarget_bitrate(current, kbps, last.map(|t| now.duration_since(t))) {
                return;
            }
            *last = Some(now);
            drop(last);
            enc.set_property("bitrate", kbps);
            debug!(camera_id, kbps, "rtpgccbwe adjusted encoder bitrate");
        });
        Some(cc.to_value())
    });
}

/// Spawn the adaptive-bitrate control loop for a transcode session. Every ~2s
/// it asks `webrtcbin` for stats, reads the worst `fraction-lost` across the
/// remote-inbound reports (the browser's RTCP feedback), feeds it to a
/// [`BitrateController`], and applies the new target to the `vah264enc`
/// `bitrate` property (kbps). If no loss report is available yet (early in the
/// session, before RTCP flows) it holds the current bitrate — it never raises
/// blindly. Aborted on session drop.
fn spawn_congestion_control(
    webrtc: gst::Element,
    enc: gst::Element,
    camera_id: CameraId,
) -> JoinHandle<()> {
    const CC_POLL: Duration = Duration::from_secs(2);
    tokio::spawn(async move {
        let controller = std::sync::Arc::new(parking_lot::Mutex::new(BitrateController::new()));
        let mut ticker = tokio::time::interval(CC_POLL);
        // The first tick fires immediately; consume it so RTCP has time to flow.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let enc = enc.clone();
            let controller = std::sync::Arc::clone(&controller);
            let promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(stats)) = reply else {
                    return;
                };
                // The browser's RTCP receiver report surfaces as one or more
                // `remote-inbound-rtp` sub-structures, each carrying
                // `fraction-lost` (0..1). Take the worst across streams.
                let mut worst: Option<f64> = None;
                for (_field, value) in stats.iter() {
                    if let Ok(sub) = value.get::<gst::Structure>() {
                        if let Ok(fl) = sub.get::<f64>("fraction-lost") {
                            worst = Some(worst.map_or(fl, |w| w.max(fl)));
                        }
                    }
                }
                let Some(loss) = worst else {
                    // No feedback yet — hold; never raise the bitrate blindly.
                    return;
                };
                let new_kbps = controller.lock().update(loss);
                let current: u32 = enc.property("bitrate");
                if new_kbps != current {
                    enc.set_property("bitrate", new_kbps);
                    debug!(
                        camera_id,
                        loss, new_kbps, "webrtc congestion control adjusted bitrate"
                    );
                }
            });
            webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        }
    })
}

/// Operator override forcing every HD session onto the passthrough pipeline.
///
/// Process-wide because it is a per-box policy, not a per-session one — the
/// same shape as the `LIBVA_DRIVER_NAME` / `AMD_DEBUG` decisions the engine
/// already applies once at startup. Set from `[runtime.live_view]
/// force_passthrough` via [`set_force_passthrough`].
static FORCE_PASSTHROUGH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Apply `[runtime.live_view] force_passthrough`. Called once at startup.
pub fn set_force_passthrough(force: bool) {
    FORCE_PASSTHROUGH.store(force, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the hardware transcode path may be used at all.
///
/// Precedence, highest first:
/// 1. the operator's `force_passthrough` override;
/// 2. the `i965` driver, which is Gen9 LP's default and must never transcode
///    (BUG-128 — its encoder `assert()`s and kills the engine);
/// 3. otherwise yes, subject to what the registry actually offers.
///
/// Pure, and deliberately separate from the registry lookup so the policy is
/// testable without a GStreamer runtime.
fn transcode_allowed(force_passthrough: bool, libva_driver: Option<&str>) -> bool {
    !force_passthrough && libva_driver != Some("i965")
}

/// Name of an available hardware H.264 **encoder** for the transcode path, or
/// `None` on boxes without one (e.g. macOS dev), where the caller falls back
/// to passthrough. Pure aside from the plugin registry lookup.
///
/// Returns `None` whenever [`transcode_allowed`] says so, whatever the registry
/// claims. The free `i965-va-driver` build advertises `VAProfileH264*:
/// VAEntrypointEncSlice` but ships none of the encode kernels (they are the
/// non-free `-shaders` payload), so `intel_enc_hw_context_init` finds a NULL
/// `mfc_context` and `assert()`s — killing the whole engine, every camera, the
/// moment one viewer opens Live HD. Element registration reflects that false
/// claim, and the caller's fallback only catches `parse::launch` errors, so
/// nothing downstream can save us from an `abort()` (BUG-128).
fn hw_h264_encoder() -> Option<&'static str> {
    let forced = FORCE_PASSTHROUGH.load(std::sync::atomic::Ordering::Relaxed);
    let driver = std::env::var("LIBVA_DRIVER_NAME").ok();
    if !transcode_allowed(forced, driver.as_deref()) {
        return None;
    }
    ["vah264enc", "vaapih264enc"]
        .into_iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
}

/// Name of an available hardware **decoder** for the camera codec, matching
/// [`hw_h264_encoder`] so the transcode chain stays fully on the GPU.
fn hw_decoder(codec: CodecKind) -> Option<&'static str> {
    let candidates: &[&str] = if codec.base() == "h265" {
        &["vah265dec", "vaapih265dec"]
    } else {
        &["vah264dec", "vaapih264dec"]
    };
    candidates
        .iter()
        .copied()
        .find(|name| gst::ElementFactory::find(name).is_some())
}

/// Maximum keyframe interval (frames) for the transcode encoder. At the
/// transcode fps (~24) this is ~4s between *scheduled* keyframes. WebRTC
/// recovers from loss via PLI-driven forced keyframes (see
/// [`wire_keyframe_on_pli`]), so a longer scheduled GOP is safe and — crucially
/// — emits far fewer oversized IDR bursts onto the GCC-paced uplink, giving the
/// send-side estimator room to ramp back up between keyframes.
const TRANSCODE_KEY_INT_MAX: u32 = 96;

/// CPB (VBV/HRD) buffer bound, expressed as a multiple of the target bitrate
/// (so a 1-second buffer). With `cpb-size`/`cpb-length` left at the driver
/// default (auto), the VA encoders sized the HRD buffer at ~2.5–3s, letting a
/// single IDR balloon to 300–355 KB at `bitrate=1000` — far past the ~250 KB
/// that one 2s GOP's paced budget can drain, which congested the uplink and
/// forced the client into a reopen loop. Bounding the CPB to ~1s of bits makes
/// the encoder raise IDR QP so keyframes stay within the paced budget.
const TRANSCODE_CPB_SECONDS: u32 = 1;

/// Encoder-family-specific rate-control arguments for the transcode chain.
///
/// The two candidate HW H.264 encoders ([`hw_h264_encoder`]) take *different*
/// property names for the same concepts, so the launch string must branch on
/// the selected element to stay correct across hardware profiles:
///
/// - Modern `va` plugin (`vah264enc`, Intel/AMD): `key-int-max`, `b-frames`,
///   `cpb-size` (in **Kbits**), `target-usage`.
/// - Legacy `gstreamer-vaapi` (`vaapih264enc`): `keyframe-period`,
///   `max-bframes`, `cpb-length` (in **milliseconds**), `quality-level`.
///
/// Both are pinned to CBR with a long GOP and a ~1s CPB so no single keyframe
/// can burst the paced uplink budget. Pure.
fn encoder_rate_control_args(encoder: &str, bitrate_kbps: u32) -> String {
    if encoder.starts_with("vaapi") {
        // Legacy gstreamer-vaapi. `cpb-length` is in milliseconds.
        let cpb_ms = TRANSCODE_CPB_SECONDS * 1000;
        format!(
            "keyframe-period={key} max-bframes=0 rate-control=cbr \
             bitrate={bitrate_kbps} cpb-length={cpb_ms} quality-level=6",
            key = TRANSCODE_KEY_INT_MAX,
        )
    } else {
        // Modern va plugin. `cpb-size` is in Kbits; a 1s buffer is bitrate*1.
        let cpb_kbits = bitrate_kbps * TRANSCODE_CPB_SECONDS;
        format!(
            "key-int-max={key} b-frames=0 rate-control=cbr \
             bitrate={bitrate_kbps} cpb-size={cpb_kbits} target-usage=6",
            key = TRANSCODE_KEY_INT_MAX,
        )
    }
}

/// Leaky decoupling queue inserted between the (frame-domain) payloader input
/// and `webrtcbin`, present on BOTH the transcode and passthrough paths.
///
/// Without it the whole chain is rigid: `webrtcbin` (and its `rtpgccbwe` pacer)
/// back-pressures the payloader → the parser → the encoder → the `block=true`
/// `appsrc`, so `appsrc.push_buffer` blocks. The feed's
/// [`FEED_PUSH_STALL_TIMEOUT`] (5 s) then ends the feed task and the manager
/// reaps the whole publisher — a transient pacing backlog on a *healthy* link
/// (measured live: `rtpgccbwe` still ramping to the 4 Mbps ceiling when the
/// stall fired) turns into a full teardown + ~15-20 s cold rebuild, i.e. the
/// residual HD black-out.
///
/// The queue absorbs that back-pressure instead: bounded to ~300 ms and
/// `leaky=downstream` (drop the OLDEST queued frame when full, keep the
/// newest), it drops a few encoded access units under a burst rather than
/// stalling the source. The browser sees those as ordinary loss and recovers
/// on the next frame via the already-wired PLI → forced-keyframe on our encoder
/// (transcode) — far cheaper than rebuilding the PeerConnection. Because it
/// leaks frame-domain buffers *after* the local decoder/encoder, the transcode
/// decoder never sees a broken bitstream (only the remote browser experiences
/// the drop, which WebRTC is built to handle).
const EGRESS_QUEUE: &str = "queue name=egress leaky=downstream max-size-time=300000000 \
     max-size-bytes=0 max-size-buffers=0";

/// Build a **transcode** launch description: HW-decode the camera codec and
/// re-encode to H.264 with a bounded-keyframe CBR profile for smooth WebRTC
/// over a lossy, congestion-controlled network.
///
/// Camera streams often use a multi-second GOP (this InSight uses ~15s). Over
/// WebRTC (UDP) a single lost packet corrupts every delta frame until the next
/// keyframe, so a long source GOP means multi-second freezes on ~1% loss.
/// Re-encoding to CBR with no B-frames, a bounded CPB (see
/// [`encoder_rate_control_args`] / [`TRANSCODE_CPB_SECONDS`]) so keyframes fit
/// the GCC-paced budget, and a [`TRANSCODE_KEY_INT_MAX`]-frame GOP keeps
/// recovery smooth while leaving the camera untouched. The output is always
/// H.264 (`encoding-name=H264`), which also covers the HEVC-camera →
/// non-HEVC-browser case. `config-interval=1` repeats SPS/PPS once a second in
/// the RTP stream for extra resilience. `videorate` normalises the camera's
/// irregular frame delivery to a **constant** rate so the browser plays frames
/// out smoothly on even RTP timestamps. Cameras run at different rates (this
/// InSight declares 25fps; another declares none), so the target rate is not
/// baked in here: the named `ratecaps` capsfilter is left unconstrained and
/// [`WebRtcSession::build_common`] sets it — to [`DEFAULT_TRANSCODE_FPS`] up
/// front, then to the camera's own declared framerate once the decoder
/// negotiates it (see the `dec` src-pad probe). Pure — unit-testable without a
/// runtime.
fn transcode_pipeline_desc(codec: CodecKind, encoder: &str, decoder: &str) -> String {
    let base = codec.base(); // "h264" | "h265"

    // Uplink-constrained bitrate. The reference edge uploads only ~5.3 Mbps to
    // Cloudflare (measured), shared with the LBR wall + detection; an un-paced
    // 2.5 Mbps WebRTC stream's keyframe bursts overflowed that thin uplink and
    // dropped ~50% of packets -> periodic multi-second freezes on BOTH cameras
    // (independent of resolution). 1000 kbps keeps the average and keyframe
    // bursts comfortably inside the uplink. The proper long-term fix is
    // send-side congestion control (rtpgccbwe) for adaptive pacing.
    let bitrate_kbps = 1000;
    let enc_args = encoder_rate_control_args(encoder, bitrate_kbps);
    format!(
        "appsrc name=src is-live=true do-timestamp=false format=time \
             block=true max-bytes=8388608 stream-type=stream \
           ! {base}parse \
           ! {decoder} name=dec \
           ! vapostproc \
           ! videorate \
           ! capsfilter name=ratecaps \
           ! {encoder} name=enc {enc_args} \
           ! h264parse config-interval=1 \
           ! {EGRESS_QUEUE} \
           ! rtph264pay name=pay pt=96 config-interval=1 mtu=1200 \
           ! application/x-rtp,media=video,encoding-name=H264,clock-rate=90000 \
           ! webrtcbin name=webrtc latency=0 bundle-policy=max-bundle"
    )
}

/// Build the passthrough launch description for a camera codec. Pure so it
/// can be unit-tested without a GStreamer runtime.
fn passthrough_pipeline_desc(codec: CodecKind) -> String {
    let base = codec.base(); // "h264" | "h265"
    let encoding = if base == "h265" { "H265" } else { "H264" };
    // `config-interval=0` (trust the source) on BOTH parse and pay: this
    // InSight camera already emits SPS/PPS in every keyframe access unit, so
    // `config-interval=-1` made h264parse re-insert them — doubling the
    // parameter sets, which intermittently corrupts the keyframe (the browser
    // receives it but never decodes it, so `keyFramesDecoded` stalls and HD
    // freezes/blacks out). The camera supplies SPS/PPS at every IDR, so the
    // browser still gets them without re-insertion. `mtu=1200` keeps RTP
    // packets inside a conservative WebRTC MTU.
    //
    // `do-timestamp=false`: the feed stamps each buffer with an explicit,
    // monotonic PTS/DTS off the camera's own per-AU timeline (see `FeedClock`).
    // Auto-timestamping collided RTP timestamps under back-pressure and left the
    // browser unable to assemble delta frames (frozen/black HD).
    //
    // Do NOT pin a `payload` on the RTP caps: the browser's offer assigns the
    // payload types (e.g. pt 96 → VP8, H264 at 102/104/…). If we hardcode
    // `payload=96`, webrtcbin looks for our H264/H265 at pt 96 — which the
    // browser mapped to VP8 — and reports "did not find compatible transceiver
    // for offer caps", so the answer carries no decodable video (blank HD).
    // Leaving the payload unset lets webrtcbin adopt the browser's H264/H265
    // payload type during answer negotiation.
    format!(
        "appsrc name=src is-live=true do-timestamp=false format=time \
             block=true max-bytes=8388608 stream-type=stream \
           ! {base}parse config-interval=0 \
           ! {EGRESS_QUEUE} \
           ! rtp{base}pay name=pay pt=96 config-interval=0 mtu=1200 \
           ! application/x-rtp,media=video,encoding-name={encoding},clock-rate=90000 \
           ! webrtcbin name=webrtc latency=0 bundle-policy=max-bundle"
    )
}

/// Normalise a wire `stun:` URL into the `stun://host:port` form
/// webrtcbin's `stun-server` property expects. Returns `None` for
/// non-STUN URLs. Pure.
fn stun_url_for(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with("stun://") {
        return Some(raw.to_string());
    }
    let rest = raw.strip_prefix("stun:").filter(|r| !r.is_empty())?;
    Some(format!("stun://{rest}"))
}

/// Build the `turn://user:pass@host:port` URL webrtcbin's
/// `add-turn-server` action expects from a wire `turn:`/`turns:` URL plus
/// long-term credentials. Returns `None` for non-TURN URLs or when
/// credentials are missing. Pure.
fn turn_url_for(raw: &str, username: Option<&str>, credential: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once(':')?;
    if scheme != "turn" && scheme != "turns" {
        return None;
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let host_port = rest.split('?').next().unwrap_or(rest);
    if host_port.is_empty() {
        return None;
    }
    let user = username?;
    let cred = credential?;
    Some(format!("{scheme}://{user}:{cred}@{host_port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `webrtcbin` must be finalized once its last owner drops. It is not if
    /// any signal closure captures the element itself: a GObject owns its
    /// handlers, so the element ends up owning a strong reference to itself and
    /// its `webrtc:pc` / `webrtc:ice` threads run for the life of the process.
    ///
    /// This leaked one `webrtcbin` per HD live-view session. Measured on an
    /// Alder Lake-N core: 41 leaked instances after a night of expand/collapse,
    /// with `pc`/`ice` thread counts rising on every open and never falling on
    /// close, until new HD sessions could no longer sustain media (BUG-136).
    ///
    /// Wires everything [`WebRtcSession::wire_offerer`] wires, not just the one
    /// handler that regressed — the hazard belongs to the shape (`connect` on
    /// the element) rather than to any single call site, so anything added to
    /// `wire_offerer` must be added here too.
    ///
    /// Note this only gates a PR carrying the `system-libs` label: `webrtc.rs`
    /// compiles solely under `--features gstreamer-webrtc`, which no other CI
    /// job builds. On an unlabeled PR it is a post-merge gate on `main`.
    #[test]
    fn negotiation_wiring_does_not_leak_the_webrtcbin() {
        if gst::init().is_err() {
            return;
        }
        let Ok(webrtc) = gst::ElementFactory::make("webrtcbin").build() else {
            // `gst-plugins-bad` absent (macOS dev boxes); covered by the
            // `system-libs` CI job, which builds the webrtc feature.
            return;
        };
        let weak = webrtc.downgrade();
        let (tx, _rx) = mpsc::unbounded_channel();
        connect_negotiation_needed(&webrtc, tx.clone(), "session-under-test".to_string(), "H264");
        connect_connection_state(&webrtc, tx, "session-under-test".to_string());

        drop(webrtc);

        assert!(
            weak.upgrade().is_none(),
            "webrtcbin outlived its last owner: something still holds a strong \
             reference, most likely a signal closure that captured the element \
             it is connected to",
        );
    }

    /// Under `i965` the VA encoders abort the process rather than failing, so
    /// no registry entry may be trusted: `vah264enc` and `vaapih264enc` both
    /// exit 134 on `intel_enc_hw_context_init`, measured on Apollo Lake. The
    /// caller reads `None` as "no hardware encoder" and takes the passthrough
    /// path, which is the only encoder-free route (BUG-128).
    #[test]
    fn i965_never_yields_a_hardware_encoder() {
        let prev = std::env::var("LIBVA_DRIVER_NAME").ok();
        std::env::set_var("LIBVA_DRIVER_NAME", "i965");
        let got = hw_h264_encoder();
        match prev {
            Some(v) => std::env::set_var("LIBVA_DRIVER_NAME", v),
            None => std::env::remove_var("LIBVA_DRIVER_NAME"),
        }
        assert_eq!(
            got, None,
            "i965 advertises H.264 EncSlice it cannot honour; selecting it aborts the engine"
        );
    }

    /// The full precedence table for choosing transcode vs passthrough. The
    /// Apollo Lake (`i965`) leg must default to passthrough with no operator
    /// action, and the override must win over an otherwise-capable box.
    #[test]
    fn transcode_allowed_precedence() {
        // Apollo Lake / Gen9 LP: passthrough is the DEFAULT, unconfigured.
        assert!(
            !transcode_allowed(false, Some("i965")),
            "i965 must default to passthrough — its encoder aborts the engine (BUG-128)"
        );
        // The operator override forces passthrough on an otherwise-capable box.
        assert!(!transcode_allowed(true, Some("iHD")));
        assert!(!transcode_allowed(true, None));
        // Left alone, a capable box still transcodes.
        assert!(transcode_allowed(false, Some("iHD")));
        assert!(transcode_allowed(false, Some("radeonsi")));
        assert!(transcode_allowed(false, None));
        // The override cannot be used to force transcode back ON for i965.
        assert!(!transcode_allowed(false, Some("i965")));
    }

    /// The config knob is only worth anything if it reaches the decision. This
    /// exercises the real global rather than the pure helper, so a broken
    /// `set_force_passthrough` wiring fails here instead of in the field.
    /// Asserting `None` is safe under test parallelism: no other test asserts
    /// this function returns `Some`.
    #[test]
    fn set_force_passthrough_reaches_the_encoder_choice() {
        set_force_passthrough(true);
        assert_eq!(
            hw_h264_encoder(),
            None,
            "[runtime.live_view] force_passthrough must suppress the hardware encoder"
        );
        set_force_passthrough(false);
    }

    /// `rtpgccbwe` re-estimates several times a second, and `vah264enc` reads
    /// `key-int-max` at configure time — so every `bitrate` write restarts the
    /// GOP with a fresh IDR. Retargeting on each notify was measured at 6-7
    /// IDR/s, which drove a 1000 kbps CBR encoder to 4.3 Mbps, saturated the
    /// uplink and collapsed the stream to 0 fps. Replays ordinary estimator
    /// jitter and asserts the retarget rate stays bounded.
    #[test]
    fn gcc_jitter_does_not_retarget_the_encoder_on_every_estimate() {
        // ~7 estimates/second for 10 s. The swing deliberately straddles both
        // thresholds, so the *delta* gate never binds and the count is bounded
        // by BITRATE_RETARGET_MIN_INTERVAL alone — zeroing that interval must
        // fail this test.
        let jitter = [1900u32, 700, 1800, 650, 2000, 720, 1750];
        let tick = Duration::from_millis(1000 / 7);

        let mut current = 1200u32;
        let mut since_last: Option<Duration> = None;
        let mut retargets = 0usize;
        for i in 0..70 {
            let candidate = jitter[i % jitter.len()];
            if should_retarget_bitrate(current, candidate, since_last) {
                current = candidate;
                since_last = Some(Duration::ZERO);
                retargets += 1;
            } else {
                since_last = Some(since_last.map_or(tick, |d| d + tick));
            }
        }

        assert!(
            retargets >= 4,
            "fixture is vacuous: swings this large must produce retargets, got {retargets}"
        );
        assert!(
            retargets <= 6,
            "10 s of GCC churn produced {retargets} encoder retargets, i.e. {retargets} \
             forced IDRs; the 2 s interval should cap this near 5"
        );
    }

    /// The damping must not swallow a genuine collapse: when the estimate falls
    /// hard the encoder still has to follow, or we keep sending far more than
    /// the link can carry.
    #[test]
    fn a_large_bitrate_drop_still_retargets_once_the_interval_has_passed() {
        assert!(
            should_retarget_bitrate(2000, 500, Some(BITRATE_RETARGET_MIN_INTERVAL)),
            "a 75% collapse in the estimate must reach the encoder"
        );
        assert!(
            !should_retarget_bitrate(2000, 500, Some(Duration::from_millis(100))),
            "but not more often than the minimum interval"
        );
        // First estimate of a session has no previous write to debounce against.
        assert!(should_retarget_bitrate(1000, 2000, None));
        // Falls are damped less than rises: 8% down lands, 8% up does not.
        assert!(
            should_retarget_bitrate(1000, 920, None),
            "an 8% fall must land"
        );
        assert!(
            !should_retarget_bitrate(1000, 1080, None),
            "an 8% rise is not worth an IDR"
        );
    }

    #[test]
    fn passthrough_desc_h264() {
        let d = passthrough_pipeline_desc(CodecKind::H264);
        assert!(d.contains("h264parse"), "{d}");
        assert!(d.contains("rtph264pay"), "{d}");
        assert!(d.contains("encoding-name=H264"), "{d}");
        assert!(d.contains("webrtcbin name=webrtc"), "{d}");
        // Leaky egress queue decouples webrtcbin/pacer back-pressure from the
        // source so a transient stall drops frames instead of tearing down.
        assert!(d.contains("queue name=egress leaky=downstream"), "{d}");
        // We stamp PTS/DTS ourselves; auto-timestamping is what broke delta
        // frame assembly in the browser.
        assert!(d.contains("do-timestamp=false"), "{d}");
    }

    #[test]
    fn passthrough_desc_h265() {
        // The `Plus` variants must map onto the same base element names.
        let d = passthrough_pipeline_desc(CodecKind::H265Plus);
        assert!(d.contains("h265parse"), "{d}");
        assert!(d.contains("rtph265pay"), "{d}");
        assert!(d.contains("encoding-name=H265"), "{d}");
    }

    #[test]
    fn feed_clock_strictly_advances() {
        let kf = |pts_ms: u64, key: bool| NalSample {
            pts: Some(Duration::from_millis(pts_ms)),
            dts: None,
            is_keyframe: key,
            data: vec![0u8],
        };
        let mut clock = FeedClock::new();
        // First AU rebases to zero regardless of the camera's absolute clock.
        assert_eq!(clock.resolve(&kf(1000, true)), Duration::ZERO);
        // Normal ~40 ms spacing is preserved.
        assert_eq!(clock.resolve(&kf(1040, false)), Duration::from_millis(40));
        // A duplicate source timestamp is nudged forward, never repeated —
        // colliding RTP timestamps are exactly what froze HD.
        assert_eq!(clock.resolve(&kf(1040, false)), Duration::from_millis(73));
        // A missing PTS still advances.
        let no_pts = NalSample {
            pts: None,
            dts: None,
            is_keyframe: false,
            data: vec![0u8],
        };
        assert_eq!(clock.resolve(&no_pts), Duration::from_millis(106));
        // A backward jump is clamped forward, keeping the timeline monotonic.
        assert_eq!(clock.resolve(&kf(500, true)), Duration::from_millis(139));
    }

    #[test]
    fn capped_transcode_dims_scales_down_4mp_only() {
        // 4MP source (cam1, 2688x1520 = 1.768:1) is scaled to fit 1080p and
        // snapped to a clean 1920x1080 (within ~2% of true 16:9).
        assert_eq!(capped_transcode_dims(2688, 1520), Some((1920, 1080)));
        // 4K (exactly 16:9) is scaled to exactly 1080p.
        assert_eq!(capped_transcode_dims(3840, 2160), Some((1920, 1080)));
        // A genuine 4:3 source keeps its aspect (letterboxed), not snapped.
        assert_eq!(capped_transcode_dims(2048, 1536), Some((1440, 1080)));
        // 720p (cam2) and 1080p already fit — never upscaled.
        assert_eq!(capped_transcode_dims(1280, 720), None);
        assert_eq!(capped_transcode_dims(1920, 1080), None);
        // Degenerate inputs are left alone.
        assert_eq!(capped_transcode_dims(0, 0), None);
        // Output dimensions are always even (H.264 requirement).
        let (w, h) = capped_transcode_dims(2688, 1520).unwrap();
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn transcode_desc_h264() {
        let d = transcode_pipeline_desc(CodecKind::H264, "vah264enc", "vah264dec");
        assert!(d.contains("h264parse"), "{d}");
        assert!(d.contains("vah264dec"), "{d}");
        assert!(d.contains("vah264enc name=enc"), "{d}");
        assert!(d.contains("key-int-max=96"), "{d}");
        assert!(d.contains("rate-control=cbr"), "{d}");
        assert!(d.contains("b-frames=0"), "{d}");
        // Bounded CPB (Kbits) keeps IDRs inside the GCC-paced budget.
        assert!(d.contains("cpb-size=1000"), "{d}");
        assert!(d.contains("videorate"), "{d}");
        assert!(d.contains("capsfilter name=ratecaps"), "{d}");
        assert!(d.contains("name=dec"), "{d}");
        assert!(d.contains("encoding-name=H264"), "{d}");
        assert!(d.contains("webrtcbin name=webrtc"), "{d}");
        // Leaky egress queue sits after the encoder/parser, before the
        // payloader, so pacer back-pressure never reaches the source.
        assert!(d.contains("queue name=egress leaky=downstream"), "{d}");
    }

    #[test]
    fn transcode_desc_legacy_vaapi_uses_matching_props() {
        // The legacy gstreamer-vaapi element takes different property names;
        // the launch string must branch so it stays valid on those boxes.
        let d = transcode_pipeline_desc(CodecKind::H264, "vaapih264enc", "vaapih264dec");
        assert!(d.contains("vaapih264enc name=enc"), "{d}");
        assert!(d.contains("keyframe-period=96"), "{d}");
        assert!(d.contains("max-bframes=0"), "{d}");
        assert!(d.contains("cpb-length=1000"), "{d}");
        assert!(d.contains("rate-control=cbr"), "{d}");
        // Must NOT leak the va-plugin-only property names.
        assert!(!d.contains("key-int-max"), "{d}");
        assert!(!d.contains("cpb-size"), "{d}");
    }

    #[test]
    fn transcode_desc_h265_outputs_h264() {
        // An H.265 camera is decoded then re-encoded to H.264 for the browser.
        let d = transcode_pipeline_desc(CodecKind::H265Plus, "vah264enc", "vah265dec");
        assert!(d.contains("h265parse"), "{d}");
        assert!(d.contains("vah265dec"), "{d}");
        assert!(d.contains("vah264enc name=enc"), "{d}");
        assert!(d.contains("encoding-name=H264"), "{d}");
    }

    #[test]
    fn bitrate_controller_backs_off_on_loss() {
        let mut c = BitrateController::new();
        assert_eq!(c.kbps, CC_START_KBPS);
        // Loss above the high threshold -> multiplicative decrease.
        let after = c.update(0.20);
        assert_eq!(after, (CC_START_KBPS as f64 * CC_DECREASE_FACTOR) as u32);
        // Sustained loss drives it to the floor, never below.
        for _ in 0..20 {
            c.update(0.30);
        }
        assert_eq!(c.kbps, CC_MIN_KBPS);
    }

    #[test]
    fn bitrate_controller_probes_up_when_clean() {
        let mut c = BitrateController::new();
        // Loss below the low threshold -> additive increase.
        assert_eq!(c.update(0.0), CC_START_KBPS + CC_INCREASE_KBPS);
        // A clean path drives it to the ceiling, never above.
        for _ in 0..50 {
            c.update(0.0);
        }
        assert_eq!(c.kbps, CC_MAX_KBPS);
    }

    #[test]
    fn bitrate_controller_holds_in_deadband() {
        let mut c = BitrateController::new();
        // Loss between LOW and HIGH -> hold steady.
        assert_eq!(c.update(0.05), CC_START_KBPS);
        assert_eq!(c.kbps, CC_START_KBPS);
    }

    #[test]
    fn stun_url_normalisation() {
        assert_eq!(
            stun_url_for("stun:host:3478").as_deref(),
            Some("stun://host:3478")
        );
        assert_eq!(
            stun_url_for("stun://host:3478").as_deref(),
            Some("stun://host:3478")
        );
        assert_eq!(stun_url_for("turn:host:3478"), None);
        assert_eq!(stun_url_for("stun:"), None);
    }

    #[test]
    fn turn_url_composition() {
        assert_eq!(
            turn_url_for("turn:host:3478", Some("u"), Some("p")).as_deref(),
            Some("turn://u:p@host:3478")
        );
        // `//` prefix and a `?transport=` query are both stripped.
        assert_eq!(
            turn_url_for("turns://host:5349?transport=tcp", Some("u"), Some("p")).as_deref(),
            Some("turns://u:p@host:5349")
        );
        // Missing credentials → no URL (webrtcbin would reject it anyway).
        assert_eq!(turn_url_for("turn:host:3478", None, Some("p")), None);
        assert_eq!(turn_url_for("turn:host:3478", Some("u"), None), None);
        // Non-TURN scheme.
        assert_eq!(turn_url_for("stun:host:3478", Some("u"), Some("p")), None);
    }
}
