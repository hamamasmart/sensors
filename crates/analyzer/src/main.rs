//! Analyzer Lambda entry point.
//!
//! This crate is DB-less (like the scraper): the server async-invokes one
//! instance per analysis shard (one camera), passing a `ShardSpec` in the
//! invoke payload. The shard lists that camera's images from S3, sends them to
//! OpenRouter in batches, and writes one measurement per image back to the
//! server over HTTP. Locally, run with a `ShardSpec` JSON in `SHARD_SPEC`.

mod analyzer;
mod configuration;
mod openrouter;
mod s3_images;
mod server;
mod system_prompt;

use anyhow::Context;
use chrono::{DateTime, Utc};
use lambda_runtime::{Error, LambdaEvent, service_fn};
use tracing_subscriber::EnvFilter;

use api_types::ShardSpec;

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
        run_local().await?;
    }

    Ok(())
}

async fn function_handler(event: LambdaEvent<ShardSpec>) -> Result<(), Error> {
    let spec = event.payload;
    let deadline = DateTime::<Utc>::from_timestamp_millis(i64::try_from(event.context.deadline).unwrap_or(i64::MAX));
    let config = configuration::Configuration::from_env().context("Failed to load configuration")?;
    analyzer::run_shard(&config, &spec, deadline)
        .await
        .context("Analyze shard failed")?;
    Ok(())
}

async fn run_local() -> anyhow::Result<()> {
    let spec_json =
        std::env::var("SHARD_SPEC").context("SHARD_SPEC (a JSON ShardSpec) is not set")?;
    let spec: ShardSpec =
        serde_json::from_str(&spec_json).context("Failed to parse SHARD_SPEC")?;
    let config = configuration::Configuration::from_env().context("Failed to load configuration")?;
    analyzer::run_shard(&config, &spec, None)
        .await
        .context("Analyze shard failed")?;
    Ok(())
}