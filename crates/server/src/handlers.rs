//! HTTP route handlers. All DB writes live here so the scraper can stay DB-less.

use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use s3::Bucket;
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use api_types::{
    AnalysisPrompt, InsertMeasurementsRequest, InsertMeasurementsResponse, Measurement,
    ResponseType, UploadCameraImageQuery, UploadCameraImageResponse, UpsertSensorRequest,
    UpsertSensorResponse,
};

/// Shared state handed to every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub s3: Bucket,
    /// Shared HTTP client for outbound calls (OpenRouter inference).
    pub http: reqwest::Client,
    /// OpenRouter API key.
    pub openrouter_api_key: String,
    /// OpenRouter API base URL.
    pub openrouter_base_url: String,
}

pub(crate) type ApiError = (StatusCode, String);

pub(crate) fn err<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub(crate) fn bad_request<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

// ── Shared DB helpers ───────────────────────────────────────────────────────
//
// The sensor upsert and measurement insert are factored out of their HTTP
// handlers so the RealiteQ (Topco) webhook can reuse the exact same writes
// rather than carrying its own SQL. The upsert uses the `query_scalar!` macro
// (compile-time checked against the `.sqlx` offline cache); the batch insert
// uses a runtime `QueryBuilder` because the row count is dynamic — same split
// the handlers already had.

/// Upsert a sensor keyed by `(external_id, provider)` and return its internal
/// id.
///
/// `value_type` pins the sensor to numeric or text measurements. The rejection
/// of a type change is done in SQL: the `ON CONFLICT DO UPDATE` carries a
/// `WHERE sensors.value_type = EXCLUDED.value_type`, so the update (and its
/// `RETURNING` row) only happens when the requested type matches the existing
/// one. When it does not, no row is returned — `fetch_one` yields
/// `RowNotFound`, which we translate to `409 Conflict`. So a sensor can never
/// change its value type once created. `category` is optional so callers
/// without category metadata (the Topco webhook) store `NULL`.
///
/// `sensors.value_type` is a native Postgres enum (`sensor_value_type`), so the
/// `&str` argument is cast `$7::text::sensor_value_type`: the inner `::text`
/// keeps the compile-time-checked macro seeing the parameter as `Text`
/// (compatible with the `&str` bind), and the outer cast hands Postgres the
/// enum. `EXCLUDED.value_type` / `sensors.value_type` are already enum-typed, so
/// the conflict `WHERE` compares them directly.
///
/// The SQL uses the compile-time-checked `query_scalar!` macro (validated
/// against the `.sqlx` offline cache); the whitespace is matched against it.
#[allow(clippy::too_many_arguments)]
async fn upsert_sensor_db(
    pool: &PgPool,
    external_id: &str,
    provider: &str,
    category: Option<&str>,
    measurement_unit: Option<&str>,
    depth_value: Option<f64>,
    depth_unit: Option<&str>,
    value_type: &str,
) -> Result<Uuid, ApiError> {
    let sensor_id = match sqlx::query_scalar!(
        r#"
        INSERT INTO sensors (external_id, provider, category, measurement_unit, depth_value, depth_unit, value_type)
        VALUES ($1, $2, $3, $4, $5, $6, $7::text::sensor_value_type)
        ON CONFLICT (external_id, provider) DO UPDATE SET
            category = EXCLUDED.category,
            measurement_unit = EXCLUDED.measurement_unit,
            depth_value = EXCLUDED.depth_value,
            depth_unit = EXCLUDED.depth_unit
        WHERE sensors.value_type = EXCLUDED.value_type
        RETURNING sensor_id as "sensor_id!"
        "#,
        external_id,
        provider,
        category,
        measurement_unit,
        depth_value,
        depth_unit,
        value_type,
    )
    .fetch_one(pool)
    .await
    {
        Ok(id) => id,
        // No row returned: the ON CONFLICT WHERE filtered the conflicting row
        // out, meaning the sensor already exists with a different value_type.
        Err(sqlx::Error::RowNotFound) => {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT value_type::text FROM sensors WHERE external_id = $1 AND provider = $2",
            )
            .bind(external_id)
            .bind(provider)
            .fetch_optional(pool)
            .await
            .map_err(err)?;
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "sensor {external_id:?} (provider {provider:?}) is already {existing}; \
                     cannot change to {requested}",
                    existing = existing.unwrap_or_else(|| "unknown".to_string()),
                    requested = value_type,
                ),
            ));
        }
        Err(e) => return Err(err(e)),
    };

    Ok(sensor_id)
}

/// Batch-insert measurements for a single sensor, deduping on conflict.
/// Rows are pushed in 1000-row chunks so each request stays well under the
/// Postgres parameter limit. Returns the number of rows actually inserted.
///
/// Each measurement's value is either a number (bound to `value`) or text
/// (bound to `value_text`); the unused column is bound `NULL`. The runtime
/// `QueryBuilder` is used (not the compile-time-checked macro) so the column
/// list here does not need a `.sqlx` offline-cache entry.
async fn insert_measurements_db(
    pool: &PgPool,
    sensor_id: Uuid,
    measurements: &[Measurement],
) -> Result<u64, ApiError> {
    use api_types::MeasurementValue;

    // Enforce the sensor's pinned `value_type` on every inserted measurement.
    // The upsert pins a sensor to `numeric` or `text` (rejecting a type change
    // with 409); this mirrors that guard at the measurement level so a
    // `numeric` sensor can never receive a text value (or vice versa). The old
    // `Measurement.value: f64` guaranteed numeric at the type level — that
    // guarantee moved to the untagged enum, so it must be re-imposed here.
    //
    // `value_type` is a native enum, so it is cast `::text` here for sqlx to
    // decode into a `String` (sqlx won't decode a custom enum OID as `String`
    // directly); `ResponseType::from_db_str` then maps it back to the enum.
    let sensor_type: Option<String> =
        sqlx::query_scalar("SELECT value_type::text FROM sensors WHERE sensor_id = $1")
            .bind(sensor_id)
            .fetch_optional(pool)
            .await
            .map_err(err)?;
    let sensor_type = sensor_type.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("sensor {sensor_id} not found"),
        )
    })?;
    let expected = ResponseType::from_db_str(&sensor_type).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sensor {sensor_id} has unknown value_type {sensor_type:?}"),
        )
    })?;
    for m in measurements {
        let matches = matches!(
            (&m.value, expected),
            (MeasurementValue::Number(_), ResponseType::Numeric)
                | (MeasurementValue::Text(_), ResponseType::Text)
        );
        if !matches {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "sensor {sensor_id} is {} but a {} measurement was provided",
                    expected.as_db_str(),
                    match &m.value {
                        MeasurementValue::Number(_) => "numeric",
                        MeasurementValue::Text(_) => "text",
                    },
                ),
            ));
        }
    }

    let mut inserted: u64 = 0;

    for chunk in measurements.chunks(1000) {
        if chunk.is_empty() {
            continue;
        }
        let mut query_builder: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO measurements (sensor_id, value, value_text, measured_at) ",
        );

        query_builder.push_values(chunk, |mut b, m| {
            let (num, text): (Option<f64>, Option<&str>) = match &m.value {
                MeasurementValue::Number(n) => (Some(*n), None),
                MeasurementValue::Text(t) => (None, Some(t.as_str())),
            };
            b.push_bind(sensor_id)
                .push_bind(num)
                .push_bind(text)
                .push_bind(m.measured_at);
        });

        query_builder.push(" ON CONFLICT (sensor_id, measured_at) DO NOTHING");

        let result = query_builder.build().execute(pool).await.map_err(err)?;
        inserted += result.rows_affected();
    }

    Ok(inserted)
}

mod image_analysis;
mod topco;

pub use topco::topco_webhook;

// ── HTTP handlers ───────────────────────────────────────────────────────────

/// `POST /sensors` — upsert a sensor and return its internal id plus the latest
/// measurement time we already hold (so the caller can resume).
pub async fn upsert_sensor(
    State(state): State<AppState>,
    Json(req): Json<UpsertSensorRequest>,
) -> Result<Json<UpsertSensorResponse>, ApiError> {
    let pool = state.pool;
    let sensor_id = upsert_sensor_db(
        &pool,
        &req.external_id,
        &req.provider,
        Some(req.category.as_str()),
        req.measurement_unit.as_deref(),
        req.depth_value,
        req.depth_unit.as_deref(),
        req.value_type.as_db_str(),
    )
    .await?;

    let last_measured_at: Option<DateTime<Utc>> = sqlx::query_scalar!(
        r#"
        SELECT MAX(measured_at) as "max_measured_at"
        FROM measurements
        WHERE sensor_id = $1
        "#,
        sensor_id
    )
    .fetch_one(&pool)
    .await
    .map_err(err)?;

    Ok(Json(UpsertSensorResponse {
        sensor_id,
        last_measured_at,
    }))
}

/// `POST /sensors/:sensor_id/measurements` — batch insert, dedup on conflict.
pub async fn insert_measurements(
    State(state): State<AppState>,
    Path(sensor_id): Path<Uuid>,
    Json(body): Json<InsertMeasurementsRequest>,
) -> Result<Json<InsertMeasurementsResponse>, ApiError> {
    let inserted = insert_measurements_db(&state.pool, sensor_id, &body.measurements).await?;
    Ok(Json(InsertMeasurementsResponse { inserted }))
}

/// `POST /cameras/images?camera_id=<id>&captured_at=<epoch seconds>` — store a
/// camera image in S3 as PNG and, optionally, run vision-LLM analysis on it.
///
/// Query params:
///   - `camera_id` — the camera the image came from;
///   - `captured_at` — when the image was taken, as a Unix epoch timestamp in
///     seconds.
///
/// Multipart body:
///   - `image` (required) — the raw PNG bytes. Streamed straight into S3 via
///     `put_object_stream`; the handler never holds the whole image in memory.
///   - `prompts` (optional) — a JSON array of [`AnalysisPrompt`]. When present,
///     each prompt is run against the image through OpenRouter (the image is
///     handed to the model as a presigned S3 GET URL, so it is not buffered
///     here), and the parsed result is stored as a measurement for a
///     `{camera_id}_{prompt_id}` sensor under the `image-analysis` provider.
///     Analysis runs in a detached `tokio::spawn` task *after* the upload
///     responds, so inference failures never affect the upload's status and
///     its results are logged rather than returned.
///
/// The object key is `<camera_id>/YYYY/MM/DD/HH/MM_SS.png`, with every component
/// taken from `captured_at` in UTC.
pub async fn upload_camera_image(
    State(state): State<AppState>,
    Query(q): Query<UploadCameraImageQuery>,
    mut multipart: Multipart,
) -> Result<Json<UploadCameraImageResponse>, ApiError> {
    let captured_at = DateTime::<Utc>::from_timestamp(q.captured_at, 0)
        .ok_or_else(|| bad_request("`captured_at` is not a valid epoch timestamp"))?;
    let key = format!(
        "{}/{}.png",
        q.camera_id,
        captured_at.format("%Y/%m/%d/%H/%M_%S"),
    );

    let mut prompts: Option<Vec<AnalysisPrompt>> = None;
    let mut uploaded = false;

    // Consume fields in arrival order. The `image` field is streamed straight
    // into S3 as it arrives — never collected into a buffer — while the small
    // `prompts` field is read into a string and parsed as JSON.
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("invalid multipart field: {e}")))?
    {
        match field.name() {
            Some("image") => {
                if uploaded {
                    return Err(bad_request("duplicate `image` part"));
                }
                tracing::info!(camera_id = %q.camera_id, %key, "streaming camera image to S3");
                image_analysis::stream_field_to_s3(field, &state.s3, &key).await?;
                uploaded = true;
            }
            Some("prompts") => {
                if prompts.is_some() {
                    return Err(bad_request("duplicate `prompts` part"));
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(format!("failed to read `prompts` part: {e}")))?;
                let parsed: Vec<AnalysisPrompt> = serde_json::from_slice(&bytes)
                    .map_err(|e| bad_request(format!("`prompts` part is not valid JSON: {e}")))?;
                prompts = Some(parsed);
            }
            _ => {
                // Ignore unknown parts rather than rejecting the whole upload.
                tracing::debug!(name = ?field.name(), "ignoring unknown multipart part");
            }
        }
    }

    if !uploaded {
        return Err(bad_request("missing required `image` part"));
    }

    // Run analysis in the background so the upload returns as soon as the
    // image is in S3. Inference results are logged, not returned: the client
    // does not wait for the vision model, and a slow/hung model no longer
    // holds the request open (a 30s client timeout bounds the spawned work).
    // The image is fetched by OpenRouter through a presigned S3 URL, so it is
    // not re-read into memory here.
    if let Some(prompts) = prompts.filter(|p| !p.is_empty()) {
        let image_url = image_analysis::presign_image_url(&state.s3, &key).await?;
        let state = state.clone();
        let camera_id = q.camera_id.clone();
        let api_key = state.openrouter_api_key.clone();
        tokio::spawn(async move {
            image_analysis::run_analyses(
                &state,
                &api_key,
                &camera_id,
                captured_at,
                image_url,
                prompts,
            )
            .await;
        });
    }

    Ok(Json(UploadCameraImageResponse {
        bucket: state.s3.name.clone(),
        key,
    }))
}
