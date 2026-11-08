//! Talk-down sessions (ADR-091): at most one per speaker. A start claims the
//! speaker before anything slow happens, and the claim is a guard that
//! releases the speaker when dropped, so every exit releases it: audio that
//! is not ready, an unanswered call, the end of playback, a panic in the play task.
//!
//! ADR-095 (d): a manual or live start pre-empts a rule play on the speaker
//! and a rule start never pre-empts anything; a rule start plays stored
//! audio only (b).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_cloud_protocol::v1::IceServer;
use nexus_config::{SpeakerConfig, SpeakerId};
use nexus_store::SpeakerOutput;
use nexus_types::CameraId;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::library::Library;
#[cfg(feature = "gstreamer-webrtc")]
use super::live::Live;
use super::sip::{place_call, SipCall, SipTarget, TalkdownError};
use super::tts;
use super::{tone_pcm, Gain};

/// How long a speaker gets to answer the INVITE.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(8);
const TONE_MS: u32 = 2000;
/// 300 ms of silence after the audio, so the BYE does not cut off what the
/// speaker still has buffered.
const TAIL_SAMPLES: usize = 300 * 8;
/// How long a manual start waits for the rule play it pre-empted to hang up.
const PREEMPT_WAIT: Duration = Duration::from_secs(3);
const PREEMPT_POLL: Duration = Duration::from_millis(50);
/// The silence between a rule step's repeats.
const REPEAT_GAP_SAMPLES: usize = 8000;

/// A start request's body: `{"mode": "tone"}`,
/// `{"mode": "stored", "audio_uid": …, "volume"?: 0-100}`,
/// `{"mode": "tts", "text": …, "voice"?: id, "volume"?: 0-100}` or
/// `{"mode": "live", "sdp_offer": …, "ice_servers": […]}`.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Audio {
    Tone,
    /// An item of the core's library; `volume` is the session's.
    Stored {
        audio_uid: String,
        #[serde(default)]
        volume: Option<u8>,
        /// How many times a rule step plays the item, with 1 s of silence
        /// between; never read from a request body (0 plays it once).
        #[serde(skip)]
        repeat: u8,
    },
    /// Typed speech, rendered on this core; `voice` defaults to the first
    /// discovered voice. The text goes into the audit row, never a span.
    Tts {
        text: String,
        #[serde(default)]
        voice: Option<String>,
        #[serde(default)]
        volume: Option<u8>,
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

impl Audio {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Tone => "tone",
            Self::Stored { .. } => "stored",
            Self::Tts { .. } => "tts",
            Self::Live { .. } => "live",
        }
    }
}

/// Who started a session. A rule play never pre-empts a manual or live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Manual,
    Rule { rule_id: String, step: u8 },
}

/// How a play that reached the speaker ended without a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Played,
    /// A stop or a parting cancelled it.
    Stopped,
    /// A manual or live start took the speaker from this rule play.
    Preempted,
}

/// Everything a start needs besides the audio. `output` is the speaker's
/// stored output, read by the caller.
pub struct StartRequest<'a> {
    pub speaker: &'a SpeakerConfig,
    pub output: SpeakerOutput,
    pub camera_id: Option<CameraId>,
    pub origin: Origin,
    pub audio: &'a Audio,
    /// The alert this play answers. The plays log records it (the caller
    /// writes the row: `speakers_admin::start_on`, the rule responder).
    pub event_id: Option<String>,
}

pub enum StartError {
    Busy,
    /// The camera no longer pairs with the speaker the caller read.
    Unpaired,
    /// A stop or a parting ended the session before it played.
    Stopped,
    /// No library item has the `audio_uid`.
    AudioNotFound,
    /// The item is still downloading or rendering, or failed to.
    AudioNotReady,
    /// The store could not be read.
    Internal(String),
    /// This core has no text-to-speech helper or voice.
    TtsUnavailable,
    /// The text or the voice was refused; never contains the text.
    TtsInvalid(String),
    /// The render failed or timed out.
    TtsFailed(String),
    Call(TalkdownError),
    /// This build has no live audio.
    #[cfg(not(feature = "gstreamer-webrtc"))]
    LiveUnsupported,
    /// The receiver could not answer the SFU's offer; never contains the SDP.
    #[cfg(feature = "gstreamer-webrtc")]
    Negotiation(String),
}

/// A busy speaker's session. `gain` is shared with the call; the session's
/// volume and boost are recomposed into it whenever either changes.
struct Claim {
    camera_id: Option<CameraId>,
    id: Uuid,
    cancel: CancellationToken,
    origin: Origin,
    /// Set when a manual start cancelled this rule play.
    preempted: Arc<AtomicBool>,
    gain: Gain,
    output: SpeakerOutput,
    session_volume: u8,
    boost: bool,
}

impl Claim {
    fn recompose(&self) {
        self.gain
            .set(Gain::compose(&self.output, self.session_volume, self.boost));
    }
}

/// Each busy speaker's session.
type Active = Arc<Mutex<HashMap<SpeakerId, Claim>>>;

/// When each speaker's last manual or live session ended.
type ManualEnds = Arc<Mutex<HashMap<SpeakerId, DateTime<Utc>>>>;

/// The engine's talk-down sessions, shared by every handler.
#[derive(Clone, Default)]
pub struct TalkdownSessions {
    active: Active,
    manual_ends: ManualEnds,
    /// Where `stored` audio comes from; `None` plays no stored audio.
    library: Option<Library>,
}

impl TalkdownSessions {
    pub fn with_library(library: Library) -> Self {
        Self {
            library: Some(library),
            ..Self::default()
        }
    }

    pub fn library(&self) -> Option<&Library> {
        self.library.as_ref()
    }

    /// What these sessions can play, as the talk-down GET lists it.
    pub fn modes(&self) -> Vec<&'static str> {
        let mut modes = vec!["tone"];
        if let Some(library) = &self.library {
            modes.push("stored");
            if library.tts().is_some() {
                modes.push("tts");
            }
        }
        if cfg!(feature = "gstreamer-webrtc") {
            modes.push("live");
        }
        modes
    }

    pub fn is_busy(&self, speaker_id: SpeakerId) -> bool {
        self.active.lock().contains_key(&speaker_id)
    }

    /// The camera that started the speaker's session (`None` for a speaker
    /// test) and its id, if one holds the speaker.
    pub fn session_of(&self, speaker_id: SpeakerId) -> Option<(Option<CameraId>, Uuid)> {
        self.active
            .lock()
            .get(&speaker_id)
            .map(|c| (c.camera_id, c.id))
    }

    /// Who started the speaker's session, if one holds it.
    pub fn origin_of(&self, speaker_id: SpeakerId) -> Option<Origin> {
        self.active
            .lock()
            .get(&speaker_id)
            .map(|c| c.origin.clone())
    }

    /// When the speaker's last manual or live session ended (ADR-095's
    /// hold-off); `None` if none has since the engine started.
    pub fn manual_ended_at(&self, speaker_id: SpeakerId) -> Option<DateTime<Utc>> {
        self.manual_ends.lock().get(&speaker_id).copied()
    }

    /// A manual session that ended at `at`, for another module's test.
    #[cfg(test)]
    pub(crate) fn record_manual_end_for_test(&self, speaker_id: SpeakerId, at: DateTime<Utc>) {
        self.manual_ends.lock().insert(speaker_id, at);
    }

    /// Set the volume (and boost) of the session `camera_id` started; false
    /// when it has none. The next frame uses it; nothing is persisted.
    pub fn set_session_volume(&self, camera_id: CameraId, volume: u8, boost: bool) -> bool {
        let mut active = self.active.lock();
        let Some(claim) = active.values_mut().find(|c| c.camera_id == Some(camera_id)) else {
            return false;
        };
        claim.session_volume = volume;
        claim.boost = boost;
        claim.recompose();
        true
    }

    /// The speaker's stored output changed: a session on it follows, without
    /// ending. The speaker's boost replaces the session's.
    pub fn set_speaker_output(&self, speaker_id: SpeakerId, output: &SpeakerOutput) {
        if let Some(claim) = self.active.lock().get_mut(&speaker_id) {
            claim.output = *output;
            claim.boost = output.boost;
            claim.recompose();
        }
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
            .filter(|(&speaker_id, c)| ends(speaker_id, c.camera_id, c.id))
            .map(|(&speaker_id, c)| {
                c.cancel.cancel();
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
    /// that finds the claim before the audio plays ends the start: the audio
    /// load or the live negotiation is abandoned, a ringing INVITE is
    /// CANCELled, and an answered call is hung up.
    pub async fn start(
        &self,
        req: StartRequest<'_>,
        paired: impl Future<Output = bool>,
    ) -> Result<
        (
            Uuid,
            Option<String>,
            JoinHandle<Result<Ended, TalkdownError>>,
        ),
        StartError,
    > {
        let StartRequest {
            speaker,
            output,
            camera_id,
            origin,
            audio,
            event_id: _,
        } = req;
        // ADR-095 (b): automation plays a stored item, never anything else.
        if matches!(origin, Origin::Rule { .. }) && !matches!(audio, Audio::Stored { .. }) {
            return Err(StartError::Internal(
                "a rule play plays stored audio only".into(),
            ));
        }
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
        let volume = match audio {
            Audio::Stored {
                volume: Some(v), ..
            }
            | Audio::Tts {
                volume: Some(v), ..
            } => *v,
            _ => 100,
        };
        // Typed speech is checked before the claim, so a refusal rings nothing.
        let speech = match audio {
            Audio::Tts { text, voice, .. } => {
                let tts = self
                    .library
                    .as_ref()
                    .and_then(|l| l.tts())
                    .ok_or(StartError::TtsUnavailable)?;
                let text = tts::validate_text(text, tts::ADHOC_MAX_CHARS)
                    .map_err(StartError::TtsInvalid)?;
                let voice = tts
                    .voice(voice.as_deref())
                    .ok_or_else(|| StartError::TtsInvalid("unknown voice".into()))?
                    .id
                    .clone();
                Some((tts.clone(), voice, text))
            }
            _ => None,
        };
        // Claim before anything slow, the store read included. A manual
        // start that cancelled a rule play waits for it to hang up.
        // shortcut: polls for the pre-empted call's release; use a per-speaker Notify if 3 s proves short.
        let deadline = tokio::time::Instant::now() + PREEMPT_WAIT;
        let mut preempted = false;
        let session = loop {
            match self.claim(speaker.id, camera_id, origin.clone(), output, volume) {
                Ok(session) => break session,
                Err(Refused::Preempting) if tokio::time::Instant::now() < deadline => {
                    preempted = true;
                    tokio::time::sleep(PREEMPT_POLL).await;
                }
                Err(_) => return Err(StartError::Busy),
            }
        };
        if !paired.await {
            return Err(StartError::Unpaired);
        }
        let load = async {
            match audio {
                Audio::Tone => Ok(tone_pcm(TONE_MS)),
                Audio::Stored {
                    audio_uid, repeat, ..
                } => match &self.library {
                    Some(lib) => lib.pcm(audio_uid).await.map(|pcm| repeated(pcm, *repeat)),
                    None => Err(StartError::AudioNotFound),
                },
                Audio::Tts { .. } => match &speech {
                    Some((tts, voice, text)) => tts
                        .speak(voice, text, tts::ADHOC_TIMEOUT)
                        .await
                        .map_err(StartError::TtsFailed),
                    None => Err(StartError::TtsUnavailable),
                },
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
        let answered = loop {
            let answered = tokio::time::timeout(
                ANSWER_TIMEOUT + Duration::from_secs(1),
                place_call(&target, ANSWER_TIMEOUT, &session.cancel),
            )
            .await
            .unwrap_or(Err(TalkdownError::Unreachable));
            // A single-line speaker still holding the pre-empted rule
            // call's dialog answers 486 (or 480) until that BYE lands.
            match answered {
                Err(TalkdownError::Rejected(486 | 480))
                    if preempted
                        && !session.cancel.is_cancelled()
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(PREEMPT_POLL).await;
                }
                answered => break answered,
            }
        };
        let mut call = answered.map_err(|e| {
            if session.cancel.is_cancelled() {
                StartError::Stopped
            } else {
                StartError::Call(e)
            }
        })?;
        call.set_gain(session.gain.clone());
        #[cfg(feature = "gstreamer-webrtc")]
        let (live, sdp_answer) = match audio {
            Audio::Live {
                sdp_offer,
                ice_servers,
            } => {
                let negotiated = tokio::select! {
                    () = session.cancel.cancelled() => Err(StartError::Stopped),
                    negotiated = Live::answer(sdp_offer, ice_servers) => {
                        negotiated.map_err(StartError::Negotiation)
                    }
                };
                match negotiated {
                    Ok((live, answer)) => (Some(live), Some(answer)),
                    Err(e) => return Err(hang_up(call, session, e)),
                }
            }
            _ => (None, None),
        };
        #[cfg(not(feature = "gstreamer-webrtc"))]
        let sdp_answer = None;
        // A stop that crossed the answer: hang up rather than report success.
        if session.cancel.is_cancelled() {
            return Err(hang_up(call, session, StartError::Stopped));
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
            let ended = if session.preempted.load(Ordering::Relaxed) {
                Ended::Preempted
            } else if session.cancel.is_cancelled() {
                Ended::Stopped
            } else {
                Ended::Played
            };
            let res = match played {
                Ok(()) => call.hangup().await.map(|()| ended),
                Err(e) => Err(e),
            };
            match &res {
                // The speaker's own hang-up, not a fault.
                Err(TalkdownError::EndedBySpeaker) => {
                    tracing::info!(
                        speaker_id = session.speaker_id,
                        "talk-down: the speaker hung up"
                    );
                }
                Err(e) => {
                    tracing::warn!(speaker_id = session.speaker_id, error = %e, "talk-down call failed");
                }
                Ok(_) => {}
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
        origin: Origin,
        output: SpeakerOutput,
        session_volume: u8,
    ) -> Result<Session, Refused> {
        let id = Uuid::now_v7();
        let cancel = CancellationToken::new();
        let preempted = Arc::new(AtomicBool::new(false));
        let gain = Gain::new(Gain::compose(&output, session_volume, output.boost));
        let manual = origin == Origin::Manual;
        let claim = Claim {
            camera_id,
            id,
            cancel: cancel.clone(),
            origin,
            preempted: preempted.clone(),
            gain: gain.clone(),
            output,
            session_volume,
            boost: output.boost,
        };
        match self.active.lock().entry(speaker_id) {
            // ADR-095 (d): an operator always wins over automation.
            Entry::Occupied(o) if manual && matches!(o.get().origin, Origin::Rule { .. }) => {
                o.get().preempted.store(true, Ordering::Relaxed);
                o.get().cancel.cancel();
                return Err(Refused::Preempting);
            }
            Entry::Occupied(_) => return Err(Refused::Busy),
            Entry::Vacant(slot) => slot.insert(claim),
        };
        Ok(Session {
            active: self.active.clone(),
            manual_ends: manual.then(|| self.manual_ends.clone()),
            id,
            speaker_id,
            camera_id,
            cancel,
            preempted,
            gain,
        })
    }
}

/// Why a claim was refused.
#[derive(Debug)]
enum Refused {
    Busy,
    /// A rule play holds the speaker and has been cancelled for this
    /// manual claim; it frees the speaker once it has hung up.
    Preempting,
}

/// `pcm` `repeat` times (0 counts as once), 1 s of silence between.
fn repeated(pcm: Vec<i16>, repeat: u8) -> Vec<i16> {
    let mut out = pcm.clone();
    for _ in 1..repeat {
        out.resize(out.len() + REPEAT_GAP_SAMPLES, 0);
        out.extend_from_slice(&pcm);
    }
    out
}

/// Refuse a start whose call was answered: hang up with the retransmitting
/// BYE (dropping `call` sends one best-effort BYE), then release the speaker.
fn hang_up(call: SipCall, session: Session, e: StartError) -> StartError {
    tokio::spawn(async move {
        let _ = call.hangup().await;
        drop(session);
    });
    e
}

/// A claimed speaker. Dropping it releases the speaker.
struct Session {
    active: Active,
    /// Where a manual session records its end, for the rule hold-off.
    manual_ends: Option<ManualEnds>,
    id: Uuid,
    speaker_id: SpeakerId,
    /// The camera that started it; `None` for an installer's speaker test.
    camera_id: Option<CameraId>,
    cancel: CancellationToken,
    preempted: Arc<AtomicBool>,
    gain: Gain,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Recorded before the release, so a rule start that finds the
        // speaker free also finds the hold-off.
        if let Some(ends) = &self.manual_ends {
            ends.lock().insert(self.speaker_id, Utc::now());
        }
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

    fn output() -> SpeakerOutput {
        SpeakerOutput {
            volume: 50,
            boost: false,
            hw_volume: false,
            auto_max_per_hour: 6,
        }
    }

    #[tokio::test]
    async fn a_session_volume_change_recomposes_the_live_gain_for_its_camera_only() {
        let sessions = TalkdownSessions::default();
        let session = sessions
            .claim(7, Some(1), Origin::Manual, output(), 100)
            .ok()
            .unwrap();
        assert!((session.gain.get() - 0.1).abs() < 1e-6, "speaker volume 50");

        assert!(sessions.set_session_volume(1, 50, false));
        assert!((session.gain.get() - 0.01).abs() < 1e-6);
        assert!(sessions.set_session_volume(1, 100, true));
        assert!((session.gain.get() - 0.2).abs() < 1e-6, "boost doubles it");
        assert!(!sessions.set_session_volume(2, 10, false), "another camera");

        // The speaker's stored output changed: the session follows, still playing.
        let louder = SpeakerOutput {
            volume: 100,
            boost: false,
            ..output()
        };
        sessions.set_speaker_output(7, &louder);
        assert_eq!(session.gain.get(), 1.0);
        assert!(sessions.is_busy(7));
        sessions.set_speaker_output(8, &louder); // no session: a no-op

        // A speaker test (no camera) has no session volume to set.
        drop(session);
        let test = sessions
            .claim(7, None, Origin::Manual, output(), 100)
            .ok()
            .unwrap();
        assert!(!sessions.set_session_volume(1, 10, false));
        drop(test);
    }

    #[tokio::test]
    async fn a_panicking_play_task_still_releases_the_speaker() {
        let sessions = TalkdownSessions::default();
        let claim = |camera| sessions.claim(7, camera, Origin::Manual, output(), 100);
        let session = claim(Some(1)).ok().unwrap();
        assert!(matches!(claim(None), Err(Refused::Busy)));
        let task = tokio::spawn(async move {
            let _session = session;
            panic!("play task panicked");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(!sessions.is_busy(7));
        assert!(sessions.stop_where(|_, _, _| true).is_empty());
    }

    fn rule(step: u8) -> Origin {
        Origin::Rule {
            rule_id: "r1".into(),
            step,
        }
    }

    /// ADR-095 (d): a manual claim cancels a rule play; a rule claim never
    /// cancels anything, rule or manual.
    #[tokio::test]
    async fn a_manual_claim_preempts_a_rule_play_and_a_rule_claim_preempts_nothing() {
        let sessions = TalkdownSessions::default();
        let held = sessions.claim(7, Some(1), rule(1), output(), 100).unwrap();
        assert!(matches!(
            sessions.claim(7, Some(1), rule(1), output(), 100),
            Err(Refused::Busy)
        ));
        assert!(
            !held.cancel.is_cancelled(),
            "a rule claim pre-empts nothing"
        );
        assert_eq!(sessions.origin_of(7), Some(rule(1)));

        assert!(matches!(
            sessions.claim(7, Some(1), Origin::Manual, output(), 100),
            Err(Refused::Preempting)
        ));
        assert!(held.cancel.is_cancelled() && held.preempted.load(Ordering::Relaxed));
        drop(held);

        let manual = sessions
            .claim(7, Some(1), Origin::Manual, output(), 100)
            .unwrap();
        assert!(matches!(
            sessions.claim(7, Some(1), rule(2), output(), 100),
            Err(Refused::Busy)
        ));
        assert!(matches!(
            sessions.claim(7, None, Origin::Manual, output(), 100),
            Err(Refused::Busy)
        ));
        assert!(
            !manual.cancel.is_cancelled(),
            "a manual play is never pre-empted"
        );
    }

    /// The hold-off's input: only a manual session records its end.
    #[tokio::test]
    async fn a_manual_session_records_its_end_and_a_rule_session_does_not() {
        let sessions = TalkdownSessions::default();
        drop(sessions.claim(7, Some(1), rule(1), output(), 100).unwrap());
        assert_eq!(sessions.manual_ended_at(7), None);
        let before = Utc::now();
        drop(
            sessions
                .claim(7, Some(1), Origin::Manual, output(), 100)
                .unwrap(),
        );
        assert!(sessions.manual_ended_at(7).is_some_and(|t| t >= before));
        assert_eq!(sessions.manual_ended_at(8), None);
    }

    #[test]
    fn a_step_repeats_its_audio_with_a_second_of_silence_between() {
        assert_eq!(repeated(vec![1, 2], 0), [1, 2]);
        assert_eq!(repeated(vec![1, 2], 1), [1, 2]);
        let twice = repeated(vec![1, 2], 2);
        assert_eq!(twice.len(), 2 + 8000 + 2);
        assert_eq!((&twice[..2], &twice[8002..]), (&[1, 2][..], &[1, 2][..]));
        assert!(twice[2..8002].iter().all(|&s| s == 0));
        assert_eq!(repeated(vec![5], 3).iter().filter(|&&s| s == 5).count(), 3);
    }

    /// ADR-095 (b): a rule start refuses anything but a stored item, before
    /// it claims the speaker.
    #[tokio::test]
    async fn a_rule_start_plays_stored_audio_only() {
        let sessions = TalkdownSessions::default();
        let sp: SpeakerConfig = serde_json::from_value(
            serde_json::json!({ "id": 7, "name": "s", "host": "127.0.0.1", "port": 9 }),
        )
        .unwrap();
        let req = StartRequest {
            speaker: &sp,
            output: output(),
            camera_id: Some(1),
            origin: rule(1),
            audio: &Audio::Tone,
            event_id: None,
        };
        let res = sessions.start(req, async { true }).await;
        assert!(matches!(res, Err(StartError::Internal(_))));
        assert!(!sessions.is_busy(7));
    }

    /// A manual start takes the speaker from a rule play: the rule play ends
    /// `Preempted`, and the manual call retries past the speaker's 486 (its
    /// line still held) within the 3 s budget.
    #[tokio::test]
    async fn a_manual_start_preempts_a_rule_play_and_retries_past_486() {
        use crate::talkdown::sip::tests::{answer, fake_uas, reply, SDP};
        use std::sync::atomic::AtomicUsize;

        let rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();
        let invites = Arc::new(AtomicUsize::new(0));
        let n = invites.clone();
        let (port, _uas) = fake_uas("NEVER", move |req| {
            if req.starts_with("INVITE") {
                match n.fetch_add(1, Ordering::SeqCst) {
                    1 => vec![reply(req, "486 Busy Here", "", "")],
                    _ => vec![reply(req, "200 OK", SDP, &answer(rtp_port, "0"))],
                }
            } else if req.starts_with("BYE") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            nexus_store::Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.path().join("n.db").display()),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        store
            .upsert_talkdown_pending(&nexus_store::NewAudio {
                uid: "u1".into(),
                name: "clip".into(),
                description: None,
                kind: nexus_store::AudioKind::Clip,
                tts_text: None,
                tts_voice: None,
                version_token: None,
            })
            .await
            .unwrap();
        store
            .set_talkdown_ready("u1", &[100; 80_000])
            .await
            .unwrap();
        let bus: Arc<dyn nexus_bus::Bus> = Arc::new(nexus_bus::BroadcastBus::new(8));
        let (library, _worker) = Library::spawn(store, bus, true, None);
        let sessions = TalkdownSessions::with_library(library);
        let sp: SpeakerConfig = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "s", "host": "127.0.0.1", "port": port
        }))
        .unwrap();
        let stored = Audio::Stored {
            audio_uid: "u1".into(),
            volume: None,
            repeat: 1,
        };
        let start = |origin, audio| StartRequest {
            speaker: &sp,
            output: output(),
            camera_id: Some(1),
            origin,
            audio,
            event_id: None,
        };
        let Ok((_, _, rule_play)) = sessions
            .start(start(rule(1), &stored), async { true })
            .await
        else {
            panic!("the rule play starts");
        };

        let started = std::time::Instant::now();
        let manual = sessions
            .start(start(Origin::Manual, &Audio::Tone), async { true })
            .await;
        let Ok((_, _, manual_play)) = manual else {
            panic!("the manual start pre-empts the rule play");
        };
        assert!(started.elapsed() < PREEMPT_WAIT, "{:?}", started.elapsed());
        assert!(matches!(rule_play.await.unwrap(), Ok(Ended::Preempted)));
        assert_eq!(invites.load(Ordering::SeqCst), 3, "rule, 486, retry");
        assert_eq!(sessions.origin_of(7), Some(Origin::Manual));
        sessions.stop_where(|_, _, _| true);
        assert!(matches!(manual_play.await.unwrap(), Ok(Ended::Stopped)));
    }
}
