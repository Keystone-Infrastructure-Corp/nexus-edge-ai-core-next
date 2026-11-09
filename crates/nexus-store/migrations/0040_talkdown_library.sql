-- SPEC-086 / ADR-094 / ADR-095. Talk-down audio stored on the core.
-- `uid` is the cloud clip id for library items (a fleet rule names the same
-- uid on every core) and a fresh UUID for items created on the core.
-- `version_token` is the cloud WAV's sha256 for clips, or sha256(voice, text)
-- for TTS: it decides whether a push needs a refetch or a re-render.
-- No URL column: a SAS URL is a bearer credential and is never persisted.
CREATE TABLE IF NOT EXISTS talkdown_audio (
    uid           TEXT    PRIMARY KEY,
    name          TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 80),
    description   TEXT    CHECK (description IS NULL OR length(description) <= 500),
    kind          TEXT    NOT NULL CHECK (kind IN ('clip', 'tts')),
    tts_text      TEXT    CHECK (tts_text IS NULL OR length(tts_text) BETWEEN 1 AND 500),
    tts_voice     TEXT,
    status        TEXT    NOT NULL CHECK (status IN ('ready', 'pending', 'failed')),
    error         TEXT,
    duration_ms   INTEGER NOT NULL DEFAULT 0,
    version_token TEXT,
    pcm           BLOB,
    prev_pcm      BLOB,
    prev_duration_ms INTEGER,
    prev_tts_text TEXT,
    created_at    TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    updated_at    TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    CHECK (kind = 'clip' OR tts_text IS NOT NULL),
    CHECK (status <> 'ready' OR pcm IS NOT NULL)
);

-- One row per play attempt that reached a speaker or failed trying; skips are
-- counted in memory, not stored. `outcome` is set ~60 s after a rule play ends.
CREATE TABLE IF NOT EXISTS talkdown_plays (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT    NOT NULL,
    origin      TEXT    NOT NULL CHECK (origin IN ('manual', 'rule')),
    rule_id     TEXT,
    step        INTEGER,
    event_id    TEXT,
    camera_id   INTEGER,
    speaker_id  INTEGER NOT NULL,
    audio_uid   TEXT,
    mode        TEXT    NOT NULL,
    started_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    ended_at    TEXT,
    result      TEXT    CHECK (result IN ('played', 'stopped', 'preempted', 'failed', 'ended_by_speaker')),
    reason      TEXT,
    outcome     TEXT    CHECK (outcome IN ('cleared', 'persisted'))
);
CREATE INDEX IF NOT EXISTS idx_talkdown_plays_event ON talkdown_plays(event_id);
CREATE INDEX IF NOT EXISTS idx_talkdown_plays_started ON talkdown_plays(started_at);

CREATE TABLE IF NOT EXISTS talkdown_core (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    auto_armed       INTEGER NOT NULL DEFAULT 1,
    auto_rearm_at    TEXT,
    auto_disarm_reason TEXT,
    cloud_allowed    INTEGER NOT NULL DEFAULT 0, -- 1 only after a heartbeat ack says the org has the flag on and has accepted the acknowledgment
    library_revision INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO talkdown_core (id) VALUES (1);

ALTER TABLE speakers ADD COLUMN auto_armed INTEGER NOT NULL DEFAULT 1 CHECK (auto_armed IN (0, 1));
ALTER TABLE speakers ADD COLUMN auto_rearm_at TEXT;
ALTER TABLE speakers ADD COLUMN auto_disarm_reason TEXT;
