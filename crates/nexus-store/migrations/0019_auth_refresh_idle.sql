-- v0.1.36 (M-HTTPS Phase 2): server-authoritative 20-min idle
-- timeout on refresh-token chains.
--
-- The session model already has a 30-day rolling TTL on refresh
-- tokens (created_at + 30 d), but no sliding inactivity window —
-- a stolen refresh token could be replayed at any time within
-- that window. This migration adds:
--
--   * `last_active_at` — bumped on every authenticated request
--     by a 250 ms-batched background task (see
--     `crates/nexus-engine/src/auth/idle_bump.rs`). Lets the
--     refresh handler reject a token whose chain has been quiet
--     longer than `IDLE_TIMEOUT` (20 min).
--
--   * `idle_revoked_at` — distinct timestamp for chains killed
--     by idle expiry (vs `revoked_at` for explicit logout /
--     replay detection / admin revoke). Lets the audit log
--     surface "expired" sessions separately from "compromised"
--     ones without a follow-up join.
--
-- Backfill: existing rows get `last_active_at = created_at` so
-- the idle check works on first request after the migration.
-- Column is added NULLable because SQLite ALTER TABLE ADD COLUMN
-- forbids non-constant DEFAULTs (`strftime('now')` doesn't
-- qualify). The Rust write path always sets it explicitly; the
-- read path uses `coalesce(last_active_at, created_at)` for
-- defense in depth.
--
-- Index: `(chain_id, last_active_at)` is the hot path for the
-- refresh handler's "is this chain still alive?" check. We
-- already index on `chain_id` alone (for replay revokes); the
-- composite lets the planner skip the lookup-then-filter for
-- idle checks too.

ALTER TABLE auth_refresh_tokens ADD COLUMN last_active_at TEXT;
ALTER TABLE auth_refresh_tokens ADD COLUMN idle_revoked_at TEXT;

UPDATE auth_refresh_tokens
   SET last_active_at = created_at
 WHERE last_active_at IS NULL;

CREATE INDEX idx_auth_refresh_tokens_chain_idle
    ON auth_refresh_tokens (chain_id, last_active_at);
