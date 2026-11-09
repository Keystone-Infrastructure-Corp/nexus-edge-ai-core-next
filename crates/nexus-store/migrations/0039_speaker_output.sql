-- SPEC-086 / ADR-094: a speaker's output. Columns, not config_json, so an
-- output change is not a speaker edit (an edit hangs up its session).
ALTER TABLE speakers ADD COLUMN volume INTEGER NOT NULL DEFAULT 100 CHECK (volume BETWEEN 0 AND 100);
ALTER TABLE speakers ADD COLUMN boost INTEGER NOT NULL DEFAULT 0 CHECK (boost IN (0, 1));
ALTER TABLE speakers ADD COLUMN hw_volume INTEGER NOT NULL DEFAULT 0 CHECK (hw_volume IN (0, 1));
ALTER TABLE speakers ADD COLUMN auto_max_per_hour INTEGER NOT NULL DEFAULT 6 CHECK (auto_max_per_hour BETWEEN 1 AND 60);
