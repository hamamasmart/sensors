mod auth;
mod configuration;
mod handlers;

use crate::configuration::Configuration;
use anyhow::Context;
use axum::{
    Router,
    http::StatusCode,
    middleware::from_fn_with_state,
    routing::{get, post},
};
use s3::BucketConfiguration;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

/// Hardcoded S3 bucket that camera images are uploaded into. Created on first
/// run if it does not yet exist, so no out-of-band `aws s3api create-bucket` step
/// is required.
const S3_BUCKET_NAME: &str = "hamama-camera-images";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let config = Configuration::from_env().context("Failed to load configuration")?;

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .context("Failed to connect to database")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("Failed to run migrations")?;

    let bucket = ensure_s3_bucket(S3_BUCKET_NAME)
        .await
        .context("Failed to set up S3 bucket")?;

    let state = handlers::AppState {
        pool,
        s3: bucket,
    };

    // Auth-guarded API routes. Every write goes through the bearer-token
    // middleware so the (publicly reachable) endpoint is never unauthenticated
    // at the app layer.
    let api = Router::new()
        .route("/sensors", post(handlers::upsert_sensor))
        .route(
            "/sensors/{sensor_id}/measurements",
            post(handlers::insert_measurements),
        )
        .route("/cameras/images", post(handlers::upload_camera_image))
        .layer(from_fn_with_state(
            auth::ExpectedToken(config.auth_token),
            auth::require_bearer_token,
        ))
        .with_state(state);

    // Unauthenticated liveness probe for the Lightsail load balancer's health
    // check (GET only, returns nothing sensitive). Kept on a separate router
    // that's merged *outside* the auth layer so the LB can reach it without the
    // shared bearer token.
    let app = Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .merge(api);

    tracing::info!("Listening on http://{}", config.bind_addr);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .with_context(|| format!("Failed to bind {}", config.bind_addr))?;
    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}

/// Resolve the S3 region + credentials from the environment the same way the
/// AWS SDK does (standard env vars — `AWS_ACCESS_KEY_ID` /
/// `AWS_SECRET_ACCESS_KEY` / `AWS_REGION`, injected by the Lightsail deploy as
/// long-lived static creds from a dedicated IAM user), create `name` if it does
/// not yet exist, and return a handle for it.
///
/// The create is idempotent: HTTP 409 (bucket already exists) is treated as
/// success so subsequent cold starts don't fail. rust-s3 always sends a
/// `LocationConstraint` unless `RUST_S3_SKIP_LOCATION_CONSTRAINT` is set, but S3
/// rejects that constraint for us-east-1, so it is skipped only there — mirroring
/// the conditional the deploy script uses.
async fn ensure_s3_bucket(name: &str) -> anyhow::Result<s3::Bucket> {
    let region = s3::Region::from_default_env().context("Failed to load AWS region")?;
    let credentials =
        s3::creds::Credentials::default().context("Failed to load AWS credentials")?;

    if region == s3::region::Region::UsEast1 {
        // Safety: this runs during single-threaded init, before any other code
        // reads the environment.
        unsafe {
            std::env::set_var("RUST_S3_SKIP_LOCATION_CONSTRAINT", "true");
        }
    }

    match s3::Bucket::create(
        name,
        region.clone(),
        credentials.clone(),
        BucketConfiguration::private(),
    )
    .await
    {
        Ok(resp) => tracing::info!(
            bucket = name,
            status = resp.response_code,
            "ensured S3 bucket exists"
        ),
        // HTTP 409 — the bucket already exists (ours or someone else's); treat
        // as success so subsequent cold starts don't fail on the idempotent
        // create.
        Err(s3::error::S3Error::HttpFailWithBody(409, body)) => {
            tracing::info!(bucket = name, "S3 bucket already exists (409): {body}");
        }
        Err(e) => return Err(e).context("Failed to create S3 bucket"),
    }

    let bucket = s3::Bucket::new(name, region, credentials).context("Failed to construct S3 bucket")?;
    Ok(*bucket)
}
