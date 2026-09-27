mod config;
mod demo;
mod error;
mod health;
mod khqr_poller;
mod payments;
mod reconciliation;
mod settlement;
mod stripe_webhook;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use dual_rail_rails::card::CardGateway;
use dual_rail_rails::khqr::{KhqrIssuer, KhqrVerifier};
use sqlx::PgPool;

pub use config::{BakongEndpoint, Config};
pub use khqr_poller::{poll_once, run as run_khqr_poller};
pub use reconciliation::{
    Mismatch, RunSummary, local_day_window, reconcile,
    run_scheduler as run_reconciliation_scheduler,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cards: Arc<dyn CardGateway>,
    pub stripe_webhook_secret: Arc<str>,
    pub stripe_publishable_key: Option<Arc<str>>,
    pub khqr: Arc<KhqrIssuer>,
    pub verifier: Arc<dyn KhqrVerifier>,
    pub khqr_ttl: Duration,
}

/// Several dependencies enable different rustls crypto backends, so rustls cannot
/// pick one on its own and panics on the first TLS client. Call before building any.
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(demo::page))
        .route("/health", get(health::health))
        .route("/payments", post(payments::create))
        .route("/payments/{id}", get(payments::get))
        .route("/payments/{id}/qr.svg", get(payments::qr_svg))
        .route("/webhooks/stripe", post(stripe_webhook::receive))
        .with_state(state)
}
