use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn health_is_ok_when_database_is_reachable(pool: PgPool) {
    let response = dual_rail_api::app(pool)
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn health_is_unavailable_when_database_is_down(pool: PgPool) {
    pool.close().await;

    let response = dual_rail_api::app(pool)
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
