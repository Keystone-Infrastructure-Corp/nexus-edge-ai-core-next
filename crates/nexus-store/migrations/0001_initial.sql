-- Initial schema for the Nexus engine.
-- Hand-applied at startup via `Store::open`.

CREATE TABLE IF NOT EXISTS engine_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cameras (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    url         TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    config_json TEXT    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    updated_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);

CREATE TABLE IF NOT EXISTS rules (
    id          TEXT    PRIMARY KEY,
    name        TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    config_json TEXT    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    updated_at  TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);

CREATE TABLE IF NOT EXISTS events (
    event_id    TEXT    PRIMARY KEY,
    camera_id   INTEGER NOT NULL,
    rule_id     TEXT    NOT NULL,
    track_id    INTEGER,
    label       TEXT    NOT NULL,
    severity    TEXT    NOT NULL,
    frame_id    INTEGER NOT NULL,
    captured_at TEXT    NOT NULL,
    trace_id    TEXT    NOT NULL,
    payload_json TEXT   NOT NULL,
    FOREIGN KEY (camera_id) REFERENCES cameras(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_events_camera_ts   ON events(camera_id, captured_at);
CREATE INDEX IF NOT EXISTS idx_events_rule_ts     ON events(rule_id, captured_at);
CREATE INDEX IF NOT EXISTS idx_events_severity_ts ON events(severity, captured_at);
CREATE INDEX IF NOT EXISTS idx_events_trace       ON events(trace_id);

CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor      TEXT    NOT NULL,
    action     TEXT    NOT NULL,
    resource   TEXT    NOT NULL,
    diff_json  TEXT    NOT NULL,
    created_at TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);

CREATE INDEX IF NOT EXISTS idx_audit_resource_ts ON audit_log(resource, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_actor_ts    ON audit_log(actor, created_at);
