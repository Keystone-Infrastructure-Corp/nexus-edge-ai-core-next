//! Phase F — edge WebRTC signalling bridge.
//!
//! Sits between the cloud tunnel ([`crate::cloud_tunnel`]) and the per-session
//! [`nexus_pipeline::WebRtcSession`] (Phase E). An inbound `webrtc_offer`
//! builds a passthrough session for the camera and pumps its answer + local
//! ICE candidates back out as `webrtc_answer` / `webrtc_ice_candidate`; an
//! inbound `webrtc_ice_candidate` feeds the browser's trickled candidates in.
//!
//! The type is compiled **unconditionally** so `cloud_tunnel.rs` can hold one
//! `Arc<WebRtcBridge>` regardless of features. When the `gstreamer-webrtc`
//! feature is off every method is a logged no-op — and the heartbeat also
//! omits the `webrtc` capability, so a cloud never sends an offer to such a
//! core in the first place.

use std::sync::Arc;

use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{
    LiveHdAnswerPayload, LiveHdStartPayload, LiveHdStopPayload, WebrtcIceCandidatePayload,
    WebrtcOfferPayload,
};
use tracing::debug;

#[cfg(feature = "gstreamer-webrtc")]
use nexus_cloud_protocol::v1::{
    Envelope, EnvelopeBody, EnvelopeMeta, LiveHdOfferPayload, LiveHdPublishingPayload,
    WebrtcAnswerPayload,
};
#[cfg(feature = "gstreamer-webrtc")]
use nexus_pipeline::PreRollIngester;
#[cfg(feature = "gstreamer-webrtc")]
use nexus_types::CameraId;
#[cfg(feature = "gstreamer-webrtc")]
use std::collections::HashMap;
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

#[cfg(feature = "gstreamer-webrtc")]
struct ActiveSession {
    /// The camera this session streams — used to evict a stale prior session
    /// for the same camera when the browser reopens HD (the browser drops the
    /// old session client-side without closing it, so we must reclaim it here).
    camera_id: CameraId,
    /// Dropping this tears the webrtcbin pipeline down.
    _session: nexus_pipeline::WebRtcSession,
    /// The task draining `WebRtcEvent`s → outbox envelopes.
    pump: tokio::task::JoinHandle<()>,
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
    /// path and by any engine compiled without `gstreamer-webrtc`. Offers are
    /// logged and dropped. Always available so `main` can build a bridge on
    /// every recorder path regardless of features.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(feature = "gstreamer-webrtc")]
            inner: parking_lot::Mutex::new(Inner {
                ingesters: Arc::new(HashMap::new()),
                sessions: HashMap::new(),
            }),
        })
    }

    /// Handle an inbound `webrtc_offer`: build a session and start answering.
    pub fn on_offer(&self, payload: &WebrtcOfferPayload, outbox: &Arc<TunnelOutbox>) {
        #[cfg(feature = "gstreamer-webrtc")]
        self.on_offer_impl(payload, outbox);
        #[cfg(not(feature = "gstreamer-webrtc"))]
        {
            let _ = outbox;
            debug!(
                session_id = %payload.session_id,
                camera_id = payload.camera_id,
                "webrtc_offer ignored: engine built without the gstreamer-webrtc feature",
            );
        }
    }

    /// Handle an inbound `webrtc_ice_candidate` (the browser's trickle).
    pub fn on_ice_candidate(&self, payload: &WebrtcIceCandidatePayload) {
        #[cfg(feature = "gstreamer-webrtc")]
        self.on_ice_impl(payload);
        #[cfg(not(feature = "gstreamer-webrtc"))]
        debug!(
            session_id = %payload.session_id,
            "webrtc_ice_candidate ignored: engine built without the gstreamer-webrtc feature",
        );
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
                active.pump.abort();
                // `active._session` drops here → pipeline to NULL.
            }
        }
    }
}

#[cfg(feature = "gstreamer-webrtc")]
impl WebRtcBridge {
    fn on_offer_impl(&self, payload: &WebrtcOfferPayload, outbox: &Arc<TunnelOutbox>) {
        use nexus_pipeline::{IceServerCfg, WebRtcMode, WebRtcSession};

        let Ok(cam_id) = CameraId::try_from(payload.camera_id) else {
            warn!(
                camera_id = payload.camera_id,
                "webrtc_offer: camera_id out of range; dropping"
            );
            return;
        };

        let mut inner = self.inner.lock();
        // Idempotent re-offer: tear any prior session for this id first, and
        // evict any stale session still bound to the same camera (browser
        // reopen leaves the old session dangling on our side otherwise).
        let stale: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(id, active)| id.as_str() == payload.session_id || active.camera_id == cam_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(prev) = inner.sessions.remove(&id) {
                prev.pump.abort();
            }
        }
        let Some(ingester) = inner.ingesters.get(&cam_id).cloned() else {
            warn!(
                camera_id = cam_id,
                session_id = %payload.session_id,
                "webrtc_offer: no ingester for this camera; dropping"
            );
            return;
        };
        let codec = ingester.codec();
        let nal_rx = ingester.subscribe();

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
        let session = match WebRtcSession::new(
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
                warn!(session_id = %payload.session_id, error = %e, "webrtc session build failed");
                return;
            }
        };
        if let Err(e) = session.accept_offer(&payload.sdp) {
            warn!(session_id = %payload.session_id, error = %e, "webrtc accept_offer failed");
            return;
        }
        let pump = tokio::spawn(pump_events(
            payload.session_id.clone(),
            ev_rx,
            Arc::clone(outbox),
        ));
        inner.sessions.insert(
            payload.session_id.clone(),
            ActiveSession {
                camera_id: cam_id,
                _session: session,
                pump,
            },
        );
        debug!(session_id = %payload.session_id, camera_id = cam_id, "webrtc session started");
    }

    fn on_ice_impl(&self, payload: &WebrtcIceCandidatePayload) {
        let inner = self.inner.lock();
        match inner.sessions.get(&payload.session_id) {
            Some(active) => {
                // Wire carries u64; webrtcbin's add-ice-candidate wants u32.
                let mline = u32::try_from(payload.sdp_mline_index).unwrap_or(0);
                active._session.add_ice_candidate(mline, &payload.candidate);
            }
            None => debug!(
                session_id = %payload.session_id,
                "webrtc_ice_candidate for unknown session; dropping"
            ),
        }
    }

    fn on_live_hd_start_impl(&self, payload: &LiveHdStartPayload, outbox: &Arc<TunnelOutbox>) {
        use nexus_pipeline::{IceServerCfg, WebRtcMode, WebRtcSession};

        // Only the SFU transport uses webrtcbin publish; MoQ (gated until
        // preview) is a separate publisher path handled elsewhere.
        if payload.transport != "sfu" {
            debug!(
                session_id = %payload.session_id,
                transport = %payload.transport,
                "live_hd_start for non-sfu transport; webrtc bridge ignoring",
            );
            return;
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
                prev.pump.abort();
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
            session_key.clone(),
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
                return;
            }
        };
        let pump = tokio::spawn(pump_publisher_events(
            session_key.clone(),
            codec.base().to_string(),
            ev_rx,
            Arc::clone(outbox),
        ));
        inner.sessions.insert(
            session_key.clone(),
            ActiveSession {
                camera_id: cam_id,
                _session: session,
                pump,
            },
        );
        debug!(session_id = %session_key, camera_id = cam_id, "webrtc publisher started");
    }

    fn on_live_hd_answer_impl(&self, payload: &LiveHdAnswerPayload) {
        let inner = self.inner.lock();
        match inner.sessions.get(&payload.session_id) {
            Some(active) => {
                if let Err(e) = active._session.set_answer(&payload.sdp) {
                    warn!(session_id = %payload.session_id, error = %e, "webrtc set_answer failed");
                }
            }
            None => debug!(
                session_id = %payload.session_id,
                "live_hd_answer for unknown session; dropping"
            ),
        }
    }

    fn on_live_hd_stop_impl(&self, payload: &LiveHdStopPayload) {
        let mut inner = self.inner.lock();
        if let Some(prev) = inner.sessions.remove(&payload.session_id) {
            prev.pump.abort();
            debug!(session_id = %payload.session_id, "webrtc publisher stopped");
        } else {
            debug!(session_id = %payload.session_id, "live_hd_stop for unknown session; no-op");
        }
    }
}

/// Drain a session's [`nexus_pipeline::WebRtcEvent`]s and forward each as the
/// matching cloud envelope until the session fails or the channel closes.
#[cfg(feature = "gstreamer-webrtc")]
async fn pump_events(
    session_id: String,
    mut ev_rx: tokio::sync::mpsc::UnboundedReceiver<nexus_pipeline::WebRtcEvent>,
    outbox: Arc<TunnelOutbox>,
) {
    use nexus_pipeline::WebRtcEvent;
    while let Some(evt) = ev_rx.recv().await {
        let env = match evt {
            WebRtcEvent::Answer { sdp, codec } => answer_envelope(session_id.clone(), sdp, codec),
            WebRtcEvent::IceCandidate {
                sdp_mline_index,
                candidate,
            } => ice_envelope(session_id.clone(), u64::from(sdp_mline_index), candidate),
            WebRtcEvent::Failed(msg) => {
                warn!(session_id = %session_id, reason = %msg, "webrtc session failed; stopping pump");
                break;
            }
            // The answerer role never emits these (publisher-only).
            WebRtcEvent::Offer { .. } | WebRtcEvent::Connected => continue,
        };
        if let Err(e) = outbox.send(env).await {
            warn!(session_id = %session_id, error = %e, "webrtc outbox send failed; stopping pump");
            break;
        }
    }
}

/// Drain a publisher session's [`nexus_pipeline::WebRtcEvent`]s and forward
/// each as the matching cloud envelope: `Offer` → `live_hd_offer`,
/// `Connected` → `live_hd_publishing`, `Failed` → stop the pump. The
/// publisher role never emits `Answer` / `IceCandidate` (candidates are
/// baked into the offer), so those arms are no-ops by contract.
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
            WebRtcEvent::Answer { .. } | WebRtcEvent::IceCandidate { .. } => continue,
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
fn answer_envelope(session_id: String, sdp: String, codec: &str) -> Envelope {
    Envelope {
        meta: base_meta(),
        body: EnvelopeBody::WebrtcAnswer(WebrtcAnswerPayload {
            codec: Some(codec.to_string()),
            sdp,
            session_id,
        }),
    }
}

#[cfg(feature = "gstreamer-webrtc")]
fn ice_envelope(session_id: String, sdp_mline_index: u64, candidate: String) -> Envelope {
    Envelope {
        meta: base_meta(),
        body: EnvelopeBody::WebrtcIceCandidate(WebrtcIceCandidatePayload {
            candidate,
            sdp_mid: None,
            sdp_mline_index,
            session_id,
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
    fn answer_envelope_shape() {
        let env = answer_envelope("sess-1".to_string(), "v=0\r\n".to_string(), "h264");
        assert_eq!(env.meta.v, 1);
        match env.body {
            EnvelopeBody::WebrtcAnswer(p) => {
                assert_eq!(p.session_id, "sess-1");
                assert_eq!(p.codec.as_deref(), Some("h264"));
                assert_eq!(p.sdp, "v=0\r\n");
            }
            other => panic!("expected WebrtcAnswer, got {other:?}"),
        }
    }

    #[test]
    fn ice_envelope_shape() {
        let env = ice_envelope(
            "sess-2".to_string(),
            0,
            "candidate:1 1 udp 2 ...".to_string(),
        );
        match env.body {
            EnvelopeBody::WebrtcIceCandidate(p) => {
                assert_eq!(p.session_id, "sess-2");
                assert_eq!(p.sdp_mline_index, 0);
                assert!(p.sdp_mid.is_none());
                assert!(p.candidate.starts_with("candidate:"));
            }
            other => panic!("expected WebrtcIceCandidate, got {other:?}"),
        }
    }
}
