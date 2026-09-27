use std::sync::Arc;

use dual_rail_api::{AppState, BakongEndpoint, Config};
use dual_rail_core::{Currency, Money};
use dual_rail_rails::card::StripeGateway;
use dual_rail_rails::khqr::{BakongVerifier, KhqrIssuer};
use khqr_api::{BakongClient, Environment};
use time::macros::format_description;
use time::{Date, OffsetDateTime};
use tokio::net::TcpListener;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dual_rail_api::install_crypto_provider();
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let manual_run_date = match std::env::args().nth(1).as_deref() {
        None => None,
        Some("reconcile") => Some(parse_run_date(std::env::args().nth(2))?),
        Some(other) => anyhow::bail!("unknown command {other:?}; expected `reconcile YYYY-MM-DD`"),
    };

    let config = Config::from_env()?;
    let khqr = KhqrIssuer::new(config.khqr_account);
    let now_ms = u64::try_from(OffsetDateTime::now_utc().unix_timestamp() * 1000)?;
    khqr.issue(
        Uuid::nil(),
        Money::new(100, Currency::Usd)?,
        now_ms,
        now_ms + 300_000,
    )
    .map_err(|err| anyhow::anyhow!("KHQR merchant settings are invalid: {err}"))?;

    let bakong = match config.bakong_endpoint {
        BakongEndpoint::Sandbox => BakongClient::new(Environment::Sandbox, config.bakong_token),
        BakongEndpoint::Production => {
            BakongClient::new(Environment::Production, config.bakong_token)
        }
        BakongEndpoint::Relay(url) => BakongClient::with_base_url(url, config.bakong_token),
    };
    let bakong = match config.bakong_renewal_email {
        Some(email) => bakong.with_renewal_email(email),
        None => bakong,
    };

    let pool = dual_rail_store::connect(&config.database_url).await?;
    dual_rail_store::MIGRATOR.run(&pool).await?;
    let state = AppState {
        pool,
        cards: Arc::new(StripeGateway::new(&config.stripe_secret_key)?),
        stripe_webhook_secret: config.stripe_webhook_secret.into(),
        stripe_publishable_key: config.stripe_publishable_key.map(Into::into),
        khqr: Arc::new(khqr),
        verifier: Arc::new(BakongVerifier::new(bakong)),
        khqr_ttl: config.khqr_ttl,
    };

    if let Some(date) = manual_run_date {
        let summary = dual_rail_api::reconcile(&state, date, config.reconciliation_offset)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a reconciliation for {date} is already running"))?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let shutdown = CancellationToken::new();
    let poller = tokio::spawn(dual_rail_api::run_khqr_poller(
        state.clone(),
        config.bakong_poll_interval,
        shutdown.clone(),
    ));
    let reconciler = tokio::spawn(dual_rail_api::run_reconciliation_scheduler(
        state.clone(),
        config.reconciliation_offset,
        shutdown.clone(),
    ));

    let listener = TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    axum::serve(listener, dual_rail_api::app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    shutdown.cancel();
    poller.await?;
    reconciler.await?;
    Ok(())
}

fn parse_run_date(arg: Option<String>) -> anyhow::Result<Date> {
    let arg = arg.ok_or_else(|| anyhow::anyhow!("usage: dual-rail-api reconcile YYYY-MM-DD"))?;
    Date::parse(&arg, format_description!("[year]-[month]-[day]"))
        .map_err(|err| anyhow::anyhow!("invalid date {arg:?}: {err}"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl-c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
