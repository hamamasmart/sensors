//! HTTP route handlers. All DB writes live here so the scraper can stay DB-less.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use api_types::{
    InsertMeasurementsRequest, InsertMeasurementsResponse, UpsertSensorRequest,
    UpsertSensorResponse,
};

type ApiError = (StatusCode, String);

fn err<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// `POST /sensors` — upsert a sensor and return its internal id plus the latest
/// measurement time we already hold (so the caller can resume).
///
/// The SQL here is copied verbatim from the previous scraper so the committed
/// `.sqlx` offline cache still matches — do not change the whitespace.
pub async fn upsert_sensor(
    State(pool): State<PgPool>,
    Json(req): Json<UpsertSensorRequest>,
) -> Result<Json<UpsertSensorResponse>, ApiError> {
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
    State(pool): State<PgPool>,
    Path(sensor_id): Path<Uuid>,
    Json(body): Json<InsertMeasurementsRequest>,
) -> Result<Json<InsertMeasurementsResponse>, ApiError> {
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
