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
//! There is no camera force-keyframe plumbing on the edge yet (see the cloud
//! repo's `docs/cloud-console/PHASE_10_LIVE_VIEW.md`, Phase E), so the feed
//! **splices in at the next keyframe** — it drops delta frames until it sees
//! an IDR, then starts pushing. That guarantees the browser's decoder begins
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
use tracing::{debug, warn};

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
                ratecaps.set_property("caps", va_framerate_caps(DEFAULT_TRANSCODE_FPS));
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
                            let declared = caps_ev
                                .caps()
                                .structure(0)
                                .and_then(|s| s.get::<gst::Fraction>("framerate").ok())
                                .filter(|fr| fr.numer() > 0 && fr.denom() > 0)
                                .map(|fr| (fr.numer() / fr.denom()).clamp(1, MAX_TRANSCODE_FPS));
                            if let Some(fps) = declared {
                                if fps != DEFAULT_TRANSCODE_FPS {
                                    ratecaps.set_property("caps", va_framerate_caps(fps));
                                    debug!(
                                        camera_id,
                                        fps, "webrtc transcode adopted camera framerate"
                                    );
                                }
                                return gst::PadProbeReturn::Remove;
                            }
                            gst::PadProbeReturn::Ok
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

        // Adaptive bitrate for the transcode path: track the browser's RTCP
        // loss feedback and step the encoder bitrate to fit the uplink.
        // Passthrough builds have no `enc` element and skip this.
        let cc = if transcoding {
            pipeline
                .by_name("enc")
                .map(|enc| spawn_congestion_control(webrtc.clone(), enc, camera_id))
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
        })
    }

    /// Wire the publisher/offerer negotiation: on `on-negotiation-needed`
    /// create an offer, adopt it locally, and — once ICE gathering completes
    /// — emit [`WebRtcEvent::Offer`] with the candidates baked into the SDP.
    /// Also emit [`WebRtcEvent::Connected`] when the peer connection is up.
    fn wire_offerer(&self) {
        let codec_label = self.codec.base();

        // create-offer → set-local → emit once gathered.
        let webrtc_neg = self.webrtc.clone();
        let events_neg = self.events.clone();
        let session_neg = self.session_id.clone();
        let emitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.webrtc
            .connect("on-negotiation-needed", false, move |_vals| {
                let webrtc = webrtc_neg.clone();
                let events = events_neg.clone();
                let session = session_neg.clone();
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
                    webrtc_for_local.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer, &None::<gst::Promise>],
                    );
                    emit_offer_when_gathered(
                        &webrtc_for_local,
                        &events,
                        &session,
                        codec_label,
                        &emitted,
                    );
                });
                webrtc
                    .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &offer_promise]);
                None
            });

        // connection established → Connected (publishing).
        let events_conn = self.events.clone();
        let session_conn = self.session_id.clone();
        let signalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.webrtc
            .connect("notify::connection-state", false, move |vals| {
                let wb = vals.first().and_then(|v| v.get::<gst::Element>().ok())?;
                let state =
                    wb.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
                match state {
                    gst_webrtc::WebRTCPeerConnectionState::Connected
                        if !signalled.swap(true, std::sync::atomic::Ordering::SeqCst) =>
                    {
                        debug!(session = %session_conn, "webrtc publisher connected");
                        let _ = events_conn.send(WebRtcEvent::Connected);
                    }
                    gst_webrtc::WebRTCPeerConnectionState::Failed => {
                        let _ = events_conn
                            .send(WebRtcEvent::Failed("peer connection failed".to_string()));
                    }
                    _ => {}
                }
                None
            });
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
}

impl Drop for WebRtcSession {
    fn drop(&mut self) {
        self.feed.abort();
        if let Some(cc) = &self.cc {
            cc.abort();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
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
async fn feed_loop(appsrc: AppSrc, mut rx: broadcast::Receiver<NalSample>, camera_id: CameraId) {
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
                if let Err(e) = push_nal(&appsrc, &sample, &mut clock) {
                    warn!(camera_id, error = %e, "webrtc feed: push failed; ending");
                    break;
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

/// Copy one NAL sample into a `gst::Buffer`, stamp it with the [`FeedClock`]'s
/// monotonic PTS/DTS, and push it into `appsrc`. `appsrc` runs
/// `do-timestamp=false` so these explicit timestamps drive `rtph264pay`'s RTP
/// clock — see [`FeedClock`] for why auto-timestamping breaks frame assembly.
fn push_nal(appsrc: &AppSrc, sample: &NalSample, clock: &mut FeedClock) -> Result<(), String> {
    let ts = gst::ClockTime::from_nseconds(clock.resolve(sample).as_nanos() as u64);
    let mut buf = gst::Buffer::with_size(sample.data.len()).map_err(|e| format!("alloc: {e}"))?;
    {
        let bm = buf.get_mut().ok_or("buffer not unique")?;
        let mut map = bm.map_writable().map_err(|e| format!("map: {e}"))?;
        map.copy_from_slice(&sample.data);
        drop(map);
        // Baseline/main-profile IP-camera live streams carry no B-frames, so
        // DTS == PTS; setting both keeps `rtph264pay` from inferring a bogus
        // reorder delay.
        bm.set_pts(ts);
        bm.set_dts(ts);
        if !sample.is_keyframe {
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

/// A VAMemory raw-video caps fixing only the output `framerate`, used to drive
/// `videorate`'s constant-rate conversion via the `ratecaps` capsfilter.
fn va_framerate_caps(fps: i32) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .features(["memory:VAMemory"])
        .field("framerate", gst::Fraction::new(fps, 1))
        .build()
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

/// Read the rtpbin session's `twcc-stats` (Phase b probe). Non-empty
/// `packets`/`bitrate-recv` here means the receiver (Cloudflare SFU) is
/// returning transport-wide-cc feedback — the signal `rtpgccbwe` depends on.
/// Returns `None` before session 0 exists or if the property is unavailable.
fn read_twcc_stats(webrtc: &gst::Element) -> Option<gst::Structure> {
    // NB: webrtcbin's ChildProxy exposes its *transceivers* by name, not the
    // internal elements — so `child_by_name("rtpbin")` returns None. webrtcbin
    // IS a GstBin, so reach the internal rtpbin via `Bin::by_name` instead.
    let rtpbin = webrtc.dynamic_cast_ref::<gst::Bin>()?.by_name("rtpbin")?;
    let session = rtpbin.emit_by_name::<Option<gst::Element>>("get-session", &[&0u32])?;
    session
        .find_property("twcc-stats")
        .map(|_| session.property::<gst::Structure>("twcc-stats"))
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
            match read_twcc_stats(&webrtc) {
                Some(tw) => debug!(camera_id, twcc = %tw.to_string(), "webrtc twcc-stats (probe)"),
                None => debug!(
                    camera_id,
                    "webrtc twcc-stats (probe): unavailable (rtpbin/session/prop lookup)"
                ),
            }
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

/// Name of an available hardware H.264 **encoder** for the transcode path, or
/// `None` on boxes without one (e.g. macOS dev), where the caller falls back
/// to passthrough. Pure aside from the plugin registry lookup.
fn hw_h264_encoder() -> Option<&'static str> {
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

/// Build a **transcode** launch description: HW-decode the camera codec and
/// re-encode to H.264 with a short GOP for smooth WebRTC over a lossy network.
///
/// Camera streams often use a multi-second GOP (this InSight uses ~15s). Over
/// WebRTC (UDP) a single lost packet corrupts every delta frame until the next
/// keyframe, so a long GOP means multi-second freezes on ~1% loss. Re-encoding
/// with `key-int-max=48` (~2s @ 24fps), CBR, and no B-frames caps the recovery
/// window at ~2s while leaving the camera untouched. The output is always
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
    format!(
        "appsrc name=src is-live=true do-timestamp=false format=time \
             block=true max-bytes=8388608 stream-type=stream \
           ! {base}parse \
           ! {decoder} name=dec \
           ! vapostproc \
           ! videorate \
           ! capsfilter name=ratecaps \
           ! {encoder} name=enc key-int-max=48 b-frames=0 rate-control=cbr \
             bitrate={bitrate_kbps} target-usage=6 \
           ! h264parse config-interval=1 \
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

    #[test]
    fn passthrough_desc_h264() {
        let d = passthrough_pipeline_desc(CodecKind::H264);
        assert!(d.contains("h264parse"), "{d}");
        assert!(d.contains("rtph264pay"), "{d}");
        assert!(d.contains("encoding-name=H264"), "{d}");
        assert!(d.contains("webrtcbin name=webrtc"), "{d}");
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
    fn transcode_desc_h264() {
        let d = transcode_pipeline_desc(CodecKind::H264, "vah264enc", "vah264dec");
        assert!(d.contains("h264parse"), "{d}");
        assert!(d.contains("vah264dec"), "{d}");
        assert!(d.contains("vah264enc name=enc"), "{d}");
        assert!(d.contains("key-int-max=48"), "{d}");
        assert!(d.contains("rate-control=cbr"), "{d}");
        assert!(d.contains("b-frames=0"), "{d}");
        assert!(d.contains("videorate"), "{d}");
        assert!(d.contains("capsfilter name=ratecaps"), "{d}");
        assert!(d.contains("name=dec"), "{d}");
        assert!(d.contains("encoding-name=H264"), "{d}");
        assert!(d.contains("webrtcbin name=webrtc"), "{d}");
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
