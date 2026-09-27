mod auth;
pub mod cli;
mod config;
mod demo;
mod error;
mod health;
mod http;
mod khqr_poller;
mod payments;
mod reconciliation;
mod settlement;
mod stripe_webhook;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::{get, post};
use axum::{Router, middleware};
use dual_rail_rails::card::CardGateway;
use dual_rail_rails::khqr::{KhqrIssuer, KhqrVerifier};
use sqlx::PgPool;

pub use auth::{NewApiKey, generate_api_key, hash_api_key};
pub use config::{BakongEndpoint, Config};
pub use http::{HttpSettings, RateLimit};
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
    /// Whether the Stripe key is a live one; events from the other mode are ignored.
    pub stripe_livemode: bool,
    pub stripe_publishable_key: Option<Arc<str>>,
    pub khqr: Arc<KhqrIssuer>,
    pub verifier: Arc<dyn KhqrVerifier>,
    pub khqr_ttl: Duration,
    pub http: HttpSettings,
    /// Signs the client tokens a browser uses to read one payment.
    pub client_token_secret: Arc<[u8]>,
    /// Serves the demo page and its keyless create endpoint.
    pub demo_mode: bool,
}

/// Several dependencies enable different rustls crypto backends, so rustls cannot
/// pick one on its own and panics on the first TLS client. Call before building any.
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Serve with `into_make_service_with_connect_info::<SocketAddr>()` when a rate
/// limit is set: it keys on the client's IP.
pub fn app(state: AppState) -> Router {
    let settings = state.http;
    let require_api_key = middleware::from_fn_with_state(state.clone(), auth::require_api_key);
    let mut public = Router::new()
        .route(
            "/payments",
            post(payments::create).route_layer(require_api_key),
        )
        .route("/payments/{id}", get(payments::get))
        .route("/payments/{id}/qr.svg", get(payments::qr_svg));
    if state.demo_mode {
        public = public
            .route("/", get(demo::page))
            .route("/demo/payments", post(payments::create_demo));
    }
    let public = match settings.rate_limit {
        Some(limit) => http::rate_limited(public, limit),
        None => public,
    };

    // Stripe retries in bursts and load balancers poll health, so neither is limited.
    let router = Router::new()
        .route("/health", get(health::health))
        .route("/webhooks/stripe", post(stripe_webhook::receive))
        .merge(public)
        .with_state(state);
    http::with_middleware(router, settings)
}
