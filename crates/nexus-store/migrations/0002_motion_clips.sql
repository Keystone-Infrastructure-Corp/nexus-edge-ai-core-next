-- M2.1 Stage A: motion timeline + clip recording (safety floor).
-- See docs/M2_STORAGE.md and plans memory note for the full design.
--
-- Hard invariants enforced here:
--   * `motion_events.clip_id` is NOT NULL — every motion-event row points
--     at a real clip. During panic-watermark we drop the motion event
--     entirely rather than emit a clip-less row (avoids the half-deleted
--     state the cascade-delete rule exists to prevent).
--   * Every FK to motion_clips is `ON DELETE CASCADE` so eviction is one
--     `DELETE motion_clips WHERE id = ?`.
--   * `events.clip_id` is NULLABLE — alerts pre-date the recorder and
--     not every alert has an associated clip (e.g. when the recorder is
--     refusing new clips during panic-watermark, an AlertEvent can still
--     fire for the underlying detection).
--   * `motion_clips.backend_id` lands here in Stage A even though only
--     'local' is wired today; M2.2 will add SMB/NFS/USB/S3 entries to
--     `storage_backends` (created in that milestone) and reference them
--     from this column without a second migration.

CREATE TABLE IF NOT EXISTS motion_clips (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id     INTEGER NOT NULL,
    started_at    TEXT    NOT NULL,
    ended_at      TEXT,                                -- NULL while still recording
    path          TEXT    NOT NULL,                    -- relative to clips_dir
    duration_ms   INTEGER NOT NULL DEFAULT 0,
    size_bytes    INTEGER NOT NULL DEFAULT 0,
    codec         TEXT    NOT NULL DEFAULT 'h264',
    container     TEXT    NOT NULL DEFAULT 'mp4',
    backend_id    TEXT    NOT NULL DEFAULT 'local',    -- M2.2 forward-compat slot
    created_at    TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    FOREIGN KEY (camera_id) REFERENCES cameras(id) ON DELETE CASCADE
);

-- Eviction is oldest-first per camera; this index is what makes
-- `oldest_clip_for_camera` an O(log n) lookup.
CREATE INDEX IF NOT EXISTS idx_motion_clips_camera_started
    ON motion_clips(camera_id, started_at);

-- The retention sweeper walks by ended_at to pick up expired clips.
CREATE INDEX IF NOT EXISTS idx_motion_clips_ended
    ON motion_clips(ended_at);

CREATE TABLE IF NOT EXISTS motion_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id     INTEGER NOT NULL,
    clip_id       INTEGER NOT NULL,                    -- see hard invariant
    track_id      INTEGER NOT NULL,                    -- TrackId from nexus-types
    kind          TEXT    NOT NULL,                    -- 'born' | 'updated' | 'died'
    captured_at   TEXT    NOT NULL,
    bbox_x1       REAL    NOT NULL,
    bbox_y1       REAL    NOT NULL,
    bbox_x2       REAL    NOT NULL,
    bbox_y2       REAL    NOT NULL,
    label         TEXT    NOT NULL,
    confidence    REAL    NOT NULL,
    attributes_json TEXT  NOT NULL DEFAULT '{}',
    FOREIGN KEY (camera_id) REFERENCES cameras(id) ON DELETE CASCADE,
    FOREIGN KEY (clip_id)   REFERENCES motion_clips(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_motion_events_camera_ts
    ON motion_events(camera_id, captured_at);
CREATE INDEX IF NOT EXISTS idx_motion_events_track
    ON motion_events(track_id);
CREATE INDEX IF NOT EXISTS idx_motion_events_clip
    ON motion_events(clip_id);

-- Cross-link alerts to the clip that captured them, so the alert
-- detail UI can deep-link into playback. NULLABLE: see invariant note above.
-- Existing rows get NULL via the default; new alerts populate this when
-- the recorder has an open clip handle for the camera at firing time.
ALTER TABLE events ADD COLUMN clip_id INTEGER
    REFERENCES motion_clips(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_events_clip ON events(clip_id);
