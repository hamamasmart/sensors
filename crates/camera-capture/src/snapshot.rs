//! Fetch raw snapshot bytes from a camera's snapshot URI.
//!
//! Snapshot URIs are plain HTTP on the LAN and are usually protected with HTTP
//! digest auth. We try unauthenticated first, then respond to a `401`
//! challenge with digest (or, as a fallback, Basic) auth.

use anyhow::Context;
use url::Url;

use onvif::soap::client::Credentials;

/// GET `uri`, authenticating only if the camera challenges with `401`.
pub async fn fetch_snapshot(
    http: &reqwest::Client,
    uri: &str,
    creds: Option<&Credentials>,
) -> anyhow::Result<Vec<u8>> {
    let resp = http
        .get(uri)
        .send()
        .await
        .with_context(|| format!("snapshot GET failed for `{uri}`"))?;
    if resp.status() == reqwest::StatusCode::OK {
        return Ok(resp
            .bytes()
            .await
            .context("reading snapshot body failed")?
            .to_vec());
    }

    // Non-OK: only retry with credentials if the camera actually challenged.
    let challenge = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .context("snapshot returned a non-OK status with no auth challenge")?;
    let creds = creds.context("snapshot requires auth but no camera credentials configured")?;

    let url = Url::parse(uri).with_context(|| format!("invalid snapshot uri `{uri}`"))?;
    let auth_header = if challenge.to_ascii_lowercase().contains("digest") {
        digest_header(challenge, creds, &url)?
    } else {
        basic_header(creds)
    };

    let resp = http
        .get(uri)
        .header(reqwest::header::AUTHORIZATION, auth_header)
        .send()
        .await
        .context("authenticated snapshot GET failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("snapshot fetch failed: HTTP {}", resp.status());
    }
    Ok(resp
        .bytes()
        .await
        .context("reading snapshot body failed")?
        .to_vec())
}

fn digest_header(challenge: &str, creds: &Credentials, url: &Url) -> anyhow::Result<String> {
    let mut ctx = digest_auth::AuthContext::new(&creds.username, &creds.password, url.path());
    ctx.method = digest_auth::HttpMethod::GET;
    Ok(digest_auth::parse(challenge)
        .context("failed to parse digest challenge")?
        .respond(&ctx)
        .context("failed to compute digest response")?
        .to_string())
}

fn basic_header(creds: &Credentials) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded = STANDARD.encode(format!("{}:{}", creds.username, creds.password));
    format!("Basic {encoded}")
}
