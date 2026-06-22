-- 0022_fleet_managed_markers.sql
--
-- Phase 7.5 · Step 7.5.5 — per-category "fleet-managed" provenance marker.
--
-- Step 7.5.4 introduced the local fleet-apply endpoint with OVERLAY
-- semantics (additive, never deletes). Step 7.5.5 changes the contract:
-- a fleet apply now REPLACES the local state for the applied category
-- (fleet overwrites local config), and the edge records which categories
-- are under fleet management so the local admin UI can surface a
-- "Fleet-managed" badge. A category becomes managed the first time any
-- fleet apply touches it; the marker is upserted on every subsequent
-- apply.
--
-- One row per fleet-settings category. `category` is the cloud `db_key`
-- segment: one of `rules`, `text_prompts`, `visual_prompts`,
-- `detector_config`, `delivery_settings`. `scope_type` / `scope_id` echo
-- the apply scope ({org|site|core|camera}, uuid) purely for display +
-- audit — the edge applies at core granularity regardless (see
-- fleet_apply.rs module docs). `effective_sha256` is the lower-hex
-- SHA-256 of the canonical JSON of the effective payload last applied
-- (same canonicalisation as the `core_state_hashes` envelope), so the UI
-- can show a short fingerprint and the edge can short-circuit a no-op
-- re-apply.
--
-- ISO-8601 `applied_at` so chrono-bound lex comparisons stay correct
-- (cf. user-memory note on SQLite CURRENT_TIMESTAMP vs RFC3339).
CREATE TABLE IF NOT EXISTS fleet_managed_markers (
    category          TEXT PRIMARY KEY,
    scope_type        TEXT,
    scope_id          TEXT,
    effective_sha256  TEXT,
    applied_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
