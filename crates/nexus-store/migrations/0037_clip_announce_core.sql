-- PR #366 review: bind a pending `clip_replicated` to the enrollment
-- that uploaded the clip.
--
-- Disconnecting keeps local recordings, and a later enrollment can name
-- a different core (or overwrite the enrollment row in place). An
-- un-acked announce from before that change would otherwise be re-sent
-- down the new core's tunnel carrying the old core's blob URL. The
-- re-announce pass only selects rows whose `cloud_announce_core_id`
-- matches the current enrollment's `core_id`; an announce stamped by a
-- prior enrollment is never re-sent.
--
--   * `cloud_announce_core_id` — `cloud_enrollment.core_id` at the time
--                                the blob URL was stamped. NULL (no
--                                enrollment, or stamped before this
--                                migration) matches no enrollment.

ALTER TABLE motion_clips ADD COLUMN cloud_announce_core_id TEXT;
ALTER TABLE alert_clips ADD COLUMN cloud_announce_core_id TEXT;
