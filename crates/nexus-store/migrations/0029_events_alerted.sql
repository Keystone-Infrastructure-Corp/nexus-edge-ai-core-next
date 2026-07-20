-- 0029_events_alerted.sql
--
-- M-Event-Audit — mark whether a recorded event was promoted to an
-- alert vs. logged as audit-only.
--
--   * `alerted = 1` (default) — the event is an alert: it fell within
--     the active M7 delivery cascade (global + per-rule enabled AND
--     within schedule), so it armed an alert clip, entered the cloud
--     alert queue, and fired notifications.
--   * `alerted = 0` — audit-only: still fully logged + snapshot +
--     motion-clip link (`events.clip_id`), but no alert clip, no cloud
--     alert-queue row, no notification. Relies on the motion clip.
--
-- The supervisor sets this from `CascadingPolicy::would_deliver` at
-- record time (`record_event_and_enqueue`). Logging is unconditional —
-- `alerted` never gates the write, it only classifies the row.
--
-- Default 1 so rows written before this migration read back as alerts:
-- they predate the events/alerts split, when every recorded event was
-- schedule-gated only at delivery time. Forward-only + idempotent via
-- the `schema_migrations` ledger (never edited after apply).
ALTER TABLE events ADD COLUMN alerted INTEGER NOT NULL DEFAULT 1;

-- Powers the operator-facing "Alerts" view (alerted = 1) and the full
-- events audit, both ordered newest-first.
CREATE INDEX IF NOT EXISTS idx_events_alerted_ts ON events(alerted, captured_at);
