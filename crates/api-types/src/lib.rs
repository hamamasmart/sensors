//! request/response contract for the `server`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `POST /sensors` — upsert a sensor and learn its internal id + resume point.
#[derive(Debug, Deserialize, Serialize)]
pub struct UpsertSensorRequest {
    pub external_id: String,
    pub provider: String,
    pub category: String,
    pub measurement_unit: Option<String>,
    pub depth_value: Option<f64>,
    pub depth_unit: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UpsertSensorResponse {
    pub sensor_id: Uuid,
    /// Latest measurement already stored for this sensor, so the scraper can filter to
    /// only-new measurements before fetching/inserting.
    pub last_measured_at: Option<DateTime<Utc>>,
}

/// A single, already-scaled measurement ready for storage. The scraper applies any
/// provider-specific scaling before sending; the server stores the value verbatim.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Measurement {
    pub value: f64,
    pub measured_at: DateTime<Utc>,
}

/// `POST /sensors/:sensor_id/measurements` body.
#[derive(Debug, Deserialize, Serialize)]
pub struct InsertMeasurementsRequest {
    pub measurements: Vec<Measurement>,
}

/// `POST /sensors/:sensor_id/measurements` response — rows actually inserted after
/// `ON CONFLICT DO NOTHING` dedup.
#[derive(Debug, Deserialize, Serialize)]
pub struct InsertMeasurementsResponse {
    pub inserted: u64,
}


/// `POST /cameras/images` request query parms.
#[derive(Deserialize)]
pub struct UploadCameraImageQuery {
    pub camera_id: String,
    /// When the image was taken, as a Unix epoch timestamp in seconds.
    pub captured_at: i64,
}

/// `POST /cameras/images` response — where the uploaded image landed.
#[derive(Debug, Deserialize, Serialize)]
pub struct UploadCameraImageResponse {
    pub bucket: String,
    pub key: String,
}

/// `POST /cameras/analyze` — enqueue analysis of one or more cameras' images
/// over a time range. The server fans out one analyzer shard per camera.
#[derive(Debug, Deserialize, Serialize)]
pub struct AnalyzeCamerasRequest {
    pub camera_ids: Vec<String>,
    /// Free-form instruction for the model (e.g. "count the fruits on the tree").
    pub prompt: String,
    /// Sensor `provider` for the created sensors. Defaults to `"vision"`.
    pub provider: Option<String>,
    pub category: String,
    pub measurement_unit: Option<String>,
    pub depth_value: Option<f64>,
    pub depth_unit: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AnalyzeCamerasResponse {
    pub job_id: Uuid,
    pub status: String,
    pub shards_total: i32,
}

/// Per-shard Lambda invoke payload — one camera. The server mints `shard_id`
/// per camera and uses it to dedup `/complete` against Lambda retries.
#[derive(Debug, Deserialize, Serialize)]
pub struct ShardSpec {
    pub shard_id: String,
    pub job_id: Uuid,
    pub camera_id: String,
    pub prompt: String,
    pub provider: String,
    pub category: String,
    pub measurement_unit: Option<String>,
    pub depth_value: Option<f64>,
    pub depth_unit: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// `POST /analysis/jobs/{job_id}/complete` — one shard reporting its result.
/// `shard_status` is `"done"` or `"failed"`; the server aggregates across shards
/// and derives the job's terminal status.
#[derive(Debug, Deserialize, Serialize)]
pub struct CompleteJobRequest {
    pub shard_id: String,
    pub shard_status: String,
    pub error: Option<String>,
    pub images_total: Option<i32>,
    pub images_ok: Option<i32>,
}

/// `GET /analysis/jobs/{job_id}` — job lifecycle status.
#[derive(Debug, Deserialize, Serialize)]
pub struct JobStatusResponse {
    pub job_id: Uuid,
    pub status: String,
    pub error: Option<String>,
    pub shards_total: i32,
    pub shards_done: i32,
    pub shards_failed: i32,
    pub images_total: i32,
    pub images_ok: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}
