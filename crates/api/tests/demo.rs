mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::TestApp;
use sqlx::PgPool;

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn serves_the_demo_page_with_the_publishable_key(pool: PgPool) {
    let app = TestApp::new(pool);

    let (status, headers, html) = app.send_raw(get("/")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(html.contains(r#"<meta name="stripe-key" content="pk_test_demo">"#));
    assert!(!html.contains("__STRIPE_PUBLISHABLE_KEY__"));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn renders_a_khqr_payment_as_svg(pool: PgPool) {
    let app = TestApp::new(pool);
    let (_, body) = app.create_khqr_payment("k", 1000, "USD").await;
    let id = body["id"].as_str().unwrap();

    let (status, headers, svg) = app.send_raw(get(&format!("/payments/{id}/qr.svg"))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml");
    assert!(svg.contains("<svg"));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn card_and_unknown_payments_have_no_qr(pool: PgPool) {
    let app = TestApp::new(pool);
    let (_, body) = app.create_card_payment("k", 1000).await;
    let card_id = body["id"].as_str().unwrap();

    for path in [
        format!("/payments/{card_id}/qr.svg"),
        "/payments/00000000-0000-0000-0000-000000000000/qr.svg".to_owned(),
    ] {
        let (status, _, _) = app.send_raw(get(&path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}
