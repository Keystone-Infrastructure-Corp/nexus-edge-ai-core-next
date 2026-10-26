//! Talk-down sessions (ADR-090): at most one per speaker. A start claims the
//! speaker before anything slow happens, and the claim is a guard that
//! releases the speaker when dropped, so every exit releases it: a refused
//! clip, an unanswered call, the end of playback, a panic in the play task.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::future::Future;
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
    /// The camera no longer pairs with the speaker the caller read.
    Unpaired,
    /// A stop or a parting ended the session before it played.
    Stopped,
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

/// Each busy speaker: the camera that started its session, the session's id,
/// and its cancel.
type Active = Arc<Mutex<HashMap<SpeakerId, (Option<CameraId>, Uuid, CancellationToken)>>>;

/// The engine's talk-down sessions, shared by every handler.
#[derive(Clone, Default)]
pub struct TalkdownSessions {
    active: Active,
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

    /// Cancel every session `ends` picks by its speaker, the camera that
    /// started it (`None` for a speaker test) and its id. Each hangs up, then
    /// releases its speaker. Returns their speakers.
    pub fn stop_where(
        &self,
        ends: impl Fn(SpeakerId, Option<CameraId>, Uuid) -> bool,
    ) -> Vec<SpeakerId> {
        let active = self.active.lock();
        active
            .iter()
            .filter(|(&speaker_id, (camera_id, id, _))| ends(speaker_id, *camera_id, *id))
            .map(|(&speaker_id, (_, _, cancel))| {
                cancel.cancel();
                speaker_id
            })
            .collect()
    }

    /// Claim the speaker, check `paired`, load the audio and call the
    /// speaker. Once it answers, the audio plays in a spawned task that then
    /// hangs up; this returns the session id, the SDP answer of a live
    /// session, and that task. A live session answers the SFU's offer only
    /// after the speaker has answered, so an unreachable speaker fails fast.
    ///
    /// `paired` re-reads whether the camera still pairs with `speaker`. A
    /// parting commits before its `stop_where`, so that stop either finds
    /// this claim, or ran before it and the re-read sees the parting. A stop
    /// that finds the claim before the audio plays ends the start: the clip
    /// fetch or the live negotiation is abandoned, a ringing INVITE is
    /// CANCELled, and an answered call is hung up.
    pub async fn start(
        &self,
        speaker: &SpeakerConfig,
        camera_id: Option<CameraId>,
        audio: &Audio,
        paired: impl Future<Output = bool>,
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
        if !paired.await {
            return Err(StartError::Unpaired);
        }
        let load = async {
            match audio {
                Audio::Tone => Ok(tone_pcm(TONE_MS)),
                Audio::Clip { clip } => clip::fetch(&clip.url, &clip.sha256, self.allow_http)
                    .await
                    .map_err(StartError::Clip),
                Audio::Live { .. } => Ok(Vec::new()),
            }
        };
        let mut pcm = tokio::select! {
            biased;
            () = session.cancel.cancelled() => return Err(StartError::Stopped),
            pcm = load => pcm?,
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
            place_call(&target, ANSWER_TIMEOUT, &session.cancel),
        )
        .await;
        let mut call = answered
            .unwrap_or(Err(TalkdownError::Unreachable))
            .map_err(|e| {
                if session.cancel.is_cancelled() {
                    StartError::Stopped
                } else {
                    StartError::Call(e)
                }
            })?;
        // A failed or stopped negotiation drops `call`, which sends its BYE,
        // then `session`, which releases the speaker.
        #[cfg(feature = "gstreamer-webrtc")]
        let (live, sdp_answer) = match audio {
            Audio::Live {
                sdp_offer,
                ice_servers,
            } => {
                let (live, answer) = tokio::select! {
                    () = session.cancel.cancelled() => return Err(StartError::Stopped),
                    negotiated = Live::answer(sdp_offer, ice_servers) => {
                        negotiated.map_err(StartError::Negotiation)?
                    }
                };
                (Some(live), Some(answer))
            }
            _ => (None, None),
        };
        #[cfg(not(feature = "gstreamer-webrtc"))]
        let sdp_answer = None;
        // A stop that crossed the answer: hang up rather than report success.
        if session.cancel.is_cancelled() {
            return Err(StartError::Stopped);
        }
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
        let id = Uuid::now_v7();
        let cancel = CancellationToken::new();
        match self.active.lock().entry(speaker_id) {
            Entry::Occupied(_) => return Err(StartError::Busy),
            Entry::Vacant(slot) => slot.insert((camera_id, id, cancel.clone())),
        };
        Ok(Session {
            active: self.active.clone(),
            id,
            speaker_id,
            camera_id,
            cancel,
        })
    }
}

/// A claimed speaker. Dropping it releases the speaker.
struct Session {
    active: Active,
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
        assert!(sessions.stop_where(|_, _, _| true).is_empty());
    }
}
