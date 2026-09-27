use std::env;
use std::net::SocketAddr;

use anyhow::Context;

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .context("BIND_ADDR must be a socket address like 0.0.0.0:8080")?;

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            bind_addr,
            stripe_secret_key: required("STRIPE_SECRET_KEY")?,
            stripe_webhook_secret: required("STRIPE_WEBHOOK_SECRET")?,
        })
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("{name} must be set"))
}
