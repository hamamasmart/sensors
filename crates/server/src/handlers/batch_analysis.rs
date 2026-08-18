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
//! Memory is bounded (the server runs in a 512 MB container): the S3 listing
//! uses `Bucket::list_page` one page at a time (never `Bucket::list`, which
//! collects the whole history into a `Vec`), items are pulled on demand by
//! `buffer_unordered`, and the full matched image set is never materialized.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::stream::{self, StreamExt};
use uuid::Uuid;

use api_types::{
    AnalysisJobProgressResponse, AnalysisJobStatus, AnalysisPrompt, AnalyzeCamerasRequest,
    AnalyzeCamerasResponse,
};

use super::{
    ApiError, AppState, bad_request,
    image_analysis::{AnalysisSummary, presign_image_url, run_analyses},
};

/// Max images analyzed concurrently. Each image's prompts are already bounded
/// to `INFERENCE_CONCURRENCY` (4) inside `run_analyses`, so this caps total
/// in-flight OpenRouter calls at `BATCH_IMAGE_CONCURRENCY * 4`. Kept low for the
/// 512 MB container and OpenRouter rate limits.
const BATCH_IMAGE_CONCURRENCY: usize = 2;

/// Soft cap on remembered jobs so a long-running 512 MB container cannot leak
/// finished job entries without bound. When the store is at capacity on
/// insert, finished (`Completed`/`Failed`) entries are evicted first; in-flight
/// jobs are never dropped.
const MAX_JOB_ENTRIES: usize = 128;

/// The S3 key-path timestamp format (see `upload_camera_image`):
/// `<camera_id>/YYYY/MM/DD/HH/MM_SS.png`.
const KEY_TIME_FORMAT: &str = "%Y/%m/%d/%H/%M_%S";

/// Shared in-memory store of batch analysis jobs. `std::sync::Mutex` is safe
/// here because no guard is ever held across an `.await` — every lock is a
/// short, synchronous counter mutation or snapshot.
pub(crate) type JobStore = Arc<Mutex<HashMap<Uuid, AnalysisJobState>>>;

/// One in-flight (or finished) batch job. Mirrors [`AnalysisJobProgressResponse`]
/// minus the `job_id` (the map key is the id).
pub(crate) struct AnalysisJobState {
    pub status: AnalysisJobStatus,
    /// Images discovered so far. Grows as the S3 listing streams in and
    /// converges once every camera's listing is exhausted (the worker does not
    /// pre-count — that would require materializing the full listing).
    pub total_images: u64,
    /// Images whose analysis has finished (success or per-prompt failure).
    pub processed_images: u64,
    /// Images where at least one prompt failed.
    pub failed_images: u64,
    /// Prompt-level successes across all processed images.
    pub succeeded: u64,
    /// Prompt-level failures across all processed images.
    pub failed: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Populated only when `status == Failed` (job-level error, e.g. S3 list
    /// failure).
    pub error: Option<String>,
}

/// The timestamp-parsed, in-window image to process, produced by streaming the
/// S3 listing.
struct ListItem {
    camera_id: String,
    key: String,
    captured_at: DateTime<Utc>,
}

/// `unfold` state driving the paginated S3 listing across all requested cameras.
struct ListState {
    cameras: Vec<String>,
    /// Index into `cameras` of the camera currently being listed.
    cam_idx: usize,
    /// S3 continuation token for `cameras[cam_idx]`; `None` means "start a new
    /// camera" or "this camera is exhausted".
    token: Option<String>,
}

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

/// `POST /cameras/analyze` — start an offline batch analysis job.
///
/// Validates the request, registers a `Pending` job in the in-memory store, and
/// spawns the worker. Returns `202 Accepted` with the `job_id` immediately —
/// analysis runs entirely in the background; poll
/// `GET /cameras/analyze/{job_id}` for progress.
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
    {
        let mut jobs = state.jobs.lock().unwrap();
        // Evict finished jobs when at capacity so the store cannot grow
        // without bound on a long-running container. Only Completed/Failed
        // entries are dropped — never in-flight work.
        if jobs.len() >= MAX_JOB_ENTRIES {
            jobs.retain(|_, j| {
                matches!(
                    j.status,
                    AnalysisJobStatus::Pending | AnalysisJobStatus::Running
                )
            });
        }
        jobs.insert(
            job_id,
            AnalysisJobState {
                status: AnalysisJobStatus::Pending,
                total_images: 0,
                processed_images: 0,
                failed_images: 0,
                succeeded: 0,
                failed: 0,
                started_at,
                finished_at: None,
                error: None,
            },
        );
    }

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
/// Returns `404` if the job id is unknown (never started, or evicted from the
/// in-memory store).
pub async fn get_batch_analysis_progress(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<AnalysisJobProgressResponse>, ApiError> {
    let jobs = state.jobs.lock().unwrap();
    let job = jobs.get(&job_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("analysis job {job_id} not found"),
        )
    })?;
    Ok(Json(AnalysisJobProgressResponse {
        job_id,
        status: job.status,
        total_images: job.total_images,
        processed_images: job.processed_images,
        failed_images: job.failed_images,
        succeeded: job.succeeded,
        failed: job.failed,
        started_at: job.started_at,
        finished_at: job.finished_at,
        error: job.error.clone(),
    }))
}

/// The batch worker: stream S3 listings → filter to the window → presign each
/// image URL → run `run_analyses` → record progress, all bounded by
/// [`BATCH_IMAGE_CONCURRENCY`]. Never materializes the full image set.
async fn run_batch_analysis(
    state: AppState,
    job_id: Uuid,
    camera_ids: Vec<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    prompts: Vec<AnalysisPrompt>,
) {
    // Flip Pending → Running. If the entry was evicted before the worker
    // started (shouldn't happen — only finished jobs are evicted), abort.
    {
        let mut jobs = state.jobs.lock().unwrap();
        let Some(job) = jobs.get_mut(&job_id) else {
            return;
        };
        job.status = AnalysisJobStatus::Running;
    }

    // Cell for a job-level (S3 listing) error, written from inside the listing
    // stream and read after it drains to decide the final status. The stream
    // itself can only yield items or end; surfacing the error via a shared cell
    // avoids losing the message.
    let list_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Clone for the listing stream; the original is read after the stream
    // drains to decide the final status.
    let list_error_stream = list_error.clone();
    let bucket = state.s3.clone();

    // Lazy stream of in-window images: one `list_page` per poll, filtered,
    // chunked per page, then flattened. `Bucket::list_page` returns a single
    // page (~1000 objects) — never the collected-all-pages `Vec` that
    // `Bucket::list` would.
    let items = stream::unfold(
        ListState {
            cameras: camera_ids,
            cam_idx: 0,
            token: None,
        },
        move |mut st| {
            let bucket = bucket.clone();
            let list_error = list_error_stream.clone();
            async move {
                if st.cam_idx >= st.cameras.len() {
                    return None; // every camera listed
                }
                let cam = st.cameras[st.cam_idx].clone();
                let prefix = format!("{cam}/");
                match bucket
                    .list_page(prefix, None, st.token.take(), None, None)
                    .await
                {
                    Ok((page, _)) => {
                        let page_items: Vec<ListItem> = page
                            .contents
                            .into_iter()
                            .filter_map(|obj| parse_key(&cam, &obj.key, from, to))
                            .collect();
                        // Advance state: keep paging the current camera while a
                        // continuation token exists, else move to the next camera.
                        match page.next_continuation_token {
                            Some(token) => st.token = Some(token),
                            None => {
                                st.cam_idx += 1;
                                st.token = None;
                            }
                        }
                        Some((page_items, st))
                    }
                    Err(e) => {
                        *list_error.lock().unwrap() =
                            Some(format!("S3 list failed for camera {cam}: {e}"));
                        None // end the stream; the error is surfaced via the cell
                    }
                }
            }
        },
    )
    .flat_map(stream::iter);

    // Discover (count total) + presign + infer + record progress, bounded to
    // BATCH_IMAGE_CONCURRENCY images in flight. The closure captures clones
    // (AppState is cheaply Clone — all its fields are Arc/clone handles), and
    // each lock is released before any `.await` so no guard spans a suspension.
    let processing_state = state.clone();
    let api_key = state.openrouter_api_key.clone();
    items
        .map(move |item: ListItem| {
            // Discovery: this image is part of the total.
            {
                let mut jobs = processing_state.jobs.lock().unwrap();
                if let Some(job) = jobs.get_mut(&job_id) {
                    job.total_images += 1;
                }
            }
            let state = processing_state.clone();
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
                    Err(e) => {
                        // Presigning is a local computation (no S3 network
                        // call), so a failure here is exceptional. Treat it as
                        // all prompts for this image failing.
                        let failed = prompts.len();
                        tracing::warn!(
                            key = %item.key,
                            "presign failed for batch image: {}",
                            e.1
                        );
                        AnalysisSummary {
                            succeeded: 0,
                            failed,
                        }
                    }
                };
                let mut jobs = state.jobs.lock().unwrap();
                if let Some(job) = jobs.get_mut(&job_id) {
                    job.processed_images += 1;
                    job.succeeded += outcome.succeeded as u64;
                    job.failed += outcome.failed as u64;
                    if outcome.failed > 0 {
                        job.failed_images += 1;
                    }
                }
            }
        })
        .buffer_unordered(BATCH_IMAGE_CONCURRENCY)
        .for_each(|()| async {})
        .await;

    // Finalize: a listing error aborts the job (Failed); otherwise it ran to
    // completion (Completed), including the zero-images-in-window case.
    let list_error = list_error.lock().unwrap().take();
    let mut jobs = state.jobs.lock().unwrap();
    if let Some(job) = jobs.get_mut(&job_id) {
        job.finished_at = Some(Utc::now());
        job.status = if list_error.is_some() {
            AnalysisJobStatus::Failed
        } else {
            AnalysisJobStatus::Completed
        };
        job.error = list_error;
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
