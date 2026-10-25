-- ADR-090: IP speakers are core devices, and a camera's talk-down speaker
-- is an assignment row, not a field on the camera.
--
--   * `speakers.config_json` — the serialised `nexus_config::SpeakerConfig`,
--                              credentials included. Edge-resident only.
--   * `speaker_cameras`      — keyed by `camera_id`, so a camera has at most
--                              one speaker while one speaker serves any
--                              number of cameras. Both foreign keys cascade:
--                              deleting either side clears the link.

CREATE TABLE IF NOT EXISTS speakers (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    config_json TEXT    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    updated_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);

CREATE TABLE IF NOT EXISTS speaker_cameras (
    camera_id  INTEGER PRIMARY KEY REFERENCES cameras(id) ON DELETE CASCADE,
    speaker_id INTEGER NOT NULL REFERENCES speakers(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_speaker_cameras_speaker ON speaker_cameras(speaker_id);
