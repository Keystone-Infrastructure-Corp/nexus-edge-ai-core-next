//! Talk-down audio library, play history and arming (SPEC-086, ADR-094/095;
//! migration `0040_talkdown_library.sql`).
//!
//! Samples are 8 kHz mono PCM16, stored as little-endian bytes. The list and
//! get calls never select them; only [`Store::talkdown_audio_pcm`] does.
//! No SAS URL is ever stored (it is a bearer credential).

use chrono::{DateTime, Utc};
use nexus_config::SpeakerId;
use sqlx::{sqlite::SqliteRow, Row};

use crate::{Store, StoreError};

const SAMPLE_RATE: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Clip,
    Tts,
}

impl AudioKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Clip => "clip",
            Self::Tts => "tts",
        }
    }
    fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "clip" => Ok(Self::Clip),
            "tts" => Ok(Self::Tts),
            o => Err(StoreError::Decode(format!("talkdown kind {o:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStatus {
    Ready,
    Pending,
    Failed,
}

impl AudioStatus {
    fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "ready" => Ok(Self::Ready),
            "pending" => Ok(Self::Pending),
            "failed" => Ok(Self::Failed),
            o => Err(StoreError::Decode(format!("talkdown status {o:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkdownAudio {
    pub uid: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: AudioKind,
    pub tts_text: Option<String>,
    pub tts_voice: Option<String>,
    pub status: AudioStatus,
    pub error: Option<String>,
    pub duration_ms: u32,
    pub version_token: Option<String>,
    pub has_previous: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct NewAudio {
    pub uid: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: AudioKind,
    pub tts_text: Option<String>,
    pub tts_voice: Option<String>,
    pub version_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkdownPlay {
    pub id: i64,
    pub session_id: String,
    pub origin: String,
    pub rule_id: Option<String>,
    pub step: Option<i64>,
    pub event_id: Option<String>,
    pub camera_id: Option<i64>,
    pub speaker_id: i64,
    pub audio_uid: Option<String>,
    pub mode: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub result: Option<String>,
    pub reason: Option<String>,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewPlay {
    pub session_id: String,
    pub origin: String,
    pub rule_id: Option<String>,
    pub step: Option<i64>,
    pub event_id: Option<String>,
    pub camera_id: Option<i64>,
    pub speaker_id: i64,
    pub audio_uid: Option<String>,
    pub mode: String,
}

#[derive(Debug, Clone, Default)]
pub struct PlayFilter {
    pub event_id: Option<String>,
    pub speaker_id: Option<i64>,
    /// Newest first; `None` means 100.
    pub limit: Option<i64>,
}

/// Arming as read back: `armed` already accounts for an elapsed `rearm_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmingState {
    pub armed: bool,
    pub rearm_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkdownCore {
    /// Effective: true when disarmed but `auto_rearm_at` has passed.
    pub auto_armed: bool,
    pub auto_rearm_at: Option<DateTime<Utc>>,
    pub auto_disarm_reason: Option<String>,
    pub cloud_allowed: bool,
    pub library_revision: i64,
}

const AUDIO_COLS: &str = "uid, name, description, kind, tts_text, tts_voice, status, error, \
    duration_ms, version_token, prev_pcm IS NOT NULL AS has_previous, updated_at";

fn audio_row(r: &SqliteRow) -> Result<TalkdownAudio, StoreError> {
    let duration: i64 = r.try_get("duration_ms")?;
    Ok(TalkdownAudio {
        uid: r.try_get("uid")?,
        name: r.try_get("name")?,
        description: r.try_get("description")?,
        kind: AudioKind::parse(r.try_get("kind")?)?,
        tts_text: r.try_get("tts_text")?,
        tts_voice: r.try_get("tts_voice")?,
        status: AudioStatus::parse(r.try_get("status")?)?,
        error: r.try_get("error")?,
        duration_ms: u32::try_from(duration).map_err(|e| StoreError::Decode(e.to_string()))?,
        version_token: r.try_get("version_token")?,
        has_previous: r.try_get("has_previous")?,
        updated_at: r.try_get("updated_at")?,
    })
}

fn play_row(r: &SqliteRow) -> Result<TalkdownPlay, StoreError> {
    Ok(TalkdownPlay {
        id: r.try_get("id")?,
        session_id: r.try_get("session_id")?,
        origin: r.try_get("origin")?,
        rule_id: r.try_get("rule_id")?,
        step: r.try_get("step")?,
        event_id: r.try_get("event_id")?,
        camera_id: r.try_get("camera_id")?,
        speaker_id: r.try_get("speaker_id")?,
        audio_uid: r.try_get("audio_uid")?,
        mode: r.try_get("mode")?,
        started_at: r.try_get("started_at")?,
        ended_at: r.try_get("ended_at")?,
        result: r.try_get("result")?,
        reason: r.try_get("reason")?,
        outcome: r.try_get("outcome")?,
    })
}

fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Armed unless disarmed with a `rearm_at` still in the future.
fn arming(
    armed: bool,
    rearm_at: Option<String>,
    reason: Option<String>,
) -> Result<ArmingState, StoreError> {
    let rearm_at = rearm_at
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| StoreError::Decode(format!("rearm_at: {e}")))
        })
        .transpose()?;
    let elapsed = rearm_at.is_some_and(|t| t <= Utc::now());
    Ok(ArmingState {
        armed: armed || elapsed,
        rearm_at,
        reason,
    })
}

fn nf(uid: &str) -> StoreError {
    StoreError::NotFound(format!("talkdown audio {uid}"))
}

impl Store {
    /// Metadata only; never selects samples.
    pub async fn list_talkdown_audio(&self) -> Result<Vec<TalkdownAudio>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {AUDIO_COLS} FROM talkdown_audio ORDER BY name, uid"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(audio_row).collect()
    }

    pub async fn get_talkdown_audio(&self, uid: &str) -> Result<Option<TalkdownAudio>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {AUDIO_COLS} FROM talkdown_audio WHERE uid = ?"
        ))
        .bind(uid)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(audio_row).transpose()
    }

    /// Samples of a `Ready` item; `None` for pending, failed or unknown.
    pub async fn talkdown_audio_pcm(&self, uid: &str) -> Result<Option<Vec<i16>>, StoreError> {
        let bytes: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT pcm FROM talkdown_audio WHERE uid = ? AND status = 'ready'")
                .bind(uid)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(bytes.map(|b| {
            b.as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect()
        }))
    }

    pub async fn count_talkdown_audio(&self) -> Result<u32, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM talkdown_audio")
            .fetch_one(&self.pool)
            .await?;
        u32::try_from(n).map_err(|e| StoreError::Decode(e.to_string()))
    }

    /// Insert, or update an existing item's definition and mark it pending.
    /// Existing samples stay until [`Store::set_talkdown_ready`] replaces
    /// them. A TTS item without text is refused by the table's CHECK.
    pub async fn upsert_talkdown_pending(&self, a: &NewAudio) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO talkdown_audio \
               (uid, name, description, kind, tts_text, tts_voice, status, version_token) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?) \
             ON CONFLICT(uid) DO UPDATE SET \
               prev_tts_text = CASE WHEN status = 'ready' THEN tts_text ELSE prev_tts_text END, \
               name = excluded.name, description = excluded.description, \
               kind = excluded.kind, tts_text = excluded.tts_text, \
               tts_voice = excluded.tts_voice, status = 'pending', error = NULL, \
               version_token = excluded.version_token, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&a.uid)
        .bind(&a.name)
        .bind(&a.description)
        .bind(a.kind.as_str())
        .bind(&a.tts_text)
        .bind(&a.tts_voice)
        .bind(&a.version_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Store new samples and mark ready; the current samples (if any) move
    /// to `prev_*` so [`Store::revert_talkdown_audio`] can bring them back.
    pub async fn set_talkdown_ready(&self, uid: &str, pcm: &[i16]) -> Result<(), StoreError> {
        let duration = i64::try_from(pcm.len() * 1000 / SAMPLE_RATE)
            .map_err(|e| StoreError::Decode(e.to_string()))?;
        let res = sqlx::query(
            "UPDATE talkdown_audio SET \
               prev_pcm = COALESCE(pcm, prev_pcm), \
               prev_duration_ms = CASE WHEN pcm IS NULL THEN prev_duration_ms ELSE duration_ms END, \
               pcm = ?, duration_ms = ?, status = 'ready', error = NULL, \
               updated_at = CURRENT_TIMESTAMP \
             WHERE uid = ?",
        )
        .bind(pcm_to_bytes(pcm))
        .bind(duration)
        .bind(uid)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(nf(uid));
        }
        Ok(())
    }

    pub async fn set_talkdown_failed(&self, uid: &str, error: &str) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE talkdown_audio SET status = 'failed', error = ?, \
             updated_at = CURRENT_TIMESTAMP WHERE uid = ?",
        )
        .bind(error)
        .bind(uid)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(nf(uid));
        }
        Ok(())
    }

    pub async fn update_talkdown_meta(
        &self,
        uid: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE talkdown_audio SET name = ?, description = ?, \
             updated_at = CURRENT_TIMESTAMP WHERE uid = ?",
        )
        .bind(name)
        .bind(description)
        .bind(uid)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(nf(uid));
        }
        Ok(())
    }

    /// Swap current and previous samples (and TTS text). `NotFound` when
    /// there is nothing to revert to. `version_token` is cleared because it
    /// described the samples that were just swapped out; the next sync
    /// re-establishes it.
    pub async fn revert_talkdown_audio(&self, uid: &str) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE talkdown_audio SET \
               pcm = prev_pcm, prev_pcm = pcm, \
               duration_ms = prev_duration_ms, prev_duration_ms = duration_ms, \
               tts_text = CASE WHEN kind = 'tts' THEN COALESCE(prev_tts_text, tts_text) ELSE tts_text END, \
               prev_tts_text = tts_text, \
               status = 'ready', error = NULL, version_token = NULL, \
               updated_at = CURRENT_TIMESTAMP \
             WHERE uid = ? AND prev_pcm IS NOT NULL",
        )
        .bind(uid)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(nf(uid));
        }
        Ok(())
    }

    pub async fn delete_talkdown_audio(&self, uid: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM talkdown_audio WHERE uid = ?")
            .bind(uid)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(nf(uid));
        }
        Ok(())
    }

    // ---- plays -----------------------------------------------------------

    /// Returns the new play's id.
    pub async fn insert_talkdown_play(&self, p: &NewPlay) -> Result<i64, StoreError> {
        let res = sqlx::query(
            "INSERT INTO talkdown_plays \
               (session_id, origin, rule_id, step, event_id, camera_id, speaker_id, audio_uid, mode) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&p.session_id)
        .bind(&p.origin)
        .bind(&p.rule_id)
        .bind(p.step)
        .bind(&p.event_id)
        .bind(p.camera_id)
        .bind(p.speaker_id)
        .bind(&p.audio_uid)
        .bind(&p.mode)
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    pub async fn finish_talkdown_play(
        &self,
        id: i64,
        result: &str,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE talkdown_plays SET ended_at = CURRENT_TIMESTAMP, result = ?, reason = ? \
             WHERE id = ?",
        )
        .bind(result)
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("talkdown play {id}")));
        }
        Ok(())
    }

    pub async fn set_talkdown_play_outcome(
        &self,
        id: i64,
        outcome: &str,
    ) -> Result<(), StoreError> {
        let res = sqlx::query("UPDATE talkdown_plays SET outcome = ? WHERE id = ?")
            .bind(outcome)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("talkdown play {id}")));
        }
        Ok(())
    }

    /// Newest first.
    pub async fn list_talkdown_plays(
        &self,
        f: &PlayFilter,
    ) -> Result<Vec<TalkdownPlay>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM talkdown_plays \
             WHERE (?1 IS NULL OR event_id = ?1) AND (?2 IS NULL OR speaker_id = ?2) \
             ORDER BY id DESC LIMIT ?3",
        )
        .bind(&f.event_id)
        .bind(f.speaker_id)
        .bind(f.limit.unwrap_or(100))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(play_row).collect()
    }

    /// Delete plays that started before `before`; returns the count removed.
    pub async fn prune_talkdown_plays(&self, before: DateTime<Utc>) -> Result<u64, StoreError> {
        // `started_at` is SQLite's `YYYY-MM-DD HH:MM:SS` UTC, which sorts as text.
        let res = sqlx::query("DELETE FROM talkdown_plays WHERE started_at < ?")
            .bind(before.format("%Y-%m-%d %H:%M:%S").to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // ---- arming and core settings ---------------------------------------

    pub async fn talkdown_core(&self) -> Result<TalkdownCore, StoreError> {
        let r = sqlx::query(
            "SELECT auto_armed, auto_rearm_at, auto_disarm_reason, cloud_allowed, library_revision \
             FROM talkdown_core WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        let a = arming(r.try_get::<i64, _>(0)? != 0, r.try_get(1)?, r.try_get(2)?)?;
        Ok(TalkdownCore {
            auto_armed: a.armed,
            auto_rearm_at: a.rearm_at,
            auto_disarm_reason: a.reason,
            cloud_allowed: r.try_get::<i64, _>(3)? != 0,
            library_revision: r.try_get(4)?,
        })
    }

    pub async fn set_talkdown_core_arming(
        &self,
        armed: bool,
        rearm_at: Option<DateTime<Utc>>,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE talkdown_core SET auto_armed = ?, auto_rearm_at = ?, auto_disarm_reason = ? \
             WHERE id = 1",
        )
        .bind(armed)
        .bind(rearm_at.map(ts))
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_talkdown_core_cloud_allowed(&self, allowed: bool) -> Result<(), StoreError> {
        sqlx::query("UPDATE talkdown_core SET cloud_allowed = ? WHERE id = 1")
            .bind(allowed)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_talkdown_core_library_revision(
        &self,
        revision: i64,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE talkdown_core SET library_revision = ? WHERE id = 1")
            .bind(revision)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_speaker_arming(
        &self,
        id: SpeakerId,
        armed: bool,
        rearm_at: Option<DateTime<Utc>>,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE speakers SET auto_armed = ?, auto_rearm_at = ?, auto_disarm_reason = ? \
             WHERE id = ?",
        )
        .bind(armed)
        .bind(rearm_at.map(ts))
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("speaker {id}")));
        }
        Ok(())
    }

    /// `None` for an unknown speaker.
    pub async fn speaker_arming(&self, id: SpeakerId) -> Result<Option<ArmingState>, StoreError> {
        let row = sqlx::query(
            "SELECT auto_armed, auto_rearm_at, auto_disarm_reason FROM speakers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| arming(r.try_get::<i64, _>(0)? != 0, r.try_get(1)?, r.try_get(2)?))
            .transpose()
    }
}
