mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{FakeCards, TestApp, create_payment_request};
use dual_rail_api::{HttpSettings, RateLimit};
use serde_json::json;
use sqlx::PgPool;

fn from_ip(mut request: Request<Body>, ip: [u8; 4]) -> Request<Body> {
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((ip, 50_000))));
    request
}

fn rate_limited(burst: u32, trust_proxy_headers: bool) -> HttpSettings {
    HttpSettings {
        rate_limit: Some(RateLimit {
            per_second: 1,
            burst,
            trust_proxy_headers,
        }),
        ..HttpSettings::default()
    }
}

fn status_request(ip: [u8; 4]) -> Request<Body> {
    from_ip(
        Request::get("/payments/00000000-0000-0000-0000-000000000000")
            .body(Body::empty())
            .unwrap(),
        ip,
    )
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn every_response_carries_a_request_id(pool: PgPool) {
    let app = TestApp::new(pool);

    let (_, generated, _) = app
        .send_full(Request::get("/health").body(Body::empty()).unwrap())
        .await;
    let (_, echoed, _) = app
        .send_full(
            Request::get("/health")
                .header("x-request-id", "trace-me-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    let generated = generated["x-request-id"].to_str().unwrap();
    assert_eq!(generated.len(), 36, "a uuid: {generated}");
    assert_eq!(echoed["x-request-id"], "trace-me-123");
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn oversized_bodies_are_refused(pool: PgPool) {
    let app = TestApp::new(pool);
    let huge = json!({ "method": "card", "amount_minor": 1000, "currency": "USD", "description": "x".repeat(100_000) });

    let declared = {
        let mut request = create_payment_request(Some("k"), huge.clone());
        let length = huge.to_string().len().to_string();
        request
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length.parse().unwrap());
        request
    };
    let (streamed, _) = app.send(create_payment_request(Some("k"), huge)).await;
    let (with_length, _) = app.send(declared).await;

    assert_eq!(streamed, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(with_length, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(app.cards.requests.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_request_cut_off_by_the_timeout_can_be_finished_by_retrying(pool: PgPool) {
    let cards = FakeCards::default();
    *cards.delay.lock().unwrap() = Some(Duration::from_millis(500));
    let settings = HttpSettings {
        request_timeout: Duration::from_millis(100),
        ..HttpSettings::default()
    };
    let app = TestApp::configured(pool, cards, settings);

    let (timed_out, _) = app.create_card_payment("k", 1000).await;
    *app.cards.delay.lock().unwrap() = None;
    let (status, body) = app.create_card_payment("k", 1000).await;

    assert_eq!(timed_out, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["client_secret"].as_str().unwrap().contains("_secret_"));
    let rows: i64 = sqlx::query_scalar("select count(*) from payments")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_client_over_its_rate_is_told_when_to_retry(pool: PgPool) {
    let app = TestApp::configured(pool, FakeCards::default(), rate_limited(2, false));

    let (first, _, _) = app.send_full(status_request([1, 1, 1, 1])).await;
    let (second, _, _) = app.send_full(status_request([1, 1, 1, 1])).await;
    let (third, headers, body) = app.send_full(status_request([1, 1, 1, 1])).await;
    let (other_client, _, _) = app.send_full(status_request([2, 2, 2, 2])).await;

    assert_eq!(
        (first, second),
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );
    assert_eq!(third, StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after >= 1, "never tell a client to retry immediately");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("too many requests")
    );
    assert_eq!(other_client, StatusCode::NOT_FOUND, "limits are per client");
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn stripe_webhooks_and_health_are_never_rate_limited(pool: PgPool) {
    let app = TestApp::configured(pool, FakeCards::default(), rate_limited(1, false));

    for _ in 0..5 {
        let webhook = from_ip(
            Request::post("/webhooks/stripe")
                .header("stripe-signature", "t=1,v1=00")
                .body(Body::from("{}"))
                .unwrap(),
            [3, 3, 3, 3],
        );
        let health = from_ip(
            Request::get("/health").body(Body::empty()).unwrap(),
            [3, 3, 3, 3],
        );
        assert_eq!(app.send(webhook).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(app.send(health).await.0, StatusCode::OK);
    }
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn behind_a_trusted_proxy_the_forwarded_address_is_the_client(pool: PgPool) {
    let app = TestApp::configured(pool, FakeCards::default(), rate_limited(1, true));
    let via_proxy = |client: &str| {
        let request = Request::get("/payments/00000000-0000-0000-0000-000000000000")
            .header("x-forwarded-for", client)
            .body(Body::empty())
            .unwrap();
        from_ip(request, [10, 0, 0, 1])
    };

    let (first, _) = app.send(via_proxy("203.0.113.7")).await;
    let (same_client, _) = app.send(via_proxy("203.0.113.7")).await;
    let (other_client, _) = app.send(via_proxy("198.51.100.9")).await;

    assert_eq!(first, StatusCode::NOT_FOUND);
    assert_eq!(same_client, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(other_client, StatusCode::NOT_FOUND);
}
