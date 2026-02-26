CREATE TABLE sensors
(
    sensor_id        TEXT      NOT NULL UNIQUE,
    category         TEXT,
    measurement_unit TEXT,
    depth_value      DOUBLE PRECISION,
    depth_unit       TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE measurements
(
    sensor_id   TEXT             NOT NULL REFERENCES sensors (sensor_id),
    value       DOUBLE PRECISION NOT NULL,
    measured_at TIMESTAMPTZ        NOT NULL,
    UNIQUE (sensor_id, measured_at)
);

CREATE TABLE scrapes
(
    scrape_id  SERIAL PRIMARY KEY,
    scraped_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);