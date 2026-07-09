-- nexus:no-transaction
--
-- Cloud audit sink: extend `alert_sink_outbox.suppression_reason` CHECK to
-- allow the new `entitlement_suspended` value.
--
-- Background:
--   The always-on cloud-console audit sink (`cloud:console`) BYPASSES the
--   operator delivery schedule / global-disable — it is the complete audit
--   trail — but STILL suppresses when the org's cloud entitlement is
--   suspended for non-payment (cloud ARCHITECTURE §12.4). The engine's
--   `CloudAwarePolicy` returns `SuppressionReason::EntitlementSuspended` for
--   a `cloud:*` outbox row when the cached entitlement JWT reports
--   `plan = "suspended"` / `max_cameras = 0`, so the dispatcher marks that
--   row `suppressed` with the new reason. External sinks are unaffected.
--   The original 0006 CHECK pinned `suppression_reason` to the four
--   schedule/global/rule values, so the new value must be added here.
--
-- SQLite cannot ALTER a CHECK constraint in place, so this follows the same
-- official rebuild recipe migrations 0004 / 0014 used:
--   * `foreign_keys=OFF` OUTSIDE any transaction (otherwise DROP TABLE
--     cascades through the `events` FK and nukes outbox history).
--   * Rebuild `alert_sink_outbox` inside a BEGIN..COMMIT block.
--   * `PRAGMA foreign_key_check` after the COMMIT to confirm no dangling
--     references slipped through.
--   * Restore `foreign_keys=ON`.
--
-- The no-transaction marker on the first line opts this migration out of the
-- runner's default "wrap every file in BEGIN/COMMIT" behaviour so the PRAGMAs
-- actually apply. See `0014_storage_backends_azure_blob.sql` for the full
-- rationale.

PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE alert_sink_outbox_new (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id           TEXT    NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
    sink_id            TEXT    NOT NULL,
    status             TEXT    NOT NULL DEFAULT 'pending',
    attempts           INTEGER NOT NULL DEFAULT 0,
    next_attempt_at    TEXT,
    last_error         TEXT,
    suppression_reason TEXT,
    created_at         TEXT    NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    delivered_at       TEXT,
    UNIQUE (event_id, sink_id),
    CHECK (status IN ('pending', 'sent', 'failed', 'dead', 'suppressed')),
    CHECK (
        (status = 'suppressed' AND suppression_reason IS NOT NULL)
        OR (status <> 'suppressed' AND suppression_reason IS NULL)
    ),
    CHECK (
        suppression_reason IS NULL
        OR suppression_reason IN (
            'global_disabled',
            'rule_disabled',
            'off_schedule_global',
            'off_schedule_rule',
            'entitlement_suspended'
        )
    )
);

INSERT INTO alert_sink_outbox_new
    (id, event_id, sink_id, status, attempts, next_attempt_at,
     last_error, suppression_reason, created_at, delivered_at)
SELECT
    id, event_id, sink_id, status, attempts, next_attempt_at,
    last_error, suppression_reason, created_at, delivered_at
  FROM alert_sink_outbox;

DROP TABLE alert_sink_outbox;
ALTER TABLE alert_sink_outbox_new RENAME TO alert_sink_outbox;

-- Recreate the two indexes from 0006 (dropped with the old table).
CREATE INDEX idx_alert_sink_outbox_pending
    ON alert_sink_outbox (next_attempt_at, id)
    WHERE status = 'pending';

CREATE INDEX idx_alert_sink_outbox_event
    ON alert_sink_outbox (event_id);

COMMIT;

PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;
