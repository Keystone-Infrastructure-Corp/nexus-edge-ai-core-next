-- M-Alert-Clip P2: durable mapping for the short, burned-in "alert clip"
-- that covers only the alert timeframe. See docs/edge-core/M_ALERT_CLIP.md.
--
-- Distinct from motion_clips (the up-to-5-minute archival passthrough
-- recording):
--   * One alert_clips row per MOTION BURST; many alert events can share
--     it (burst coalescing), linked via events.alert_clip_id.
--   * Built by re-encoding a short window from the SAME per-camera
--     ingester the motion recorder uses, with the bbox burned in.
--     Native-res, seconds long, delivered to sinks within ~post_secs.
--   * TRANSIENT: the hot file is reclaimed once every linked event has
--     been delivered to all sinks. Eviction MUST NOT delete the alert
--     event, so events.alert_clip_id is ON DELETE SET NULL (the
--     opposite of events.clip_id -> motion_clips, which is CASCADE
--     because a motion clip IS the alert's only record of the moment).
--   * `path` is relative to clips_dir (same root as motion_clips.path),
--     under a fixed `alert/` subdir, so the sink dispatcher reuses the
--     existing clips_dir resolution without a second root.

CREATE TABLE IF NOT EXISTS alert_clips (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id    INTEGER NOT NULL,
    path         TEXT    NOT NULL,                     -- relative to clips_dir
    started_at   TEXT    NOT NULL,                     -- window start (alert_ts - pre_secs)
    ready_at     TEXT,                                 -- NULL until the mp4 is finalized
    state        TEXT    NOT NULL DEFAULT 'building',  -- 'building' | 'ready' | 'failed' | 'evicted'
    duration_ms  INTEGER NOT NULL DEFAULT 0,
    size_bytes   INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    FOREIGN KEY (camera_id) REFERENCES cameras(id) ON DELETE CASCADE
);

-- Per-camera newest-first lookup (the builder reuses the most recent
-- in-flight burst clip when coalescing).
CREATE INDEX IF NOT EXISTS idx_alert_clips_camera_started
    ON alert_clips(camera_id, started_at);
-- The dispatcher resolves readiness by state; the evictor scans by state.
CREATE INDEX IF NOT EXISTS idx_alert_clips_state
    ON alert_clips(state);

-- Link alerts to the burst clip that captured them. NULLABLE: an alert
-- can fire with no alert clip (feature disabled, or the builder failed /
-- timed out). ON DELETE SET NULL so reclaiming the transient clip row
-- leaves the durable alert row intact.
ALTER TABLE events ADD COLUMN alert_clip_id INTEGER
    REFERENCES alert_clips(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_events_alert_clip ON events(alert_clip_id);
