//! Offline batch analysis of already-captured camera images.
//!
//! [`start_batch_analysis`] kicks off a background job that, for every camera
//! in the request, streams the object listing of that camera's S3 prefix
//! (`<camera_id>/...`), filters to keys whose encoded capture instant falls
//! within `[from, to]`, and runs the supplied [`AnalysisPrompt`]s against each
//! image — reusing the exact on-the-fly inference path
//! ([`super::image_analysis::run_analyses`]) so results land in the same
//! `{camera_id}_{prompt_id}` sensors. The request returns immediately with a
//! `job_id`; poll [`get_batch_analysis_progress`] for status.
//!
//! Job state is persisted in the `batch_analysis_jobs` table (not in memory)
//! so progress survives server restarts. The worker runs two streaming passes
//! over the S3 listing, both memory-bounded (the server runs in a 512 MB
//! container): a **count pass** that establishes `total_images` up front so
//! progress is meaningful from the start, then a **process pass** that
//! presigns and infers each image. Neither pass materializes the full listing
//! — each uses `Bucket::list_page` one page at a time (never `Bucket::list`,
//! which collects the whole history into a `Vec`).

use async_stream::try_stream;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::stream::TryStreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use api_types::{
    AnalysisJobProgressResponse, AnalysisJobStatus, AnalysisPrompt, AnalyzeCamerasRequest,
    AnalyzeCamerasResponse,
};

use super::{
    ApiError, AppState, KEY_TIME_FORMAT, bad_request, err,
    image_analysis::{AnalysisSummary, presign_image_url, run_analyses},
};

/// Max images analyzed concurrently. Each image's prompts are already bounded
/// to `INFERENCE_CONCURRENCY` (4) inside `run_analyses`, so this caps total
/// in-flight OpenRouter calls at `BATCH_IMAGE_CONCURRENCY * 4`. Kept low for
/// the 512 MB container and OpenRouter rate limits.
const BATCH_IMAGE_CONCURRENCY: usize = 2;

/// One row of `batch_analysis_jobs`, decoded by the `query_as!` macro below
/// (each field matched to a selected column by name — `FromRow` is not needed).
/// `status` decodes straight into [`AnalysisJobStatus`] (a native Postgres enum
/// via `sqlx::Type`); `BIGINT` columns decode as `i64`, carried as `u64` in the
/// progress response (counts are non-negative, so the cast is safe).
struct JobRow {
    status: AnalysisJobStatus,
    total_images: i64,
    processed_images: i64,
    failed_images: i64,
    succeeded: i64,
    failed: i64,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    error: Option<String>,
}

/// The timestamp-parsed, in-window image to process, produced by streaming the
/// S3 listing.
struct ListItem {
    camera_id: String,
    key: String,
    captured_at: DateTime<Utc>,
}

// ── DB helpers ───────────────────────────────────────────────────────────────
//
// Compile-time-checked `query!` / `query_as!` macros (validated against the
// `.sqlx` offline cache), matching `upsert_sensor_db`'s `query_scalar!` /
// `previous_results_text`'s `query_as!`. The one runtime `QueryBuilder` in the
// crate (`insert_measurements_db`) is reserved for the dynamic-row-count batch
// insert; these job queries all have a fixed shape, so they get macros.
//
// `status` is a native Postgres enum (`batch_job_status`) and `AnalysisJobStatus`
// derives `sqlx::Type` (via the `api-types` `sqlx` feature), so it binds and
// decodes directly — no `::text` cast on select and no string→enum bridge. This
// differs from `value_type` handling (which still text-bridges) only because
// `value_type` predates the feature flag; `AnalysisJobStatus` was added with it.

/// Insert a fresh `pending` job row. Counter columns default to 0 / NULL.
///
/// The `as AnalysisJobStatus` cast is the sqlx `query!` idiom for binding a
/// custom enum: the macro has no built-in Postgres→Rust mapping for the
/// `batch_job_status` enum, so a no-op cast on the bind expression makes the
/// macro skip its type-check (the cast compiles trivially since it casts a
/// value to its own type) and lets the `sqlx::Type`-derived `Encode` handle the
/// actual encoding at runtime.
async fn insert_job(
    pool: &PgPool,
    job_id: Uuid,
    started_at: DateTime<Utc>,
) -> Result<(), ApiError> {
    sqlx::query!(
        r#"INSERT INTO batch_analysis_jobs (job_id, status, started_at)
           VALUES ($1, $2, $3)"#,
        job_id,
        AnalysisJobStatus::Pending as AnalysisJobStatus,
        started_at,
    )
    .execute(pool)
    .await
    .map_err(err)?;
    Ok(())
}

/// Flip the job to `running`.
async fn set_job_running(pool: &PgPool, job_id: Uuid) -> Result<(), ApiError> {
    sqlx::query!(
        r#"UPDATE batch_analysis_jobs SET status = $2 WHERE job_id = $1"#,
        job_id,
        AnalysisJobStatus::Running as AnalysisJobStatus,
    )
    .execute(pool)
    .await
    .map_err(err)?;
    Ok(())
}

/// Record the up-front total image count (after the count pass).
async fn set_total_images(pool: &PgPool, job_id: Uuid, total: u64) -> Result<(), ApiError> {
    sqlx::query!(
        r#"UPDATE batch_analysis_jobs SET total_images = $2 WHERE job_id = $1"#,
        job_id,
        total as i64,
    )
    .execute(pool)
    .await
    .map_err(err)?;
    Ok(())
}

/// Increment the per-image progress counters after one image is processed.
/// `failed_images` bumps by 1 only when at least one prompt for that image
/// failed. All counter columns are `BIGINT`, so the increments bind as `i64`.
async fn record_image(
    pool: &PgPool,
    job_id: Uuid,
    outcome: &AnalysisSummary,
) -> Result<(), ApiError> {
    sqlx::query!(
        r#"UPDATE batch_analysis_jobs
           SET processed_images = processed_images + 1,
               succeeded = succeeded + $2,
               failed = failed + $3,
               failed_images = failed_images + $4
           WHERE job_id = $1"#,
        job_id,
        outcome.succeeded as i64,
        outcome.failed as i64,
        (outcome.failed > 0) as i64,
    )
    .execute(pool)
    .await
    .map_err(err)?;
    Ok(())
}

/// Mark the job finished: terminal status + `finished_at` + optional error.
/// `error` is a nullable `TEXT` column, so `Option<&str>` binds directly.
async fn finish_job(
    pool: &PgPool,
    job_id: Uuid,
    status: AnalysisJobStatus,
    error: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query!(
        r#"UPDATE batch_analysis_jobs
           SET status = $2, finished_at = NOW(), error = $3
           WHERE job_id = $1"#,
        job_id,
        status as AnalysisJobStatus,
        error,
    )
    .execute(pool)
    .await
    .map_err(err)?;
    Ok(())
}

/// Fetch a job row for the progress endpoint. `None` if the id is unknown.
/// `query_as!` decodes each selected column into the matching `JobRow` field by
/// name (no `FromRow` derive needed); `status` decodes straight into
/// [`AnalysisJobStatus`] as the native enum.
async fn fetch_job(pool: &PgPool, job_id: Uuid) -> Result<Option<JobRow>, ApiError> {
    sqlx::query_as!(
        JobRow,
        r#"SELECT status as "status!: AnalysisJobStatus", total_images, processed_images,
                  failed_images, succeeded, failed, started_at, finished_at, error
           FROM batch_analysis_jobs WHERE job_id = $1"#,
        job_id
    )
    .fetch_optional(pool)
    .await
    .map_err(err)
}

// ── S3 listing ───────────────────────────────────────────────────────────────

/// Parse an S3 object key into an in-window [`ListItem`].
///
/// Keys are `<camera_id>/YYYY/MM/DD/HH/MM_SS.png`; the `<camera_id>/` prefix is
/// known (we listed with it), so strip it plus the `.png` suffix and parse the
/// middle as a UTC `NaiveDateTime`. Returns `None` for any key that does not
/// match the expected shape or falls outside `[from, to]` — non-matching keys
/// (e.g. stray objects) are silently skipped.
fn parse_key(
    camera_id: &str,
    key: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<ListItem> {
    let prefix = format!("{camera_id}/");
    let rest = key.strip_prefix(&prefix)?;
    let stripped = rest.strip_suffix(".png")?;
    let captured_at = NaiveDateTime::parse_from_str(stripped, KEY_TIME_FORMAT)
        .ok()?
        .and_utc();
    if captured_at < from || captured_at > to {
        return None;
    }
    Some(ListItem {
        camera_id: camera_id.to_string(),
        key: key.to_string(),
        captured_at,
    })
}

/// Build a lazy stream of in-window images plus a shared cell for a listing
/// error. The stream pulls one S3 `list_page` (~1000 objects) per poll,
/// filters keys to the window, and flattens; the error cell is written if any
/// `list_page` fails (the stream then ends). Keys are never collected into a
/// single `Vec` across the whole history, so peak memory is ~one page.
///
/// Streams in-window images for every camera's S3 prefix, one `list_page` at a
/// time (never materializing the full listing). A listing failure is yielded as
/// a single `Err` and ends the stream (`?` inside `try_stream!` propagates it).
///
/// `Box::pin` because `try_stream!` builds a `!Unpin` async generator, and the
/// combinators the worker uses (`try_fold`, `fold`, `buffer_unordered`) require
/// `Self: Unpin` — `Pin<Box<_>>` is `Unpin`, so the call sites stay clean.
fn stream_list_items(
    bucket: s3::Bucket,
    cameras: Vec<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> impl futures::Stream<Item = Result<ListItem, String>> {
    Box::pin(try_stream! {
        for cam in cameras {
            let prefix = format!("{cam}/");
            let mut token = None;
            loop {
                let (page, _) = bucket
                    .list_page(prefix.clone(), None, token.take(), None, None)
                    .await
                    .map_err(|e| format!("S3 list failed for camera {cam}: {e}"))?;
                for obj in page.contents {
                    if let Some(item) = parse_key(&cam, &obj.key, from, to) {
                        yield item;
                    }
                }
                token = page.next_continuation_token;
                if token.is_none() {
                    break;
                }
            }
        }
    })
}

// ── HTTP handlers ────────────────────────────────────────────────────────────

/// `POST /cameras/analyze` — start an offline batch analysis job.
///
/// Validates the request, inserts a `pending` job row, and spawns the worker.
/// Returns `202 Accepted` with the `job_id` immediately — analysis runs
/// entirely in the background; poll `GET /cameras/analyze/{job_id}` for
/// progress.
pub async fn start_batch_analysis(
    State(state): State<AppState>,
    Json(req): Json<AnalyzeCamerasRequest>,
) -> Result<(StatusCode, Json<AnalyzeCamerasResponse>), ApiError> {
    if req.camera_ids.is_empty() {
        return Err(bad_request("`camera_ids` must not be empty"));
    }
    if req.prompts.is_empty() {
        return Err(bad_request("`prompts` must not be empty"));
    }
    if req.from >= req.to {
        return Err(bad_request("`from` must be earlier than `to`"));
    }

    // Dedup camera ids so a repeated id does not list (and analyze) the same
    // S3 prefix twice.
    let mut camera_ids = req.camera_ids.clone();
    camera_ids.sort_unstable();
    camera_ids.dedup();

    let job_id = Uuid::new_v4();
    let started_at = Utc::now();
    insert_job(&state.pool, job_id, started_at).await?;

    let state = state.clone();
    tokio::spawn(async move {
        run_batch_analysis(state, job_id, camera_ids, req.from, req.to, req.prompts).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(AnalyzeCamerasResponse { job_id }),
    ))
}

/// `GET /cameras/analyze/{job_id}` — report progress of a batch analysis job.
///
/// Returns `404` if the job id is unknown.
pub async fn get_batch_analysis_progress(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<AnalysisJobProgressResponse>, ApiError> {
    let row = fetch_job(&state.pool, job_id).await?.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("analysis job {job_id} not found"),
        )
    })?;
    Ok(Json(AnalysisJobProgressResponse {
        job_id,
        status: row.status,
        total_images: row.total_images as u64,
        processed_images: row.processed_images as u64,
        failed_images: row.failed_images as u64,
        succeeded: row.succeeded as u64,
        failed: row.failed as u64,
        started_at: row.started_at,
        finished_at: row.finished_at,
        error: row.error,
    }))
}

// ── Worker ───────────────────────────────────────────────────────────────────

/// The batch worker: count the in-window images (up-front `total_images`),
/// then stream them again to presign + infer each, recording progress per
/// image. Both passes stream S3 pages one at a time and never materialize the
/// full image set.
async fn run_batch_analysis(
    state: AppState,
    job_id: Uuid,
    camera_ids: Vec<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    prompts: Vec<AnalysisPrompt>,
) {
    let pool = state.pool.clone();

    if let Err(e) = set_job_running(&pool, job_id).await {
        tracing::warn!(%job_id, error = ?e, "failed to mark batch job running");
        return;
    }

    // Phase 1 — count in-window images. `try_fold` drains the stream without
    // storing keys (peak memory: one S3 page), giving an up-front total that
    // makes progress meaningful from the first processed image. A listing
    // error short-circuits straight to a failed job.
    let total = stream_list_items(state.s3.clone(), camera_ids.clone(), from, to)
        .try_fold(0u64, |n, _item| async move { Ok(n + 1) })
        .await;
    let total = match total {
        Ok(total) => total,
        Err(err) => {
            let _ = finish_job(&pool, job_id, AnalysisJobStatus::Failed, Some(&err)).await;
            return;
        }
    };
    // Best-effort: a failed total write is non-fatal; progress just reads the old value.
    if let Err(e) = set_total_images(&pool, job_id, total).await {
        tracing::warn!(%job_id, error = ?e, "failed to set total_images");
    }

    // Phase 2 — presign + infer each image, bounded to BATCH_IMAGE_CONCURRENCY.
    // `try_for_each_concurrent` takes the async closure directly and returns
    // `Result<(), String>`: `Ok` on a clean run, `Err` if the listing itself
    // failed (the closure always returns `Ok`, so only the stream can error).
    // It short-circuits on the first `Err`, dropping in-flight work — at most
    // `BATCH_IMAGE_CONCURRENCY` (2) images, and the job is failing anyway.
    let api_key = state.openrouter_api_key.clone();
    let proc_state = state.clone();
    let proc_result = stream_list_items(state.s3.clone(), camera_ids, from, to)
        .try_for_each_concurrent(BATCH_IMAGE_CONCURRENCY, |item| {
            let state = proc_state.clone();
            let api_key = api_key.clone();
            let prompts = prompts.clone();
            async move {
                let outcome = match presign_image_url(&state.s3, &item.key).await {
                    Ok(url) => {
                        run_analyses(
                            &state,
                            &api_key,
                            &item.camera_id,
                            item.captured_at,
                            url,
                            prompts,
                        )
                        .await
                    }
                    // Presigning is local (no S3 call), so failure is
                    // exceptional; treat it as every prompt failing.
                    Err(e) => {
                        let failed = prompts.len();
                        tracing::warn!(key = %item.key, "presign failed: {}", e.1);
                        AnalysisSummary {
                            succeeded: 0,
                            failed,
                        }
                    }
                };
                if let Err(e) = record_image(&state.pool, job_id, &outcome).await {
                    tracing::warn!(%job_id, error = ?e, "failed to record image progress");
                }
                Ok(())
            }
        })
        .await;

    // A mid-process listing failure fails the job; otherwise it's completed
    // (including the zero-images case).
    let (status, error) = match proc_result {
        Err(e) => (AnalysisJobStatus::Failed, Some(e)),
        Ok(()) => (AnalysisJobStatus::Completed, None),
    };
    if let Err(e) = finish_job(&pool, job_id, status, error.as_deref()).await {
        tracing::warn!(%job_id, error = ?e, "failed to finalize batch job");
    }
}

#[cfg(test)]
mod tests {
    use super::parse_key;
    use chrono::TimeZone;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn parse_key_in_window() {
        let from = dt(2026, 8, 19, 0, 0, 0);
        let to = dt(2026, 8, 19, 23, 59, 59);
        let key = "cam-1/2026/08/19/14/30_00.png";
        let item = parse_key("cam-1", key, from, to).expect("in-window key parses");
        assert_eq!(item.camera_id, "cam-1");
        assert_eq!(item.key, key);
        assert_eq!(item.captured_at, dt(2026, 8, 19, 14, 30, 0));
    }

    #[test]
    fn parse_key_out_of_window() {
        let from = dt(2026, 8, 19, 0, 0, 0);
        let to = dt(2026, 8, 19, 23, 59, 59);
        // strictly before / after the window
        assert!(parse_key("cam-1", "cam-1/2026/08/18/14/30_00.png", from, to).is_none());
        assert!(parse_key("cam-1", "cam-1/2026/08/20/00/00_00.png", from, to).is_none());
        // boundaries are inclusive
        assert!(parse_key("cam-1", "cam-1/2026/08/19/00/00_00.png", from, to).is_some());
        assert!(parse_key("cam-1", "cam-1/2026/08/19/23/59_59.png", from, to).is_some());
    }

    #[test]
    fn parse_key_bad_shape() {
        let from = dt(2000, 1, 1, 0, 0, 0);
        let to = dt(2100, 1, 1, 0, 0, 0);
        // wrong prefix
        assert!(parse_key("cam-1", "other-cam/2026/08/19/14/30_00.png", from, to).is_none());
        // missing .png
        assert!(parse_key("cam-1", "cam-1/2026/08/19/14/30_00", from, to).is_none());
        // unparseable timestamp
        assert!(parse_key("cam-1", "cam-1/not-a-date.png", from, to).is_none());
    }
}
