//! Vision-LLM image analysis via OpenRouter.
//!
//! After an image is streamed into S3, the server can run a set of caller-supplied
//! prompts against it: each prompt is sent to OpenRouter with a system instruction
//! that makes the model reply with a single value — a number (for `numeric`
//! prompts) or a short text string (for `text` prompts, where the user's own
//! prompt specifies the exact format). The result is stored as a measurement for
//! a per-`(camera, prompt)` sensor.
//!
//! The image is never buffered in server memory: OpenRouter fetches it through a
//! presigned S3 GET URL, so only the URL crosses the wire here.

use std::time::Duration;

use anyhow::Context;
use api_types::{AnalysisPrompt, Measurement, MeasurementValue, ResponseType};
use futures::{
    future::FutureExt,
    stream::{self, StreamExt},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{ApiError, insert_measurements_db, upsert_sensor_db};

/// The `provider` recorded for sensors fed by image analysis. Stored as plain
/// text alongside the existing `phytech` / `tera` / `topco` providers.
const IMAGE_ANALYSIS_PROVIDER: &str = "image-analysis";

/// How long the presigned S3 GET URL handed to OpenRouter stays valid. Needs to
/// outlive the inference round-trip; an hour is far more than enough.
const PRESIGN_EXPIRY_SECS: u32 = 3600;

/// Number of inference attempts per prompt when the model returns an HTTP error
/// or content that does not parse to a usable value.
const MAX_TRIES: u8 = 3;

/// Path appended to the configured OpenRouter base URL for chat completions.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Maximum number of prior measurements attached as context when a prompt opts
/// into [`AnalysisPrompt::include_previous_results`]. Bounds the prompt size so
/// a long-running sensor cannot grow the context without limit.
const MAX_PREVIOUS_RESULTS: i64 = 20;

/// System instruction for `numeric` prompts. Forces a single numeric reply so
/// the result is parseable as `f64` and storable in `measurements.value`.
const NUMERIC_SYSTEM_PROMPT: &str = "\
You are a vision model analyzing a camera image. Answer the user's question with a SINGLE \
numeric value only. Output nothing except that number — no units, no prose, no markdown, \
no explanation. The number must be parseable as a floating-point value. Use a plain decimal \
representation (e.g. `0.73` or `42`).";

/// System instruction for `text` prompts. Asks for a single short value in the
/// exact format the user's prompt specifies — the user prompt, not this system
/// prompt, is responsible for dictating that format (e.g. "answer as `r,g,b`").
const TEXT_SYSTEM_PROMPT: &str = "\
You are a vision model analyzing a camera image. Answer the user's question with a SINGLE \
short value in the exact format the user's prompt requests. Output only that value — no \
prose, no markdown, no explanation, no surrounding quotes.";

// ── OpenRouter request/response wire types ───────────────────────────────────

#[derive(Serialize)]
struct OpenRouterRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: MessageContent<'a>,
}

/// `system` messages carry a plain string; `user` messages carry the prompt text
/// plus the image URL as multimodal content blocks.
#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent<'a> {
    Text(&'a str),
    Parts(Vec<ContentPart<'a>>),
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum ContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
struct ImageUrl<'a> {
    url: &'a str,
    detail: &'static str,
}

#[derive(Deserialize)]
struct OpenRouterResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// Maximum number of OpenRouter inferences allowed to run at once. Bounds
/// outbound connections so a large `prompts` array (or several concurrent
/// uploads) cannot exhaust the connection pool or trip OpenRouter rate limits
/// — which would then each be retried, compounding the load.
const INFERENCE_CONCURRENCY: usize = 4;

/// Run every prompt against the image at `image_url`, returning one result per
/// prompt in the input order. Each prompt is inferred independently, with at
/// most [`INFERENCE_CONCURRENCY`] in flight at once; failures (after retries)
/// and DB write errors are reported per-prompt and never abort the batch.
///
/// `key` is the OpenRouter API key. The caller gates on it being `Some` and
/// never invokes this function without one, so — unlike the previous shape —
/// there is no `None` arm here: the invariant is encoded in the `&str` type.
pub(crate) async fn run_analyses(
    state: &crate::handlers::AppState,
    key: &str,
    camera_id: &str,
    captured_at: chrono::DateTime<chrono::Utc>,
    image_url: String,
    prompts: Vec<AnalysisPrompt>,
) {
    // `buffer_unordered` caps in-flight inferences at INFERENCE_CONCURRENCY
    // while still completing as fast as the model allows. The index is carried
    // through so results can be sorted back into input order before storage.
    stream::iter(prompts)
        .map(|prompt| {
            let image_url = image_url.clone();
            let key = key.to_string();
            let base_url = state.openrouter_base_url.clone();
            let camera_id = camera_id.to_string();
            let prompt_cloned = prompt.clone();
            async move {
                let previous_results = if prompt.include_previous_results {
                    previous_results_text(&state.pool, &camera_id, &prompt, captured_at).await?
                } else {
                    None
                };
                let value = infer_with_retries(
                    &state.http,
                    &key,
                    &base_url,
                    &prompt.model,
                    &image_url,
                    &prompt.prompt_text,
                    previous_results.as_deref(),
                    prompt.response_type,
                )
                .await?;
                store_measurement(state, &camera_id, &prompt, value, captured_at).await?;
                Ok(())
            }
            .map(|res: Result<(), anyhow::Error>| (prompt_cloned, res))
        })
        .buffer_unordered(INFERENCE_CONCURRENCY)
        .for_each(async |(prompt, res)| match &res {
            Err(e) => tracing::warn!(
                prompt_id = %prompt.prompt_id,
                camera_id,
                prompt_id = %prompt.prompt_id,
                captured_at = %captured_at,
                error = ?e,
                "image analysis failed",
            ),
            Ok(()) => tracing::info!(
                prompt_id = %prompt.prompt_id,
                "image analysis stored",
            ),
        })
        .await;
}

/// The `external_id` of the per-`(camera, prompt)` sensor that an analysis
/// measurement is stored under. Centralized so the write path and the
/// prior-results read path share one definition — a change to the scheme can
/// never silently make the two target different sensors.
fn sensor_external_id(camera_id: &str, prompt_id: &str) -> String {
    format!("{camera_id}_{prompt_id}")
}

/// Upsert the `(camera_id, prompt_id)` sensor and insert the inferred value as a
/// single measurement at the image's capture time. Reuses the shared DB helpers
/// so no analysis-specific SQL is maintained here.
///
/// The shared DB helpers return [`ApiError`] (`(StatusCode, String)`) so the HTTP
/// handlers can map a failure to the right response code; this background-task
/// caller has no response to build, so it folds the status and message into an
/// [`anyhow::Error`] — the status code is preserved as text in the message
/// rather than discarded.
async fn store_measurement(
    state: &crate::handlers::AppState,
    camera_id: &str,
    prompt: &AnalysisPrompt,
    value: MeasurementValue,
    captured_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let external_id = sensor_external_id(camera_id, &prompt.prompt_id);
    let sensor_id = upsert_sensor_db(
        &state.pool,
        &external_id,
        IMAGE_ANALYSIS_PROVIDER,
        None,
        prompt.measurement_unit.as_deref(),
        None,
        None,
        prompt.response_type.as_db_str(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("upsert sensor failed: {} {}", e.0, e.1))?;

    let measurements = [Measurement {
        value,
        measured_at: captured_at,
    }];
    let inserted = insert_measurements_db(&state.pool, sensor_id, &measurements)
        .await
        .map_err(|e| anyhow::anyhow!("insert measurements failed: {} {}", e.0, e.1))?;

    if inserted == 0 {
        anyhow::bail!(
            "image analysis measurement not stored: a row for this sensor at captured_at already exists"
        );
    }

    Ok(())
}

/// One prior measurement row decoded straight out of the [`query_as!`] query
/// below. `value` / `value_text` are nullable (exactly one is set per row); the
/// macro infers `Option` from the column nullability, so no manual nullability
/// annotations are needed.
struct PreviousResult {
    value: Option<f64>,
    value_text: Option<String>,
    measured_at: chrono::DateTime<chrono::Utc>,
}

/// Build the prior-results context block for a prompt that opts into
/// [`AnalysisPrompt::include_previous_results`].
///
/// Queries the most recent [`MAX_PREVIOUS_RESULTS`] measurements of the
/// `{camera_id}_{prompt_id}` sensor taken *before* the current image
/// (`measured_at < captured_at`), and formats them as a text content part:
/// each prior value with its measurement time, oldest → newest, followed by
/// the current analysis datetime. The compile-time-checked `query_as!` macro
/// decodes each row into [`PreviousResult`] by column name — no manual
/// `try_get`.
///
/// Returns `Ok(None)` when the sensor has no prior data yet (first run). A DB
/// error is returned as `Err` so the caller fails the prompt rather than
/// silently inferring without context — a context-free outlier would persist
/// and pollute the series this feature exists to keep monotonic.
async fn previous_results_text(
    pool: &PgPool,
    camera_id: &str,
    prompt: &AnalysisPrompt,
    captured_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<String>> {
    let external_id = sensor_external_id(camera_id, &prompt.prompt_id);
    let mut rows = sqlx::query_as!(
        PreviousResult,
        r#"
        SELECT m.value, m.value_text, m.measured_at
        FROM measurements m
        JOIN sensors s ON s.sensor_id = m.sensor_id
        WHERE s.external_id = $1 AND s.provider = $2 AND m.measured_at < $3
        ORDER BY m.measured_at DESC
        LIMIT $4
        "#,
        external_id,
        IMAGE_ANALYSIS_PROVIDER,
        captured_at,
        MAX_PREVIOUS_RESULTS
    )
    .fetch_all(pool)
    .await
    .context("failed to fetch previous results")?;

    if rows.is_empty() {
        return Ok(None); // first run — no history to attach
    }

    // Fetched most-recent first; reverse so the block reads chronologically
    // (oldest → newest), which is how a model reasons over a time series.
    rows.reverse();

    let mut out = String::from("Previous image analysis results, oldest first:");
    for row in &rows {
        let value = match (row.value, row.value_text.as_deref()) {
            (Some(n), _) => n.to_string(),
            (_, Some(t)) => t.to_string(),
            (None, None) => String::from("null"),
        };
        out.push_str(&format!("\n- {}: {value}", row.measured_at.to_rfc3339()));
    }
    // The current image's datetime — the measurement this inference will
    // produce, stored at `captured_at` — so the model sees where the new
    // reading sits in the timeline.
    out.push_str(&format!(
        "\nCurrent image datetime: {}",
        captured_at.to_rfc3339()
    ));
    Ok(Some(out))
}

/// Classification of an inference failure, used to decide whether to retry.
/// Transient errors (transport failure, 5xx, 429 rate limit, or content that
/// does not parse to a usable value) may succeed on a later attempt. Permanent
/// errors — 4xx other than 429 (bad/expired key, bad model slug, quota) — will
/// fail identically on every retry and surface immediately.
///
/// Each variant carries the underlying [`anyhow::Error`] so the original source
/// (a `reqwest::Error`, a parse failure, etc.) is preserved through the retry
/// loop and into the logged result rather than being flattened to a string.
enum InferError {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

/// Call OpenRouter up to [`MAX_TRIES`] times, retrying only on transient
/// errors (transport, 5xx, 429, or content that does not parse to a usable
/// value). Permanent errors surface on the first attempt without wasting
/// retries or backoff. Returns the parsed value or the last error.
#[allow(clippy::too_many_arguments)]
async fn infer_with_retries(
    http: &Client,
    key: &str,
    base_url: &str,
    model: &str,
    image_url: &str,
    prompt_text: &str,
    previous_results: Option<&str>,
    response_type: ResponseType,
) -> anyhow::Result<MeasurementValue> {
    let mut last_err: anyhow::Error = anyhow::anyhow!("no attempts made");
    for attempt in 1..=MAX_TRIES {
        match infer_once(
            http,
            key,
            base_url,
            model,
            image_url,
            prompt_text,
            previous_results,
            response_type,
        )
        .await
        {
            Ok(v) => return Ok(v),
            // A permanent error (e.g. 401 bad key, 400/404 bad model slug) will
            // not change across retries — surface it now instead of sleeping
            // through the remaining attempts.
            Err(InferError::Permanent(err)) => return Err(err),
            Err(InferError::Transient(err)) => {
                tracing::warn!(attempt, error = ?err, "image analysis inference failed, retrying");
                last_err = err;
                if attempt < MAX_TRIES {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// One OpenRouter inference attempt. Returns the parsed [`MeasurementValue`] or
/// a classified [`InferError`].
///
/// For `numeric` prompts the reply must parse as a finite `f64`; for `text`
/// prompts any non-empty trimmed reply is accepted (the user's prompt
/// dictates the format).
#[allow(clippy::too_many_arguments)]
async fn infer_once(
    http: &Client,
    key: &str,
    base_url: &str,
    model: &str,
    image_url: &str,
    prompt_text: &str,
    previous_results: Option<&str>,
    response_type: ResponseType,
) -> Result<MeasurementValue, InferError> {
    let system_prompt = match response_type {
        ResponseType::Numeric => NUMERIC_SYSTEM_PROMPT,
        ResponseType::Text => TEXT_SYSTEM_PROMPT,
    };
    let body = OpenRouterRequest {
        model,
        messages: vec![
            Message {
                role: "system",
                content: MessageContent::Text(system_prompt),
            },
            Message {
                role: "user",
                content: MessageContent::Parts({
                    let mut parts = vec![
                        ContentPart::Text { text: prompt_text },
                        ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: image_url,
                                detail: "original",
                            },
                        },
                    ];
                    if let Some(context) = previous_results {
                        parts.push(ContentPart::Text { text: context });
                    }
                    parts
                }),
            },
        ],
    };

    let url = format!("{base_url}{CHAT_COMPLETIONS_PATH}");
    let resp = http
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("request failed")
        .map_err(InferError::Transient)?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let err = anyhow::anyhow!("openrouter returned {status}: {text}");
        // 429 (rate limit) and any 5xx are transient; any other 4xx (401 bad
        // key, 400/404 bad model slug, quota) is permanent — retrying won't
        // change the response.
        return Err(if status.as_u16() == 429 || status.is_server_error() {
            InferError::Transient(err)
        } else {
            InferError::Permanent(err)
        });
    }

    let parsed: OpenRouterResponse = resp
        .json()
        .await
        .context("failed to decode response")
        .map_err(InferError::Transient)?;

    let content = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| InferError::Transient(anyhow::anyhow!("response had no choices")))?;

    parse_reply(content, response_type).map_err(InferError::Transient)
}

/// Turn the model's raw reply into a storable [`MeasurementValue`] according to
/// the prompt's [`ResponseType`]. `numeric` requires a parseable, *finite*
/// `f64` — `str::parse` accepts `"NaN"`/`"inf"`/`"-inf"` as valid `f64`, so a
/// non-finite parse is rejected explicitly to avoid persisting a value that
/// would poison Postgres aggregates (`AVG`/`SUM`/`MAX`/`MIN` all return NaN)
/// and that round-trips as JSON `null`. `text` accepts any non-empty trimmed
/// string.
fn parse_reply(content: String, response_type: ResponseType) -> anyhow::Result<MeasurementValue> {
    let trimmed = content.trim();
    match response_type {
        ResponseType::Numeric => {
            let n: f64 = trimmed
                .parse()
                .with_context(|| format!("model reply is not a number: {trimmed:?}"))?;
            if !n.is_finite() {
                anyhow::bail!(
                    "model reply is not a finite number: {trimmed:?} (NaN/Infinity rejected)"
                );
            }
            Ok(MeasurementValue::Number(n))
        }
        ResponseType::Text => {
            if trimmed.is_empty() {
                Err(anyhow::anyhow!("model reply was empty"))
            } else {
                Ok(MeasurementValue::Text(trimmed.to_string()))
            }
        }
    }
}

/// Presign an S3 GET URL for the just-uploaded object so OpenRouter can fetch
/// the image without the server buffering it.
pub(crate) async fn presign_image_url(bucket: &s3::Bucket, key: &str) -> Result<String, ApiError> {
    bucket
        .presign_get(key, PRESIGN_EXPIRY_SECS, None)
        .await
        .map_err(super::err)
}

/// Stream a multipart `image` field straight into S3 without buffering it. The
/// field is a `Stream<Item = Result<Bytes, axum::Error>>`; its error is mapped to
/// `io::Error` and `StreamReader` adapts the `Stream<Bytes>` into the `AsyncRead`
/// that `put_object_stream` consumes — the same streaming pattern the raw-body
/// handler used, so the image never lives in memory as a single buffer.
pub(crate) async fn stream_field_to_s3(
    field: axum::extract::multipart::Field<'_>,
    bucket: &s3::Bucket,
    key: &str,
) -> Result<(), ApiError> {
    let mapped = field.map(|res| res.map_err(std::io::Error::other));
    let mut reader = tokio_util::io::StreamReader::new(mapped);
    bucket
        .put_object_stream(&mut reader, key)
        .await
        .map_err(super::err)?;
    Ok(())
}
