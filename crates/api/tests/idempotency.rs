mod common;

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{FakeCards, TestApp, create_payment_request, payment_intent_event};
use serde_json::{Value, json};
use sqlx::PgPool;

fn card(amount_minor: i64) -> Value {
    json!({ "method": "card", "amount_minor": amount_minor, "currency": "USD", "description": "Order #1" })
}

fn replayed(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("idempotent-replayed")
        .is_some_and(|value| value == "true")
}

async fn payment_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("select count(*) from payments")
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn same_key_and_body_replays_the_original_payment(pool: PgPool) {
    let app = TestApp::new(pool);

    let (first_status, first_headers, first) = app
        .send_full(create_payment_request(Some("k"), card(1000)))
        .await;
    let (second_status, second_headers, second) = app
        .send_full(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(
        (first_status, second_status),
        (StatusCode::CREATED, StatusCode::CREATED)
    );
    assert!(!replayed(&first_headers));
    assert!(replayed(&second_headers));
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["client_secret"], first["client_secret"]);
    assert_eq!(app.cards.creates(), 1);
    assert_eq!(app.cards.secret_lookups.load(Ordering::SeqCst), 1);
    assert_eq!(payment_count(&app).await, 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn json_key_order_and_whitespace_do_not_change_the_request(pool: PgPool) {
    let app = TestApp::new(pool);
    let reordered = r#"{ "description": "Order #1",
        "currency": "USD", "amount_minor": 1000, "method": "card" }"#;

    let (_, first) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;
    let (status, headers, second) = app
        .send_full(
            Request::post("/payments")
                .header("content-type", "application/json")
                .header("idempotency-key", "k")
                .body(Body::from(reordered))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(replayed(&headers));
    assert_eq!(second["id"], first["id"]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn same_key_with_a_different_request_conflicts(pool: PgPool) {
    let app = TestApp::new(pool);
    app.send(create_payment_request(Some("k"), card(1000)))
        .await;

    let variants = [
        card(1001),
        json!({ "method": "card", "amount_minor": 1000, "currency": "KHR", "description": "Order #1" }),
        json!({ "method": "khqr", "amount_minor": 1000, "currency": "USD", "description": "Order #1" }),
        json!({ "method": "card", "amount_minor": 1000, "currency": "USD", "description": "Order #2" }),
        json!({ "method": "card", "amount_minor": 1000, "currency": "USD" }),
    ];
    for body in variants {
        let (status, _) = app
            .send(create_payment_request(Some("k"), body.clone()))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    assert_eq!(app.cards.creates(), 1);
    assert_eq!(payment_count(&app).await, 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn retry_after_stripe_outage_links_the_same_payment(pool: PgPool) {
    let app = TestApp::with_cards(pool, FakeCards::failing());

    let (failed, _) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;
    app.cards.fail.store(false, Ordering::SeqCst);
    let (status, headers, body) = app
        .send_full(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(failed, StatusCode::BAD_GATEWAY);
    assert_eq!(status, StatusCode::CREATED);
    assert!(replayed(&headers));
    assert!(body["client_secret"].as_str().unwrap().contains("_secret_"));
    let requests = app.cards.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "same payment id and parameters both times"
    );
    assert_eq!(payment_count(&app).await, 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn request_still_in_progress_at_stripe_is_not_failed(pool: PgPool) {
    let app = TestApp::with_cards(pool, FakeCards::failing());
    app.send(create_payment_request(Some("k"), card(1000)))
        .await;
    app.cards.fail.store(false, Ordering::SeqCst);
    app.cards.in_progress.store(true, Ordering::SeqCst);

    let (status, _) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(status, StatusCode::CONFLICT);
    let payment_status: String = sqlx::query_scalar("select status from payments")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(payment_status, "pending");
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn rejected_payment_replays_the_rejection_without_calling_stripe(pool: PgPool) {
    let app = TestApp::with_cards(
        pool,
        FakeCards {
            reject: true,
            ..FakeCards::default()
        },
    );

    let (first, _) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;
    let (second, headers, _) = app
        .send_full(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(first, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(second, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(replayed(&headers));
    assert_eq!(app.cards.creates(), 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn replay_after_settlement_returns_the_current_state(pool: PgPool) {
    let app = TestApp::new(pool);
    let (_, created) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;
    let id = created["id"].as_str().unwrap();
    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.succeeded",
        id,
        1000,
    ))
    .await;

    let (status, headers, body) = app
        .send_full(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(replayed(&headers));
    assert_eq!(body["status"], "succeeded");
    assert!(body.get("client_secret").is_none());
    assert_eq!(app.cards.creates(), 1);
    assert_eq!(app.cards.secret_lookups.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn concurrent_identical_requests_create_one_payment(pool: PgPool) {
    let app = TestApp::new(pool);

    let request = || app.send(create_payment_request(Some("k"), card(1000)));
    let (a, b, c, d, e) = tokio::join!(request(), request(), request(), request(), request());
    let responses = [a, b, c, d, e];

    assert_eq!(payment_count(&app).await, 1);
    let ids: Vec<&Value> = responses
        .iter()
        .filter(|(status, _)| *status == StatusCode::CREATED)
        .map(|(_, body)| &body["id"])
        .collect();
    assert!(!ids.is_empty());
    assert!(ids.iter().all(|id| *id == ids[0]));
    assert!(
        responses
            .iter()
            .all(|(status, _)| *status == StatusCode::CREATED || *status == StatusCode::CONFLICT)
    );
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn khqr_replay_returns_the_same_qr(pool: PgPool) {
    let app = TestApp::new(pool);

    let (_, first) = app.create_khqr_payment("k", 1000, "USD").await;
    let (status, second) = app.create_khqr_payment("k", 1000, "USD").await;
    let (changed, _) = app.create_khqr_payment("k", 2000, "USD").await;

    assert_eq!(status, StatusCode::CREATED);
    for field in ["id", "qr", "md5", "expires_at"] {
        assert_eq!(second[field], first[field], "{field}");
    }
    assert_eq!(changed, StatusCode::CONFLICT);
    assert_eq!(payment_count(&app).await, 1);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn rejected_validation_does_not_consume_the_key(pool: PgPool) {
    let app = TestApp::new(pool);

    let (invalid, _) = app.send(create_payment_request(Some("k"), card(0))).await;
    let (valid, _) = app
        .send(create_payment_request(Some("k"), card(1000)))
        .await;

    assert_eq!(invalid, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(valid, StatusCode::CREATED);
}
