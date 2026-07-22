//! Phase 2/3 — edge HD publisher bridge (dual-transport).
//!
//! Sits between the cloud tunnel ([`crate::cloud_tunnel`]) and the per-session
//! publisher pipeline. An inbound `live_hd_start` builds a send-only publisher
//! for the camera on the transport the cloud selected:
//!
//! * `sfu` → a [`nexus_pipeline::WebRtcSession`] (webrtcbin) that offers to the
//!   Cloudflare SFU via the api-gateway, pumping out `live_hd_offer` +
//!   `live_hd_publishing`; an inbound `live_hd_answer` applies the SFU's answer.
//! * `moq` → a [`nexus_pipeline::MoqSession`] (moqsink) that publishes straight
//!   to the Cloudflare MoQ relay using the `moq_*` coordinates in the payload.
//!   MoQ has no signalling round-trip, so no `live_hd_offer` / `_answer`.
//!
//! `live_hd_stop` tears either session down.
//!
//! The type is compiled **unconditionally** so `cloud_tunnel.rs` can hold one
//! `Arc<WebRtcBridge>` regardless of features. When the `gstreamer-webrtc`
//! feature is off every method is a logged no-op — and the heartbeat also omits
//! the `hd_sfu` / `hd_moq` capability, so a cloud never starts an HD publish on
//! such a core in the first place.

use std::sync::Arc;

use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{LiveHdAnswerPayload, LiveHdStartPayload, LiveHdStopPayload};
use tracing::debug;

#[cfg(feature = "gstreamer-webrtc")]
use nexus_cloud_protocol::v1::{
    Envelope, EnvelopeBody, EnvelopeMeta, LiveHdOfferPayload, LiveHdPublishingPayload,
};
#[cfg(feature = "gstreamer-webrtc")]
use nexus_pipeline::{NalSample, PreRollIngester};
#[cfg(feature = "gstreamer-webrtc")]
use nexus_types::{CameraId, CodecKind};
#[cfg(feature = "gstreamer-webrtc")]
use std::collections::HashMap;
#[cfg(feature = "gstreamer-webrtc")]
use tokio::sync::broadcast;
#[cfg(feature = "gstreamer-webrtc")]
use tracing::warn;

/// Boot-time snapshot of the camera → ingester registry (the compressed-NAL
/// sources) the bridge builds passthrough sessions from.
#[cfg(feature = "gstreamer-webrtc")]
pub type IngesterRegistry = Arc<HashMap<CameraId, Arc<PreRollIngester>>>;

/// Owns every live edge WebRTC session and the seam to the camera ingesters.
pub struct WebRtcBridge {
    #[cfg(feature = "gstreamer-webrtc")]
    inner: parking_lot::Mutex<Inner>,
}

#[cfg(feature = "gstreamer-webrtc")]
struct Inner {
    ingesters: IngesterRegistry,
    sessions: HashMap<String, ActiveSession>,
}

/// The transport-specific publisher pipeline behind an [`ActiveSession`].
/// Dropping either variant tears its pipeline down.
///
/// `Moq` is held only for that `Drop` (the moqsink pipeline runs itself with no
/// further calls), so its field is never read after construction; `Sfu` is also
/// read for `set_answer`. The enum-level allow covers the `Moq` keep-alive.
#[cfg(feature = "gstreamer-webrtc")]
#[allow(dead_code)]
enum HdSession {
    /// SFU publisher (webrtcbin, offers to the Cloudflare SFU).
    Sfu(nexus_pipeline::WebRtcSession),
    /// MoQ publisher (moqsink, publishes to the Cloudflare relay).
    Moq(nexus_pipeline::MoqSession),
}

#[cfg(feature = "gstreamer-webrtc")]
struct ActiveSession {
    /// The camera this session streams — used to evict a stale prior session
    /// for the same camera when the browser reopens HD (the browser drops the
    /// old session client-side without closing it, so we must reclaim it here).
    camera_id: CameraId,
    /// Dropping this tears the publisher pipeline down.
    _session: HdSession,
    /// The task draining `WebRtcEvent`s → outbox envelopes (SFU only). MoQ has
    /// no signalling round-trip, so its pump is `None`.
    pump: Option<tokio::task::JoinHandle<()>>,
}

impl WebRtcBridge {
    /// Build an active bridge over the given ingester registry.
    #[cfg(feature = "gstreamer-webrtc")]
    pub fn new(ingesters: IngesterRegistry) -> Arc<Self> {
        Arc::new(Self {
            inner: parking_lot::Mutex::new(Inner {
                ingesters,
                sessions: HashMap::new(),
            }),
        })
    }

    /// Build a bridge that can serve no cameras — used by the Stub recorder
    /// path and by any engine compiled without `gstreamer-webrtc`. Publish
    /// requests are logged and dropped. Always available so `main` can build a
    /// bridge on every recorder path regardless of features.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(feature = "gstreamer-webrtc")]
            inner: parking_lot::Mutex::new(Inner {
                ingesters: Arc::new(HashMap::new()),
                sessions: HashMap::new(),
            }),
        })
    }

    /// Handle an inbound `live_hd_start`: build a publisher (offerer) session
    /// for the camera and begin offering to the SFU (Phase 2 SFU transport).
    pub fn on_live_hd_start(&self, payload: &LiveHdStartPayload, outbox: &Arc<TunnelOutbox>) {
        #[cfg(feature = "gstreamer-webrtc")]
        self.on_live_hd_start_impl(payload, outbox);
        #[cfg(not(feature = "gstreamer-webrtc"))]
        {
            let _ = outbox;
            debug!(
                session_id = %payload.session_id,
                camera_id = payload.camera_id,
                "live_hd_start ignored: engine built without the gstreamer-webrtc feature",
            );
        }
    }

    /// Handle an inbound `live_hd_answer` (the SFU's answer, relayed by cloud).
    pub fn on_live_hd_answer(&self, payload: &LiveHdAnswerPayload) {
        #[cfg(feature = "gstreamer-webrtc")]
        self.on_live_hd_answer_impl(payload);
        #[cfg(not(feature = "gstreamer-webrtc"))]
        debug!(
            session_id = %payload.session_id,
            "live_hd_answer ignored: engine built without the gstreamer-webrtc feature",
        );
    }

    /// Handle an inbound `live_hd_stop`: tear the publisher session down.
    pub fn on_live_hd_stop(&self, payload: &LiveHdStopPayload) {
        #[cfg(feature = "gstreamer-webrtc")]
        self.on_live_hd_stop_impl(payload);
        #[cfg(not(feature = "gstreamer-webrtc"))]
        debug!(
            session_id = %payload.session_id,
            "live_hd_stop ignored: engine built without the gstreamer-webrtc feature",
        );
    }

    /// Tear down every live session (called on tunnel disconnect).
    pub fn clear_all(&self) {
        #[cfg(feature = "gstreamer-webrtc")]
        {
            let mut inner = self.inner.lock();
            for (_, active) in inner.sessions.drain() {
                if let Some(pump) = active.pump {
                    pump.abort();
                }
                // `active._session` drops here → pipeline to NULL.
            }
        }
    }

    /// Spawn the idle-session reaper: a background task that periodically drops
    /// any live HD session whose NAL feed has ended (the browser tab closed or
    /// the PeerConnection / relay consumer died without a `live_hd_stop`). Such
    /// a session produces nothing and only keeps a still-parked `push_buffer`
    /// blocking-pool thread alive until it is dropped; the reaper reclaims it
    /// proactively instead of waiting for a `live_hd_stop` that may never come.
    ///
    /// Holds a [`std::sync::Weak`] to the bridge so it stops itself once the
    /// last owner (the cloud tunnel) drops. Only compiled with
    /// `gstreamer-webrtc` — the sole caller (`main`) is likewise gated.
    #[cfg(feature = "gstreamer-webrtc")]
    pub fn spawn_reaper(self: &Arc<Self>) {
        {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(REAP_INTERVAL);
                loop {
                    tick.tick().await;
                    let Some(bridge) = weak.upgrade() else { break };
                    bridge.reap_dead_sessions();
                }
            });
        }
    }
}

/// How often the idle-session reaper scans for dead HD sessions.
#[cfg(feature = "gstreamer-webrtc")]
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(feature = "gstreamer-webrtc")]
impl WebRtcBridge {
    fn on_live_hd_start_impl(&self, payload: &LiveHdStartPayload, outbox: &Arc<TunnelOutbox>) {
        // Both dual-transport publishers are handled here; any other transport
        // is unknown to this core and ignored.
        match payload.transport.as_str() {
            "sfu" | "moq" => {}
            other => {
                debug!(
                    session_id = %payload.session_id,
                    transport = other,
                    "live_hd_start for unknown transport; ignoring",
                );
                return;
            }
        }

        let Ok(cam_id) = CameraId::try_from(payload.camera_id) else {
            warn!(
                camera_id = payload.camera_id,
                "live_hd_start: camera_id out of range; dropping"
            );
            return;
        };
        let session_key = payload.session_id.clone();

        let mut inner = self.inner.lock();
        // Idempotent restart: tear any prior session for this id, and evict a
        // stale session still bound to the same camera (browser reopen leaves
        // the old session dangling on our side otherwise).
        let stale: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(id, active)| id.as_str() == session_key || active.camera_id == cam_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(prev) = inner.sessions.remove(&id) {
                if let Some(pump) = prev.pump {
                    pump.abort();
                }
            }
        }
        let Some(ingester) = inner.ingesters.get(&cam_id).cloned() else {
            warn!(
                camera_id = cam_id,
                session_id = %payload.session_id,
                "live_hd_start: no ingester for this camera; dropping"
            );
            return;
        };
        let codec = ingester.codec();
        let nal_rx = ingester.subscribe();

        // Build the transport-specific publisher over the shared NAL feed.
        let active = if payload.transport == "moq" {
            build_moq_session(payload, cam_id, codec, nal_rx)
        } else {
            build_sfu_session(payload, cam_id, codec, nal_rx, outbox)
        };
        let Some(active) = active else { return };
        inner.sessions.insert(session_key.clone(), active);
        debug!(
            session_id = %session_key,
            camera_id = cam_id,
            transport = %payload.transport,
            "hd publisher started"
        );
    }

    fn on_live_hd_answer_impl(&self, payload: &LiveHdAnswerPayload) {
        let inner = self.inner.lock();
        match inner.sessions.get(&payload.session_id) {
            Some(active) => match &active._session {
                HdSession::Sfu(session) => {
                    if let Err(e) = session.set_answer(&payload.sdp) {
                        warn!(session_id = %payload.session_id, error = %e, "webrtc set_answer failed");
                    }
                }
                HdSession::Moq(_) => debug!(
                    session_id = %payload.session_id,
                    "live_hd_answer for a MoQ session (no SDP handshake); ignoring"
                ),
            },
            None => debug!(
                session_id = %payload.session_id,
                "live_hd_answer for unknown session; dropping"
            ),
        }
    }

    fn on_live_hd_stop_impl(&self, payload: &LiveHdStopPayload) {
        let mut inner = self.inner.lock();
        if let Some(prev) = inner.sessions.remove(&payload.session_id) {
            if let Some(pump) = prev.pump {
                pump.abort();
            }
            debug!(session_id = %payload.session_id, "hd publisher stopped");
        } else {
            debug!(session_id = %payload.session_id, "live_hd_stop for unknown session; no-op");
        }
    }

    /// Drop every session whose NAL feed has ended (see [`Self::spawn_reaper`]).
    /// Dropping the `ActiveSession` runs the publisher's `Drop` (pipeline →
    /// NULL), which unblocks the still-parked `push_buffer` and frees its
    /// blocking-pool thread.
    fn reap_dead_sessions(&self) {
        let mut inner = self.inner.lock();
        let dead: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, active)| active._session.feed_ended())
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            if let Some(prev) = inner.sessions.remove(&id) {
                if let Some(pump) = prev.pump {
                    pump.abort();
                }
                warn!(session_id = %id, "reaped idle hd publisher (feed ended without live_hd_stop)");
            }
        }
    }
}

#[cfg(feature = "gstreamer-webrtc")]
impl HdSession {
    /// True once the underlying publisher's NAL feed task has ended.
    fn feed_ended(&self) -> bool {
        match self {
            HdSession::Sfu(s) => s.feed_ended(),
            HdSession::Moq(s) => s.feed_ended(),
        }
    }
}

/// Build an SFU publisher (webrtcbin offerer) + its event pump.
#[cfg(feature = "gstreamer-webrtc")]
fn build_sfu_session(
    payload: &LiveHdStartPayload,
    cam_id: CameraId,
    codec: CodecKind,
    nal_rx: broadcast::Receiver<NalSample>,
    outbox: &Arc<TunnelOutbox>,
) -> Option<ActiveSession> {
    use nexus_pipeline::{IceServerCfg, WebRtcMode, WebRtcSession};

    let ice_servers: Vec<IceServerCfg> = payload
        .ice_servers
        .as_ref()
        .map(|servers| {
            servers
                .iter()
                .map(|s| IceServerCfg {
                    urls: s.urls.clone(),
                    username: s.username.clone(),
                    credential: s.credential.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mode = match payload.mode.as_deref() {
        Some("transcode") => WebRtcMode::Transcode,
        _ => WebRtcMode::Passthrough,
    };

    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = match WebRtcSession::new_publisher(
        payload.session_id.clone(),
        cam_id,
        codec,
        mode,
        &ice_servers,
        nal_rx,
        ev_tx,
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(session_id = %payload.session_id, error = %e, "webrtc publisher build failed");
            return None;
        }
    };
    let pump = tokio::spawn(pump_publisher_events(
        payload.session_id.clone(),
        codec.base().to_string(),
        ev_rx,
        Arc::clone(outbox),
    ));
    Some(ActiveSession {
        camera_id: cam_id,
        _session: HdSession::Sfu(session),
        pump: Some(pump),
    })
}

/// Build a MoQ publisher (moqsink) from the `live_hd_start.moq_*` coordinates.
/// MoQ has no signalling round-trip, so there is no event pump.
#[cfg(feature = "gstreamer-webrtc")]
fn build_moq_session(
    payload: &LiveHdStartPayload,
    cam_id: CameraId,
    codec: CodecKind,
    nal_rx: broadcast::Receiver<NalSample>,
) -> Option<ActiveSession> {
    use nexus_pipeline::MoqSession;

    let (Some(relay_url), Some(broadcast_name), Some(token)) = (
        payload.moq_relay_url.as_deref(),
        payload.moq_broadcast.as_deref(),
        payload.moq_publish_token.as_deref(),
    ) else {
        warn!(
            session_id = %payload.session_id,
            "live_hd_start moq: missing relay_url / broadcast / publish_token; dropping"
        );
        return None;
    };
    let session = match MoqSession::new_publisher(
        payload.session_id.clone(),
        cam_id,
        codec,
        relay_url,
        broadcast_name,
        token,
        nal_rx,
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(session_id = %payload.session_id, error = %e, "moq publisher build failed");
            return None;
        }
    };
    Some(ActiveSession {
        camera_id: cam_id,
        _session: HdSession::Moq(session),
        pump: None,
    })
}

/// Drain a publisher session's [`nexus_pipeline::WebRtcEvent`]s and forward
/// each as the matching cloud envelope: `Offer` → `live_hd_offer`,
/// `Connected` → `live_hd_publishing`, `Failed` → stop the pump.
#[cfg(feature = "gstreamer-webrtc")]
async fn pump_publisher_events(
    session_id: String,
    codec: String,
    mut ev_rx: tokio::sync::mpsc::UnboundedReceiver<nexus_pipeline::WebRtcEvent>,
    outbox: Arc<TunnelOutbox>,
) {
    use nexus_pipeline::WebRtcEvent;
    while let Some(evt) = ev_rx.recv().await {
        let env = match evt {
            WebRtcEvent::Offer { sdp, .. } => offer_envelope(session_id.clone(), sdp),
            WebRtcEvent::Connected => publishing_envelope(session_id.clone(), codec.clone()),
            WebRtcEvent::Failed(msg) => {
                warn!(session_id = %session_id, reason = %msg, "webrtc publisher failed; stopping pump");
                break;
            }
        };
        if let Err(e) = outbox.send(env).await {
            warn!(session_id = %session_id, error = %e, "publisher outbox send failed; stopping pump");
            break;
        }
    }
}

#[cfg(feature = "gstreamer-webrtc")]
fn offer_envelope(session_id: String, sdp: String) -> Envelope {
    Envelope {
        meta: base_meta(),
        body: EnvelopeBody::LiveHdOffer(LiveHdOfferPayload { sdp, session_id }),
    }
}

#[cfg(feature = "gstreamer-webrtc")]
fn publishing_envelope(session_id: String, codec: String) -> Envelope {
    Envelope {
        meta: base_meta(),
        body: EnvelopeBody::LiveHdPublishing(LiveHdPublishingPayload {
            broadcast: None,
            codec: Some(codec),
            session_id,
            track: None,
            track_name: None,
            transport: "sfu".to_string(),
        }),
    }
}

#[cfg(feature = "gstreamer-webrtc")]
fn base_meta() -> EnvelopeMeta {
    EnvelopeMeta {
        v: 1,
        id: uuid::Uuid::now_v7().to_string(),
        ts: chrono::Utc::now().to_rfc3339(),
        in_reply_to: None,
        seq: None,
        trace: None,
    }
}

#[cfg(all(test, feature = "gstreamer-webrtc"))]
mod tests {
    use super::*;

    #[test]
    fn offer_envelope_shape() {
        let env = offer_envelope("sess-1".to_string(), "v=0\r\n".to_string());
        assert_eq!(env.meta.v, 1);
        match env.body {
            EnvelopeBody::LiveHdOffer(p) => {
                assert_eq!(p.session_id, "sess-1");
                assert_eq!(p.sdp, "v=0\r\n");
            }
            other => panic!("expected LiveHdOffer, got {other:?}"),
        }
    }

    #[test]
    fn publishing_envelope_shape() {
        let env = publishing_envelope("sess-2".to_string(), "h264".to_string());
        match env.body {
            EnvelopeBody::LiveHdPublishing(p) => {
                assert_eq!(p.session_id, "sess-2");
                assert_eq!(p.transport, "sfu");
                assert_eq!(p.codec.as_deref(), Some("h264"));
            }
            other => panic!("expected LiveHdPublishing, got {other:?}"),
        }
    }
}
