use std::net::IpAddr;
use std::time::Duration;

use axum::http::{HeaderName, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use tower_governor::GovernorError;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::{KeyExtractor, PeerIpKeyExtractor, SmartIpKeyExtractor};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::AppState;

const BODY_LIMIT_BYTES: usize = 64 * 1024;
const LIMITER_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpSettings {
    /// Longer than the Stripe client's own 20s timeout, so a slow Stripe call
    /// fails as a provider error before the whole request is cut off.
    pub request_timeout: Duration,
    pub rate_limit: Option<RateLimit>,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            rate_limit: None,
        }
    }
}

/// Per client IP: a steady `per_second` with room for bursts of `burst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub per_second: u32,
    pub burst: u32,
    /// Take the client IP from `Forwarded` / `X-Forwarded-For`. Only safe behind a
    /// proxy that overwrites those headers, since clients can set them freely.
    pub trust_proxy_headers: bool,
}

/// Wraps every route. The timeout answers 503 so clients retry; a create that was
/// cut off mid-way is finished by retrying with the same Idempotency-Key.
pub fn with_middleware(router: Router, settings: HttpSettings) -> Router {
    let sensitive = [
        header::AUTHORIZATION,
        HeaderName::from_static("stripe-signature"),
    ];
    router
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            settings.request_timeout,
        ))
        .layer(CatchPanicLayer::new())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http().make_span_with(request_span))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(SetSensitiveRequestHeadersLayer::new(sensitive))
}

fn request_span<B>(request: &Request<B>) -> tracing::Span {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // The path only: query strings can carry tokens.
    tracing::info_span!(
        "request",
        method = %request.method(),
        path = request.uri().path(),
        request_id,
    )
}

pub fn rate_limited(router: Router<AppState>, limit: RateLimit) -> Router<AppState> {
    let config = GovernorConfigBuilder::default()
        .per_millisecond(u64::from(1000 / limit.per_second.clamp(1, 1000)))
        .burst_size(limit.burst.max(1))
        .key_extractor(ClientIp {
            trust_proxy_headers: limit.trust_proxy_headers,
        })
        .finish()
        .expect("rate limit periods and bursts are clamped to be non-zero");

    // The limiter keeps a bucket per IP it has seen; drop the idle ones.
    let limiter = config.limiter().clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(LIMITER_CLEANUP_INTERVAL);
        loop {
            ticker.tick().await;
            limiter.retain_recent();
        }
    });

    router.layer(GovernorLayer::new(config).error_handler(too_many_requests))
}

fn too_many_requests(error: GovernorError) -> Response {
    match error {
        GovernorError::TooManyRequests { wait_time, .. } => {
            // The limiter rounds a sub-second wait down to 0, which reads as "retry now".
            let retry_after = wait_time.max(1);
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after.to_string())],
                Json(json!({ "error": format!("too many requests, retry in {retry_after}s") })),
            )
                .into_response()
        }
        other => {
            tracing::error!(%other, "rate limiter could not identify the client");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ClientIp {
    trust_proxy_headers: bool,
}

impl KeyExtractor for ClientIp {
    type Key = IpAddr;

    fn extract<T>(&self, request: &Request<T>) -> Result<IpAddr, GovernorError> {
        if self.trust_proxy_headers {
            SmartIpKeyExtractor.extract(request)
        } else {
            PeerIpKeyExtractor.extract(request)
        }
    }
}
