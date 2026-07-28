-- 0030_native_aspect_shape_remap.sql
--
-- M_NATIVE_ASPECT Phase 5 — remap existing cameras' detector
-- `model_override` from the legacy square shapes to the native 16:9
-- ladder (exact 16:9 ∩ stride-32, W=512k / H=288k). Cameras store their
-- full `CameraConfig` as `config_json`; the override lives at
-- `$.detector.model_override.{input_width,input_height,preset}`.
--
-- Mapping (matches nexus-config `remap_to_ladder`):
--   640×640   → 512×288   (Standard)
--   960×960   → 1024×576  (Long range)
--   1280×1280 → 1536×864  (High detail)
--
-- Only rows whose override exists AND carries one of those exact square
-- shapes are touched; every other row (no override, or already on the
-- ladder) is left byte-for-byte unchanged. The engine's config-load
-- remap (`Config::normalize_shapes`) covers file-defined cameras and any
-- non-square legacy value; this migration covers store-persisted
-- cloud-managed overrides so they never resolve a missing square file.
--
-- Ensemble members (nested `$.detector.model_override.members[]`) are NOT
-- rewritten here — an operator re-saving the ensemble from the console
-- normalizes them, and file-defined ensembles are covered by the
-- recursive load-time remap. The top-level shape of an ensemble override
-- is ignored at runtime, so touching it (below) is harmless.
--
-- Forward-only + idempotent via `schema_migrations` (never edited after
-- apply); re-running is a no-op because the WHERE clauses no longer match
-- once a row is rewritten.

UPDATE cameras
SET config_json = json_set(
    config_json,
    '$.detector.model_override.input_width', 512,
    '$.detector.model_override.input_height', 288,
    '$.detector.model_override.preset', '512x288'
)
WHERE json_extract(config_json, '$.detector.model_override.input_width') = 640
  AND json_extract(config_json, '$.detector.model_override.input_height') = 640;

UPDATE cameras
SET config_json = json_set(
    config_json,
    '$.detector.model_override.input_width', 1024,
    '$.detector.model_override.input_height', 576,
    '$.detector.model_override.preset', '1024x576'
)
WHERE json_extract(config_json, '$.detector.model_override.input_width') = 960
  AND json_extract(config_json, '$.detector.model_override.input_height') = 960;

UPDATE cameras
SET config_json = json_set(
    config_json,
    '$.detector.model_override.input_width', 1536,
    '$.detector.model_override.input_height', 864,
    '$.detector.model_override.preset', '1536x864'
)
WHERE json_extract(config_json, '$.detector.model_override.input_width') = 1280
  AND json_extract(config_json, '$.detector.model_override.input_height') = 1280;
