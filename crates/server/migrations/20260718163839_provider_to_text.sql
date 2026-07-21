-- Store `provider` as a plain TEXT string instead of the provider_type enum, so
-- new providers can be recorded without a schema change + enum migration.
ALTER TABLE sensors ALTER COLUMN provider DROP DEFAULT;
ALTER TABLE sensors ALTER COLUMN provider TYPE TEXT USING provider::text;

-- The enum type is no longer referenced; drop it.
DROP TYPE provider_type;
