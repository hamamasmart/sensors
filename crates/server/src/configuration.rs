use anyhow::Context;

pub struct Configuration {
    pub database_url: String,
    /// Bind address for the HTTP server. The Lightsail load balancer proxies to
    /// the container on 8080, so the deploy sets this to `0.0.0.0:8080`.
    pub bind_addr: String,
    /// Shared secret that callers must present as `Authorization: Bearer <token>`
    /// to reach any route. Required so routes are never exposed unauthenticated.
    pub auth_token: String,
    /// OpenRouter API key used for image-analysis inference.
    pub openrouter_api_key: String,
    /// OpenRouter API base URL. Defaults to the public endpoint; overridable to
    /// point at an OpenRouter-compatible gateway or a local stub.
    pub openrouter_base_url: String,
}

impl Configuration {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
        let bind_addr =
            std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let auth_token =
            std::env::var("AUTH_TOKEN").context("AUTH_TOKEN is not set")?;
        // An empty `OPENROUTER_API_KEY=` is rejected the same as unset so a
        // misconfigured deploy fails to start rather than silently accepting
        // prompt-bearing uploads it can't analyze.
        let openrouter_api_key = std::env::var("OPENROUTER_API_KEY")
            .context("OPENROUTER_API_KEY is not set")?;
        anyhow::ensure!(
            !openrouter_api_key.trim().is_empty(),
            "OPENROUTER_API_KEY is set but empty"
        );
        let openrouter_base_url = std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());

        Ok(Self {
            database_url,
            bind_addr,
            auth_token,
            openrouter_api_key,
            openrouter_base_url,
        })
    }
}
