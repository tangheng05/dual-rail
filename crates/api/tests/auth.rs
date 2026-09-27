mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{API_KEY, TestApp, api_get, create_payment_request};
use serde_json::json;
use sqlx::PgPool;

fn card_body() -> serde_json::Value {
    json!({ "method": "card", "amount_minor": 1000, "currency": "USD" })
}

fn with_authorization(value: Option<&str>) -> Request<Body> {
    let mut request = create_payment_request(Some("k"), card_body());
    request.headers_mut().remove(header::AUTHORIZATION);
    if let Some(value) = value {
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, value.parse().unwrap());
    }
    request
}

fn read(path: String, client_token: Option<&str>) -> Request<Body> {
    let mut request = Request::get(path);
    if let Some(token) = client_token {
        request = request.header("x-client-token", token);
    }
    request.body(Body::empty()).unwrap()
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn creating_a_payment_needs_a_valid_api_key(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let wrong_key = format!("Bearer {}", API_KEY.replace('1', "2"));

    for authorization in [
        None,
        Some("Bearer "),
        Some(wrong_key.as_str()),
        Some(API_KEY),
        Some("Basic dXNlcjpwYXNz"),
    ] {
        let (status, headers, body) = app.send_full(with_authorization(authorization)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        assert!(body["error"].as_str().is_some());
    }
    assert!(
        app.cards.requests.lock().unwrap().is_empty(),
        "nothing reached Stripe"
    );

    let (status, _) = app
        .send(create_payment_request(Some("k"), card_body()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_revoked_key_stops_working(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let key_id = dual_rail_store::api_keys::list(&app.pool).await.unwrap()[0].id;

    dual_rail_store::api_keys::revoke(&app.pool, key_id)
        .await
        .unwrap();
    let (status, _) = app
        .send(create_payment_request(Some("k"), card_body()))
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_client_token_reads_only_its_own_payment(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, first) = app.create_card_payment("first", 1000).await;
    let (_, second) = app.create_card_payment("second", 1000).await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    let first_token = first["client_token"].as_str().unwrap();

    let (own, _) = app
        .send(read(format!("/payments/{first_id}"), Some(first_token)))
        .await;
    let (other, _) = app
        .send(read(format!("/payments/{second_id}"), Some(first_token)))
        .await;
    let (forged, _) = app
        .send(read(format!("/payments/{first_id}"), Some("00")))
        .await;
    let (anonymous, _) = app.send(read(format!("/payments/{first_id}"), None)).await;
    let (merchant, _) = app.send(api_get(format!("/payments/{second_id}"))).await;

    assert_eq!(own, StatusCode::OK);
    assert_eq!(other, StatusCode::UNAUTHORIZED);
    assert_eq!(forged, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous, StatusCode::UNAUTHORIZED);
    assert_eq!(
        merchant,
        StatusCode::OK,
        "the merchant's key reads every payment"
    );
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn an_unauthenticated_caller_cannot_tell_whether_a_payment_exists(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, created) = app.create_card_payment("k", 1000).await;
    let id = created["id"].as_str().unwrap();

    let (real, _) = app.send(read(format!("/payments/{id}"), None)).await;
    let (missing, _) = app
        .send(read(
            "/payments/00000000-0000-0000-0000-000000000000".to_owned(),
            None,
        ))
        .await;

    assert_eq!(real, StatusCode::UNAUTHORIZED);
    assert_eq!(missing, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn replays_return_the_same_client_token(pool: PgPool) {
    let app = TestApp::new(pool).await;

    let (_, first) = app.create_card_payment("k", 1000).await;
    let (_, replay) = app.create_card_payment("k", 1000).await;

    assert_eq!(first["client_token"], replay["client_token"]);
    assert_eq!(first["client_token"].as_str().unwrap().len(), 64);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn demo_callers_cannot_replay_a_merchants_payment(pool: PgPool) {
    let app = TestApp::demo(pool).await;
    let body = json!({ "method": "khqr", "amount_minor": 1000, "currency": "USD" });
    let (_, merchant) = app.create_khqr_payment("order-42", 1000, "USD").await;

    let demo_request = Request::post("/demo/payments")
        .header("content-type", "application/json")
        .header("idempotency-key", "order-42")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, headers, stranger) = app.send_full(demo_request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(!headers.contains_key("idempotent-replayed"));
    assert_ne!(
        stranger["id"], merchant["id"],
        "a new payment, not the merchant's"
    );
    assert_ne!(stranger["client_token"], merchant["client_token"]);
}
