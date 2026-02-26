-- 1. Create enum type for provider
CREATE TYPE provider_type AS ENUM ('phytech', 'tera');

-- 2. Add provider column to sensors with default 'phytech'
ALTER TABLE sensors ADD COLUMN provider provider_type NOT NULL DEFAULT 'phytech';

-- 3. Rename current sensor_id (TEXT) to external_id
ALTER TABLE sensors RENAME COLUMN sensor_id TO external_id;

-- 4. Add new sensor_id (UUID) to sensors with default gen_random_uuid()
ALTER TABLE sensors ADD COLUMN sensor_id UUID DEFAULT gen_random_uuid();

-- 5. Backfill existing sensors (to make sure they have a UUID)
UPDATE sensors SET sensor_id = gen_random_uuid();

-- 6. Update measurements table to use the new UUID sensor_id
-- Drop the existing foreign key first
ALTER TABLE measurements DROP CONSTRAINT IF EXISTS measurements_sensor_id_fkey;
-- First rename the current sensor_id (TEXT) to external_id for backfilling
ALTER TABLE measurements RENAME COLUMN sensor_id TO external_id;
-- Add the new UUID sensor_id column
ALTER TABLE measurements ADD COLUMN sensor_id UUID;

-- 7. Backfill measurements' sensor_id (UUID) by joining with sensors
UPDATE measurements m
SET sensor_id = s.sensor_id
FROM sensors s
WHERE m.external_id = s.external_id;

-- 8. Finalize sensors table constraints
-- Remove old unique constraint on external_id (formerly sensor_id)
ALTER TABLE sensors DROP CONSTRAINT IF EXISTS sensors_sensor_id_key;
-- Set new primary key to sensor_id (UUID)
ALTER TABLE sensors ALTER COLUMN sensor_id SET NOT NULL;
ALTER TABLE sensors ADD PRIMARY KEY (sensor_id);
-- Add a unique constraint on (external_id, provider)
ALTER TABLE sensors ADD CONSTRAINT sensors_external_id_provider_key UNIQUE (external_id, provider);

-- 9. Finalize measurements table constraints
-- Make sensor_id NOT NULL and add foreign key reference
ALTER TABLE measurements ALTER COLUMN sensor_id SET NOT NULL;
ALTER TABLE measurements ADD FOREIGN KEY (sensor_id) REFERENCES sensors (sensor_id);
-- Update the unique constraint on measurements to use the new UUID sensor_id
-- The original constraint on (sensor_id, measured_at) was renamed to (external_id, measured_at)
ALTER TABLE measurements DROP CONSTRAINT IF EXISTS measurements_sensor_id_measured_at_key;
ALTER TABLE measurements ADD UNIQUE (sensor_id, measured_at);
-- Remove the temporary external_id column from measurements
ALTER TABLE measurements DROP COLUMN external_id;
