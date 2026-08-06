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
use futures::future::join_all;
use s3::Bucket;
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use api_types::{
    AnalyzeCamerasRequest, AnalyzeCamerasResponse, CompleteJobRequest, InsertMeasurementsRequest,
    InsertMeasurementsResponse, JobStatusResponse, ShardSpec, UploadCameraImageQuery,
    UploadCameraImageResponse, UpsertSensorRequest, UpsertSensorResponse,
};

/// Shared state handed to every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub s3: Bucket,
    /// Used by `POST /cameras/analyze` to async-invoke one analyzer shard per
    /// camera (`InvocationType=Event`).
    pub lambda: aws_sdk_lambda::Client,
    pub analyzer_function_name: String,
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

// ── Camera-image analysis ───────────────────────────────────────────────────
//
// `POST /cameras/analyze` inserts an `analysis_jobs` row and async-invokes one
// `analyzer` Lambda per camera (a "shard"). Each shard lists that camera's
// images, sends them to OpenRouter in batches, and writes one measurement per
// image back through the existing `/sensors` + `/sensors/{id}/measurements`
// endpoints. The shard then calls `/analysis/jobs/{id}/complete`, which
// aggregates counts across shards; the job is terminal once every shard has
// reported in. `shard_id` dedups `/complete` against Lambda async-invocation
// retries. All new SQL uses `sqlx::query!`/`query_as!` (compile-time checked),
// so the root `.sqlx` offline cache must be refreshed via `cargo sqlx prepare`
// when these queries change — see the warning above on `upsert_sensor`.

/// `POST /cameras/analyze` — enqueue a job and fan out one shard per camera.
pub async fn analyze_cameras(
    State(state): State<AppState>,
    Json(req): Json<AnalyzeCamerasRequest>,
) -> Result<(StatusCode, Json<AnalyzeCamerasResponse>), ApiError> {
    if req.starts_at >= req.ends_at {
        return Err(bad_request("`starts_at` must be before `ends_at`"));
    }
    if req.camera_ids.is_empty() {
        return Err(bad_request("`camera_ids` must not be empty"));
    }
    if req.prompt.trim().is_empty() {
        return Err(bad_request("`prompt` must not be empty"));
    }
    if req.camera_ids.iter().any(|c| c.contains('/')) {
        return Err(bad_request(
            "`camera_id` must not contain '/' (it is the S3 key prefix)",
        ));
    }

    let provider = req.provider.unwrap_or_else(|| "vision".to_string());
    let shards_total: i32 = req.camera_ids.len().try_into().unwrap_or(i32::MAX);

    let row = sqlx::query!(
        r#"
        INSERT INTO analysis_jobs
            (camera_ids, prompt, provider, category, measurement_unit, depth_value,
             depth_unit, starts_at, ends_at, shards_total, status)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending')
        RETURNING job_id, prompt, provider, category, measurement_unit, depth_value,
                  depth_unit, starts_at, ends_at
        "#,
        &req.camera_ids,
        &req.prompt,
        &provider,
        &req.category,
        req.measurement_unit,
        req.depth_value,
        req.depth_unit,
        req.starts_at,
        req.ends_at,
        shards_total,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(err)?;

    let job_id = row.job_id;

    // Build one shard spec per camera, then fire all invokes concurrently.
    let specs: Vec<ShardSpec> = req
        .camera_ids
        .iter()
        .map(|camera_id| ShardSpec {
            shard_id: Uuid::new_v4().to_string(),
            job_id,
            camera_id: camera_id.clone(),
            prompt: row.prompt.clone(),
            provider: row.provider.clone(),
            category: row.category.clone(),
            measurement_unit: row.measurement_unit.clone(),
            depth_value: row.depth_value,
            depth_unit: row.depth_unit.clone(),
            starts_at: row.starts_at,
            ends_at: row.ends_at,
        })
        .collect();

    let results = join_all(
        specs
            .iter()
            .map(|spec| invoke_shard(&state.lambda, &state.analyzer_function_name, spec)),
    )
    .await;

    // Any invoke that failed is recorded as a failed shard inline, so the
    // job's shard counter stays consistent and the job can still finish.
    let mut last_agg: Option<Aggregate> = None;
    for (spec, result) in specs.iter().zip(results) {
        if let Err(e) = result {
            let complete = CompleteJobRequest {
                shard_id: spec.shard_id.clone(),
                shard_status: "failed".to_string(),
                error: Some(format!("invoke failed: {e}")),
                images_total: Some(0),
                images_ok: Some(0),
            };
            last_agg = apply_shard_complete(&state.pool, job_id, &complete).await?;
        }
    }
    if let Some(agg) = last_agg {
        finalize_if_done(&state.pool, job_id, &agg).await?;
    }

    Ok((
        StatusCode::CREATED,
        Json(AnalyzeCamerasResponse {
            job_id,
            status: "pending".to_string(),
            shards_total,
        }),
    ))
}

/// `POST /analysis/jobs/{job_id}/start` — a shard announcing it has begun. Only
/// the first shard flips `pending` → `running`; later starts are no-ops.
pub async fn start_analysis_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    sqlx::query!(
        r#"
        UPDATE analysis_jobs
        SET status = 'running', started_at = COALESCE(started_at, NOW())
        WHERE job_id = $1 AND status = 'pending'
        "#,
        job_id,
    )
    .execute(&state.pool)
    .await
    .map_err(err)?;
    Ok(StatusCode::OK)
}

/// `POST /analysis/jobs/{job_id}/complete` — one shard reporting its result.
/// Aggregates counts across shards and flips the job terminal once all shards
/// are in. Idempotent per `shard_id` (safe against Lambda retries).
pub async fn complete_analysis_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    Json(req): Json<CompleteJobRequest>,
) -> Result<StatusCode, ApiError> {
    match apply_shard_complete(&state.pool, job_id, &req).await? {
        Some(agg) => {
            finalize_if_done(&state.pool, job_id, &agg).await?;
            Ok(StatusCode::OK)
        }
        None => Err((StatusCode::NOT_FOUND, "job not found".to_string())),
    }
}

/// `GET /analysis/jobs/{job_id}` — job lifecycle status.
pub async fn get_analysis_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    let row = sqlx::query_as!(
        JobStatusResponse,
        r#"
        SELECT job_id, status, error, shards_total, shards_done, shards_failed,
               images_total, images_ok, created_at, started_at, completed_at
        FROM analysis_jobs
        WHERE job_id = $1
        "#,
        job_id,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(err)?;

    match row {
        Some(resp) => Ok(Json(resp)),
        None => Err((StatusCode::NOT_FOUND, "job not found".to_string())),
    }
}

/// Aggregate shard counters returned while applying a `/complete`.
#[derive(Clone, Copy)]
struct Aggregate {
    images_total: i32,
    images_ok: i32,
    shards_total: i32,
    shards_done: i32,
    shards_failed: i32,
}

/// Async-invoke one analyzer shard. `InvocationType=Event` returns immediately
/// (Lambda queues the work); the analyzer reports back via `/complete`.
async fn invoke_shard(
    lambda: &aws_sdk_lambda::Client,
    function_name: &str,
    spec: &ShardSpec,
) -> Result<(), String> {
    let payload = serde_json::to_vec(spec).map_err(|e| e.to_string())?;
    lambda
        .invoke()
        .function_name(function_name)
        .invocation_type(aws_sdk_lambda::types::InvocationType::Event)
        .payload(aws_sdk_lambda::primitives::Blob::new(payload))
        .send()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Atomically fold one shard's result into the job counters, deduping by
/// `shard_id` so a retried shard's second `/complete` is a no-op. Returns the
/// post-update aggregate, or `None` if the job doesn't exist.
async fn apply_shard_complete(
    pool: &PgPool,
    job_id: Uuid,
    req: &CompleteJobRequest,
) -> Result<Option<Aggregate>, ApiError> {
    let row = sqlx::query!(
        r#"
        UPDATE analysis_jobs SET
            completed_shards = CASE WHEN NOT completed_shards @> ARRAY[$5]::text[]
                                    THEN completed_shards || $5 ELSE completed_shards END,
            images_total  = images_total  + CASE WHEN NOT completed_shards @> ARRAY[$5]::text[]
                                                THEN $2 ELSE 0 END,
            images_ok     = images_ok     + CASE WHEN NOT completed_shards @> ARRAY[$5]::text[]
                                                THEN $3 ELSE 0 END,
            shards_done   = shards_done   + CASE WHEN NOT completed_shards @> ARRAY[$5]::text[]
                                                 AND $4 = 'done'   THEN 1 ELSE 0 END,
            shards_failed = shards_failed + CASE WHEN NOT completed_shards @> ARRAY[$5]::text[]
                                                  AND $4 = 'failed' THEN 1 ELSE 0 END
        WHERE job_id = $1
        RETURNING images_total, images_ok, shards_total, shards_done, shards_failed
        "#,
        job_id,
        req.images_total.unwrap_or(0),
        req.images_ok.unwrap_or(0),
        req.shard_status,
        req.shard_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(err)?;

    Ok(row.map(|r| Aggregate {
        images_total: r.images_total,
        images_ok: r.images_ok,
        shards_total: r.shards_total,
        shards_done: r.shards_done,
        shards_failed: r.shards_failed,
    }))
}

/// If every shard has reported in, derive and set the job's terminal status.
/// The `status = 'running'` guard makes a duplicate (retried) call a no-op.
async fn finalize_if_done(pool: &PgPool, job_id: Uuid, agg: &Aggregate) -> Result<(), ApiError> {
    if agg.shards_done + agg.shards_failed < agg.shards_total {
        return Ok(());
    }

    let (status, error) = if agg.shards_total > 0 && agg.shards_failed == agg.shards_total {
        (
            "failed".to_string(),
            Some(format!("all {} shards failed", agg.shards_total)),
        )
    } else if agg.images_total == 0 {
        ("done".to_string(), Some("no images in range".to_string()))
    } else if agg.images_ok == 0 {
        ("failed".to_string(), None)
    } else {
        let failed_images = agg.images_total - agg.images_ok;
        let error = if agg.shards_failed > 0 || failed_images > 0 {
            Some(format!(
                "{} shards failed / {} images failed",
                agg.shards_failed, failed_images,
            ))
        } else {
            None
        };
        ("done".to_string(), error)
    };

    sqlx::query!(
        r#"
        UPDATE analysis_jobs
        SET status = $2, error = $3, completed_at = NOW()
        WHERE job_id = $1 AND status = 'running'
        "#,
        job_id,
        status.as_str(),
        error.as_deref(),
    )
    .execute(pool)
    .await
    .map_err(err)?;

    Ok(())
}
