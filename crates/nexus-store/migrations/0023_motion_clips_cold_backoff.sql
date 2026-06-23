-- 0023_motion_clips_cold_backoff.sql — cold-replicator backoff +
-- quarantine bookkeeping.
--
-- Before this migration the cold replicator drained pending clips
-- oldest-first with NO failure tracking. A clip that could never
-- upload — e.g. a corrupt camera stream whose byte-rate exploded a
-- single 640x360 clip to ~2 GiB, which cannot finish inside the
-- client PUT timeout on a typical edge uplink — sat at the head of
-- the queue forever, was re-attempted every tick, and head-of-line-
-- blocked every newer (healthy, small) clip behind it.
--
-- These additive columns let the replicator (a) exponentially back a
-- failing clip off so it stops monopolising the batch, and (b)
-- permanently quarantine a clip after MAX attempts (or pre-emptively
-- when its on-disk size exceeds the cold-upload ceiling) so the rest
-- of the queue drains. Every column has a safe default, so the
-- migration is a no-op for every healthy existing row.

ALTER TABLE motion_clips
    ADD COLUMN cold_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE motion_clips
    ADD COLUMN cold_last_attempt_at TEXT;
ALTER TABLE motion_clips
    ADD COLUMN cold_next_attempt_at TEXT;
ALTER TABLE motion_clips
    ADD COLUMN cold_quarantined INTEGER NOT NULL DEFAULT 0;
ALTER TABLE motion_clips
    ADD COLUMN cold_last_error TEXT;

-- Rebuild the partial index that drives clips_pending_cold_upload so
-- a permanently-quarantined clip drops out of the pending working
-- set entirely. The helper's WHERE clause adds the per-tick backoff
-- gate (cold_next_attempt_at <= now); the index just keeps the scan
-- over the still-eligible subset cheap.
DROP INDEX IF EXISTS idx_motion_clips_pending_cold;
CREATE INDEX IF NOT EXISTS idx_motion_clips_pending_cold
    ON motion_clips(ended_at)
    WHERE cold_handle IS NULL
      AND ended_at IS NOT NULL
      AND sha256 IS NOT NULL
      AND cold_quarantined = 0;
