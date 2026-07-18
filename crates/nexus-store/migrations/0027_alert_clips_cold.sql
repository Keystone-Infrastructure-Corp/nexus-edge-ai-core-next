-- M-Alert-Clip cloud delivery: cold-replicate the short, burned-in alert
-- clip to the cloud `clips` table so the console alert shows the SAME
-- bbox-overlaid evidence the local sinks receive (not just the covering
-- motion clip resolved by timestamp). See docs/edge-core/M_ALERT_CLIP.md.
--
-- Alert clips are small (seconds, native-res) and few (one per motion
-- burst), so they deliberately SKIP the motion-clip cold replicator's
-- priority / quarantine / exponential-backoff machinery (migrations 0015
-- / 0023). We add only what the drain + evictor need:
--   * `sha256`            — hex digest of the finalized MP4, stamped when
--                           the builder marks the clip `ready`. The cloud
--                           `clip_replicated` envelope requires sha256_hex,
--                           and the drain skips any NULL-sha256 row.
--   * cold pointer        — `cold_handle` / `cold_path` / `cold_uploaded_at`
--                           record where the clip landed so the P6 evictor
--                           only reclaims the hot file after it is durably
--                           replicated (when the core is enrolled).
--   * failure bookkeeping — `cold_attempts` / `cold_last_attempt_at` /
--                           `cold_last_error` gate a fixed retry interval
--                           and surface the last error for diagnostics.
--
-- A NULL cold pointer forever (LAN-only / un-enrolled / cold disabled) is
-- normal: the evictor's grace window reclaims the hot file regardless, so
-- the local experience never depends on cloud reachability (fail-open).

ALTER TABLE alert_clips ADD COLUMN sha256 TEXT;                 -- 64-char lower hex, set at 'ready'
ALTER TABLE alert_clips ADD COLUMN cold_handle TEXT
    REFERENCES storage_backends(handle) ON DELETE RESTRICT;     -- active cold backend when replicated
ALTER TABLE alert_clips ADD COLUMN cold_path TEXT;             -- backend-relative path of the cold copy
ALTER TABLE alert_clips ADD COLUMN cold_uploaded_at TEXT;      -- RFC3339, NULL until replicated
ALTER TABLE alert_clips ADD COLUMN cold_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE alert_clips ADD COLUMN cold_last_attempt_at TEXT;  -- RFC3339 of most recent attempt
ALTER TABLE alert_clips ADD COLUMN cold_last_error TEXT;       -- last cold-upload error, for diagnostics

-- Drives alert_clips_pending_cold_upload: ready, hashed, not yet cold.
-- Partial index keeps the working set tiny (only un-replicated ready rows).
CREATE INDEX IF NOT EXISTS idx_alert_clips_pending_cold
    ON alert_clips(ready_at)
    WHERE state = 'ready' AND cold_uploaded_at IS NULL;
