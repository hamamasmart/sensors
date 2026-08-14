//! Upload a captured image to the cloud `server` via `POST /cameras/images`.
//!
//! The image is sent as a `multipart/form-data` `image` part; the server
//! streams that part straight into S3 keyed `<camera_id>/YYYY/MM/DD/HH/MM_SS.png`
//! (see `crates/server/src/handlers.rs`). Any per-location analysis prompts are
//! sent alongside as a `prompts` JSON part, which the server fans out to the
//! vision LLM after the image lands in S3. We send the snapshot bytes with the
//! camera credentials' shared bearer token; the server keys off `camera_id`
//! and `captured_at`.

use anyhow::Context;
use url::Url;

use api_types::AnalysisPrompt;

/// `captured_at` is the Unix epoch (seconds) the image was taken. `prompts`
/// is the location's configured prompts; when empty no `prompts` part is sent
/// and the image is stored without analysis.
pub async fn upload_image(
    http: &reqwest::Client,
    server_url: &str,
    auth_token: &str,
    camera_id: &str,
    captured_at: i64,
    bytes: Vec<u8>,
    prompts: &[AnalysisPrompt],
) -> anyhow::Result<()> {
    // Build the query string manually so we don't depend on reqwest's
    // feature-gated `.query()`. `query_pairs_mut` percent-encodes the values.
    let mut url =
        Url::parse(&format!("{server_url}/cameras/images")).context("invalid server_url")?;
    let captured_at_str = captured_at.to_string();
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("camera_id", camera_id);
        q.append_pair("captured_at", &captured_at_str);
    }

    let form = reqwest::multipart::Form::new().part(
        "image",
        reqwest::multipart::Part::bytes(bytes)
            .file_name("image.png")
            .mime_str("image/png")
            .context("invalid mime type")?,
    );

    // The `prompts` part is a JSON array of `AnalysisPrompt` — the exact shape
    // `POST /cameras/images` parses. Only attach it when there's something to
    // analyze, so a location with no prompts behaves exactly as before.
    let form = if prompts.is_empty() {
        form
    } else {
        let prompts_json = serde_json::to_vec(prompts).context("failed to serialize prompts")?;
        form.part(
            "prompts",
            reqwest::multipart::Part::bytes(prompts_json)
                .file_name("prompts.json")
                .mime_str("application/json")
                .context("invalid mime type")?,
        )
    };

    let resp = http
        .post(url.as_str())
        .bearer_auth(auth_token)
        .multipart(form)
        .send()
        .await
        .context("failed to send image to server")?;

    let status = resp.status();
    if status.is_success() {
        // Drain the body so the connection can be reused by the next upload.
        let _ = resp.bytes().await;
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("server rejected image: HTTP {status} {body}")
    }
}
