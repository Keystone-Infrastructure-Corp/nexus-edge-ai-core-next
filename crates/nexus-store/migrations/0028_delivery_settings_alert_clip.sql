-- 0028_delivery_settings_alert_clip.sql
--
-- M-Alert-Clip: operator-facing on/off for the short, burned-in
-- truncated "alert clip", living in the delivery settings (the
-- delivery-for-alert-sinks control). Default 1 (ON): the edge builds
-- the clip covering only the alert timeframe, attaches it to
-- clip-wanting alert sinks, and cold-replicates it so the cloud
-- console shows the same evidence — unless an operator disables it via
-- the cloud console (per-core Delivery tab or fleet delivery settings)
-- or the local admin API. AND-gated by the pipeline capability switch
-- `clips.alert_clips.enabled`. Applied live on `delivery.settings.changed`.
-- See docs/edge-core/M_ALERT_CLIP.md.

ALTER TABLE delivery_settings ADD COLUMN attach_alert_clip INTEGER NOT NULL DEFAULT 1;
