//! M3.1: visual-prompt storage layer.
//!
//! Wraps the `visual_prompts` and `camera_visual_prompts` tables
//! (migration `0012_visual_prompts.sql`) with a strongly-typed
//! Rust API the engine handlers + worker bootstrap consume.
//!
//! Two design choices the handlers depend on:
//!
//! 1. **Embedding shape lives at the boundary.** The DB stores
//!    `f32` little-endian bytes — flat and dense. Encoders +
//!    detectors trade in `Vec<f32>`. This module is the only
//!    place that knows the conversion exists; everything above
//!    or below sees the typed form.
//!
//! 2. **`delete_visual_prompt` returns [`VisualPromptError::Conflict`]
//!    iff any camera is still attached.** The FK on
//!    `camera_visual_prompts` cascades — but a silent cascade
//!    would mean an accidental click in the admin UI quietly
//!    detaches the prompt from every camera that uses it. The
//!    handler renders the conflict as 409, the UI confirms
//!    detach-then-delete with the operator. Cascade stays
//!    behind so a CAMERA delete doesn't trip the same guard
//!    (cameras come and go; prompts shouldn't block).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use nexus_types::{CameraId, VisualPromptId};

use crate::{Store, StoreError};

/// Full visual-prompt row. The embedding is fetched as `Vec<f32>` on
/// demand via [`Store::list_camera_visual_prompts`] — listing the
/// admin grid uses [`VisualPromptSummary`] instead so we don't
/// haul ~50 KB per row over JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualPrompt {
    pub id: VisualPromptId,
    pub name: String,
    pub description: Option<String>,
    pub image_path: String,
    pub image_sha256: String,
    pub embedding_dim: i64,
    pub encoder_model_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lightweight projection for the `/api/v1/admin/visual-prompts` listing.
/// Omits the embedding so the JSON payload stays in the kilobyte
/// range even with hundreds of prompts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualPromptSummary {
    pub id: VisualPromptId,
    pub name: String,
    pub description: Option<String>,
    pub image_path: String,
    pub image_sha256: String,
    pub embedding_dim: i64,
    pub encoder_model_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How many cameras currently have this prompt attached.
    /// Drives the admin grid's "attached to N cameras" column
    /// AND the delete-confirmation flow.
    pub attached_camera_count: i64,
}

/// What the caller hands to [`Store::create_visual_prompt`]. The
/// engine pre-computes the embedding via the image encoder before
/// calling — the store is a dumb writer.
#[derive(Debug, Clone)]
pub struct NewVisualPrompt<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub image_path: &'a str,
    pub image_sha256: &'a str,
    pub embedding: &'a [f32],
    pub encoder_model_id: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum VisualPromptError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("visual prompt name already exists")]
    NameTaken,
    #[error("visual prompt not found")]
    NotFound,
    #[error("visual prompt is attached to {0} camera(s); detach first")]
    Conflict(i64),
    #[error("embedding is empty")]
    EmptyEmbedding,
    #[error("embedding blob has {actual} bytes, expected {expected} (dim={dim})")]
    EmbeddingShape {
        dim: i64,
        expected: usize,
        actual: usize,
    },
}

impl Store {
    /// Insert a new visual prompt. The embedding is encoded as
    /// `f32` little-endian bytes; the schema's
    /// `CHECK (length(embedding_blob) = embedding_dim * 4)` is a
    /// belt-and-suspenders check on this layout.
    pub async fn create_visual_prompt(
        &self,
        new: &NewVisualPrompt<'_>,
    ) -> Result<VisualPromptId, VisualPromptError> {
        if new.embedding.is_empty() {
            return Err(VisualPromptError::EmptyEmbedding);
        }
        let blob = encode_embedding(new.embedding);
        let dim = new.embedding.len() as i64;
        let res = sqlx::query_scalar::<_, i64>(
            "INSERT INTO visual_prompts (
                name, description, image_path, image_sha256,
                embedding_blob, embedding_dim, encoder_model_id
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            RETURNING id",
        )
        .bind(new.name)
        .bind(new.description)
        .bind(new.image_path)
        .bind(new.image_sha256)
        .bind(&blob)
        .bind(dim)
        .bind(new.encoder_model_id)
        .fetch_one(&self.pool)
        .await;
        match res {
            Ok(id) => Ok(id),
            Err(sqlx::Error::Database(db)) if is_unique_violation(&*db) => {
                Err(VisualPromptError::NameTaken)
            }
            Err(e) => Err(VisualPromptError::Store(StoreError::Sqlx(e))),
        }
    }

    /// Lookup a visual prompt by id. Returns the full row plus
    /// the decoded embedding. Used by the worker bootstrap path
    /// + the admin "edit" surface.
    pub async fn get_visual_prompt(
        &self,
        id: VisualPromptId,
    ) -> Result<Option<(VisualPrompt, Vec<f32>)>, VisualPromptError> {
        let row = sqlx::query(
            "SELECT id, name, description, image_path, image_sha256,
                    embedding_blob, embedding_dim, encoder_model_id,
                    created_at, updated_at
             FROM visual_prompts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        row.map(decode_visual_prompt_row).transpose()
    }

    /// Lookup by canonical name. Used by the engine before insert
    /// to render a friendly conflict ("a prompt named 'amazon_van'
    /// already exists, use a different name") instead of letting
    /// the UNIQUE constraint trip after the embedding's already
    /// been encoded.
    pub async fn get_visual_prompt_by_name(
        &self,
        name: &str,
    ) -> Result<Option<VisualPromptSummary>, VisualPromptError> {
        let row = sqlx::query(
            "SELECT vp.id, vp.name, vp.description, vp.image_path, vp.image_sha256,
                    vp.embedding_dim, vp.encoder_model_id,
                    vp.created_at, vp.updated_at,
                    COALESCE((SELECT COUNT(*) FROM camera_visual_prompts cvp
                              WHERE cvp.visual_prompt_id = vp.id), 0) AS attached_camera_count
             FROM visual_prompts vp
             WHERE vp.name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        row.map(decode_visual_prompt_summary_row).transpose()
    }

    /// List every visual prompt with attachment count. Drives the
    /// admin grid. Embeddings deliberately NOT included — see
    /// [`VisualPromptSummary`] doc.
    pub async fn list_visual_prompts(&self) -> Result<Vec<VisualPromptSummary>, VisualPromptError> {
        let rows = sqlx::query(
            "SELECT vp.id, vp.name, vp.description, vp.image_path, vp.image_sha256,
                    vp.embedding_dim, vp.encoder_model_id,
                    vp.created_at, vp.updated_at,
                    COALESCE((SELECT COUNT(*) FROM camera_visual_prompts cvp
                              WHERE cvp.visual_prompt_id = vp.id), 0) AS attached_camera_count
             FROM visual_prompts vp
             ORDER BY vp.name COLLATE NOCASE",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        rows.into_iter()
            .map(decode_visual_prompt_summary_row)
            .collect()
    }

    /// Delete a visual prompt. Returns [`VisualPromptError::Conflict`]
    /// with the attached-camera count if any camera still has the
    /// prompt attached. The handler maps that to 409; UI confirms
    /// detach-then-delete with the operator.
    pub async fn delete_visual_prompt(&self, id: VisualPromptId) -> Result<(), VisualPromptError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        let attached: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM camera_visual_prompts WHERE visual_prompt_id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        if attached > 0 {
            return Err(VisualPromptError::Conflict(attached));
        }
        let res = sqlx::query("DELETE FROM visual_prompts WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        if res.rows_affected() == 0 {
            return Err(VisualPromptError::NotFound);
        }
        tx.commit()
            .await
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        Ok(())
    }

    /// Attach a visual prompt to a camera. Idempotent: re-attaching
    /// the same pair is a no-op (UNIQUE on the composite PK).
    pub async fn attach_camera_visual_prompt(
        &self,
        camera_id: CameraId,
        visual_prompt_id: VisualPromptId,
    ) -> Result<(), VisualPromptError> {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO camera_visual_prompts (camera_id, visual_prompt_id)
             VALUES (?, ?)",
        )
        .bind(camera_id)
        .bind(visual_prompt_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if is_foreign_key_violation(&*db) => {
                // Either the camera or the visual prompt doesn't exist.
                // The handler verifies both ids before calling, so this
                // only fires under a race (e.g. the prompt was deleted
                // between fetch and attach).
                Err(VisualPromptError::NotFound)
            }
            Err(e) => Err(VisualPromptError::Store(StoreError::Sqlx(e))),
        }
    }

    /// Detach a single visual prompt from a single camera. Returns
    /// `NotFound` if the join row didn't exist (handler maps to 404).
    pub async fn detach_camera_visual_prompt(
        &self,
        camera_id: CameraId,
        visual_prompt_id: VisualPromptId,
    ) -> Result<(), VisualPromptError> {
        let res = sqlx::query(
            "DELETE FROM camera_visual_prompts
             WHERE camera_id = ? AND visual_prompt_id = ?",
        )
        .bind(camera_id)
        .bind(visual_prompt_id)
        .execute(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        if res.rows_affected() == 0 {
            return Err(VisualPromptError::NotFound);
        }
        Ok(())
    }

    /// Load every visual prompt attached to a camera, with the
    /// decoded embedding inline. This is the worker-bootstrap +
    /// per-camera-config-update hot path: the router uses the
    /// returned `Vec<f32>` to fill the YOLOE visual-prompt slot
    /// for the camera. Ordered by name for deterministic class-id
    /// assignment across restarts.
    pub async fn list_camera_visual_prompts(
        &self,
        camera_id: CameraId,
    ) -> Result<Vec<(VisualPrompt, Vec<f32>)>, VisualPromptError> {
        let rows = sqlx::query(
            "SELECT vp.id, vp.name, vp.description, vp.image_path, vp.image_sha256,
                    vp.embedding_blob, vp.embedding_dim, vp.encoder_model_id,
                    vp.created_at, vp.updated_at
             FROM visual_prompts vp
             JOIN camera_visual_prompts cvp ON cvp.visual_prompt_id = vp.id
             WHERE cvp.camera_id = ?
             ORDER BY vp.name COLLATE NOCASE",
        )
        .bind(camera_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        rows.into_iter().map(decode_visual_prompt_row).collect()
    }

    /// Lightweight list of the visual-prompt ids attached to a camera,
    /// without decoding embeddings. Phase 7.5.5 replace-reconcile uses
    /// this to diff the attached set against the fleet-listed set so it
    /// can detach prompts the fleet no longer manages.
    pub async fn list_camera_visual_prompt_ids(
        &self,
        camera_id: CameraId,
    ) -> Result<Vec<VisualPromptId>, VisualPromptError> {
        let rows = sqlx::query(
            "SELECT visual_prompt_id FROM camera_visual_prompts
             WHERE camera_id = ?",
        )
        .bind(camera_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
        Ok(rows.into_iter().map(|r| r.get(0)).collect())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_embedding(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_embedding(blob: &[u8], dim: i64) -> Result<Vec<f32>, VisualPromptError> {
    let expected = (dim as usize)
        .checked_mul(4)
        .ok_or(VisualPromptError::EmbeddingShape {
            dim,
            expected: usize::MAX,
            actual: blob.len(),
        })?;
    if blob.len() != expected {
        return Err(VisualPromptError::EmbeddingShape {
            dim,
            expected,
            actual: blob.len(),
        });
    }
    let mut out = Vec::with_capacity(dim as usize);
    for chunk in blob.chunks_exact(4) {
        // chunks_exact guarantees the slice length on each iteration.
        let arr: [u8; 4] = chunk.try_into().expect("chunks_exact yields 4 bytes");
        out.push(f32::from_le_bytes(arr));
    }
    Ok(out)
}

fn decode_visual_prompt_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<(VisualPrompt, Vec<f32>), VisualPromptError> {
    let blob: Vec<u8> = row
        .try_get("embedding_blob")
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
    let embedding_dim: i64 = row
        .try_get("embedding_dim")
        .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?;
    let embedding = decode_embedding(&blob, embedding_dim)?;
    let prompt = VisualPrompt {
        id: row
            .try_get("id")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        name: row
            .try_get("name")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        description: row
            .try_get("description")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        image_path: row
            .try_get("image_path")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        image_sha256: row
            .try_get("image_sha256")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        embedding_dim,
        encoder_model_id: row
            .try_get("encoder_model_id")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
    };
    Ok((prompt, embedding))
}

fn decode_visual_prompt_summary_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<VisualPromptSummary, VisualPromptError> {
    Ok(VisualPromptSummary {
        id: row
            .try_get("id")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        name: row
            .try_get("name")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        description: row
            .try_get("description")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        image_path: row
            .try_get("image_path")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        image_sha256: row
            .try_get("image_sha256")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        embedding_dim: row
            .try_get("embedding_dim")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        encoder_model_id: row
            .try_get("encoder_model_id")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
        attached_camera_count: row
            .try_get("attached_camera_count")
            .map_err(|e| VisualPromptError::Store(StoreError::Sqlx(e)))?,
    })
}

fn is_unique_violation(db: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLITE_CONSTRAINT_UNIQUE = 2067; primary = 19; PK = 1555.
    db.code()
        .map(|c| c == "2067" || c == "19" || c == "1555")
        .unwrap_or(false)
}

fn is_foreign_key_violation(db: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLITE_CONSTRAINT_FOREIGNKEY = 787; primary = 19.
    db.code().map(|c| c == "787" || c == "19").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_round_trips_through_le_bytes() {
        let values: Vec<f32> = vec![0.0, 1.0, -1.0, 1.5e-3, 4.20667, f32::MIN_POSITIVE];
        let encoded = encode_embedding(&values);
        assert_eq!(encoded.len(), values.len() * 4);
        let decoded = decode_embedding(&encoded, values.len() as i64).expect("decode ok");
        assert_eq!(decoded, values);
    }

    #[test]
    fn decode_rejects_mismatched_length() {
        let encoded = encode_embedding(&[0.0_f32, 1.0]);
        // Claim dim=3 against an 8-byte blob (dim*4 = 12, mismatched).
        let err = decode_embedding(&encoded, 3).expect_err("mismatch must error");
        assert!(matches!(err, VisualPromptError::EmbeddingShape { .. }));
    }
}
