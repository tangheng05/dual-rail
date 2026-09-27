use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use dual_rail_rails::khqr::MerchantAccount;

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
    pub khqr_account: MerchantAccount,
    pub khqr_ttl: Duration,
    pub bakong_token: String,
    pub bakong_endpoint: BakongEndpoint,
    pub bakong_renewal_email: Option<String>,
    pub bakong_poll_interval: Duration,
}

pub enum BakongEndpoint {
    Sandbox,
    Production,
    Relay(String),
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .context("BIND_ADDR must be a socket address like 0.0.0.0:8080")?;
        let khqr_ttl_secs: u64 = optional("KHQR_TTL_SECS")
            .map_or(Ok(300), |secs| secs.parse())
            .context("KHQR_TTL_SECS must be a whole number of seconds")?;
        if !(60..=86_400).contains(&khqr_ttl_secs) {
            bail!("KHQR_TTL_SECS must be between 60 and 86400");
        }
        let poll_interval_secs: u64 = optional("BAKONG_POLL_INTERVAL_SECS")
            .map_or(Ok(2), |secs| secs.parse())
            .context("BAKONG_POLL_INTERVAL_SECS must be a whole number of seconds")?;
        if poll_interval_secs == 0 {
            bail!("BAKONG_POLL_INTERVAL_SECS must be at least 1");
        }
        let bakong_endpoint = match (optional("BAKONG_BASE_URL"), optional("BAKONG_ENV")) {
            (Some(url), _) => BakongEndpoint::Relay(url),
            (None, Some(env)) if env == "sandbox" => BakongEndpoint::Sandbox,
            (None, Some(env)) if env == "production" => BakongEndpoint::Production,
            _ => bail!("set BAKONG_ENV to sandbox or production, or BAKONG_BASE_URL to a relay"),
        };

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            bind_addr,
            stripe_secret_key: required("STRIPE_SECRET_KEY")?,
            stripe_webhook_secret: required("STRIPE_WEBHOOK_SECRET")?,
            khqr_account: MerchantAccount {
                account_id: required("KHQR_ACCOUNT_ID")?,
                merchant_name: required("KHQR_MERCHANT_NAME")?,
                merchant_city: required("KHQR_MERCHANT_CITY")?,
                merchant_id: optional("KHQR_MERCHANT_ID"),
                acquiring_bank: optional("KHQR_ACQUIRING_BANK"),
            },
            khqr_ttl: Duration::from_secs(khqr_ttl_secs),
            bakong_token: required("BAKONG_TOKEN")?,
            bakong_endpoint,
            bakong_renewal_email: optional("BAKONG_RENEWAL_EMAIL"),
            bakong_poll_interval: Duration::from_secs(poll_interval_secs),
        })
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    optional(name).with_context(|| format!("{name} must be set"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}
