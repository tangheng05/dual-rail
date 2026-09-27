use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::AppState;

pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match dual_rail_store::ping(&state.pool).await {
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
