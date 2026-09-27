mod common;

use axum::http::StatusCode;
use common::{FakeCards, TestApp, api_get, create_payment_request};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn creates_a_pending_card_payment_with_a_stripe_intent(pool: PgPool) {
    let app = TestApp::new(pool).await;

    let (status, body) = app
        .send(create_payment_request(
            Some("order-1"),
            json!({ "method": "card", "amount_minor": 1000, "currency": "USD", "description": "Demo order #1" }),
        ))
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "pending");
    assert_eq!(body["amount_minor"], 1000);
    assert_eq!(body["currency"], "USD");
    let id = body["id"].as_str().unwrap();
    assert!(body["client_secret"].as_str().unwrap().contains("_secret_"));

    let requests = app.cards.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].payment_id.to_string(), id);
    assert_eq!(requests[0].description.as_deref(), Some("Demo order #1"));

    let (status, fetched) = app.send(api_get(format!("/payments/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["status"], "pending");
    assert!(fetched.get("client_secret").is_none());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn rejects_invalid_requests(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let valid = json!({ "method": "card", "amount_minor": 1000, "currency": "USD" });

    let cases = [
        (None, valid.clone(), StatusCode::BAD_REQUEST),
        (Some(""), valid.clone(), StatusCode::BAD_REQUEST),
        (
            Some("k"),
            json!({ "method": "card", "amount_minor": 0, "currency": "USD" }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            Some("k"),
            json!({ "method": "card", "amount_minor": 1.5, "currency": "USD" }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            Some("k"),
            json!({ "method": "card", "amount_minor": 1000, "currency": "EUR" }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            Some("k"),
            json!({ "method": "cash", "amount_minor": 1000, "currency": "USD" }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (key, body, expected) in cases {
        let (status, _) = app.send(create_payment_request(key, body.clone())).await;
        assert_eq!(status, expected, "{key:?} {body}");
    }
    assert!(app.cards.requests.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn stripe_outage_returns_bad_gateway_and_leaves_payment_pending(pool: PgPool) {
    let app = TestApp::with_cards(pool.clone(), FakeCards::failing()).await;

    let (status, _) = app.create_card_payment("order-1", 1000).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, provider_ref): (String, Option<String>) =
        sqlx::query_as("select status, provider_ref from payments")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(provider_ref, None);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn unknown_payment_is_not_found(pool: PgPool) {
    let app = TestApp::new(pool).await;

    let (status, _) = app
        .send(api_get("/payments/00000000-0000-0000-0000-000000000000"))
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn usd_amounts_outside_stripe_limits_are_rejected_before_calling_stripe(pool: PgPool) {
    let app = TestApp::new(pool).await;

    let (too_small, _) = app.create_card_payment("order-1", 49).await;
    let (too_large, _) = app.create_card_payment("order-2", 100_000_000).await;

    assert_eq!(too_small, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(too_large, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(app.cards.requests.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn stripe_rejection_fails_the_payment_instead_of_leaving_it_pending(pool: PgPool) {
    let app = TestApp::with_cards(
        pool.clone(),
        FakeCards {
            reject: true,
            ..FakeCards::default()
        },
    )
    .await;

    let (first, body) = app
        .send(create_payment_request(
            Some("order-1"),
            json!({ "method": "card", "amount_minor": 100_000_000, "currency": "KHR" }),
        ))
        .await;
    let (second, _) = app
        .send(create_payment_request(
            Some("order-2"),
            json!({ "method": "card", "amount_minor": 100_000_000, "currency": "KHR" }),
        ))
        .await;

    assert_eq!(first, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        second,
        StatusCode::UNPROCESSABLE_ENTITY,
        "two rejected rows must not collide"
    );
    assert!(body["error"].as_str().unwrap().contains("999,999.99"));
    let statuses: Vec<String> = sqlx::query_scalar("select status from payments")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(statuses, ["failed", "failed"]);
}
