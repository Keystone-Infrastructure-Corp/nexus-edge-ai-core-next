-- #759: re-announce `clip_replicated` until the cloud acks it.
--
-- The cloud creates a clip's `clips` row only when it receives
-- `clip_replicated`, and the tunnel outbox does not persist envelopes,
-- so a send that is dropped (tunnel down, transient cloud failure) used
-- to lose the cloud row for good. These columns make the announce
-- durable: the cold replicator's polling backstop re-sends
-- `clip_replicated` for every cold-uploaded clip whose
-- `cloud_announced_at` is still NULL, and the tunnel's
-- `clip_replicated_ack` handler stamps it.
--
--   * `cloud_blob_url`       — unsigned blob URL from the upload receipt.
--                              NULL = nothing to announce (LAN/USB
--                              backends, or a clip uploaded before this
--                              migration).
--   * `cloud_announce_id`    — `meta.id` of the most recent
--                              `clip_replicated` sent; the ack's
--                              `in_reply_to` is matched against it.
--   * `cloud_announced_at`   — RFC3339 of the ack. Set on every ack, so a
--                              permanently rejected clip is not retried.
--   * `cloud_announce_error` — the cloud's `permanent_failure` reason;
--                              NULL when the cloud stored the clip.

ALTER TABLE motion_clips ADD COLUMN cloud_blob_url TEXT;
ALTER TABLE motion_clips ADD COLUMN cloud_announce_id TEXT;
ALTER TABLE motion_clips ADD COLUMN cloud_announced_at TEXT;
ALTER TABLE motion_clips ADD COLUMN cloud_announce_error TEXT;

ALTER TABLE alert_clips ADD COLUMN cloud_blob_url TEXT;
ALTER TABLE alert_clips ADD COLUMN cloud_announce_id TEXT;
ALTER TABLE alert_clips ADD COLUMN cloud_announced_at TEXT;
ALTER TABLE alert_clips ADD COLUMN cloud_announce_error TEXT;

-- Drive the re-announce pass: only cold-uploaded, un-acked rows with a
-- URL, so the working set stays tiny.
CREATE INDEX IF NOT EXISTS idx_motion_clips_pending_announce
    ON motion_clips(cold_uploaded_at)
    WHERE cloud_blob_url IS NOT NULL AND cloud_announced_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_alert_clips_pending_announce
    ON alert_clips(cold_uploaded_at)
    WHERE cloud_blob_url IS NOT NULL AND cloud_announced_at IS NULL;
