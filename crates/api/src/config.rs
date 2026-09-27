use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use dual_rail_rails::khqr::MerchantAccount;

use crate::http::{HttpSettings, RateLimit};
use time::UtcOffset;
use time::macros::{format_description, offset};

const CAMBODIA: UtcOffset = offset!(+7);

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub stripe_secret_key: String,
    pub stripe_livemode: bool,
    pub stripe_webhook_secret: String,
    pub stripe_publishable_key: Option<String>,
    pub khqr_account: MerchantAccount,
    pub khqr_ttl: Duration,
    pub bakong_token: String,
    pub bakong_endpoint: BakongEndpoint,
    pub bakong_renewal_email: Option<String>,
    pub bakong_poll_interval: Duration,
    pub reconciliation_offset: UtcOffset,
    pub http: HttpSettings,
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
        let reconciliation_offset = match optional("RECONCILIATION_UTC_OFFSET") {
            Some(offset) => UtcOffset::parse(
                &offset,
                format_description!("[offset_hour sign:mandatory]:[offset_minute]"),
            )
            .context("RECONCILIATION_UTC_OFFSET must look like +07:00")?,
            None => CAMBODIA,
        };
        let request_timeout_secs: u64 = optional("REQUEST_TIMEOUT_SECS")
            .map_or(Ok(30), |secs| secs.parse())
            .context("REQUEST_TIMEOUT_SECS must be a whole number of seconds")?;
        if !(25..=300).contains(&request_timeout_secs) {
            bail!("REQUEST_TIMEOUT_SECS must be between 25 and 300, above the Stripe client's 20s");
        }
        let rate_limit_per_second: u32 = optional("RATE_LIMIT_PER_SECOND")
            .map_or(Ok(10), |rate| rate.parse())
            .context("RATE_LIMIT_PER_SECOND must be a whole number, 0 to turn it off")?;
        let rate_limit_burst: u32 = optional("RATE_LIMIT_BURST")
            .map_or(Ok(20), |burst| burst.parse())
            .context("RATE_LIMIT_BURST must be a whole number")?;
        if rate_limit_per_second > 1000 || (rate_limit_per_second > 0 && rate_limit_burst == 0) {
            bail!("RATE_LIMIT_PER_SECOND must be at most 1000, and RATE_LIMIT_BURST at least 1");
        }
        let trust_proxy_headers = match optional("TRUST_PROXY_HEADERS").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => bail!("TRUST_PROXY_HEADERS must be true or false"),
        };
        let http = HttpSettings {
            request_timeout: Duration::from_secs(request_timeout_secs),
            rate_limit: (rate_limit_per_second > 0).then_some(RateLimit {
                per_second: rate_limit_per_second,
                burst: rate_limit_burst,
                trust_proxy_headers,
            }),
        };
        let stripe_secret_key = required("STRIPE_SECRET_KEY")?;
        let stripe_livemode = stripe_livemode(&stripe_secret_key)?;
        let stripe_publishable_key = optional("STRIPE_PUBLISHABLE_KEY");
        // It is written into the demo page's HTML, so only a key's own characters pass.
        if let Some(key) = &stripe_publishable_key
            && !(key.starts_with("pk_")
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            bail!("STRIPE_PUBLISHABLE_KEY must look like pk_test_...");
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
            stripe_secret_key,
            stripe_livemode,
            stripe_webhook_secret: required("STRIPE_WEBHOOK_SECRET")?,
            stripe_publishable_key,
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
            reconciliation_offset,
            http,
        })
    }
}

/// Whether the key is a live one. Secret (`sk_`) and restricted (`rk_`) keys both
/// say their mode in the prefix.
fn stripe_livemode(secret_key: &str) -> anyhow::Result<bool> {
    match secret_key.get(..8) {
        Some("sk_live_" | "rk_live_") => Ok(true),
        Some("sk_test_" | "rk_test_") => Ok(false),
        _ => bail!("STRIPE_SECRET_KEY must start with sk_live_, sk_test_, rk_live_ or rk_test_"),
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    optional(name).with_context(|| format!("{name} must be set"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stripe_key_prefix_decides_the_mode() {
        assert!(stripe_livemode("sk_live_abc").unwrap());
        assert!(stripe_livemode("rk_live_abc").unwrap());
        assert!(!stripe_livemode("sk_test_abc").unwrap());
        assert!(!stripe_livemode("rk_test_abc").unwrap());
        for key in ["pk_live_abc", "whsec_abc", "sk_abc", ""] {
            assert!(stripe_livemode(key).is_err(), "{key}");
        }
    }
}
