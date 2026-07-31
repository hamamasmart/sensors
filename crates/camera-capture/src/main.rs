//! Long-running camera capture loop.
//!
//! Runs on a Raspberry Pi on the same network as the cameras. Each configured
//! PTZ camera runs as its own task on its own cadence (`interval_secs`): for
//! every configured location it moves the camera (ONVIF preset / absolute
//! move), waits for it to settle, grabs a still snapshot, and uploads it to
//! the cloud `server` via `POST /cameras/images`. When a camera opts into
//! `daylight_only`, its ticks outside the sunrise→sunset window are skipped.
//!
//! Every error is isolated at the camera/location boundary — a single failure
//! never aborts that camera's loop, and one camera's failure never affects the
//! others, mirroring `crates/scraper/src/scraper.rs`.

mod configuration;
mod daylight;
mod onvif_client;
mod rtsp_snapshot;
mod uploader;

use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use onvif::soap::client::Credentials;
use sunrise::Coordinates;
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
    if config.cameras.is_empty() {
        warn!("no cameras configured; exiting");
        return Ok(());
    }
    info!(
        cameras = config.cameras.len(),
        "starting camera capture loop"
    );

    // One client for snapshot fetches and uploads. rustls so it runs on a Pi
    // (no system OpenSSL) and can still reach the cloud server over HTTPS.
    // Cheaply cloneable (internally Arc'd), so each camera task gets a handle.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    // Global site coordinates: shared by every camera that opts into daylight
    // gating. Required and range-validated in `Configuration::load`.
    let coords = Coordinates::new(config.latitude, config.longitude)
        .context("invalid latitude/longitude")?;

    // One concurrent task per camera so each can keep its own interval and
    // daylight window independently. A failure or panic in one task never
    // tears down the others.
    let server_url = config.server_url.clone();
    let auth_token = config.auth_token.clone();
    let mut handles = Vec::with_capacity(config.cameras.len());
    for cam in config.cameras {
        let http = http.clone();
        handles.push(tokio::spawn(camera_loop(
            http,
            server_url.clone(),
            auth_token.clone(),
            coords,
            cam,
        )));
    }

    // The loops run forever; awaiting keeps the process alive. A panicked task
    // surfaces as a JoinError here and is logged without taking down the rest.
    for handle in handles {
        if let Err(e) = handle.await {
            error!("camera loop task ended unexpectedly: {e:?}");
        }
    }
    Ok(())
}

/// Per-camera capture loop: daylight gate → move → settle → snapshot → upload,
/// sleeping `interval_secs` between ticks. Runs forever; every capture error is
/// isolated so the loop continues on the next tick.
async fn camera_loop(
    http: reqwest::Client,
    server_url: String,
    auth_token: String,
    coords: Coordinates,
    cam: CameraConfig,
) {
    let interval = Duration::from_secs(cam.interval_secs);
    // Tracks the daylight state across ticks so we only log on the day/night
    // transition instead of once per tick all night long.
    let mut dark = false;

    loop {
        let tick = Instant::now();

        // Skip the whole pass while it's dark: no ONVIF moves, no snapshots,
        // no uploads. We still honour `interval_secs`, so the loop wakes up
        // every tick and self-corrects as the sun crosses the horizon.
        if daylight::is_daylight(cam.daylight_only, cam.daylight_margin_mins, coords, Utc::now()) {
            if dark {
                info!(camera = %cam.uri, "resuming capture: entered daylight hours");
                dark = false;
            }
            if let Err(e) = capture_camera(&http, &server_url, &auth_token, &cam).await {
                error!(camera = %cam.uri, "camera capture failed: {e:?}");
            }
        } else if !dark {
            info!(camera = %cam.uri, "skipping capture: outside daylight hours");
            dark = true;
        }

        let elapsed = tick.elapsed();
        if elapsed < interval {
            sleep(interval - elapsed).await;
        } else {
            warn!(
                camera = %cam.uri,
                elapsed_secs = elapsed.as_secs(),
                "tick overran its interval; continuing immediately"
            );
        }
    }
}

/// Discover one camera, then capture every configured location on it.
async fn capture_camera(
    http: &reqwest::Client,
    server_url: &str,
    auth_token: &str,
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
        if let Err(e) =
            capture_location(http, server_url, auth_token, &clients, &creds, loc, settle).await
        {
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
    server_url: &str,
    auth_token: &str,
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
    let png = tokio::task::spawn_blocking(move || rtsp_snapshot::capture_frame_png(&rtsp_url_clone))
        .await
        .context("spawn_blocking failed")?
        .context("RTSP frame capture failed")?;

    let captured_at = Utc::now().timestamp();
    uploader::upload_image(
        http,
        server_url,
        auth_token,
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
