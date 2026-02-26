use anyhow::Context;

pub struct Configuration {
    pub database_url: String,
    pub phytech_email: String,
    pub phytech_password: String,
}

impl Configuration {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
        let phytech_email = std::env::var("PHYTECH_EMAIL").context("PHYTECH_EMAIL is not set")?;
        let phytech_password =
            std::env::var("PHYTECH_PASSWORD").context("PHYTECH_PASSWORD is not set")?;

        Ok(Self {
            database_url,
            phytech_email,
            phytech_password,
        })
    }
}
