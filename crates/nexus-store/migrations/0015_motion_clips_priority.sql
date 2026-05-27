-- 0015_motion_clips_priority.sql
--
-- Phase 2 Step 2.1c — cold-replicator priority lane.
--
-- The cold replicator (`crates/nexus-engine/src/cold_replicator.rs`) drains
-- pending clips in `ended_at ASC` order. Phase 2 introduces the cloud-side
-- Expedite flow (`POST /v1/orgs/{org}/cameras/{cam}/clips/{edge_clip_id}/replicate`)
-- which proxies an `rpc_call` down the tunnel to the engine. The engine's
-- handler bumps the matching clip's `priority` and notifies the replicator
-- kick so the expedited clip jumps to the head of the next tick.
--
-- Ordering contract going forward: `priority DESC, ended_at ASC`. Default
-- priority `0` keeps the existing FIFO behaviour for un-expedited clips;
-- the handler sets `priority = 1` (binary lane — expedited vs. not). Both
-- ranks within the same priority drain oldest-first to preserve stream
-- order on backfills.
--
-- Index strategy: the existing partial `idx_motion_clips_pending_cold`
-- still serves the WHERE clause. The added ORDER BY column means SQLite
-- does a small in-memory sort over the (already-filtered) pending subset;
-- at the replicator's `LIMIT 32` per tick that is trivially fast and
-- doesn't justify a covering index.

ALTER TABLE motion_clips
    ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
