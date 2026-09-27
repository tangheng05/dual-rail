use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

pub async fn health(State(pool): State<PgPool>) -> (StatusCode, Json<Value>) {
    match dual_rail_store::ping(&pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok", "db": "ok" }))),
        Err(err) => {
            tracing::error!(%err, "health check database ping failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "degraded", "db": "unreachable" })),
            )
        }
    }
}
