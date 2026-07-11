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
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;

use nexus_types::{CameraId, CodecKind};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

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

/// An outbound signalling artefact produced by the edge answerer. The
/// caller (the Phase F tunnel manager) forwards these to the cloud as
/// `webrtc_answer` / `webrtc_ice_candidate` envelopes.
#[derive(Debug, Clone)]
pub enum WebRtcEvent {
    /// The local SDP answer is ready.
    Answer {
        /// The answer SDP text.
        sdp: String,
        /// Negotiated video codec label (`"h264"` / `"h265"`).
        codec: &'static str,
    },
    /// A local ICE candidate was gathered.
    IceCandidate {
        /// The media-line index the candidate belongs to.
        sdp_mline_index: u32,
        /// The candidate attribute value (`candidate:…`).
        candidate: String,
    },
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
}

impl WebRtcSession {
    /// Build the passthrough sub-pipeline for one camera and start pumping
    /// its compressed NAL stream into the pipeline. Negotiation does not
    /// begin until [`WebRtcSession::accept_offer`] is called.
    ///
    /// `nal_rx` is a fresh subscription to the camera's
    /// [`crate::preroll_ingester::PreRollIngester`] broadcast; `codec` is
    /// that ingester's `codec()`.
    pub fn new(
        session_id: String,
        camera_id: CameraId,
        codec: CodecKind,
        mode: WebRtcMode,
        ice_servers: &[IceServerCfg],
        nal_rx: broadcast::Receiver<NalSample>,
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

        // Local ICE candidates → events (relayed to the browser via cloud).
        let ice_tx = events.clone();
        webrtc.connect("on-ice-candidate", false, move |vals| {
            let mline = vals.get(1).and_then(|v| v.get::<u32>().ok()).unwrap_or(0);
            let candidate = vals
                .get(2)
                .and_then(|v| v.get::<String>().ok())
                .unwrap_or_default();
            let _ = ice_tx.send(WebRtcEvent::IceCandidate {
                sdp_mline_index: mline,
                candidate,
            });
            None
        });
        // We're the answerer, so we never create an offer on renegotiation;
        // log for diagnostics only.
        let sess = session_id.clone();
        webrtc.connect("on-negotiation-needed", false, move |_vals| {
            debug!(session = %sess, "webrtcbin on-negotiation-needed (answerer; ignored)");
            None
        });

        // Pump compressed NALs into appsrc, splicing in at the next IDR.
        let feed = tokio::spawn(feed_loop(appsrc.clone(), nal_rx, camera_id));

        Ok(Self {
            session_id,
            camera_id,
            codec,
            pipeline,
            webrtc,
            _appsrc: appsrc,
            events,
            feed,
        })
    }

    /// Apply the browser's SDP offer, create + adopt the local answer, and
    /// emit [`WebRtcEvent::Answer`]. Also transitions the pipeline to
    /// `Playing` so media starts flowing.
    pub fn accept_offer(&self, sdp_offer: &str) -> Result<(), WebRtcError> {
        let msg = gst_sdp::SDPMessage::parse_buffer(sdp_offer.as_bytes())
            .map_err(|e| WebRtcError::Sdp(format!("parse offer: {e}")))?;
        let offer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, msg);

        let webrtc_for_local = self.webrtc.clone();
        let events_ok = self.events.clone();
        let codec_label = self.codec.base();

        // Second stage: once the answer exists, adopt it locally + emit it.
        let answer_promise = gst::Promise::with_change_func(move |reply| {
            let answer = reply
                .ok()
                .flatten()
                .and_then(|s| s.get::<gst_webrtc::WebRTCSessionDescription>("answer").ok());
            let Some(answer) = answer else {
                let _ = events_ok.send(WebRtcEvent::Failed(
                    "create-answer: no answer in reply".to_string(),
                ));
                return;
            };
            webrtc_for_local
                .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
            match answer.sdp().as_text() {
                Ok(sdp) => {
                    let _ = events_ok.send(WebRtcEvent::Answer {
                        sdp,
                        codec: codec_label,
                    });
                }
                Err(e) => {
                    let _ = events_ok.send(WebRtcEvent::Failed(format!("answer as_text: {e}")));
                }
            }
        });

        // First stage: set the remote (offer); on success create the answer.
        let webrtc_for_answer = self.webrtc.clone();
        let events_err = self.events.clone();
        let remote_promise = gst::Promise::with_change_func(move |reply| {
            if let Err(e) = reply {
                let _ = events_err.send(WebRtcEvent::Failed(format!(
                    "set-remote-description: {e:?}"
                )));
                return;
            }
            webrtc_for_answer
                .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &answer_promise]);
        });

        // Bring the pipeline up BEFORE applying the offer. `webrtcbin` only
        // opens its internal peer-connection once it has reached at least the
        // READY state; emitting `set-remote-description` / `create-answer`
        // while the bin is still in NULL makes webrtcbin abort both async
        // tasks with "Peerconnection is closed, aborting execution", so the
        // create-answer promise resolves empty ("no answer in reply") and the
        // browser hangs on "Negotiating HD". Starting the pipeline first keeps
        // the SDP exchange on an open peer-connection.
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| WebRtcError::State(format!("set Playing: {e}")))?;

        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&offer, &remote_promise]);

        Ok(())
    }

    /// Add a remote ICE candidate received from the browser (via cloud).
    pub fn add_ice_candidate(&self, sdp_mline_index: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
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
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Pump compressed NAL samples from the ingester broadcast into `appsrc`.
///
/// Splices in at the next keyframe (drops delta frames until an IDR) so a
/// mid-GOP subscribe never feeds the browser a broken reference frame. A
/// broadcast lag re-arms the splice (we may have dropped the frames between
/// the last IDR and now).
async fn feed_loop(appsrc: AppSrc, mut rx: broadcast::Receiver<NalSample>, camera_id: CameraId) {
    let mut started = false;
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

/// Build the passthrough launch description for a camera codec. Pure so it
/// can be unit-tested without a GStreamer runtime.
fn passthrough_pipeline_desc(codec: CodecKind) -> String {
    let base = codec.base(); // "h264" | "h265"
    let encoding = if base == "h265" { "H265" } else { "H264" };
    // `config-interval=-1` on parse + pay repeats SPS/PPS with every IDR so
    // a mid-stream browser join can start decoding; `mtu=1200` keeps RTP
    // packets inside a conservative WebRTC MTU.
    format!(
        "appsrc name=src is-live=true do-timestamp=true format=time \
             block=true max-bytes=8388608 stream-type=stream \
           ! {base}parse config-interval=-1 \
           ! rtp{base}pay name=pay pt=96 config-interval=-1 mtu=1200 \
           ! application/x-rtp,media=video,encoding-name={encoding},payload=96,clock-rate=90000 \
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

    /// End-to-end answerer flow against a real `webrtcbin`. Requires the
    /// GStreamer runtime + gst-plugins-bad `webrtc` element + libnice, so
    /// it's `#[ignore]`d in the default run and executed manually with
    /// `cargo test -p nexus-pipeline --features gstreamer-webrtc -- --ignored`
    /// on a host that has GStreamer installed.
    #[tokio::test]
    #[ignore = "needs a live GStreamer webrtcbin runtime"]
    async fn answer_from_canned_offer() {
        let (_nal_tx, nal_rx) = broadcast::channel::<NalSample>(8);
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<WebRtcEvent>();

        let session = WebRtcSession::new(
            "test-session-1".to_string(),
            1,
            CodecKind::H264,
            WebRtcMode::Passthrough,
            &[],
            nal_rx,
            ev_tx,
        )
        .expect("build session");

        session
            .accept_offer(CANNED_H264_OFFER)
            .expect("accept offer");

        let evt = tokio::time::timeout(std::time::Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("answer within 5s")
            .expect("event channel open");
        match evt {
            WebRtcEvent::Answer { sdp, codec } => {
                assert_eq!(codec, "h264");
                assert!(sdp.contains("m=video"), "answer sdp: {sdp}");
            }
            other => panic!("expected an Answer, got {other:?}"),
        }
    }

    /// A minimal browser-style recvonly H.264 offer (dummy but well-formed
    /// fingerprint; DTLS never runs during answer creation).
    const CANNED_H264_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:sTmA\r\n\
a=ice-pwd:1TS7iGCGqZLtVQjSVGodpAsr\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 \
AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:\
AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";
}
