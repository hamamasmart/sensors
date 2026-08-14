-- Support non-numeric measurement values (e.g. RGB color strings from image
-- analysis) alongside the existing numeric readings.
--
-- `value` becomes nullable and a new `value_text` column carries string
-- values. Exactly one of the two must be set per row so a measurement always
-- carries a value. The numeric-only scraper / Topco paths keep writing `value`
-- unchanged; the image-analysis path writes `value_text` for `text` prompts.

-- 1. The numeric value is no longer required (text values have no number).
ALTER TABLE measurements ALTER COLUMN value DROP NOT NULL;

-- 2. New text value column.
ALTER TABLE measurements ADD COLUMN value_text TEXT;

-- 3. A measurement must carry exactly one value.
ALTER TABLE measurements ADD CONSTRAINT measurements_value_present
    CHECK (value IS NOT NULL OR value_text IS NOT NULL);

-- Pin each sensor to a single measurement value type (numeric or text).
--
-- `value_type` records whether the sensor's measurements write `value` (a
-- number) or `value_text` (a string). Existing sensors are backfilled to
-- `numeric` since every pre-existing reading is numeric; the upsert rejects a
-- type change once a sensor exists, so this column never changes for a given
-- (external_id, provider) sensor.

ALTER TABLE sensors ADD COLUMN value_type TEXT NOT NULL DEFAULT 'numeric';
