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
    /// The kind of value this sensor stores — `numeric` (a `f64` in
    /// `measurements.value`) or `text` (a string in `measurements.value_text`).
    /// Once a sensor exists it is pinned to this type; a subsequent upsert with a
    /// different value is rejected with `409 Conflict`. Defaults to `numeric`.
    #[serde(default)]
    pub value_type: ResponseType,
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
///
/// The value is either a number (stored in `measurements.value`) or free-form
/// text (stored in `measurements.value_text`, e.g. an `"r,g,b"` color reading).
/// The `#[serde(untagged)]` form keeps the wire shape flat — `{"value": 1.5}`
/// and `{"value": "120,45,200"}` — so numeric callers are unaffected.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MeasurementValue {
    Number(f64),
    Text(String),
}

/// A single, already-scaled measurement ready for storage. The scraper applies any
/// provider-specific scaling before sending; the server stores the value verbatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Measurement {
    pub value: MeasurementValue,
    pub measured_at: DateTime<Utc>,
}

impl Measurement {
    /// Construct a numeric measurement — convenience for the numeric-only
    /// callers (scraper, Topco webhook).
    pub fn number(value: f64, measured_at: DateTime<Utc>) -> Self {
        Self {
            value: MeasurementValue::Number(value),
            measured_at,
        }
    }
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
///
/// Analysis runs in the background after this response is sent, so its
/// results are not included here; they are logged server-side instead.
#[derive(Debug, Deserialize, Serialize)]
pub struct UploadCameraImageResponse {
    pub bucket: String,
    pub key: String,
}

/// A vision-LLM prompt to run against an uploaded camera image. Sent as the
/// `prompts` multipart part (a JSON array of these) on `POST /cameras/images`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnalysisPrompt {
    pub prompt_id: String,
    pub prompt_text: String,
    /// OpenRouter model slug, e.g. `google/gemini-2.5-flash`.
    pub model: String,
    pub measurement_unit: Option<String>,
    /// Whether the model should return a single numeric value or free-form text.
    /// For `text`, the `prompt_text` is responsible for specifying the exact
    /// output format (e.g. "answer as `r,g,b`"); the system prompt only asks for
    /// a single short value with no prose/markdown. Defaults to `numeric`.
    #[serde(default)]
    pub response_type: ResponseType,
    /// When true, the most recent prior measurements for this
    /// `{camera_id}_{prompt_id}` sensor are fetched and attached to the prompt
    /// as an extra text content part — each prior value with its measurement
    /// time, plus the current analysis datetime — so the model can reason over
    /// the sensor's history. No context part is added when there is no history
    /// yet (first run) or when this is false. Defaults to `false`.
    #[serde(default)]
    pub include_previous_results: bool,
}

/// Kind of value an analysis prompt expects back from the model, which selects
/// the system prompt and how the reply is parsed/stored. Also the value type
/// recorded on a sensor (`sensors.value_type`), pinning it to numeric or text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    /// A single number parseable as `f64`, stored in `measurements.value`.
    #[default]
    Numeric,
    /// Free-form text (e.g. `"120,45,200"` for an RGB prompt), stored in
    /// `measurements.value_text`.
    Text,
}

impl ResponseType {
    /// The string stored in `sensors.value_type` (and checked on upsert).
    pub const fn as_db_str(self) -> &'static str {
        match self {
            ResponseType::Numeric => "numeric",
            ResponseType::Text => "text",
        }
    }

    /// Parse a `sensors.value_type` string back into the enum. Returns `None`
    /// for an unknown value.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "numeric" => Some(ResponseType::Numeric),
            "text" => Some(ResponseType::Text),
            _ => None,
        }
    }
}
