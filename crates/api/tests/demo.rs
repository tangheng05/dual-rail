mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TestApp, api_get};
use serde_json::json;
use sqlx::PgPool;

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn demo_create(key: &str, body: serde_json::Value) -> Request<Body> {
    Request::post("/demo/payments")
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn serves_the_demo_page_with_the_publishable_key(pool: PgPool) {
    let app = TestApp::demo(pool).await;

    let (status, headers, html) = app.send_raw(get("/")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(html.contains(r#"<meta name="stripe-key" content="pk_test_demo">"#));
    assert!(!html.contains("__STRIPE_PUBLISHABLE_KEY__"));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn the_demo_is_off_unless_enabled(pool: PgPool) {
    let app = TestApp::new(pool).await;

    let (page, _) = app.send(get("/")).await;
    let (create, _) = app
        .send(demo_create(
            "k",
            json!({ "method": "khqr", "amount_minor": 1000, "currency": "USD" }),
        ))
        .await;

    assert_eq!(page, StatusCode::NOT_FOUND);
    assert_eq!(create, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn the_demo_creates_payments_without_a_key_and_reads_them_by_token(pool: PgPool) {
    let app = TestApp::demo(pool).await;

    let (status, created) = app
        .send(demo_create(
            "k",
            json!({ "method": "khqr", "amount_minor": 1000, "currency": "USD" }),
        ))
        .await;
    let id = created["id"].as_str().unwrap();
    let token = created["client_token"].as_str().unwrap();
    let with_token = |path: String| {
        Request::get(path)
            .header("x-client-token", token)
            .body(Body::empty())
            .unwrap()
    };
    let (read, fetched) = app.send(with_token(format!("/payments/{id}"))).await;
    let (qr, headers, svg) = app
        .send_raw(with_token(format!("/payments/{id}/qr.svg")))
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        (read, fetched["status"].as_str()),
        (StatusCode::OK, Some("pending"))
    );
    assert_eq!(qr, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml");
    assert!(svg.contains("<svg"));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn card_and_unknown_payments_have_no_qr(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, body) = app.create_card_payment("k", 1000).await;
    let card_id = body["id"].as_str().unwrap();

    for path in [
        format!("/payments/{card_id}/qr.svg"),
        "/payments/00000000-0000-0000-0000-000000000000/qr.svg".to_owned(),
    ] {
        let (status, _, _) = app.send_raw(api_get(&path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}
