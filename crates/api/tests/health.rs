mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::TestApp;
use sqlx::PgPool;

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn health_is_ok_when_database_is_reachable(pool: PgPool) {
    let app = TestApp::new(pool);

    let (status, _) = app
        .send(Request::get("/health").body(Body::empty()).unwrap())
        .await;

    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn health_is_unavailable_when_database_is_down(pool: PgPool) {
    pool.close().await;
    let app = TestApp::new(pool);

    let (status, _) = app
        .send(Request::get("/health").body(Body::empty()).unwrap())
        .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
