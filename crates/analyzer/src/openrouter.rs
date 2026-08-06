//! OpenRouter (OpenAI-compatible) chat-completions client. Sends a batch of
//! images in one call and returns one numeric value per image, mapped back by
//! the `index` the model echoes.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use base64::Engine;
use regex::Regex;
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};
use tracing::warn;

use crate::system_prompt::SYSTEM_PROMPT;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MAX_ATTEMPTS: u32 = 5;

/// Monotonic counter used to spread concurrent retries so they don't back off
/// in lockstep. Avoids pulling in a `rand` dependency.
static RETRY_JITTER_SEED: AtomicU64 = AtomicU64::new(0);

/// Analyze a batch of images. Returns one entry per input image: `Some(value)`
/// on a parseable numeric result, `None` if that image had no usable result.
/// Fails (whole batch) only if the HTTP call itself fails after retries — the
/// caller then counts every image in the batch as a failure.
pub async fn analyze_batch(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    images: &[Vec<u8>],
    user_prompt: &str,
) -> anyhow::Result<Vec<Option<f64>>> {
    if images.is_empty() {
        return Ok(Vec::new());
    }

    let n = images.len();
    let mut content = Vec::with_capacity(1 + n);
    content.push(json!({
        "type": "text",
        "text": format!(
            "{user_prompt}\n\nAnalyze each of the {n} images below in order and return a result for every image, indexed 0..{}.",
            n - 1
        ),
    }));
    for bytes in images {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{b64}") },
        }));
    }

    let body = json!({
        "model": model,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": content },
        ],
    });

    let response = send_with_retry(|| {
        client
            .post(OPENROUTER_URL)
            .bearer_auth(api_key)
            .json(&body)
    })
    .await
    .context("OpenRouter request failed")?;

    let text = response
        .text()
        .await
        .context("Failed to read OpenRouter response body")?;
    Ok(parse_results(&text, n))
}

/// Parse `{"results":[{"index":i,"value":v}, ...]}` into per-image values. A
/// `value` may be a JSON number or a numeric string. A sequential regex fill
/// covers any slots the JSON parse left empty (the model returns results in
/// order, so value-appearance order maps to image order).
fn parse_results(content: &str, n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; n];

    if let Ok(value) = serde_json::from_str::<Value>(content)
        && let Some(results) = value.get("results").and_then(|r| r.as_array())
    {
        for item in results {
            let index = item
                .get("index")
                .and_then(|i| i.as_u64())
                .map(|i| i as usize);
            let val = item.get("value").and_then(value_to_f64);
            if let (Some(i), Some(val)) = (index, val)
                && i < n
            {
                out[i] = Some(val);
            }
        }
    }

    if out.iter().any(|v| v.is_none()) {
        let value_re =
            Regex::new(r#""value"\s*:\s*(-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)"#).unwrap();
        let mut slot = 0;
        for caps in value_re.captures_iter(content) {
            while slot < n && out[slot].is_some() {
                slot += 1;
            }
            if slot >= n {
                break;
            }
            if let Ok(v) = caps[1].parse::<f64>() {
                out[slot] = Some(v);
                slot += 1;
            }
        }
    }

    out
}

/// Accept a `value` whether the model emitted it as a number or a numeric string.
fn value_to_f64(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

/// Retry transient failures (429 and 5xx) with exponential backoff + jitter,
/// honoring `Retry-After` when present. Non-retryable 4xx surface as errors.
async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let resp = build().send().await.context("Failed to send OpenRouter request")?;
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
                "OpenRouter returned retryable status, backing off"
            );
            sleep(delay).await;
            continue;
        }
        return resp
            .error_for_status()
            .context("OpenRouter rejected request");
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