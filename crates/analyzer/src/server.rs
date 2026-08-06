//! HTTP client for the `server`. The analyzer is DB-less: it pushes sensors,
//! measurements, and job lifecycle here over HTTP (matching the scraper's
//! convention). Retries transient failures (429 / 5xx) with backoff.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use reqwest::{StatusCode, header};
use tokio::time::{Duration, sleep};
use tracing::warn;
use uuid::Uuid;

use api_types::{
    CompleteJobRequest, InsertMeasurementsRequest, Measurement, UpsertSensorRequest,
    UpsertSensorResponse,
};

const MAX_ATTEMPTS: u32 = 5;
static RETRY_JITTER_SEED: AtomicU64 = AtomicU64::new(0);

pub struct ServerClient {
    base: String,
    client: reqwest::Client,
}

impl Clone for ServerClient {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            client: self.client.clone(),
        }
    }
}

impl ServerClient {
    pub fn new(server_url: &str, auth_token: &str) -> anyhow::Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {auth_token}")
                .parse()
                .context("Invalid server auth token format")?,
        );
        let client = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to build server HTTP client")?;
        Ok(Self {
            base: server_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// `POST /analysis/jobs/{id}/start` — announce the shard has begun.
    pub async fn start_job(&self, job_id: Uuid) -> anyhow::Result<()> {
        let response = send_with_retry(|| {
            self.client
                .post(format!("{}/analysis/jobs/{job_id}/start", self.base))
        })
        .await
        .context("Server rejected job start")?;
        let _ = response.bytes().await;
        Ok(())
    }

    /// `POST /analysis/jobs/{id}/complete` — report this shard's result.
    pub async fn complete_job(&self, job_id: Uuid, req: CompleteJobRequest) -> anyhow::Result<()> {
        let response = send_with_retry(|| {
            self.client
                .post(format!("{}/analysis/jobs/{job_id}/complete", self.base))
                .json(&req)
        })
        .await
        .context("Server rejected job complete")?;
        let _ = response.bytes().await;
        Ok(())
    }

    /// `POST /sensors` — upsert the per-camera sensor, learn its internal id.
    pub async fn upsert_sensor(&self, req: UpsertSensorRequest) -> anyhow::Result<UpsertSensorResponse> {
        let response = send_with_retry(|| self.client.post(format!("{}/sensors", self.base)).json(&req))
            .await
            .context("Server rejected sensor upsert")?;
        response
            .json::<UpsertSensorResponse>()
            .await
            .context("Failed to parse sensor upsert response")
    }

    /// `POST /sensors/{id}/measurements` — batch insert, dedup on conflict.
    pub async fn insert_measurements(
        &self,
        sensor_id: Uuid,
        measurements: &[Measurement],
    ) -> anyhow::Result<()> {
        let response = send_with_retry(|| {
            self.client
                .post(format!("{}/sensors/{sensor_id}/measurements", self.base))
                .json(&InsertMeasurementsRequest {
                    measurements: measurements.to_vec(),
                })
        })
        .await
        .context("Server rejected measurements insert")?;
        let _ = response.bytes().await;
        Ok(())
    }
}

/// Retry transient failures (429 and 5xx) with exponential backoff + jitter,
/// honoring `Retry-After` when present. Non-retryable 4xx surface as errors.
async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let resp = build().send().await.context("Failed to send server request")?;
        let status = resp.status();
        let retryable =
            status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if retryable && attempt < MAX_ATTEMPTS {
            let delay = backoff_delay(attempt, resp.headers().get(header::RETRY_AFTER));
            let _ = resp.bytes().await;
            warn!(
                attempt,
                status = status.as_u16(),
                delay_ms = delay.as_millis() as u64,
                "Server returned retryable status, backing off"
            );
            sleep(delay).await;
            continue;
        }
        return resp.error_for_status().context("Server rejected request");
    }
}

/// Exponential backoff: 500ms, 1s, 2s, 4s, … plus 0–250ms jitter. Honors the
/// `Retry-After` header (seconds form) when the server provides it.
fn backoff_delay(attempt: u32, retry_after: Option<&reqwest::header::HeaderValue>) -> Duration {
    if let Some(header) = retry_after
        && let Ok(s) = header.to_str()
        && let Ok(secs) = s.trim().parse::<u64>()
    {
        return Duration::from_secs(secs);
    }
    let base_ms = 500u64 * 2u64.pow(attempt - 1);
    let jitter = RETRY_JITTER_SEED.fetch_add(1, Ordering::Relaxed) % 250;
    Duration::from_millis(base_ms + jitter)
}