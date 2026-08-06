//! Read-only access to the camera-image S3 bucket. Lists a camera's images in a
//! time range (by parsing the capture time out of the S3 key) and fetches the
//! raw PNG bytes — no decoding; the bytes go straight to OpenRouter as base64.

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use s3::Bucket;
use tracing::warn;

/// One image ready for analysis: its capture time (parsed from the S3 key) and
/// raw PNG bytes.
#[derive(Clone)]
pub struct Image {
    pub measured_at: DateTime<Utc>,
    pub bytes: Vec<u8>,
}

/// List `<camera_id>/` and fetch every image in `[starts_at, ends_at]`, sorted
/// by capture time (so batch `index` maps deterministically to a `measured_at`).
/// Returns the images and whether the `max` cap was hit (results may be partial).
pub async fn list_images(
    bucket: &Bucket,
    camera_id: &str,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    max: usize,
) -> anyhow::Result<(Vec<Image>, bool)> {
    let prefix = format!("{camera_id}/");
    let pages = bucket
        .list(prefix, None)
        .await
        .context("Failed to list camera images in S3")?;

    let mut keys: Vec<(String, DateTime<Utc>)> = Vec::new();
    for page in pages {
        for object in page.contents {
            let Some(measured_at) = parse_key_time(&object.key, camera_id) else {
                warn!(key = %object.key, "skipping unparseable image key");
                continue;
            };
            if measured_at < starts_at || measured_at > ends_at {
                continue;
            }
            keys.push((object.key, measured_at));
        }
    }

    let capped = keys.len() > max;
    keys.sort_by_key(|(_, t)| *t);
    keys.truncate(max);

    let mut images = Vec::with_capacity(keys.len());
    for (key, measured_at) in keys {
        let response = bucket
            .get_object(&key)
            .await
            .with_context(|| format!("Failed to fetch image {key} from S3"))?;
        images.push(Image {
            measured_at,
            bytes: response.to_vec(),
        });
    }

    Ok((images, capped))
}

/// S3 key layout is `<camera_id>/YYYY/MM/DD/HH/MM_SS.png` (UTC). Parse the
/// capture time out of the part after the camera prefix.
fn parse_key_time(key: &str, camera_id: &str) -> Option<DateTime<Utc>> {
    let rest = key.strip_prefix(&format!("{camera_id}/"))?.strip_suffix(".png")?;
    let ndt = NaiveDateTime::parse_from_str(rest, "%Y/%m/%d/%H/%M_%S").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
}