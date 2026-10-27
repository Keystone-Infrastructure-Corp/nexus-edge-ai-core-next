//! Live talk-down receiver (ADR-091 §5): pull an operator's microphone track
//! from the SFU with a receive-only `webrtcbin` and hand it out as 20 ms
//! frames of 8 kHz mono PCM, ready for a G.711 speaker call.
//!
//! ```text
//!   webrtcbin (recvonly audio) ──pad-added──▶ rtpopusdepay ! opusdec
//!     ! audioconvert ! audioresample ! audio/x-raw,S16LE,8000,mono
//!     ! appsink ──160-sample frames──▶ mpsc::Receiver<Vec<i16>>
//! ```
//!
//! The SFU is the offerer. [`TalkdownReceiver::answer`] takes its offer and
//! returns the answer with every gathered candidate baked in, because the
//! answer travels back in one RPC response (no trickle).
//!
//! Gated behind `gstreamer-webrtc`, like [`crate::webrtc`], whose ICE server
//! handling it shares.

use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::warn;

use crate::webrtc::{apply_ice_servers, IceServerCfg, WebRtcError};

/// Samples in one 20 ms frame at 8 kHz: one G.711 RTP packet.
const FRAME_SAMPLES: usize = 160;
/// One second of frames waits for a reader that fell behind; past it, frames
/// are dropped, since late talk-down audio is worse than a gap.
const BACKLOG_FRAMES: usize = 50;
const DECODE: &str = "rtpopusdepay ! opusdec ! audioconvert ! audioresample \
     ! audio/x-raw,format=S16LE,rate=8000,channels=1 ! appsink name=pcm sync=false";

/// One live talk-down leg. Dropping it tears the pipeline down, which closes
/// its frame channel.
pub struct TalkdownReceiver {
    pipeline: gst::Pipeline,
}

impl TalkdownReceiver {
    /// Answer the SFU's `offer_sdp` with a receive-only audio leg. Returns the
    /// answer SDP once ICE gathering completes, and the channel the decoded
    /// frames arrive on once media flows. Fails if that takes over `timeout`.
    /// Errors never contain the SDP: it carries ICE credentials.
    pub async fn answer(
        offer_sdp: &str,
        ice_servers: &[IceServerCfg],
        timeout: Duration,
    ) -> Result<(Self, String, mpsc::Receiver<Vec<i16>>), WebRtcError> {
        gst::init().map_err(|e| WebRtcError::Init(e.to_string()))?;
        let offer = gst_sdp::SDPMessage::parse_buffer(offer_sdp.as_bytes())
            .map_err(|e| WebRtcError::Sdp(format!("parse offer: {e}")))?;
        if !offer.medias().any(|m| m.media() == Some("audio")) {
            return Err(WebRtcError::Sdp("the offer has no audio".into()));
        }
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()
            .map_err(|e| WebRtcError::Build(format!("webrtcbin: {e}")))?;
        apply_ice_servers(&webrtc, ice_servers);
        let opus = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48_000i32)
            .build();
        webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &opus],
        );
        let (tx, rx) = mpsc::channel(BACKLOG_FRAMES);
        // The element arrives as the argument: a closure that captured it would
        // make the webrtcbin own itself, and it would never be freed (BUG-136).
        webrtc.connect_pad_added(move |webrtc, pad| {
            if let Err(e) = decode_into(webrtc, pad, tx.clone()) {
                warn!(error = %e, "talk-down receiver could not decode a track");
            }
        });
        let pipeline = gst::Pipeline::new();
        pipeline
            .add(&webrtc)
            .map_err(|e| WebRtcError::Build(e.to_string()))?;
        let leg = Self { pipeline };
        leg.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| WebRtcError::State(format!("set Playing: {e}")))?;
        let answer = tokio::time::timeout(timeout, negotiate(&webrtc, offer))
            .await
            .map_err(|_| WebRtcError::State(format!("no answer within {timeout:?}")))??;
        Ok((leg, answer, rx))
    }
}

impl Drop for TalkdownReceiver {
    fn drop(&mut self) {
        crate::teardown::null_pipeline_detached(
            self.pipeline.clone(),
            "talkdown_rx::TalkdownReceiver::drop",
            None,
        );
    }
}

async fn negotiate(
    webrtc: &gst::Element,
    offer: gst_sdp::SDPMessage,
) -> Result<String, WebRtcError> {
    let offer = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, offer);
    promised(webrtc, "set-remote-description", &offer).await?;
    let answer = promised(webrtc, "create-answer", &None::<gst::Structure>)
        .await?
        .and_then(|s| s.get::<gst_webrtc::WebRTCSessionDescription>("answer").ok())
        .ok_or_else(|| WebRtcError::Sdp("create-answer: no answer".into()))?;
    local_sdp_when_gathered(webrtc, &answer).await
}

/// Adopt `desc` as the local description and return it once ICE gathering
/// completes, with every candidate in it.
async fn local_sdp_when_gathered(
    webrtc: &gst::Element,
    desc: &gst_webrtc::WebRTCSessionDescription,
) -> Result<String, WebRtcError> {
    let gathered = Arc::new(Notify::new());
    let notify = gathered.clone();
    webrtc.connect_notify(Some("ice-gathering-state"), move |webrtc, _| {
        let state = webrtc.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        if state == gst_webrtc::WebRTCICEGatheringState::Complete {
            notify.notify_one();
        }
    });
    promised(webrtc, "set-local-description", desc).await?;
    gathered.notified().await;
    let local = webrtc
        .property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")
        .ok_or_else(|| WebRtcError::Sdp("no local description".into()))?;
    local
        .sdp()
        .as_text()
        .map_err(|e| WebRtcError::Sdp(format!("as_text: {e}")))
}

/// Emit a `webrtcbin` action that answers through a promise and await the
/// reply. Not `async fn`: `arg` must not live across the await.
fn promised(
    webrtc: &gst::Element,
    action: &'static str,
    arg: &dyn ToValue,
) -> impl std::future::Future<Output = Result<Option<gst::Structure>, WebRtcError>> {
    let (tx, rx) = oneshot::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let _ = tx.send(reply.map(|s| s.map(ToOwned::to_owned)));
    });
    webrtc.emit_by_name::<()>(action, &[arg, &promise]);
    async move {
        let reply = rx
            .await
            .map_err(|_| WebRtcError::State(format!("{action}: dropped unanswered")))?
            .map_err(|e| WebRtcError::State(format!("{action}: {e:?}")))?;
        match reply.as_ref().map(|s| s.get::<gst::glib::Error>("error")) {
            Some(Ok(e)) => Err(WebRtcError::Sdp(format!("{action}: {e}"))),
            _ => Ok(reply),
        }
    }
}

/// Link a new `webrtcbin` source pad to a decoder whose appsink cuts the PCM
/// into 160-sample frames and sends them to `tx`.
fn decode_into(
    webrtc: &gst::Element,
    pad: &gst::Pad,
    tx: mpsc::Sender<Vec<i16>>,
) -> Result<(), String> {
    if pad.direction() != gst::PadDirection::Src {
        return Ok(());
    }
    let parent = webrtc
        .parent()
        .and_downcast::<gst::Bin>()
        .ok_or("webrtcbin has no parent bin")?;
    let bin = gst::parse::bin_from_description(DECODE, true).map_err(|e| e.to_string())?;
    let sink = bin
        .by_name("pcm")
        .and_downcast::<AppSink>()
        .ok_or("no appsink")?;
    let mut pending: Vec<i16> = Vec::with_capacity(2 * FRAME_SAMPLES);
    sink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                pending.extend(
                    map.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b)),
                );
                while pending.len() >= FRAME_SAMPLES {
                    let frame = pending.drain(..FRAME_SAMPLES).collect();
                    let _ = tx.try_send(frame);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    parent.add(&bin).map_err(|e| e.to_string())?;
    bin.sync_state_with_parent().map_err(|e| e.to_string())?;
    let sink_pad = bin.static_pad("sink").ok_or("decoder has no sink pad")?;
    pad.link(&sink_pad).map_err(|e| e.to_string())?;
    Ok(())
}

/// Stands in for the SFU in tests: a send-only Opus tone that makes the
/// offer. Shared with `nexus-engine`'s talk-down endpoint tests, which have no
/// GStreamer bindings of their own; not for production use.
#[doc(hidden)]
pub struct ToneSender {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
}

#[doc(hidden)]
impl ToneSender {
    /// Start a 440 Hz tone and return its offer, host candidates baked in.
    pub async fn offer() -> Result<(Self, String), WebRtcError> {
        gst::init().map_err(|e| WebRtcError::Init(e.to_string()))?;
        let pipeline = gst::parse::launch(
            "audiotestsrc is-live=true wave=sine freq=440 volume=0.5 ! audioconvert \
             ! audioresample ! opusenc ! rtpopuspay \
             ! application/x-rtp,media=audio,encoding-name=OPUS,payload=111 \
             ! webrtcbin name=webrtc bundle-policy=max-bundle",
        )
        .map_err(|e| WebRtcError::Build(e.to_string()))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| WebRtcError::Build("downcast Pipeline".into()))?;
        let webrtc = pipeline
            .by_name("webrtc")
            .ok_or_else(|| WebRtcError::Build("no webrtcbin".into()))?;
        // Loopback only: macOS Local Network privacy sometimes drops a test
        // binary's datagrams to its own LAN address, which stalled ICE.
        let ice = webrtc.property::<gst::glib::Object>("ice-agent");
        ice.emit_by_name::<bool>("add-local-ip-address", &[&"127.0.0.1"]);
        let sender = Self { pipeline, webrtc };
        sender
            .webrtc
            .emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>("get-transceiver", &[&0i32])
            .set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );
        sender
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| WebRtcError::State(format!("set Playing: {e}")))?;
        let offer = promised(&sender.webrtc, "create-offer", &None::<gst::Structure>)
            .await?
            .and_then(|s| s.get::<gst_webrtc::WebRTCSessionDescription>("offer").ok())
            .ok_or_else(|| WebRtcError::Sdp("create-offer: no offer".into()))?;
        let sdp = local_sdp_when_gathered(&sender.webrtc, &offer).await?;
        Ok((sender, sdp))
    }

    pub async fn set_answer(&self, answer_sdp: &str) -> Result<(), WebRtcError> {
        let msg = gst_sdp::SDPMessage::parse_buffer(answer_sdp.as_bytes())
            .map_err(|e| WebRtcError::Sdp(format!("parse answer: {e}")))?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, msg);
        promised(&self.webrtc, "set-remote-description", &answer)
            .await
            .map(drop)
    }
}

impl Drop for ToneSender {
    fn drop(&mut self) {
        crate::teardown::null_pipeline_detached(
            self.pipeline.clone(),
            "talkdown_rx::ToneSender::drop",
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, timeout_at, Instant};

    /// The live path needs libnice's `nicesrc`/`nicesink`, which CI's
    /// `system-libs` job installs; a host without them reports a skip.
    fn missing_elements() -> Vec<&'static str> {
        if gst::init().is_err() {
            return vec!["gstreamer"];
        }
        [
            "webrtcbin",
            "nicesrc",
            "nicesink",
            "audiotestsrc",
            "opusenc",
            "opusdec",
            "rtpopuspay",
            "rtpopusdepay",
            "audioresample",
            "appsink",
        ]
        .into_iter()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn talkdown_receiver_answers_a_loopback_offer_and_yields_8khz_frames() {
        let missing = missing_elements();
        if !missing.is_empty() {
            eprintln!("SKIPPED: GStreamer elements missing: {missing:?}");
            return;
        }
        let (sender, offer) = ToneSender::offer().await.unwrap();
        assert!(offer.contains("a=sendonly"), "{offer}");

        let (leg, answer, mut frames) =
            TalkdownReceiver::answer(&offer, &[], Duration::from_secs(5))
                .await
                .unwrap();
        assert!(answer.contains("a=recvonly"), "{answer}");
        assert!(
            answer.contains("a=candidate:"),
            "candidates baked in: {answer}"
        );
        sender.set_answer(&answer).await.unwrap();

        // Opus starts with a few quiet frames; the tone (about -6 dBFS) follows.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let frame = timeout_at(deadline, frames.recv())
                .await
                .expect("a non-silent frame within 5 s")
                .expect("channel open while the leg lives");
            assert_eq!(frame.len(), FRAME_SAMPLES);
            if frame.iter().map(|s| s.unsigned_abs()).max() > Some(4_000) {
                break;
            }
        }

        drop(leg);
        let closed = timeout(Duration::from_secs(5), async {
            while frames.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "dropping the leg closes its frame channel");
    }

    #[tokio::test]
    async fn talkdown_receiver_refuses_an_offer_without_audio() {
        let missing = missing_elements();
        if !missing.is_empty() {
            eprintln!("SKIPPED: GStreamer elements missing: {missing:?}");
            return;
        }
        let res = TalkdownReceiver::answer("not an offer", &[], Duration::from_secs(5)).await;
        let err = res.err().expect("refused").to_string();
        assert!(err.contains("no audio"), "{err}");
    }
}
