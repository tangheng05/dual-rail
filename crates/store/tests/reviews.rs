use dual_rail_store::reviews::{self, Resolved, ReviewReason};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn flagged_payment(pool: &PgPool) -> Uuid {
    let payment_id: Uuid = sqlx::query_scalar(
        "insert into payments (method, provider, status, amount_minor, currency, idempotency_key)
         values ('card', 'stripe', 'pending', 1000, 'USD', 'k1') returning id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    reviews::flag(
        &mut conn,
        payment_id,
        ReviewReason::AmountMismatch,
        json!({ "received": 999 }),
    )
    .await
    .unwrap();
    payment_id
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_flag_is_resolved_once_and_leaves_the_open_queue(pool: PgPool) {
    let payment_id = flagged_payment(&pool).await;
    let open = reviews::list(&pool, false).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].payment_id, payment_id);
    assert_eq!(open[0].reason, "amount_mismatch");
    assert_eq!(reviews::count_open(&pool).await.unwrap(), 1);

    let flag_id = open[0].id;
    assert_eq!(
        reviews::resolve(&pool, flag_id, "refunded by hand")
            .await
            .unwrap(),
        Resolved::Now
    );

    assert!(reviews::list(&pool, false).await.unwrap().is_empty());
    assert_eq!(reviews::count_open(&pool).await.unwrap(), 0);
    let all = reviews::list(&pool, true).await.unwrap();
    assert_eq!(all[0].resolution.as_deref(), Some("refunded by hand"));
    assert!(all[0].resolved_at.is_some());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn a_resolution_is_never_overwritten(pool: PgPool) {
    flagged_payment(&pool).await;
    let flag_id = reviews::list(&pool, false).await.unwrap()[0].id;
    reviews::resolve(&pool, flag_id, "first decision")
        .await
        .unwrap();

    assert_eq!(
        reviews::resolve(&pool, flag_id, "second thoughts")
            .await
            .unwrap(),
        Resolved::Already
    );
    let all = reviews::list(&pool, true).await.unwrap();
    assert_eq!(all[0].resolution.as_deref(), Some("first decision"));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn resolving_an_unknown_flag_is_not_found(pool: PgPool) {
    assert_eq!(
        reviews::resolve(&pool, Uuid::from_u128(7), "note")
            .await
            .unwrap(),
        Resolved::NotFound
    );
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn the_database_refuses_a_resolution_without_a_note(pool: PgPool) {
    flagged_payment(&pool).await;
    let result = sqlx::query("update review_flags set resolved_at = now()")
        .execute(&pool)
        .await;
    assert!(result.is_err());
}
