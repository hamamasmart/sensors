use anyhow::Context;

pub struct Configuration {
    pub database_url: String,
    /// Bind address for the local (non-Lambda) HTTP mode. Ignored on Lambda.
    pub bind_addr: String,
    /// Shared secret that callers must present as `Authorization: Bearer <token>`
    /// to reach any route. Required so routes are never exposed unauthenticated.
    pub auth_token: String,
    /// Name of the `analyzer` Lambda function the server async-invokes per
    /// analysis shard (one per camera). Required so `POST /cameras/analyze`
    /// can fan the job out.
    pub analyzer_function_name: String,
}

impl Configuration {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
        let bind_addr =
            std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let auth_token =
            std::env::var("AUTH_TOKEN").context("AUTH_TOKEN is not set")?;
        let analyzer_function_name =
            std::env::var("ANALYZER_FUNCTION_NAME").context("ANALYZER_FUNCTION_NAME is not set")?;

        Ok(Self {
            database_url,
            bind_addr,
            auth_token,
            analyzer_function_name,
        })
    }
}
