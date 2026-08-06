-- Camera-image analysis jobs. Created by `POST /cameras/analyze`, processed
-- asynchronously by the `analyzer` Lambda (one shard per camera, async-invoked
-- by the server). Results land in the existing `sensors`/`measurements` tables
-- as one measurement per analyzed image; this table only tracks job lifecycle.
CREATE TABLE analysis_jobs
(
    job_id           UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    camera_ids       TEXT[]       NOT NULL,
    prompt           TEXT         NOT NULL,
    provider         TEXT         NOT NULL DEFAULT 'vision',
    category         TEXT         NOT NULL,
    measurement_unit TEXT,
    depth_value      DOUBLE PRECISION,
    depth_unit       TEXT,
    starts_at        TIMESTAMPTZ  NOT NULL,
    ends_at          TIMESTAMPTZ  NOT NULL,
    status           TEXT         NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'running', 'done', 'failed')),
    error            TEXT,
    -- Sharding: one shard per camera. The job is terminal once every shard has
    -- reported in (done or failed). `completed_shards` dedups `/complete`
    -- against Lambda async-invocation retries.
    shards_total     INTEGER      NOT NULL DEFAULT 0,
    shards_done      INTEGER      NOT NULL DEFAULT 0,
    shards_failed    INTEGER      NOT NULL DEFAULT 0,
    images_total     INTEGER      NOT NULL DEFAULT 0,
    images_ok        INTEGER      NOT NULL DEFAULT 0,
    completed_shards TEXT[]       NOT NULL DEFAULT '{}',
    created_at       TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    started_at       TIMESTAMPTZ,
    completed_at     TIMESTAMPTZ
);

CREATE INDEX analysis_jobs_status_created_idx ON analysis_jobs (status, created_at);