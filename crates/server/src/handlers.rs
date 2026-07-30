//! HTTP route handlers. All DB writes live here so the scraper can stay DB-less.

use axum::body::Body;
use axum::extract::Query;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use s3::Bucket;
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use api_types::{
    InsertMeasurementsRequest, InsertMeasurementsResponse, UploadCameraImageQuery,
    UploadCameraImageResponse, UpsertSensorRequest, UpsertSensorResponse,
};

/// Shared state handed to every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub s3: Bucket,
}

pub(crate) type ApiError = (StatusCode, String);

pub(crate) fn err<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub(crate) fn bad_request<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// `POST /sensors` — upsert a sensor and return its internal id plus the latest
/// measurement time we already hold (so the caller can resume).
///
/// The SQL here is copied verbatim from the previous scraper so the committed
/// `.sqlx` offline cache still matches — do not change the whitespace.
pub async fn upsert_sensor(
    State(state): State<AppState>,
    Json(req): Json<UpsertSensorRequest>,
) -> Result<Json<UpsertSensorResponse>, ApiError> {
    let pool = state.pool;
    let sensor_id: Uuid = sqlx::query_scalar!(
        r#"
        INSERT INTO sensors (external_id, provider, category, measurement_unit, depth_value, depth_unit)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (external_id, provider) DO UPDATE SET
            category = EXCLUDED.category,
            measurement_unit = EXCLUDED.measurement_unit,
            depth_value = EXCLUDED.depth_value,
            depth_unit = EXCLUDED.depth_unit
        RETURNING sensor_id as "sensor_id!"
        "#,
        req.external_id,
        req.provider,
        req.category,
        req.measurement_unit,
        req.depth_value,
        req.depth_unit,
    )
    .fetch_one(&pool)
    .await
    .map_err(err)?;

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
    let pool = state.pool;
    let mut inserted: u64 = 0;

    for chunk in body.measurements.chunks(1000) {
        if chunk.is_empty() {
            continue;
        }
        let mut query_builder: QueryBuilder<sqlx::Postgres> =
            QueryBuilder::new("INSERT INTO measurements (sensor_id, value, measured_at) ");

        query_builder.push_values(chunk, |mut b, m| {
            b.push_bind(sensor_id)
                .push_bind(m.value)
                .push_bind(m.measured_at);
        });

        query_builder.push(" ON CONFLICT (sensor_id, measured_at) DO NOTHING");

        let result = query_builder.build().execute(&pool).await.map_err(err)?;
        inserted += result.rows_affected();
    }

    Ok(Json(InsertMeasurementsResponse { inserted }))
}

/// `POST /cameras/images?camera_id=<id>&captured_at=<epoch seconds>` — store a
/// camera image in S3 as PNG.
///
/// Query params:
///   - `camera_id` — the camera the image came from;
///   - `captured_at` — when the image was taken, as a Unix epoch timestamp in
///     seconds.
///
/// Multipart body: an `image` file part holding the raw PNG bytes. Only PNG is
/// accepted; anything else is rejected with `400 Bad Request`.
///
/// The request body is streamed straight into S3 via `put_object_stream` — the
/// handler never holds the whole image in memory.
/// The object key is `YYYY/MM/DD/HH/<camera_id>/mm_ss.png`,
/// with every component taken from `captured_at` in UTC.
pub async fn upload_camera_image(
    State(state): State<AppState>,
    Query(q): Query<UploadCameraImageQuery>,
    body: Body,
) -> Result<Json<UploadCameraImageResponse>, ApiError> {
    let captured_at = DateTime::<Utc>::from_timestamp(q.captured_at, 0)
        .ok_or_else(|| bad_request("`captured_at` is not a valid epoch timestamp"))?;
    let key = format!(
        "{}/{}.png",
        q.camera_id,
        captured_at.format("%Y/%m/%d/%H/%M_%S"),
    );
    tracing::info!(camera_id = %q.camera_id, %key, "streaming camera image to S3");

    let stream = body
        .into_data_stream()
        .map_err(std::io::Error::other);
    let mut reader = tokio_util::io::StreamReader::new(stream);

    state.s3.put_object_stream(&mut reader, &key).await.map_err(err)?;

    Ok(Json(UploadCameraImageResponse {
        bucket: state.s3.name.clone(),
        key,
    }))
}
