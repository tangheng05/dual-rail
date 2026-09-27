mod config;
mod error;
mod health;
mod payments;
mod stripe_webhook;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use dual_rail_rails::card::CardGateway;
use sqlx::PgPool;

pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cards: Arc<dyn CardGateway>,
    pub stripe_webhook_secret: Arc<str>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::health))
        .route("/payments", post(payments::create))
        .route("/payments/{id}", get(payments::get))
        .route("/webhooks/stripe", post(stripe_webhook::receive))
        .with_state(state)
}
