use std::env;
use std::net::SocketAddr;

use anyhow::Context;

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        let bind_addr = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .context("BIND_ADDR must be a socket address like 0.0.0.0:8080")?;

        Ok(Self {
            database_url,
            bind_addr,
        })
    }
}
