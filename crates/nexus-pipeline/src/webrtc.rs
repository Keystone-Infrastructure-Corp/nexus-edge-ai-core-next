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

use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;

use nexus_types::{CameraId, CodecKind};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::gst_clip_recorder::contains_slice_nal;
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
    appsrc: AppSrc,
    events: mpsc::UnboundedSender<WebRtcEvent>,
    /// Spawned by `new_publisher` once the pipeline is PLAYING, so the
    /// ring-seed prepend lands in an appsrc that already accepts buffers.
    feed: Option<JoinHandle<()>>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new_publisher(
        session_id: String,
        camera_id: CameraId,
        codec: CodecKind,
        mode: WebRtcMode,
        ice_servers: &[IceServerCfg],
        nal_rx: broadcast::Receiver<NalSample>,
        seed: Vec<NalSample>,
        events: mpsc::UnboundedSender<WebRtcEvent>,
    ) -> Result<Self, WebRtcError> {
        let mut sess = Self::build_common(session_id, camera_id, codec, mode, ice_servers, events)?;
        sess.wire_offerer();
        // Bring the pipeline up so `webrtcbin` opens its peer-connection,
        // fires `on-negotiation-needed`, and begins gathering ICE.
        sess.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| WebRtcError::State(format!("set Playing: {e}")))?;
        // Spawn the NAL feed only AFTER the pipeline is live: the ring-seed
        // prepend is pushed immediately (unlike the live path, which blocks on
        // `recv`), so the appsrc must already be accepting buffers.
        sess.feed = Some(tokio::spawn(feed_loop(
            sess.appsrc.clone(),
            nal_rx,
            seed,
            codec,
            camera_id,
        )));
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
        events: mpsc::UnboundedSender<WebRtcEvent>,
    ) -> Result<Self, WebRtcError> {
        if mode == WebRtcMode::Transcode {
            return Err(WebRtcError::Unsupported(
                "transcode fallback is not implemented in Phase E (passthrough only)".to_string(),
            ));
        }
        gst::init().map_err(|e| WebRtcError::Init(e.to_string()))?;

        let desc = passthrough_pipeline_desc(codec);
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| WebRtcError::Build(format!("parse::launch: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| WebRtcError::Build("downcast Pipeline".to_string()))?;

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| WebRtcError::Build("appsrc 'src' missing".to_string()))?
            .downcast::<AppSrc>()
            .map_err(|_| WebRtcError::Build("downcast AppSrc".to_string()))?;
        let webrtc = pipeline
            .by_name("webrtc")
            .ok_or_else(|| WebRtcError::Build("webrtcbin 'webrtc' missing".to_string()))?;

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

        // The NAL feed (with its ring-seed prepend) is spawned by the caller
        // once the pipeline is PLAYING — see `new_publisher`.
        Ok(Self {
            session_id,
            camera_id,
            codec,
            pipeline,
            webrtc,
            appsrc,
            events,
            feed: None,
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
        if let Some(feed) = &self.feed {
            feed.abort();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Pump compressed NAL samples into `appsrc`, seeding from a ring snapshot.
///
/// `seed` is the ingester's ring snapshot taken at subscribe time — the
/// current GOP, which by the ring invariant starts at a keyframe. Pushing it
/// first hands `h264parse` its SPS/PPS and the browser's decoder an intra
/// frame *immediately*, instead of waiting up to a full camera GOP (2–15 s)
/// for the next natural IDR. That SPS/PPS is what unblocks `webrtcbin`'s
/// offer (the payloader's caps must resolve before `on-negotiation-needed`
/// fires), so the seed collapses the dominant first-frame cost. Live samples
/// already covered by the seed are de-duped by PTS.
///
/// Without a usable seed (pre-roll disabled, or a snapshot that doesn't start
/// on a keyframe) this falls back to the original behaviour: splice in at the
/// next live keyframe, dropping delta frames until an IDR so a mid-GOP
/// subscribe never feeds the browser a broken reference frame. A broadcast
/// lag re-arms the splice.
async fn feed_loop(
    appsrc: AppSrc,
    mut rx: broadcast::Receiver<NalSample>,
    seed: Vec<NalSample>,
    codec: CodecKind,
    camera_id: CameraId,
) {
    let mut started = false;
    let mut last_seeded_pts: Option<Duration> = None;

    // Trim trailing header-only samples (AUD/SEI with no slice NAL) off the
    // seed: the matching slice for that access unit may still be in flight on
    // the live channel with the SAME pts, and the de-dup below
    // (`pts <= last_seeded_pts`) would drop it — orphaning the header and
    // smearing the picture until the next IDR. Trimming lets the complete AU
    // arrive intact on the live path.
    let seed_end = seed
        .iter()
        .rposition(|s| contains_slice_nal(&s.data, codec));
    if let Some(end) = seed_end {
        if seed.first().is_some_and(|s| s.is_keyframe) {
            let mut to_push = seed;
            to_push.truncate(end + 1);
            let seeded_pts = to_push.iter().filter_map(|s| s.pts).next_back();
            let n = to_push.len();
            // `block=true` appsrc can stall on downstream backpressure; push
            // the (up to one whole GOP) seed off the tokio worker.
            let appsrc_seed = appsrc.clone();
            match tokio::task::spawn_blocking(move || {
                for sample in &to_push {
                    push_nal(&appsrc_seed, sample)?;
                }
                Ok::<(), String>(())
            })
            .await
            {
                Ok(Ok(())) => {
                    started = true;
                    last_seeded_pts = seeded_pts;
                    debug!(
                        camera_id,
                        seeded = n,
                        "webrtc feed: seeded from ring keyframe"
                    );
                }
                Ok(Err(e)) => {
                    // Fall back to the live splice rather than aborting — a
                    // failed seed must not leave the session with no media.
                    debug!(camera_id, error = %e, "webrtc feed: seed push failed; live splice");
                }
                Err(e) => {
                    debug!(camera_id, error = %e, "webrtc feed: seed task join failed; live splice");
                }
            }
        }
    }

    loop {
        match rx.recv().await {
            Ok(sample) => {
                if !started {
                    if !sample.is_keyframe {
                        continue;
                    }
                    started = true;
                    debug!(camera_id, "webrtc feed: spliced in at keyframe");
                } else if let (Some(spts), Some(lpts)) = (sample.pts, last_seeded_pts) {
                    // De-dup the seed↔live overlap: a ring sample can also
                    // arrive on the broadcast a moment later.
                    if spts <= lpts {
                        continue;
                    }
                }
                if let Err(e) = push_nal(&appsrc, &sample) {
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
                last_seeded_pts = None;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = appsrc.end_of_stream();
}

/// Copy one NAL sample into a `gst::Buffer` and push it into `appsrc`.
/// `appsrc` is configured `do-timestamp=true`, so we don't set PTS; we
/// only flag delta (non-key) frames for the downstream payloader.
fn push_nal(appsrc: &AppSrc, sample: &NalSample) -> Result<(), String> {
    let mut buf = gst::Buffer::with_size(sample.data.len()).map_err(|e| format!("alloc: {e}"))?;
    {
        let bm = buf.get_mut().ok_or("buffer not unique")?;
        let mut map = bm.map_writable().map_err(|e| format!("map: {e}"))?;
        map.copy_from_slice(&sample.data);
        drop(map);
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

/// Build the passthrough launch description for a camera codec. Pure so it
/// can be unit-tested without a GStreamer runtime.
fn passthrough_pipeline_desc(codec: CodecKind) -> String {
    let base = codec.base(); // "h264" | "h265"
    let encoding = if base == "h265" { "H265" } else { "H264" };
    // `config-interval=-1` on parse + pay repeats SPS/PPS with every IDR so
    // a mid-stream browser join can start decoding; `mtu=1200` keeps RTP
    // packets inside a conservative WebRTC MTU.
    //
    // Do NOT pin a `payload` on the RTP caps: the browser's offer assigns the
    // payload types (e.g. pt 96 → VP8, H264 at 102/104/…). If we hardcode
    // `payload=96`, webrtcbin looks for our H264/H265 at pt 96 — which the
    // browser mapped to VP8 — and reports "did not find compatible transceiver
    // for offer caps", so the answer carries no decodable video (blank HD).
    // Leaving the payload unset lets webrtcbin adopt the browser's H264/H265
    // payload type during answer negotiation.
    format!(
        "appsrc name=src is-live=true do-timestamp=true format=time \
             block=true max-bytes=8388608 stream-type=stream \
           ! {base}parse config-interval=-1 \
           ! rtp{base}pay name=pay pt=96 config-interval=-1 mtu=1200 \
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
