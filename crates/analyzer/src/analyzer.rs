//! Per-shard orchestration: list one camera's images, analyze them in batches
//! via OpenRouter, and push one measurement per image back to the server.

use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

use api_types::{CompleteJobRequest, Measurement, ShardSpec, UpsertSensorRequest};

use crate::configuration::Configuration;
use crate::openrouter;
use crate::server::ServerClient;
use crate::s3_images::{self, Image};

/// Stop launching new batches when this little time remains before the Lambda
/// deadline, so the shard isn't killed mid-write.
const DEADLINE_BUFFER: i64 = 30;

/// Run one shard (one camera) of an analysis job.
pub async fn run_shard(
    config: &Configuration,
    spec: &ShardSpec,
    deadline: Option<DateTime<Utc>>,
) -> anyhow::Result<()> {
    info!(job_id = %spec.job_id, camera_id = %spec.camera_id, "starting analysis shard");

    let bucket = build_bucket(config)?;
    let server = ServerClient::new(&config.server_url, &config.server_auth_token)
        .context("Failed to build server client")?;

    server.start_job(spec.job_id).await.context("start_job failed")?;

    // One sensor per camera per job: external_id = "{camera_id}:{job_id}".
    let external_id = format!("{}:{}", spec.camera_id, spec.job_id);
    let upsert = server
        .upsert_sensor(UpsertSensorRequest {
            external_id,
            provider: spec.provider.clone(),
            category: spec.category.clone(),
            measurement_unit: spec.measurement_unit.clone(),
            depth_value: spec.depth_value,
            depth_unit: spec.depth_unit.clone(),
        })
        .await
        .context("upsert_sensor failed")?;
    let sensor_id = upsert.sensor_id;

    let (images, capped) = s3_images::list_images(
        &bucket,
        &spec.camera_id,
        spec.starts_at,
        spec.ends_at,
        config.max_images_per_shard,
    )
    .await
    .context("list_images failed")?;
    let images_total: i32 = images.len() as i32;
    info!(images = images_total, capped, "listed images for shard");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .context("Failed to build OpenRouter HTTP client")?;
    let batch_size = config.batch_size.max(1);
    let max_concurrent = config.max_concurrent_batches.max(1);

    // Own each batch so the concurrent stream can move it into each task.
    let batches: Vec<Vec<Image>> = images.chunks(batch_size).map(Vec::from).collect();

    // Run batches concurrently (bounded), but stop launching new ones once the
    // Lambda deadline approaches so the shard isn't killed mid-write. Each task
    // flushes its own measurements, so a killed shard still keeps prior batches.
    let results: Vec<anyhow::Result<(i32, Option<String>)>> = stream::iter(batches)
        .take_while(|_| {
            let keep = match deadline {
                Some(deadline) => (deadline - Utc::now()).num_seconds() >= DEADLINE_BUFFER,
                None => true,
            };
            async move { keep }
        })
        .map(|batch| {
            let server = server.clone();
            let http = http.clone();
            async move {
                analyze_and_store(
                    &server,
                    &http,
                    &config.openrouter_api_key,
                    &config.openrouter_model,
                    &spec.prompt,
                    sensor_id,
                    batch,
                )
                .await
            }
        })
        .buffer_unordered(max_concurrent)
        .collect()
        .await;

    let mut images_ok: i32 = 0;
    let mut last_error: Option<String> = None;
    for result in results {
        match result {
            Ok((ok, error)) => {
                images_ok += ok;
                if let Some(error) = error {
                    last_error = Some(error);
                }
            }
            // A server-side error (insert) aborts the shard; Lambda retries it.
            Err(error) => return Err(error),
        }
    }

    let (shard_status, error) = shard_result(images_total, images_ok, capped, last_error);
    info!(shard_status, images_ok, images_total, "completing analysis shard");
    server
        .complete_job(
            spec.job_id,
            CompleteJobRequest {
                shard_id: spec.shard_id.clone(),
                shard_status,
                error,
                images_total: Some(images_total),
                images_ok: Some(images_ok),
            },
        )
        .await
        .context("complete_job failed")?;

    Ok(())
}

/// Analyze one batch and store its measurements. Returns `(ok_count, error)`
/// where `error` is set only for a failed OpenRouter call (per-image failures are
/// counted, not propagated). A server/insert error propagates so Lambda retries
/// the shard (re-runs are idempotent via `ON CONFLICT DO NOTHING`).
async fn analyze_and_store(
    server: &ServerClient,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    user_prompt: &str,
    sensor_id: Uuid,
    batch: Vec<Image>,
) -> anyhow::Result<(i32, Option<String>)> {
    let bytes: Vec<Vec<u8>> = batch.iter().map(|i| i.bytes.clone()).collect();
    match openrouter::analyze_batch(http, api_key, model, &bytes, user_prompt).await {
        Ok(values) => {
            let measurements: Vec<Measurement> = batch
                .iter()
                .zip(values.iter())
                .filter_map(|(image, value)| {
                    value.map(|value| Measurement {
                        value,
                        measured_at: image.measured_at,
                    })
                })
                .collect();
            let ok = measurements.len() as i32;
            for chunk in measurements.chunks(1000) {
                if chunk.is_empty() {
                    continue;
                }
                server
                    .insert_measurements(sensor_id, chunk)
                    .await
                    .context("insert_measurements failed")?;
            }
            Ok((ok, None))
        }
        Err(error) => {
            warn!(error = %error, "OpenRouter batch failed");
            Ok((0, Some(error.to_string())))
        }
    }
}

/// Derive this shard's terminal status and note. A shard with at least one good
/// measurement counts as `done` (with a note on any failures); only a shard that
/// produced zero good measurements out of a non-empty set is `failed`.
fn shard_result(
    images_total: i32,
    images_ok: i32,
    capped: bool,
    last_error: Option<String>,
) -> (String, Option<String>) {
    if images_total == 0 {
        return ("done".to_string(), Some("no images in range".to_string()));
    }
    if images_ok == 0 {
        return (
            "failed".to_string(),
            last_error.or(Some(format!("{images_total} images failed"))),
        );
    }
    let mut notes = Vec::new();
    if capped {
        notes.push("image cap exceeded (results may be partial)".to_string());
    }
    let failed = images_total - images_ok;
    if failed > 0 {
        notes.push(format!("{failed} images failed"));
    }
    let error = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    ("done".to_string(), error)
}

/// Resolve region + credentials from the environment (same as the server) and
/// return a read-only bucket handle. The analyzer never creates buckets.
fn build_bucket(config: &Configuration) -> anyhow::Result<Box<s3::Bucket>> {
    let region = s3::Region::from_default_env().context("Failed to load AWS region")?;
    let credentials =
        s3::creds::Credentials::default().context("Failed to load AWS credentials")?;
    let bucket = s3::Bucket::new(&config.s3_bucket, region, credentials)
        .context("Failed to construct S3 bucket")?;
    Ok(bucket)
}