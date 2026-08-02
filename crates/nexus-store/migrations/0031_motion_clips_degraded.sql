-- Record sample loss on a recording so an undecodable clip is
-- self-describing instead of having to be diagnosed from ffprobe
-- error counts weeks later.
--
-- `dropped_samples` counts NAL samples the recorder's live pump never
-- managed to push into the muxer (broadcast ring overruns). Because
-- tokio's broadcast evicts the OLDEST entry first, and the oldest
-- entry in a GOP is its IDR, any non-zero value here means keyframes
-- are missing and playback will break somewhere after the pre-roll.
--
-- `degraded` is the cheap boolean the UI and the retention sweep can
-- index on; it is set whenever `dropped_samples > 0`.
ALTER TABLE motion_clips ADD COLUMN dropped_samples INTEGER NOT NULL DEFAULT 0;
ALTER TABLE motion_clips ADD COLUMN degraded INTEGER NOT NULL DEFAULT 0;
