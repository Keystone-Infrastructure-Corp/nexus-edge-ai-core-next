-- SPEC-090 — authoritative recording generation / delivery accounting.
--
-- `recording_minute_buckets` holds one row per (UTC minute, camera, source)
-- with counters written at the transition that caused them: generation at
-- clip finalization, attempted bytes when a transfer starts (retries
-- included), delivery at the durable cold completion, failures at the
-- recorded failure, and eviction before upload at the evictor. Rows older
-- than seven days are pruned on write.
--
--   * `minute_utc`  — unix seconds of the minute start (UTC). Leads the key
--                     so window reads and pruning are range scans.
--   * `source_kind` — 'ordinary' (motion_clips) or 'alert' (alert_clips).
--   * `*_unknown_size_count` / `evicted_before_upload_count` — rows whose
--                     size was not known (size_bytes <= 0) still count, so
--                     a byte total is never silently short.
--
-- No FK to cameras: a deleted camera's history stays until it ages out.

CREATE TABLE IF NOT EXISTS recording_minute_buckets (
    minute_utc                   INTEGER NOT NULL,
    camera_id                    INTEGER NOT NULL,
    source_kind                  TEXT    NOT NULL CHECK (source_kind IN ('ordinary', 'alert')),
    generated_bytes              INTEGER NOT NULL DEFAULT 0,
    generated_unknown_size_count INTEGER NOT NULL DEFAULT 0,
    recorded_ms                  INTEGER NOT NULL DEFAULT 0,
    delivered_bytes              INTEGER NOT NULL DEFAULT 0,
    attempted_bytes              INTEGER NOT NULL DEFAULT 0,
    retry_count                  INTEGER NOT NULL DEFAULT 0,
    failure_count                INTEGER NOT NULL DEFAULT 0,
    evicted_before_upload_bytes  INTEGER NOT NULL DEFAULT 0,
    evicted_before_upload_count  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (minute_utc, camera_id, source_kind)
) WITHOUT ROWID;

-- When accounting began on this core. Counters cover only time after it;
-- a window that starts earlier is reported incomplete, never backfilled.
CREATE TABLE IF NOT EXISTS recording_delivery_baseline (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    accounting_since TEXT    NOT NULL
);
INSERT OR IGNORE INTO recording_delivery_baseline (id, accounting_since)
    VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

-- Idempotent markers: set by the transition that counts a clip, so a
-- duplicate close, completion or overwritten receipt never counts twice.
-- Alert generation needs none: `building -> ready` happens once.
ALTER TABLE motion_clips ADD COLUMN acct_generated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE motion_clips ADD COLUMN acct_delivered INTEGER NOT NULL DEFAULT 0;
ALTER TABLE alert_clips ADD COLUMN acct_delivered INTEGER NOT NULL DEFAULT 0;
-- When the evictor counted an alert clip lost (RFC 3339, ms, Z — the
-- `accounting_since` shape). A PUT that read the file before the unlink
-- can still complete; its delivery takes that loss back from the same
-- bucket and clears this.
ALTER TABLE alert_clips ADD COLUMN acct_evicted_at TEXT;
-- Cold transfers started per clip, counted in the transaction that counts
-- the attempt. `cold_attempts` grows only on a recorded failure, so a
-- transfer cut short by a restart would leave no trace and the next one
-- would not count as a retry.
ALTER TABLE motion_clips ADD COLUMN acct_transfer_starts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE alert_clips ADD COLUMN acct_transfer_starts INTEGER NOT NULL DEFAULT 0;

-- One-time baseline: work that already happened is marked counted, so it
-- never lands in a bucket. Open clips and the undelivered backlog stay
-- unmarked and count when they finalize or deliver.
UPDATE motion_clips
   SET acct_generated = (ended_at IS NOT NULL),
       acct_delivered = (cold_handle IS NOT NULL)
 WHERE ended_at IS NOT NULL OR cold_handle IS NOT NULL;
UPDATE alert_clips SET acct_delivered = 1 WHERE cold_uploaded_at IS NOT NULL;
