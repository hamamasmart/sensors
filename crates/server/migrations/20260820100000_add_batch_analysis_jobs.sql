-- Track offline batch analysis jobs (`POST /cameras/analyze`) so their progress
-- survives server restarts. The server runs in a 512 MB container; an in-memory
-- store would be lost on every redeploy, so job state lives here instead.
--
-- `status` is a native Postgres enum, mirroring `sensor_value_type`. The labels
-- match the serde representation of `AnalysisJobStatus` (snake_case), and the
-- Rust enum's `as_db_str` / `from_db_str` map between the two — so the DB and
-- JSON stay in sync.
--
-- `total_images` is the up-front count (a separate S3 listing pass) so progress
-- is meaningful from the start; `processed_images` climbs toward it as each
-- image is analyzed. `succeeded` / `failed` are prompt-level tallies; a single
-- image with any failed prompt also bumps `failed_images`.
CREATE TYPE batch_job_status AS ENUM ('pending', 'running', 'completed', 'failed');

CREATE TABLE batch_analysis_jobs
(
    job_id           UUID            PRIMARY KEY,
    status           batch_job_status NOT NULL,
    total_images     BIGINT          NOT NULL DEFAULT 0,
    processed_images BIGINT          NOT NULL DEFAULT 0,
    failed_images    BIGINT          NOT NULL DEFAULT 0,
    succeeded        BIGINT          NOT NULL DEFAULT 0,
    failed           BIGINT          NOT NULL DEFAULT 0,
    started_at       TIMESTAMPTZ     NOT NULL,
    finished_at      TIMESTAMPTZ,
    error            TEXT
);
