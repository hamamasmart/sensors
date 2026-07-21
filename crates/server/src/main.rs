mod auth;
mod configuration;
mod handlers;

use crate::configuration::Configuration;
use anyhow::Context;
use axum::{Router, middleware::from_fn_with_state, routing::post};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

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

    let app = Router::new()
        .route("/sensors", post(handlers::upsert_sensor))
        .route(
            "/sensors/{sensor_id}/measurements",
            post(handlers::insert_measurements),
        )
        .layer(from_fn_with_state(
            auth::ExpectedToken(config.auth_token),
            auth::require_bearer_token,
        ))
        .with_state(pool);

    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
        // Keep API Gateway stage names out of the path so routing matches.
        // Safety: this runs before any other thread reads the env; the Lambda runtime
        // has not started polling yet.
        unsafe {
            std::env::set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
        }
        tracing::info!("Running as an AWS Lambda HTTP function");
        lambda_http::run(app)
            .await
            .map_err(|e| anyhow::anyhow!("lambda_http runtime error: {e:?}"))?;
    } else {
        tracing::info!("Running locally on http://{}", config.bind_addr);
        let listener = tokio::net::TcpListener::bind(&config.bind_addr)
            .await
            .with_context(|| format!("Failed to bind {}", config.bind_addr))?;
        axum::serve(listener, app)
            .await
            .context("Local server error")?;
    }

    Ok(())
}
