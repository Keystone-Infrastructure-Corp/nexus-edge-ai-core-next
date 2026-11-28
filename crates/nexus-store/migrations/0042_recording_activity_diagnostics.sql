-- SPEC-089 Task 2. Bounded summaries of the fresh-evidence recording policy
-- running in shadow beside legacy recording. Written only for cameras in
-- shadow mode: one row per camera-minute of counters and one per finalized
-- legacy clip. Never a per-frame row, an image or a track id. Rows older
-- than seven days are pruned on write (`nexus-store::recording_activity`).
-- No credentials: counters, clip ids and an opaque policy revision.

-- One camera-minute of one supervisor generation under one saved policy
-- revision: a live policy change mid-minute leaves a row per revision, and
-- a repeated key is refused, never replaced. `minute` is the index of the
-- generation's monotonic minute, so a wall-clock step never splits or
-- merges a bucket; `started_at_ms` is the wall clock of its first frame,
-- for reading and retention. `counters` is the JSON of the pipeline's
-- `MinuteCounters`.
CREATE TABLE IF NOT EXISTS recording_activity_minutes (
    camera_id       INTEGER NOT NULL,
    generation      INTEGER NOT NULL,
    policy_revision TEXT    NOT NULL,
    minute          INTEGER NOT NULL,
    started_at_ms   INTEGER NOT NULL,
    counters        TEXT    NOT NULL,
    PRIMARY KEY (camera_id, generation, policy_revision, minute)
);

CREATE INDEX IF NOT EXISTS idx_recording_activity_minutes_started
    ON recording_activity_minutes (started_at_ms);

-- One clip as the policy saw it: its reason when the clip opened and
-- closed (`minimum_duration` when observed mode held a short clip for the
-- recorder's minimum), and the clip's time under each reason (`elapsed`,
-- JSON of the pipeline's `ReasonMs`). No foreign key: clip retention
-- deletes clips independently of these summaries.
CREATE TABLE IF NOT EXISTS recording_activity_episodes (
    id              INTEGER PRIMARY KEY,
    camera_id       INTEGER NOT NULL,
    generation      INTEGER NOT NULL,
    clip_id         INTEGER NOT NULL,
    policy_revision TEXT    NOT NULL,
    opened_at_ms    INTEGER NOT NULL,
    closed_at_ms    INTEGER NOT NULL,
    open_reason     TEXT    NOT NULL
        CHECK (open_reason IN ('activity', 'post_roll', 'degraded_coverage', 'quiet',
                               'minimum_duration')),
    close_reason    TEXT    NOT NULL
        CHECK (close_reason IN ('activity', 'post_roll', 'degraded_coverage', 'quiet',
                                'minimum_duration')),
    elapsed         TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_recording_activity_episodes_closed
    ON recording_activity_episodes (closed_at_ms);
