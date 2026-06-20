-- M7 cloud-managed alert sinks.
--
-- Until now the alert-delivery sink set was defined exclusively in
-- `nexus.toml` (`[[sinks]]`) and frozen at engine boot. This table
-- lets the cloud console (and the local admin UI) add / edit / remove
-- sinks at runtime without a config edit + restart, mirroring the
-- camera + delivery-settings precedent.
--
-- ## Effective set
--
-- The engine builds the live `SinkRegistry` from the UNION of:
--
--   1. file sinks  (`nexus.toml` `[[sinks]]`)
--   2. db sinks    (rows in this table)
--
-- keyed by `sink_id` (`"<kind>:<name>"`). On a collision the db row
-- WINS, so a sink the operator pins in `nexus.toml` can still be
-- re-pointed from the console. Deleting a db row falls back to the
-- file definition (if any); deleting a sink that exists only in the
-- db removes it from the live set entirely.
--
-- ## Secrets
--
-- `config_json` is the full `nexus_config::SinkConfig` blob, INCLUDING
-- secrets (SureView `api_key`, webhook `hmac_secret`). This mirrors how
-- camera RTSP credentials live in plaintext in the edge-resident DB:
-- the edge is the trust boundary. The cloud NEVER persists the secret
-- — it forwards the operator's input verbatim to the edge and the edge
-- stores it here. The admin GET surface redacts the secret before it
-- leaves the box.

CREATE TABLE alert_sinks (
    -- "<kind>:<name>", e.g. "sureview:front-gate". Matches the
    -- `SinkId` the dispatcher resolves against and the format the
    -- `alert_sink_outbox` stores.
    sink_id     TEXT PRIMARY KEY,
    -- Denormalised from `config_json` for cheap listing / filtering.
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    -- Serialised `nexus_config::SinkConfig` (tagged enum, carries the
    -- `kind` discriminator + every variant field, secrets included).
    config_json TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);
