//! Phase 3 — edge MoQ HD publisher.
//!
//! The MoQ counterpart to [`crate::webrtc::WebRtcSession`]. Where the SFU path
//! offers a WebRTC send-only track, the MoQ path publishes the camera's
//! compressed H.264 / H.265 straight to a Cloudflare MoQ relay via the
//! `moqsink` GStreamer element (the `moq-gst` / `gstreamer1.0-moq` plugin):
//!
//! ```text
//!   PreRollIngester ── broadcast::Receiver<NalSample> ──▶ appsrc
//!   appsrc ! h264parse ! moqsink url=<relay>/?jwt=<token> broadcast=<name>
//! ```
//!
//! `moqsink` CMAF-muxes the parsed elementary stream and publishes it as a
//! `hang` broadcast; the browser subscribes it via `@moq/watch`. There is no
//! signalling round-trip (unlike SFU): the cloud chooses the broadcast name and
//! mints the publish token, hands them to the edge in `live_hd_start`, and the
//! edge just publishes. The relay buffers, so a browser may subscribe before —
//! or after — the edge connects.
//!
//! Gated behind the same `gstreamer-webrtc` feature as the SFU publisher: every
//! MoQ-capable core also runs SFU (the mandatory Safari fallback), so the two
//! ship together. `moqsink` is resolved from the GStreamer registry at runtime
//! (not a build dependency); on a core without the plugin installed,
//! [`MoqSession::new_publisher`] fails with [`MoqError::PluginMissing`] and the
//! engine keeps running (fail-open — the operator simply gets no HD-MoQ).

use std::fmt;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use nexus_types::{CameraId, CodecKind};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::preroll::NalSample;
use crate::webrtc::feed_loop;

/// Failure building or starting a MoQ publisher session.
#[derive(Debug)]
pub enum MoqError {
    /// GStreamer failed to initialise.
    Init(String),
    /// The `moqsink` element is not registered (the `moq-gst` plugin is not
    /// installed on this core).
    PluginMissing,
    /// Pipeline construction failed.
    Build(String),
    /// A pipeline state change failed.
    State(String),
}

impl fmt::Display for MoqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(e) => write!(f, "gstreamer init: {e}"),
            Self::PluginMissing => {
                write!(f, "moqsink element missing (install the moq-gst plugin)")
            }
            Self::Build(e) => write!(f, "pipeline build: {e}"),
            Self::State(e) => write!(f, "pipeline state: {e}"),
        }
    }
}

impl std::error::Error for MoqError {}

/// A single live MoQ HD publish for one `(session, camera)`.
///
/// Dropping the session tears the pipeline down and stops the NAL feed.
pub struct MoqSession {
    session_id: String,
    camera_id: CameraId,
    pipeline: gst::Pipeline,
    /// Kept alive for the session's lifetime; the feed task holds a clone.
    _appsrc: AppSrc,
    feed: JoinHandle<()>,
}

impl MoqSession {
    /// Build a MoQ publisher that streams the camera's compressed NALs to the
    /// relay `broadcast` under a signed publish token.
    ///
    /// `relay_url` is the relay data-plane base (e.g.
    /// `https://relay.cloudflare.mediaoverquic.com`); the publish JWT rides as
    /// `?jwt=` at the ROOT path (a sub-path connect is rejected `forbidden`).
    ///
    /// # Errors
    /// [`MoqError::PluginMissing`] when `moqsink` is not installed, or
    /// [`MoqError::Build`] / [`MoqError::State`] on pipeline failure.
    pub fn new_publisher(
        session_id: String,
        camera_id: CameraId,
        codec: CodecKind,
        relay_url: &str,
        broadcast_name: &str,
        publish_token: &str,
        nal_rx: broadcast::Receiver<NalSample>,
    ) -> Result<Self, MoqError> {
        gst::init().map_err(|e| MoqError::Init(e.to_string()))?;

        // `moqsink` is a runtime plugin, not a build dep — fail cleanly (and
        // fail-open at the caller) when it isn't installed.
        if gst::ElementFactory::find("moqsink").is_none() {
            return Err(MoqError::PluginMissing);
        }

        let is_h265 = codec.base() == "h265";
        let parse_el = if is_h265 { "h265parse" } else { "h264parse" };
        // `config-interval=0` trusts the source's per-keyframe SPS/PPS (the
        // ingester already repeats them) — forcing re-insertion doubles them on
        // some cameras and corrupts the CMAF init segment.
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false ! \
             {parse_el} config-interval=0 ! moqsink name=mux"
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| MoqError::Build(e.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| MoqError::Build("parsed element is not a Pipeline".to_string()))?;

        let mux = pipeline
            .by_name("mux")
            .ok_or_else(|| MoqError::Build("moqsink 'mux' missing".to_string()))?;
        mux.set_property("url", build_publish_url(relay_url, publish_token).as_str());
        mux.set_property("broadcast", broadcast_name);

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| MoqError::Build("appsrc 'src' missing".to_string()))?
            .downcast::<AppSrc>()
            .map_err(|_| MoqError::Build("downcast AppSrc".to_string()))?;
        // Tell appsrc the exact byte-stream codec so the parser negotiates
        // without a probe (matches the SFU publisher's appsrc caps).
        let caps_name = if is_h265 {
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

        let feed = tokio::spawn(feed_loop(appsrc.clone(), nal_rx, camera_id));

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| MoqError::State(format!("set Playing: {e}")))?;
        debug!(session_id = %session_id, camera_id, broadcast = broadcast_name, "moq publisher started");

        Ok(Self {
            session_id,
            camera_id,
            pipeline,
            _appsrc: appsrc,
            feed,
        })
    }
}

impl MoqSession {
    /// True once the NAL feed task has ended (broadcast closed, EOS, or the
    /// relay consumer stalled past the shared push timeout). Mirrors
    /// [`crate::WebRtcSession::feed_ended`] so the manager-side reaper can drop
    /// dead MoQ publishers promptly and free their parked blocking-pool thread.
    pub fn feed_ended(&self) -> bool {
        self.feed.is_finished()
    }
}

impl Drop for MoqSession {
    fn drop(&mut self) {
        self.feed.abort();
        crate::teardown::null_pipeline_detached(
            self.pipeline.clone(),
            "moq_publish::MoqSession::drop",
            Some(self.camera_id),
        );
        debug!(session_id = %self.session_id, camera_id = self.camera_id, "moq publisher stopped");
    }
}

/// Build the relay connect URL: the relay root plus `?jwt=<token>`.
///
/// Cloudflare validates the token from the `?jwt=` query param at the root
/// path; `broadcast` is a MoQ-layer name, not a URL path segment.
fn build_publish_url(relay_url: &str, token: &str) -> String {
    let base = relay_url.trim_end_matches('/');
    format!("{base}/?jwt={token}")
}

#[cfg(test)]
mod tests {
    use super::build_publish_url;

    #[test]
    fn publish_url_appends_jwt_at_root() {
        assert_eq!(
            build_publish_url("https://relay.cloudflare.mediaoverquic.com", "eyJ.a.b"),
            "https://relay.cloudflare.mediaoverquic.com/?jwt=eyJ.a.b"
        );
    }

    #[test]
    fn publish_url_trims_trailing_slash() {
        assert_eq!(
            build_publish_url("https://relay.example.com/", "tok"),
            "https://relay.example.com/?jwt=tok"
        );
    }
}
