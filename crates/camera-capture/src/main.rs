//! Long-running camera capture loop.
//!
//! Runs on a Raspberry Pi on the same network as the cameras. Every
//! `interval_secs` it iterates the configured PTZ cameras and, for each
//! configured location, moves the camera (ONVIF preset / absolute move),
//! waits for it to settle, grabs a still snapshot, and uploads it to the cloud
//! `server` via `POST /cameras/images`.
//!
//! Every error is isolated at the camera/location boundary — a single failure
//! never aborts the loop, mirroring `crates/scraper/src/scraper.rs`.

mod configuration;
mod onvif_client;
mod snapshot;
mod uploader;

use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use onvif::soap::client::Credentials;
use tokio::time::sleep;
use tracing::{Level, error, info, warn};

use crate::configuration::{CameraConfig, Configuration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
        )
        .init();

    let config = Configuration::load().context("Failed to load configuration")?;
    info!(
        interval_secs = config.interval_secs,
        cameras = config.cameras.len(),
        "starting camera capture loop"
    );

    // One client for snapshot fetches and uploads. rustls so it runs on a Pi
    // (no system OpenSSL) and can still reach the cloud server over HTTPS.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let interval = Duration::from_secs(config.interval_secs);

    loop {
        let tick = Instant::now();
        for cam in &config.cameras {
            if let Err(e) = capture_camera(&http, &config, cam).await {
                error!(camera = %cam.uri, "camera capture failed: {e:?}");
            }
        }

        let elapsed = tick.elapsed();
        if elapsed < interval {
            sleep(interval - elapsed).await;
        } else {
            warn!(
                elapsed_secs = elapsed.as_secs(),
                "tick overran its interval; continuing immediately"
            );
        }
    }
}

/// Discover one camera, then capture every configured location on it.
async fn capture_camera(
    http: &reqwest::Client,
    config: &Configuration,
    cam: &CameraConfig,
) -> anyhow::Result<()> {
    // Discovery is cheap (a couple of SOAP calls) and re-runs each tick, so a
    // camera that reboots or was down at startup self-heals on later ticks.
    let clients = onvif_client::discover(cam)
        .await
        .context("ONVIF discovery failed")?;
    let creds = Credentials {
        username: cam.username.clone(),
        password: cam.password.clone(),
    };
    let settle = Duration::from_secs_f64(cam.settle_secs);

    for loc in &cam.locations {
        if let Err(e) = capture_location(http, config, &clients, &creds, loc, settle).await {
            error!(
                camera = %cam.uri,
                camera_id = %loc.camera_id,
                "location capture failed: {e:?}"
            );
        }
    }
    Ok(())
}

/// Move → settle → snapshot → upload for a single location.
async fn capture_location(
    http: &reqwest::Client,
    config: &Configuration,
    clients: &onvif_client::CameraClients,
    creds: &Credentials,
    loc: &crate::configuration::LocationConfig,
    settle: Duration,
) -> anyhow::Result<()> {
    let target = loc.target()?;

    clients.move_to(&target).await?;
    sleep(settle).await;

    let uri = clients.snapshot_uri().await?;
    let bytes = snapshot::fetch_snapshot(http, &uri, Some(creds)).await?;
    // Cameras usually return JPEG; the server stores under a `.png` key, so
    // re-encode to PNG unless the snapshot is already one.
    let bytes = snapshot::ensure_png(bytes).context("normalizing snapshot to PNG failed")?;

    let captured_at = Utc::now().timestamp();
    uploader::upload_image(
        http,
        &config.server_url,
        &config.auth_token,
        &loc.camera_id,
        captured_at,
        bytes,
    )
    .await
    .context("upload failed")?;

    info!(
        camera_id = %loc.camera_id,
        captured_at,
        "captured and uploaded image"
    );
    Ok(())
}
