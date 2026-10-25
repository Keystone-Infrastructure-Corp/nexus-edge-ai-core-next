//! Talk-down sessions (ADR-090): at most one per speaker. A start claims the
//! speaker before anything slow happens, and the claim is a guard that
//! releases the speaker when dropped, so every exit releases it: a refused
//! clip, an unanswered call, the end of playback, a panic in the play task.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nexus_cloud_protocol::v1::IceServer;
use nexus_config::{SpeakerConfig, SpeakerId};
use nexus_types::CameraId;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(feature = "gstreamer-webrtc")]
use super::live::Live;
use super::sip::{place_call, SipTarget, TalkdownError};
use super::{clip, tone_pcm};

/// What this build can play, as the talk-down GET lists it.
pub const MODES: &[&str] = if cfg!(feature = "gstreamer-webrtc") {
    &["tone", "clip", "live"]
} else {
    &["tone", "clip"]
};

/// How long a speaker gets to answer the INVITE.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(8);
const TONE_MS: u32 = 2000;
/// 300 ms of silence after the audio, so the BYE does not cut off what the
/// speaker still has buffered.
const TAIL_SAMPLES: usize = 300 * 8;

/// A start request's body: `{"mode": "tone"}`,
/// `{"mode": "clip", "clip": {"url": …, "sha256": …}}` or
/// `{"mode": "live", "sdp_offer": …, "ice_servers": […]}`.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Audio {
    Tone,
    Clip {
        clip: ClipRef,
    },
    /// The SFU's subscriber offer for the operator's microphone track. Parsed
    /// in every build so a build without live audio refuses it by name.
    #[cfg_attr(not(feature = "gstreamer-webrtc"), allow(dead_code))]
    Live {
        /// Carries ICE credentials: never logged.
        sdp_offer: String,
        /// TURN entries carry credentials: never logged.
        #[serde(default)]
        ice_servers: Vec<IceServer>,
    },
}

#[derive(Deserialize)]
pub struct ClipRef {
    /// A read SAS URL: a bearer credential.
    url: String,
    sha256: String,
}

impl Audio {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Tone => "tone",
            Self::Clip { .. } => "clip",
            Self::Live { .. } => "live",
        }
    }
}

pub enum StartError {
    Busy,
    /// Why the clip was refused; never contains its URL.
    Clip(String),
    Call(TalkdownError),
    /// This build has no live audio.
    #[cfg(not(feature = "gstreamer-webrtc"))]
    LiveUnsupported,
    /// The receiver could not answer the SFU's offer; never contains the SDP.
    #[cfg(feature = "gstreamer-webrtc")]
    Negotiation(String),
}

/// The engine's talk-down sessions, shared by every handler.
#[derive(Clone, Default)]
pub struct TalkdownSessions {
    active: Arc<Mutex<HashMap<SpeakerId, CancellationToken>>>,
    /// Only [`Self::allowing_http`] sets it.
    allow_http: bool,
}

impl TalkdownSessions {
    /// Sessions that also fetch clips over plain http, for a test clip server.
    #[cfg(test)]
    pub fn allowing_http() -> Self {
        Self {
            allow_http: true,
            ..Self::default()
        }
    }

    pub fn is_busy(&self, speaker_id: SpeakerId) -> bool {
        self.active.lock().contains_key(&speaker_id)
    }

    /// Cancel the speaker's session, whoever started it. It hangs up, then
    /// releases the speaker. Returns whether there was one.
    pub fn stop(&self, speaker_id: SpeakerId) -> bool {
        let active = self.active.lock();
        active
            .get(&speaker_id)
            .map(CancellationToken::cancel)
            .is_some()
    }

    /// Claim the speaker, load the audio and call the speaker. Once it
    /// answers, the audio plays in a spawned task that then hangs up; this
    /// returns the session id, the SDP answer of a live session, and that
    /// task. A live session answers the SFU's offer only after the speaker
    /// has answered, so an unreachable speaker fails fast.
    pub async fn start(
        &self,
        speaker: &SpeakerConfig,
        camera_id: Option<CameraId>,
        audio: &Audio,
    ) -> Result<(Uuid, Option<String>, JoinHandle<Result<(), TalkdownError>>), StartError> {
        #[cfg(not(feature = "gstreamer-webrtc"))]
        if matches!(audio, Audio::Live { .. }) {
            return Err(StartError::LiveUnsupported);
        }
        // The receiver refuses an offer with no audio too, but only after the
        // call: refuse it here so the speaker never rings for it.
        #[cfg(feature = "gstreamer-webrtc")]
        if let Audio::Live { sdp_offer, .. } = audio {
            if !sdp_offer.lines().any(|l| l.starts_with("m=audio ")) {
                return Err(StartError::Negotiation("the offer has no audio".into()));
            }
        }
        let session = self.claim(speaker.id, camera_id)?;
        let mut pcm = match audio {
            Audio::Tone => tone_pcm(TONE_MS),
            Audio::Clip { clip } => clip::fetch(&clip.url, &clip.sha256, self.allow_http)
                .await
                .map_err(StartError::Clip)?,
            Audio::Live { .. } => Vec::new(),
        };
        pcm.resize(pcm.len() + TAIL_SAMPLES, 0);
        let target = SipTarget {
            host: speaker.host.clone(),
            port: speaker.port,
            username: speaker.username.clone(),
            password: speaker.password.clone(),
        };
        // `place_call` bounds only its waits for replies; a socket that never
        // turns writable (seen on a macOS bench host whose Local Network
        // privacy had not admitted a fresh build) would hold the speaker
        // forever. The extra second lets its own deadline CANCEL first.
        // shortcut: the play task's RTP and BYE sends are not bounded the same
        // way; bound them too if a socket ever stalls after the answer.
        let answered = tokio::time::timeout(
            ANSWER_TIMEOUT + Duration::from_secs(1),
            place_call(&target, ANSWER_TIMEOUT),
        )
        .await;
        let mut call = answered
            .unwrap_or(Err(TalkdownError::Unreachable))
            .map_err(StartError::Call)?;
        // A failed negotiation drops `call`, which sends its BYE, then
        // `session`, which releases the speaker.
        #[cfg(feature = "gstreamer-webrtc")]
        let (live, sdp_answer) = match audio {
            Audio::Live {
                sdp_offer,
                ice_servers,
            } => {
                let (live, answer) = Live::answer(sdp_offer, ice_servers)
                    .await
                    .map_err(StartError::Negotiation)?;
                (Some(live), Some(answer))
            }
            _ => (None, None),
        };
        #[cfg(not(feature = "gstreamer-webrtc"))]
        let sdp_answer = None;
        let id = session.id;
        let task = tokio::spawn(async move {
            #[cfg(feature = "gstreamer-webrtc")]
            let played = match live {
                // The silent tail plays after a stop too, so the BYE does
                // not clip the operator's last words.
                Some(live) => match live.forward(&mut call, &session.cancel).await {
                    Ok(()) => call.play_pcm(&pcm, &CancellationToken::new()).await,
                    e => e,
                },
                None => call.play_pcm(&pcm, &session.cancel).await,
            };
            #[cfg(not(feature = "gstreamer-webrtc"))]
            let played = call.play_pcm(&pcm, &session.cancel).await;
            let res = match played {
                Ok(()) => call.hangup().await,
                Err(e) => Err(e),
            };
            if let Err(e) = &res {
                tracing::warn!(speaker_id = session.speaker_id, error = %e, "talk-down call failed");
            }
            res
            // `session` drops here, after the hangup: the speaker is free.
        });
        Ok((id, sdp_answer, task))
    }

    fn claim(
        &self,
        speaker_id: SpeakerId,
        camera_id: Option<CameraId>,
    ) -> Result<Session, StartError> {
        let cancel = CancellationToken::new();
        match self.active.lock().entry(speaker_id) {
            Entry::Occupied(_) => return Err(StartError::Busy),
            Entry::Vacant(slot) => slot.insert(cancel.clone()),
        };
        Ok(Session {
            active: self.active.clone(),
            id: Uuid::now_v7(),
            speaker_id,
            camera_id,
            cancel,
        })
    }
}

/// A claimed speaker. Dropping it releases the speaker.
struct Session {
    active: Arc<Mutex<HashMap<SpeakerId, CancellationToken>>>,
    id: Uuid,
    speaker_id: SpeakerId,
    /// The camera that started it; `None` for an installer's speaker test.
    camera_id: Option<CameraId>,
    cancel: CancellationToken,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.active.lock().remove(&self.speaker_id);
        tracing::info!(
            session_id = %self.id,
            speaker_id = self.speaker_id,
            camera_id = ?self.camera_id,
            "talk-down session ended"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_play_task_still_releases_the_speaker() {
        let sessions = TalkdownSessions::default();
        let session = sessions.claim(7, Some(1)).ok().unwrap();
        assert!(matches!(sessions.claim(7, None), Err(StartError::Busy)));
        let task = tokio::spawn(async move {
            let _session = session;
            panic!("play task panicked");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(!sessions.is_busy(7));
        assert!(!sessions.stop(7));
    }
}
