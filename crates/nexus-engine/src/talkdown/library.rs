//! The talk-down audio library (SPEC-086, ADR-094): items the fleet's
//! `talkdown_audio` category syncs onto the core and that a play names by
//! `audio_uid`, so playback needs no cloud (fail-open). A sync answers at
//! once; clips download, and TTS items render, on one background worker.
//! A clip's read SAS URL lives only in its in-memory [`Job`]: never stored,
//! logged or put in an error.

use std::sync::Arc;

use nexus_bus::{topic, Bus, BusExt};
use nexus_store::{AudioKind, AudioStatus, NewAudio, Store, StoreError, TalkdownAudio};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, OwnedMutexGuard};
use tokio::task::JoinHandle;

use super::session::StartError;
use super::tts::{self, Tts};
use super::{clip, limit};

/// Room for every item of one sync (the org cap is 25) and a re-push.
const QUEUE: usize = 64;
/// -20 dBFS RMS, the loudness every clip and render is brought to.
const TARGET_RMS: f64 = 32767.0 * 0.1;

/// Background work on one item.
pub enum Job {
    /// `url` is a read SAS: a bearer credential.
    Fetch {
        uid: String,
        url: String,
        sha256: String,
    },
    /// Render the item's stored text in its stored voice.
    Render { uid: String },
}

/// The core's library. Cheap to clone: every clone shares the worker.
#[derive(Clone)]
pub struct Library {
    store: Arc<Store>,
    jobs: mpsc::Sender<Job>,
    /// Held by a sync from its marker read to its marker write, and by the
    /// worker while it records a result, so neither acts on a stale row.
    lock: Arc<Mutex<()>>,
    /// Renders TTS items, typed speech and previews; `None` on a core
    /// without the helper or a voice.
    tts: Option<Arc<Tts>>,
    allow_http: bool,
}

/// One item of a `talkdown_audio` push. Unknown fields (`duration_ms`)
/// are ignored.
#[derive(Deserialize)]
pub struct Item {
    uid: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(flatten)]
    source: Source,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Source {
    Clip { sha256: String, url: String },
    Tts { text: String, voice: String },
}

impl Item {
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The table's limits, checked before anything is written.
    pub fn validate(&self) -> Result<(), String> {
        let chars = |s: &str| s.chars().count();
        let uid = &self.uid;
        if uuid::Uuid::parse_str(uid).is_err() {
            return Err(format!("item {uid:?}: uid must be a UUID"));
        }
        if !(1..=80).contains(&chars(&self.name)) {
            return Err(format!("item {uid}: name must be 1 to 80 characters"));
        }
        if self.description.as_deref().is_some_and(|d| chars(d) > 500) {
            return Err(format!("item {uid}: description is over 500 characters"));
        }
        match &self.source {
            Source::Clip { sha256, .. } => {
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("item {uid}: sha256 must be 64 hex digits"));
                }
            }
            // A voice this core lacks fails its render, not the sync.
            Source::Tts { text, voice } => {
                if !(1..=500).contains(&chars(text)) {
                    return Err(format!("item {uid}: text must be 1 to 500 characters"));
                }
                if !(1..=64).contains(&chars(voice)) {
                    return Err(format!("item {uid}: voice must be 1 to 64 characters"));
                }
            }
        }
        Ok(())
    }

    /// Decides whether a push needs a refetch or a re-render: the WAV's
    /// sha256 for a clip, sha256(voice, text) for TTS.
    fn version_token(&self) -> String {
        match &self.source {
            Source::Clip { sha256, .. } => sha256.clone(),
            Source::Tts { text, voice } => {
                crate::fleet_apply::sha256_hex(format!("{voice}\n{text}").as_bytes())
            }
        }
    }
}

/// The hash contract's shape of a stored item (cloud `fleet/hash.rs`):
/// `{uid, name, description, kind, sha256}` for a clip and
/// `{uid, name, description, kind, text, voice}` for TTS.
pub fn contract_json(a: &TalkdownAudio) -> Value {
    match a.kind {
        AudioKind::Clip => json!({
            "uid": a.uid, "name": a.name, "description": a.description,
            "kind": "clip", "sha256": a.version_token,
        }),
        AudioKind::Tts => json!({
            "uid": a.uid, "name": a.name, "description": a.description,
            "kind": "tts", "text": a.tts_text, "voice": a.tts_voice,
        }),
    }
}

pub fn kind_str(kind: AudioKind) -> &'static str {
    match kind {
        AudioKind::Clip => "clip",
        AudioKind::Tts => "tts",
    }
}

pub fn status_str(status: AudioStatus) -> &'static str {
    match status {
        AudioStatus::Ready => "ready",
        AudioStatus::Pending => "pending",
        AudioStatus::Failed => "failed",
    }
}

/// Bring `pcm` to -20 dBFS RMS through the soft limiter, so every item
/// plays at the same loudness and none wraps. Digital silence stays.
pub fn normalize(pcm: &mut [i16]) {
    if pcm.is_empty() {
        return;
    }
    let energy: f64 = pcm.iter().map(|&s| f64::from(s).powi(2)).sum();
    let rms = (energy / pcm.len() as f64).sqrt();
    if rms == 0.0 {
        return;
    }
    let gain = (TARGET_RMS / rms) as f32;
    for s in pcm {
        *s = limit(f32::from(*s) * gain);
    }
}

impl Library {
    /// The library and its worker, which runs until every clone is dropped.
    /// `allow_http` exists for the tests' plain-http clip and preview servers only.
    pub fn spawn(
        store: Arc<Store>,
        bus: Arc<dyn Bus>,
        allow_http: bool,
        tts: Option<Arc<Tts>>,
    ) -> (Self, JoinHandle<()>) {
        let (jobs, rx) = mpsc::channel(QUEUE);
        let lock = Arc::new(Mutex::new(()));
        let worker = tokio::spawn(work(
            store.clone(),
            bus,
            lock.clone(),
            rx,
            allow_http,
            tts.clone(),
        ));
        let library = Self {
            store,
            jobs,
            lock,
            tts,
            allow_http,
        };
        (library, worker)
    }

    pub fn tts(&self) -> Option<&Arc<Tts>> {
        self.tts.as_ref()
    }

    pub fn allow_http(&self) -> bool {
        self.allow_http
    }

    /// The samples of a ready item.
    pub async fn pcm(&self, uid: &str) -> Result<Vec<i16>, StartError> {
        let internal = |e: StoreError| StartError::Internal(e.to_string());
        if let Some(pcm) = self.store.talkdown_audio_pcm(uid).await.map_err(internal)? {
            return Ok(pcm);
        }
        match self.store.get_talkdown_audio(uid).await.map_err(internal)? {
            Some(_) => Err(StartError::AudioNotReady),
            None => Err(StartError::AudioNotFound),
        }
    }

    /// Serialises syncs with each other and with the worker's writes.
    pub async fn lock(&self) -> OwnedMutexGuard<()> {
        self.lock.clone().lock_owned().await
    }

    /// Write a push: an item whose content is unchanged and ready only takes
    /// its new name and description; any other is marked pending and queued.
    /// `retract` (uids the fleet owned and no longer lists) are deleted.
    /// Returns each item's `{uid, status}`. Call it holding [`Self::lock`].
    pub async fn sync(&self, items: &[Item], retract: &[&str]) -> Result<Vec<Value>, StoreError> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let token = item.version_token();
            let current = self.store.get_talkdown_audio(&item.uid).await?;
            let unchanged = current.as_ref().is_some_and(|a| {
                a.status == AudioStatus::Ready && a.version_token.as_deref() == Some(&token)
            });
            if unchanged {
                self.store
                    .update_talkdown_meta(&item.uid, &item.name, item.description.as_deref())
                    .await?;
                out.push(json!({ "uid": item.uid, "status": "ready" }));
                continue;
            }
            let (kind, tts_text, tts_voice, job) = match &item.source {
                Source::Clip { sha256, url } => (
                    AudioKind::Clip,
                    None,
                    None,
                    Job::Fetch {
                        uid: item.uid.clone(),
                        url: url.clone(),
                        sha256: sha256.clone(),
                    },
                ),
                Source::Tts { text, voice } => (
                    AudioKind::Tts,
                    Some(text.clone()),
                    Some(voice.clone()),
                    Job::Render {
                        uid: item.uid.clone(),
                    },
                ),
            };
            self.store
                .upsert_talkdown_pending(&NewAudio {
                    uid: item.uid.clone(),
                    name: item.name.clone(),
                    description: item.description.clone(),
                    kind,
                    tts_text,
                    tts_voice,
                    version_token: Some(token),
                })
                .await?;
            // A full queue leaves the item pending; its absence from the
            // hash makes the cloud re-push it.
            if self.jobs.try_send(job).is_err() {
                tracing::warn!(uid = %item.uid, "talk-down library queue full; item stays pending");
            }
            out.push(json!({ "uid": item.uid, "status": "pending" }));
        }
        for uid in retract {
            match self.store.delete_talkdown_audio(uid).await {
                Ok(()) | Err(StoreError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// The item is still pending at this version, so the job's result applies.
async fn wanted(store: &Store, uid: &str, token: Option<&str>) -> bool {
    match store.get_talkdown_audio(uid).await {
        Ok(Some(a)) => {
            a.status != AudioStatus::Ready
                && token.is_none_or(|t| a.version_token.as_deref() == Some(t))
        }
        _ => false,
    }
}

async fn work(
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    lock: Arc<Mutex<()>>,
    mut rx: mpsc::Receiver<Job>,
    allow_http: bool,
    tts: Option<Arc<Tts>>,
) {
    while let Some(job) = rx.recv().await {
        let (uid, token, result) = match job {
            Job::Fetch { uid, url, sha256 } => {
                // A re-push queues the same version again: fetch it once.
                if !wanted(&store, &uid, Some(&sha256)).await {
                    continue;
                }
                let result = clip::fetch(&url, &sha256, allow_http).await.map(|mut pcm| {
                    normalize(&mut pcm);
                    pcm
                });
                (uid, Some(sha256), result)
            }
            // One at a time: this worker, and `Tts`'s own lock against
            // typed speech and previews.
            Job::Render { uid } => {
                let Ok(Some(item)) = store.get_talkdown_audio(&uid).await else {
                    continue;
                };
                if item.status == AudioStatus::Ready {
                    continue;
                }
                let result = match (&tts, item.tts_text, item.tts_voice) {
                    (None, ..) => Err("text-to-speech is not available on this core".to_string()),
                    (Some(tts), Some(text), Some(voice)) => {
                        match tts::validate_text(&text, tts::LIBRARY_MAX_CHARS) {
                            Ok(text) => tts.speak(&voice, &text, tts::LIBRARY_TIMEOUT).await,
                            Err(why) => Err(why),
                        }
                    }
                    _ => Err("the item has no text or voice".to_string()),
                };
                (uid, item.version_token, result)
            }
        };
        let _held = lock.lock().await;
        // A sync that changed or removed the item since wins.
        if !wanted(&store, &uid, token.as_deref()).await {
            continue;
        }
        let written = match &result {
            Ok(pcm) => store.set_talkdown_ready(&uid, pcm).await,
            Err(why) => {
                tracing::warn!(uid = %uid, error = %why, "talk-down audio failed");
                store.set_talkdown_failed(&uid, why).await
            }
        };
        if let Err(e) = written {
            tracing::warn!(uid = %uid, error = %e, "talk-down audio state not recorded");
        }
        let _ = bus
            .publish(
                topic::CONFIG_CHANGED,
                &json!({ "kind": "talkdown_audio", "uid": uid }),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dbfs_rms(pcm: &[i16]) -> f64 {
        let energy: f64 = pcm.iter().map(|&s| f64::from(s).powi(2)).sum();
        20.0 * ((energy / pcm.len() as f64).sqrt() / 32767.0).log10()
    }

    fn sine(amplitude: f64) -> Vec<i16> {
        (0..8000)
            .map(|i| {
                (amplitude * (std::f64::consts::TAU * 440.0 * f64::from(i) / 8000.0).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn normalize_brings_a_quiet_clip_to_minus_20_dbfs_with_headroom() {
        // A sine's RMS is its peak / sqrt(2): -40 dBFS RMS.
        let mut pcm = sine(32767.0 * 0.01 * std::f64::consts::SQRT_2);
        assert!((dbfs_rms(&pcm) + 40.0).abs() < 0.2);
        normalize(&mut pcm);
        assert!((dbfs_rms(&pcm) + 20.0).abs() < 0.2, "{}", dbfs_rms(&pcm));
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(f64::from(peak) <= 32767.0 * 0.891, "peak {peak}");
    }

    #[test]
    fn normalize_leaves_silence_and_limits_a_loud_clip_without_wrapping() {
        let mut silent = vec![0i16; 800];
        normalize(&mut silent);
        assert!(silent.iter().all(|&s| s == 0));
        normalize(&mut []);

        // Mostly quiet with full-scale spikes: the gain pushes the spikes
        // past full scale, and the limiter keeps their sign.
        let mut spiky: Vec<i16> = (0..8000)
            .map(|i| match i % 400 {
                0 => i16::MAX,
                200 => i16::MIN,
                _ => 30,
            })
            .collect();
        normalize(&mut spiky);
        for (i, &s) in spiky.iter().enumerate() {
            match i % 400 {
                0 => assert!(s > 29_000, "positive spike at {i}: {s}"),
                200 => assert!(s < -29_000, "negative spike at {i}: {s}"),
                _ => assert!(s > 0, "quiet sample at {i}: {s}"),
            }
        }
    }

    #[test]
    fn items_are_validated_against_the_tables_limits() {
        let item = |v: Value| serde_json::from_value::<Item>(v).map_err(|e| e.to_string());
        let uid = "a1b2c3d4-0000-4000-8000-000000000001";
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let clip = json!({ "uid": uid, "name": "n", "kind": "clip", "sha256": sha,
            "url": "https://x/y", "duration_ms": 1 });
        item(clip.clone()).unwrap().validate().unwrap();
        let tts = json!({ "uid": uid, "name": "n", "description": null, "kind": "tts",
            "text": "Leave", "voice": "en_US-female" });
        item(tts.clone()).unwrap().validate().unwrap();

        for (field, bad) in [
            ("uid", json!("not-a-uuid")),
            ("name", json!("")),
            ("name", json!("x".repeat(81))),
            ("description", json!("x".repeat(501))),
            ("sha256", json!("00")),
            ("sha256", json!("z".repeat(64))),
        ] {
            let mut v = clip.clone();
            v[field] = bad;
            assert!(item(v).unwrap().validate().is_err(), "{field}");
        }
        for (field, bad) in [
            ("text", json!("")),
            ("text", json!("x".repeat(501))),
            ("voice", json!("")),
        ] {
            let mut v = tts.clone();
            v[field] = bad;
            assert!(item(v).unwrap().validate().is_err(), "{field}");
        }
        // Shape errors are refused at parse.
        for missing in ["sha256", "url"] {
            let mut v = clip.clone();
            v.as_object_mut().unwrap().remove(missing);
            assert!(item(v).is_err(), "{missing}");
        }
        let mut v = tts.clone();
        v["kind"] = json!("siren");
        assert!(item(v).is_err());
    }
}
