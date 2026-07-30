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
mod rtsp_snapshot;
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

    // Build an authenticated RTSP URL for ffmpeg.
    // ffmpeg expects: rtsp://user:pass@host/path
    let stream_uri = clients.stream_uri().await?;
    let rtsp_url = inject_credentials(&stream_uri, &creds.username, &creds.password)?;

    let rtsp_url_clone = rtsp_url.clone();
    let png = tokio::task::spawn_blocking(move || {
        rtsp_snapshot::capture_frame_png(&rtsp_url_clone)
    })
    .await
    .context("spawn_blocking failed")?
    .context("RTSP frame capture failed")?;

    let captured_at = Utc::now().timestamp();
    uploader::upload_image(
        http,
        &config.server_url,
        &config.auth_token,
        &loc.camera_id,
        captured_at,
        png,
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

/// Inject `user:pass@` into an RTSP URL so ffmpeg can authenticate.
fn inject_credentials(url: &str, user: &str, pass: &str) -> anyhow::Result<String> {
    // rtsp://host:port/path → rtsp://user:pass@host:port/path
    let parts: Vec<&str> = url.splitn(2, "://").collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid RTSP URL: {url}");
    }
    let scheme = parts[0];
    let rest = parts[1];
    Ok(format!("{scheme}://{user}:{pass}@{rest}"))
}
