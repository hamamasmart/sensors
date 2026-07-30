//! Fetch raw snapshot bytes from a camera's snapshot URI, and normalize them
//! to PNG.
//!
//! Snapshot URIs are plain HTTP on the LAN and are usually protected with HTTP
//! digest auth. We try unauthenticated first, then respond to a `401`
//! challenge with digest (or, as a fallback, Basic) auth. Cameras almost
//! always return JPEG from the snapshot URI, but the server keys the stored
//! object as `.png`, so `ensure_png` re-encodes anything that isn't already a
//! PNG.

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

/// PNG signature: `\x89PNG\r\n\x1a\n`.
const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Return `bytes` unchanged when it's already a PNG; otherwise decode it and
/// re-encode as PNG. The server stores every snapshot under a `.png` key, so
/// this keeps that label honest regardless of what format the camera returns
/// (typically JPEG).
pub fn ensure_png(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    if bytes.starts_with(&PNG_MAGIC) {
        return Ok(bytes);
    }
    let img = image::load_from_memory(&bytes)
        .context("snapshot is neither PNG nor a decodable image")?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .context("encoding snapshot as PNG failed")?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_is_passed_through_unchanged() {
        let img = image::DynamicImage::new_rgb8(2, 3);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        let original = buf.into_inner();

        let out = ensure_png(original.clone()).unwrap();
        assert_eq!(out, original, "a PNG must round-trip byte-for-byte");
    }

    #[test]
    fn jpeg_is_reencoded_as_png() {
        // Encode a small image as JPEG — what a camera snapshot URI typically
        // returns.
        let img = image::DynamicImage::new_rgb8(4, 4);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        let jpeg = buf.into_inner();

        assert!(!jpeg.starts_with(&PNG_MAGIC));
        let out = ensure_png(jpeg).unwrap();
        assert!(out.starts_with(&PNG_MAGIC), "output must be a PNG");
        assert!(image::load_from_memory(&out).is_ok(), "output must decode");
    }

    #[test]
    fn garbage_fails_instead_of_uploading() {
        let err = ensure_png(b"definitely not an image".to_vec()).unwrap_err();
        assert!(err.to_string().contains("neither PNG nor a decodable image"));
    }
}
