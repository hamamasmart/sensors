//! Upload a captured image to the cloud `server` via `POST /cameras/images`.
//!
//! The image is sent as a `multipart/form-data` `image` part; the server
//! streams that part straight into S3 keyed `<camera_id>/YYYY/MM/DD/HH/MM_SS.png`
//! (see `crates/server/src/handlers.rs`). We send the snapshot bytes with the
//! camera credentials' shared bearer token; the server keys off `camera_id`
//! and `captured_at`.

use anyhow::Context;
use url::Url;

/// `captured_at` is the Unix epoch (seconds) the image was taken.
pub async fn upload_image(
    http: &reqwest::Client,
    server_url: &str,
    auth_token: &str,
    camera_id: &str,
    captured_at: i64,
    bytes: Vec<u8>,
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
