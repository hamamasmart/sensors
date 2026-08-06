use anyhow::Context;

pub struct Configuration {
    /// Server base URL (the `server` Lambda Function URL, or `http://localhost:8080`
    /// locally) — the analyzer pushes sensors/measurements and job lifecycle here.
    pub server_url: String,
    /// Bearer token presented to the server (`Authorization: Bearer <token>`).
    pub server_auth_token: String,
    /// Bearer token for OpenRouter.
    pub openrouter_api_key: String,
    /// OpenRouter model slug.
    pub openrouter_model: String,
    /// S3 bucket the camera images live in (matches the server's hardcoded name).
    pub s3_bucket: String,
    /// Images per OpenRouter chat-completions call.
    pub batch_size: usize,
    /// Concurrent in-flight OpenRouter calls per shard.
    pub max_concurrent_batches: usize,
    /// Safety cap on images analyzed per shard (one camera). Over-cap → partial.
    pub max_images_per_shard: usize,
}

impl Configuration {
    pub fn from_env() -> anyhow::Result<Self> {
        let server_url =
            std::env::var("SERVER_URL").context("SERVER_URL is not set")?;
        let server_auth_token =
            std::env::var("AUTH_TOKEN").context("AUTH_TOKEN is not set")?;
        let openrouter_api_key =
            std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set")?;
        let openrouter_model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| "google/gemini-2.5-flash-lite".to_string());
        let s3_bucket = std::env::var("S3_BUCKET")
            .unwrap_or_else(|_| "phytech-camera-images".to_string());
        let batch_size = env_usize("BATCH_SIZE", 8);
        let max_concurrent_batches = env_usize("MAX_CONCURRENT_BATCHES", 3);
        let max_images_per_shard = env_usize("MAX_IMAGES_PER_SHARD", 1000);

        Ok(Self {
            server_url,
            server_auth_token,
            openrouter_api_key,
            openrouter_model,
            s3_bucket,
            batch_size,
            max_concurrent_batches,
            max_images_per_shard,
        })
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(default)
}