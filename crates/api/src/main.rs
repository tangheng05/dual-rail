use std::sync::Arc;

use clap::Parser;
use dual_rail_api::cli::{Cli, Command, FlagsCommand};
use dual_rail_api::{AppState, BakongEndpoint, Config};
use dual_rail_core::{Currency, Money};
use dual_rail_rails::card::StripeGateway;
use dual_rail_rails::khqr::{BakongVerifier, KhqrIssuer};
use dual_rail_store::reviews::{self, Resolved};
use khqr_api::{BakongClient, Environment};
use serde_json::json;
use time::OffsetDateTime;
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

    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Reconcile { date } => {
            let (config, state) = build_state().await?;
            let summary = dual_rail_api::reconcile(&state, date, config.reconciliation_offset)
                .await?
                .ok_or_else(|| anyhow::anyhow!("a reconciliation for {date} is already running"))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Command::Flags(command) => flags(command).await,
    }
}

async fn serve() -> anyhow::Result<()> {
    let (config, state) = build_state().await?;
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

async fn build_state() -> anyhow::Result<(Config, AppState)> {
    let config = Config::from_env()?;
    let khqr = KhqrIssuer::new(config.khqr_account.clone());
    let now_ms = u64::try_from(OffsetDateTime::now_utc().unix_timestamp() * 1000)?;
    khqr.issue(
        Uuid::nil(),
        Money::new(100, Currency::Usd)?,
        now_ms,
        now_ms + 300_000,
    )
    .map_err(|err| anyhow::anyhow!("KHQR merchant settings are invalid: {err}"))?;

    let bakong = match &config.bakong_endpoint {
        BakongEndpoint::Sandbox => BakongClient::new(Environment::Sandbox, &config.bakong_token),
        BakongEndpoint::Production => {
            BakongClient::new(Environment::Production, &config.bakong_token)
        }
        BakongEndpoint::Relay(url) => BakongClient::with_base_url(url, &config.bakong_token),
    };
    let bakong = match &config.bakong_renewal_email {
        Some(email) => bakong.with_renewal_email(email),
        None => bakong,
    };

    let pool = dual_rail_store::connect(&config.database_url).await?;
    dual_rail_store::MIGRATOR.run(&pool).await?;
    let state = AppState {
        pool,
        cards: Arc::new(StripeGateway::new(&config.stripe_secret_key)?),
        stripe_webhook_secret: config.stripe_webhook_secret.as_str().into(),
        stripe_livemode: config.stripe_livemode,
        stripe_publishable_key: config.stripe_publishable_key.as_deref().map(Into::into),
        khqr: Arc::new(khqr),
        verifier: Arc::new(BakongVerifier::new(bakong)),
        khqr_ttl: config.khqr_ttl,
    };
    Ok((config, state))
}

async fn flags(command: FlagsCommand) -> anyhow::Result<()> {
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL must be set"))?;
    let pool = dual_rail_store::connect(&database_url).await?;

    match command {
        FlagsCommand::List { all } => {
            let flags: Vec<_> = reviews::list(&pool, all)
                .await?
                .into_iter()
                .map(|flag| {
                    json!({
                        "id": flag.id,
                        "payment_id": flag.payment_id,
                        "reason": flag.reason,
                        "details": flag.details,
                        "created_at": flag.created_at.to_string(),
                        "resolved_at": flag.resolved_at.map(|at| at.to_string()),
                        "resolution": flag.resolution,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&flags)?);
        }
        FlagsCommand::Resolve { id, note } => match reviews::resolve(&pool, id, &note).await? {
            Resolved::Now => println!("resolved {id}"),
            Resolved::Already => anyhow::bail!("flag {id} is already resolved"),
            Resolved::NotFound => anyhow::bail!("no review flag {id}"),
        },
    }
    Ok(())
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
