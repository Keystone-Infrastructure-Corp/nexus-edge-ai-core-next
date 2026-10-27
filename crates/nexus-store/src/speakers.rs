//! IP speakers and their camera assignments (ADR-091, migration
//! `0038_speakers.sql`). `config_json` is the serialised
//! [`SpeakerConfig`], credentials included; it never leaves the edge.
//!
//! An unknown speaker or camera id is reported as [`StoreError::NotFound`]
//! so the admin API can answer 404/400 instead of 500.

use nexus_config::{SpeakerConfig, SpeakerId};
use nexus_types::CameraId;
use sqlx::Row;

use crate::{SqliteTx, Store, StoreError};

impl Store {
    pub async fn list_speakers(&self) -> Result<Vec<SpeakerConfig>, StoreError> {
        let rows = sqlx::query("SELECT config_json FROM speakers ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok(serde_json::from_str(r.try_get(0)?)?))
            .collect()
    }

    pub async fn get_speaker(&self, id: SpeakerId) -> Result<Option<SpeakerConfig>, StoreError> {
        let row = sqlx::query("SELECT config_json FROM speakers WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| Ok(serde_json::from_str(r.try_get(0)?)?))
            .transpose()
    }

    /// INSERT a new speaker and stamp the assigned rowid into `sp.id`, as
    /// [`Store::create_camera_tx`] does for cameras.
    pub async fn create_speaker_tx(
        &self,
        tx: &mut SqliteTx<'_>,
        sp: &mut SpeakerConfig,
    ) -> Result<(), StoreError> {
        let res = sqlx::query("INSERT INTO speakers (id, name, config_json) VALUES (NULL, ?, ?)")
            .bind(&sp.name)
            .bind(serde_json::to_string(sp)?)
            .execute(&mut **tx)
            .await?;
        sp.id = res.last_insert_rowid();
        sqlx::query("UPDATE speakers SET config_json = ? WHERE id = ?")
            .bind(serde_json::to_string(sp)?)
            .bind(sp.id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn update_speaker_tx(
        &self,
        tx: &mut SqliteTx<'_>,
        sp: &SpeakerConfig,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE speakers SET name = ?, config_json = ?, updated_at = CURRENT_TIMESTAMP \
             WHERE id = ?",
        )
        .bind(&sp.name)
        .bind(serde_json::to_string(sp)?)
        .bind(sp.id)
        .execute(&mut **tx)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("speaker {}", sp.id)));
        }
        Ok(())
    }

    /// Deletes the speaker; `ON DELETE CASCADE` clears its assignments.
    pub async fn delete_speaker_tx(
        &self,
        tx: &mut SqliteTx<'_>,
        id: SpeakerId,
    ) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM speakers WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("speaker {id}")));
        }
        Ok(())
    }

    pub async fn speaker_camera_ids(
        &self,
        speaker_id: SpeakerId,
    ) -> Result<Vec<CameraId>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT camera_id FROM speaker_cameras WHERE speaker_id = ? ORDER BY camera_id",
        )
        .bind(speaker_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The camera's talk-down speaker; `None` when it has none.
    pub async fn speaker_for_camera(
        &self,
        camera_id: CameraId,
    ) -> Result<Option<SpeakerConfig>, StoreError> {
        let row = sqlx::query(
            "SELECT s.config_json FROM speaker_cameras sc \
             JOIN speakers s ON s.id = sc.speaker_id WHERE sc.camera_id = ?",
        )
        .bind(camera_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok(serde_json::from_str(r.try_get(0)?)?))
            .transpose()
    }

    /// Make `camera_ids` exactly the speaker's cameras: each listed camera
    /// moves to this speaker, and a camera it had that is not listed is
    /// unassigned. An unknown camera fails the call; drop the tx to roll back.
    pub async fn set_speaker_cameras_tx(
        &self,
        tx: &mut SqliteTx<'_>,
        speaker_id: SpeakerId,
        camera_ids: &[CameraId],
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM speaker_cameras WHERE speaker_id = ?")
            .bind(speaker_id)
            .execute(&mut **tx)
            .await?;
        for &camera_id in camera_ids {
            assign_tx(tx, camera_id, speaker_id).await?;
        }
        Ok(())
    }

    /// Give the camera this speaker, or none. Unknown camera or speaker
    /// fails the call.
    pub async fn set_camera_speaker_tx(
        &self,
        tx: &mut SqliteTx<'_>,
        camera_id: CameraId,
        speaker_id: Option<SpeakerId>,
    ) -> Result<(), StoreError> {
        match speaker_id {
            Some(speaker_id) => assign_tx(tx, camera_id, speaker_id).await,
            None => {
                camera_exists_tx(tx, camera_id).await?;
                sqlx::query("DELETE FROM speaker_cameras WHERE camera_id = ?")
                    .bind(camera_id)
                    .execute(&mut **tx)
                    .await?;
                Ok(())
            }
        }
    }
}

/// Point the camera at the speaker, replacing any speaker it had.
async fn assign_tx(
    tx: &mut SqliteTx<'_>,
    camera_id: CameraId,
    speaker_id: SpeakerId,
) -> Result<(), StoreError> {
    camera_exists_tx(tx, camera_id).await?;
    let speaker: Option<i64> = sqlx::query_scalar("SELECT 1 FROM speakers WHERE id = ?")
        .bind(speaker_id)
        .fetch_optional(&mut **tx)
        .await?;
    if speaker.is_none() {
        return Err(StoreError::NotFound(format!("speaker {speaker_id}")));
    }
    sqlx::query(
        "INSERT INTO speaker_cameras (camera_id, speaker_id) VALUES (?, ?) \
         ON CONFLICT(camera_id) DO UPDATE SET speaker_id = excluded.speaker_id",
    )
    .bind(camera_id)
    .bind(speaker_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn camera_exists_tx(tx: &mut SqliteTx<'_>, camera_id: CameraId) -> Result<(), StoreError> {
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM cameras WHERE id = ?")
        .bind(camera_id)
        .fetch_optional(&mut **tx)
        .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(StoreError::NotFound(format!("camera {camera_id}"))),
    }
}
