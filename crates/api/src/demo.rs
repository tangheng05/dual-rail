use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::AppState;

const PAGE: &str = include_str!("../../../web/demo.html");
const KEY_PLACEHOLDER: &str = "__STRIPE_PUBLISHABLE_KEY__";

pub async fn page(State(state): State<AppState>) -> impl IntoResponse {
    let key = state.stripe_publishable_key.as_deref().unwrap_or_default();
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PAGE.replace(KEY_PLACEHOLDER, key),
    )
}
