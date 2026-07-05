-- 0024_metrics_samples.sql
--
-- Rolling 24-hour buffer of host `SystemMetrics` snapshots so the cloud
-- console can render a "last 24 hours" trend view for a core while it is
-- online. The engine samples `system_metrics::render()` every 5 seconds
-- and inserts the full JSON blob here; a periodic sweeper enforces a
-- two-tier retention policy (see `nexus-store::metrics`):
--
--   * everything younger than 60 minutes is kept at full 5-second
--     resolution (powers the console's "full resolution" toggle);
--   * rows older than 60 minutes are coarsened to one sample per
--     5-minute boundary;
--   * everything older than 24 hours is dropped outright.
--
-- Because captured_at_ms is floored to the 5-second sample grid at
-- insert time, the coarsening prune can select 5-minute boundaries with
-- a pure modulo (`captured_at_ms % 300000 = 0`) and the PRIMARY KEY
-- dedups jittered ticks that land in the same slot.
--
-- ## No PII, no secrets
--
-- `payload` is the verbatim `SystemMetrics` JSON: host name / OS, CPU /
-- memory / GPU / NPU / disk counters, and the engine process RSS. It
-- carries no camera credentials, no identity data, and no operator
-- secrets, so it is safe to surface to any authenticated viewer via the
-- admin metrics-history endpoint. (It is nonetheless excluded from the
-- scrubbed diagnostics snapshot by the fail-closed `KEEP_TABLES`
-- allowlist in `diag_snapshot.rs`, keeping support tarballs small.)
CREATE TABLE IF NOT EXISTS metrics_samples (
    -- Wall-clock capture instant, Unix epoch MILLISECONDS, floored to
    -- the 5-second sample grid. The true (un-floored) capture time is
    -- preserved inside `payload.captured_at`; the console plots by that
    -- field, while this key drives storage, dedup, and pruning.
    captured_at_ms  INTEGER PRIMARY KEY,
    -- Compact JSON of the full `SystemMetrics` struct produced by
    -- `nexus-engine`'s `system_metrics::render()`. Stored verbatim so
    -- the cloud viewer sees every field the live endpoint has and so a
    -- schema evolution never breaks reads of older rows.
    payload         TEXT NOT NULL
);
