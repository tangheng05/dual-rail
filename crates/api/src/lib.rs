mod config;
mod health;

use axum::Router;
use axum::routing::get;
use sqlx::PgPool;

pub use config::Config;

pub fn app(pool: PgPool) -> Router {
    Router::new()
        .route("/health", get(health::health))
        .with_state(pool)
}
