mod configuration;
mod scraper;

use crate::configuration::Configuration;
use anyhow::Context;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let _ = dotenvy::dotenv();

    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .with_ansi(false)
            .without_time()
            .init();

        tracing::info!("Running as an AWS Lambda function");
        lambda_runtime::run(service_fn(function_handler)).await?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .init();
        tracing::info!("Running locally outside of AWS Lambda");
        run_job().await?;
    }

    Ok(())
}

async fn function_handler(_event: LambdaEvent<Value>) -> Result<(), Error> {
    run_job().await?;
    Ok(())
}

async fn run_job() -> anyhow::Result<()> {
    tracing::info!("Starting phytech scrape job");
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

    scraper::run_scrape(&pool, &config.phytech_email, &config.phytech_password)
        .await
        .context("Scrape failed")?;

    tracing::info!("Scrape completed successfully");
    Ok(())
}
