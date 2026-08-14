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

use api_types::{AnalysisPrompt, AnalysisResult, Measurement, MeasurementValue, ResponseType};
use futures::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

/// Run every prompt against the image at `image_url`, returning one result per
/// prompt in the input order. Each prompt is inferred independently and
/// concurrently; failures (after retries) and DB write errors are reported
/// per-prompt and never abort the batch.
pub(crate) async fn run_analyses(
    state: &crate::handlers::AppState,
    camera_id: &str,
    captured_at: chrono::DateTime<chrono::Utc>,
    image_url: String,
    prompts: Vec<AnalysisPrompt>,
) -> Vec<AnalysisResult> {
    let key = match &state.openrouter_api_key {
        Some(k) => k.clone(),
        None => {
            return prompts
                .into_iter()
                .map(|p| AnalysisResult {
                    prompt_id: p.prompt_id,
                    value: None,
                    error: Some("OPENROUTER_API_KEY is not configured".to_string()),
                })
                .collect();
        }
    };

    let results = join_all(prompts.into_iter().map(|prompt| {
        let image_url = image_url.clone();
        let key = key.clone();
        let base_url = state.openrouter_base_url.clone();
        async move {
            let value = infer_with_retries(
                &state.http,
                &key,
                &base_url,
                &prompt.model,
                &image_url,
                &prompt.prompt_text,
                prompt.response_type,
            )
            .await;
            (prompt, value)
        }
    }))
    .await;

    // Store each successful inference as a measurement for the
    // `{camera_id}_{prompt_id}` sensor. DB failures are non-fatal and surfaced
    // as the per-prompt `error` rather than aborting the response.
    let mut analyses = Vec::with_capacity(results.len());
    for (prompt, value) in results {
        let result = match value {
            Ok(v) => match store_measurement(state, camera_id, &prompt, v.clone(), captured_at).await {
                Ok(()) => AnalysisResult {
                    prompt_id: prompt.prompt_id,
                    value: Some(v),
                    error: None,
                },
                Err(e) => AnalysisResult {
                    prompt_id: prompt.prompt_id,
                    value: None,
                    error: Some(e),
                },
            },
            Err(e) => AnalysisResult {
                prompt_id: prompt.prompt_id,
                value: None,
                error: Some(e),
            },
        };
        analyses.push(result);
    }
    analyses
}

/// Upsert the `(camera_id, prompt_id)` sensor and insert the inferred value as a
/// single measurement at the image's capture time. Reuses the shared DB helpers
/// so no analysis-specific SQL is maintained here.
async fn store_measurement(
    state: &crate::handlers::AppState,
    camera_id: &str,
    prompt: &AnalysisPrompt,
    value: MeasurementValue,
    captured_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let external_id = format!("{camera_id}_{}", prompt.prompt_id);
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
    .map_err(|e| e.1)?;

    let measurements = [Measurement {
        value,
        measured_at: captured_at,
    }];
    insert_measurements_db(&state.pool, sensor_id, &measurements)
        .await
        .map_err(|e| e.1)?;

    Ok(())
}

/// Call OpenRouter up to [`MAX_TRIES`] times, retrying on HTTP errors or content
/// that does not parse to a usable value. Returns the parsed value or the last
/// error.
async fn infer_with_retries(
    http: &Client,
    key: &str,
    base_url: &str,
    model: &str,
    image_url: &str,
    prompt_text: &str,
    response_type: ResponseType,
) -> Result<MeasurementValue, String> {
    let mut last_err = String::from("no attempts made");
    for attempt in 1..=MAX_TRIES {
        match infer_once(http, key, base_url, model, image_url, prompt_text, response_type).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "image analysis inference failed, retrying");
                last_err = e;
                if attempt < MAX_TRIES {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// One OpenRouter inference attempt. Returns the parsed [`MeasurementValue`] or
/// an error describing why the call or parse failed.
///
/// For `numeric` prompts the reply must parse as `f64`; for `text` prompts any
/// non-empty trimmed reply is accepted (the user's prompt dictates the format).
async fn infer_once(
    http: &Client,
    key: &str,
    base_url: &str,
    model: &str,
    image_url: &str,
    prompt_text: &str,
    response_type: ResponseType,
) -> Result<MeasurementValue, String> {
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
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: prompt_text },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: image_url },
                    },
                ]),
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
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openrouter returned {status}: {text}"));
    }

    let parsed: OpenRouterResponse = resp
        .json()
        .await
        .map_err(|e| format!("failed to decode response: {e}"))?;

    let content = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| "response had no choices".to_string())?;

    parse_reply(content, response_type)
}

/// Turn the model's raw reply into a storable [`MeasurementValue`] according to
/// the prompt's [`ResponseType`]. `numeric` requires a parseable `f64`; `text`
/// accepts any non-empty trimmed string.
fn parse_reply(content: String, response_type: ResponseType) -> Result<MeasurementValue, String> {
    let trimmed = content.trim();
    match response_type {
        ResponseType::Numeric => trimmed
            .parse::<f64>()
            .map(MeasurementValue::Number)
            .map_err(|_| format!("model reply is not a number: {trimmed:?}")),
        ResponseType::Text => {
            if trimmed.is_empty() {
                Err("model reply was empty".to_string())
            } else {
                Ok(MeasurementValue::Text(trimmed.to_string()))
            }
        }
    }
}

/// Presign an S3 GET URL for the just-uploaded object so OpenRouter can fetch
/// the image without the server buffering it.
pub(crate) async fn presign_image_url(
    bucket: &s3::Bucket,
    key: &str,
) -> Result<String, ApiError> {
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
    use futures::StreamExt;
    let mapped = field.map(|res| res.map_err(std::io::Error::other));
    let mut reader = tokio_util::io::StreamReader::new(mapped);
    bucket
        .put_object_stream(&mut reader, key)
        .await
        .map_err(super::err)?;
    Ok(())
}