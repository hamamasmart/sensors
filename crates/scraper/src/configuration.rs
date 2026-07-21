use anyhow::Context;

pub struct Configuration {
    pub server_url: String,
    pub phytech_email: String,
    pub phytech_password: String,
    /// Bearer token sent to the server as `Authorization: Bearer <token>`.
    pub server_auth_token: String,
}

impl Configuration {
    pub fn from_env() -> anyhow::Result<Self> {
        let server_url =
            std::env::var("SERVER_URL").context("SERVER_URL is not set")?;
        let phytech_email =
            std::env::var("PHYTECH_EMAIL").context("PHYTECH_EMAIL is not set")?;
        let phytech_password =
            std::env::var("PHYTECH_PASSWORD").context("PHYTECH_PASSWORD is not set")?;
        let server_auth_token =
            std::env::var("AUTH_TOKEN").context("AUTH_TOKEN is not set")?;

        Ok(Self {
            server_url,
            phytech_email,
            phytech_password,
            server_auth_token,
        })
    }
}
