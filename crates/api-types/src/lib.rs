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
