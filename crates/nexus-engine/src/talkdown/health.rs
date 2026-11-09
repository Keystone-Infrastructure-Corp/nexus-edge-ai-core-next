//! Speaker health: a SIP `OPTIONS` probe of every speaker once a minute, so
//! an installer or operator learns a speaker is down before a talk-down
//! fails, and the heartbeat can say so (`talkdown/speaker_offline`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_config::{SpeakerConfig, SpeakerId};
use nexus_store::Store;
use parking_lot::Mutex;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::session::TalkdownSessions;
use super::sip::options_ping;

pub const PROBE_EVERY: Duration = Duration::from_secs(60);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What the last probe of a speaker found. `since` is when `online` last
/// changed (or the first probe), not the probe time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpeakerStatus {
    pub online: bool,
    pub since: DateTime<Utc>,
    /// The `User-Agent` (or `Server`) the speaker answered with; empty when
    /// it sent none or is offline.
    pub user_agent: String,
}

/// The probe results, shared by the prober, the speakers GET and the
/// heartbeat's health roll-up.
#[derive(Clone, Default)]
pub struct SpeakerHealthMap(Arc<Mutex<HashMap<SpeakerId, SpeakerStatus>>>);

impl SpeakerHealthMap {
    /// `None` until the speaker has been probed.
    pub fn get(&self, id: SpeakerId) -> Option<SpeakerStatus> {
        self.0.lock().get(&id).cloned()
    }

    /// The ids of the speakers whose last probe got no answer, ascending.
    pub fn offline(&self) -> Vec<SpeakerId> {
        let mut ids: Vec<_> = self
            .0
            .lock()
            .iter()
            .filter(|(_, st)| !st.online)
            .map(|(&id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// A speaker whose probe got no answer, for another module's test.
    #[cfg(test)]
    pub(crate) fn mark_offline_for_test(&self, id: SpeakerId) {
        self.0.lock().insert(
            id,
            SpeakerStatus {
                online: false,
                since: Utc::now(),
                user_agent: String::new(),
            },
        );
    }

    fn record(&self, sp: &SpeakerConfig, user_agent: Option<String>) {
        let online = user_agent.is_some();
        let mut map = self.0.lock();
        let prev = map.get(&sp.id).map(|p| (p.online, p.since));
        let changed = prev.is_none_or(|(was, _)| was != online);
        // A speaker first seen online is the normal case, not a transition.
        if changed && (prev.is_some() || !online) {
            info!(speaker_id = sp.id, online, "speaker health changed");
        }
        let since = match prev {
            Some((_, since)) if !changed => since,
            _ => Utc::now(),
        };
        map.insert(
            sp.id,
            SpeakerStatus {
                online,
                since,
                user_agent: user_agent.unwrap_or_default(),
            },
        );
    }
}

/// Probe every speaker, serially, skipping the ones playing (their SIP
/// dialog is in use, and a session proves them online). A deleted speaker
/// leaves the map.
async fn probe_all(
    store: &Store,
    sessions: &TalkdownSessions,
    map: &SpeakerHealthMap,
    timeout: Duration,
) {
    let Ok(speakers) = store.list_speakers().await else {
        return;
    };
    map.0
        .lock()
        .retain(|id, _| speakers.iter().any(|sp| sp.id == *id));
    for sp in speakers.iter().filter(|sp| !sessions.is_busy(sp.id)) {
        let ua = options_ping(&sp.host, sp.port, None, timeout).await;
        map.record(sp, ua);
    }
}

/// Probe every [`PROBE_EVERY`] until `cancel`.
pub async fn run(
    store: Arc<Store>,
    sessions: TalkdownSessions,
    map: SpeakerHealthMap,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = probe_all(&store, &sessions, &map, PROBE_TIMEOUT) => {}
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(PROBE_EVERY) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::talkdown::session::{Audio, Origin, StartRequest};
    use crate::talkdown::sip::tests::{answer, fake_uas, reply, SDP};
    use nexus_store::SpeakerOutput;

    async fn store_with(
        speakers: &[(&str, u16)],
    ) -> (Arc<Store>, Vec<SpeakerConfig>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = nexus_config::StoreConfig {
            url: format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("nexus.db").display()
            ),
            ..Default::default()
        };
        let store = Arc::new(Store::open(&cfg).await.unwrap());
        let mut out = Vec::new();
        for (name, port) in speakers {
            let mut sp: SpeakerConfig = serde_json::from_value(
                serde_json::json!({ "name": name, "host": "127.0.0.1", "port": port }),
            )
            .unwrap();
            let mut tx = store.begin_tx().await.unwrap();
            store.create_speaker_tx(&mut tx, &mut sp).await.unwrap();
            Store::commit_tx(tx).await.unwrap();
            out.push(sp);
        }
        (store, out, dir)
    }

    /// A UDP port nothing answers on (the socket is held open so the probe
    /// times out instead of being refused).
    async fn silent_port() -> (u16, tokio::net::UdpSocket) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        (sock.local_addr().unwrap().port(), sock)
    }

    #[tokio::test]
    async fn an_answering_speaker_is_online_and_a_silent_one_goes_offline() {
        let (port, uas) = fake_uas("OPTIONS", |req| {
            vec![reply(req, "200 OK", "User-Agent: TSIP\r\n", "")]
        })
        .await;
        let (silent, _held) = silent_port().await;
        let (store, sps, _dir) = store_with(&[("up", port), ("down", silent)]).await;
        let (map, sessions) = (SpeakerHealthMap::default(), TalkdownSessions::default());
        assert_eq!(map.get(sps[0].id), None, "unknown before the first probe");

        let started = std::time::Instant::now();
        let before = Utc::now();
        probe_all(&store, &sessions, &map, Duration::from_millis(400)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "one full timeout"
        );
        uas.await.unwrap();

        let up = map.get(sps[0].id).unwrap();
        assert!(
            up.online && up.user_agent == "TSIP" && up.since >= before,
            "{up:?}"
        );
        let down = map.get(sps[1].id).unwrap();
        assert!(
            !down.online && down.user_agent.is_empty() && down.since >= before,
            "{down:?}"
        );
        assert_eq!(map.offline(), [sps[1].id]);

        // Still silent: `since` stays the time it went offline.
        probe_all(&store, &sessions, &map, Duration::from_millis(100)).await;
        assert_eq!(map.get(sps[1].id).unwrap().since, down.since);
        assert_eq!(PROBE_TIMEOUT, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_speaker_that_starts_answering_comes_back_online_with_a_new_since() {
        let (port, uas) = fake_uas("OPTIONS", |req| vec![reply(req, "200 OK", "", "")]).await;
        let (store, sps, _dir) = store_with(&[("a", port)]).await;
        let (map, sessions) = (SpeakerHealthMap::default(), TalkdownSessions::default());
        map.0.lock().insert(
            sps[0].id,
            SpeakerStatus {
                online: false,
                since: DateTime::<Utc>::MIN_UTC,
                user_agent: String::new(),
            },
        );
        probe_all(&store, &sessions, &map, Duration::from_secs(2)).await;
        uas.await.unwrap();
        let st = map.get(sps[0].id).unwrap();
        assert!(st.online && st.since > DateTime::<Utc>::MIN_UTC, "{st:?}");
        assert!(map.offline().is_empty());
    }

    #[tokio::test]
    async fn a_busy_speaker_is_not_probed_and_a_deleted_one_leaves_the_map() {
        let rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();
        let (port, uas) = fake_uas("BYE", move |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "200 OK", SDP, &answer(rtp_port, "0"))]
            } else if req.starts_with("BYE") || req.starts_with("OPTIONS") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let (store, sps, _dir) = store_with(&[("a", port)]).await;
        let (map, sessions) = (SpeakerHealthMap::default(), TalkdownSessions::default());
        let req = StartRequest {
            speaker: &sps[0],
            output: SpeakerOutput {
                volume: 100,
                boost: false,
                hw_volume: false,
                auto_max_per_hour: 6,
            },
            camera_id: None,
            origin: Origin::Manual,
            audio: &Audio::Tone,
            event_id: None,
        };
        let Ok((_, _, playing)) = sessions.start(req, async { true }).await else {
            panic!("the call starts");
        };
        assert!(sessions.is_busy(sps[0].id));
        probe_all(&store, &sessions, &map, Duration::from_millis(200)).await;
        assert_eq!(map.get(sps[0].id), None, "not probed while it plays");
        sessions.stop_where(|_, _, _| true);
        let _ = playing.await;
        let seen = uas.await.unwrap();
        assert!(seen.iter().all(|r| !r.starts_with("OPTIONS")), "{seen:?}");

        map.0.lock().insert(
            999,
            SpeakerStatus {
                online: false,
                since: Utc::now(),
                user_agent: String::new(),
            },
        );
        probe_all(&store, &sessions, &map, Duration::from_millis(100)).await;
        assert_eq!(
            map.get(999),
            None,
            "a speaker the store does not have is dropped"
        );
    }
}
