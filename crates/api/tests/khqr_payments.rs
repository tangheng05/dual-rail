mod common;

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{KHQR_ACCOUNT, TestApp, unix_now_ms};
use dual_rail_rails::khqr::KhqrTransfer;
use sqlx::PgPool;

struct Created {
    id: String,
    md5: String,
}

async fn new_khqr(app: &TestApp, key: &str) -> Created {
    let (status, body) = app.create_khqr_payment(key, 1000, "USD").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Created {
        id: body["id"].as_str().unwrap().to_owned(),
        md5: body["md5"].as_str().unwrap().to_owned(),
    }
}

fn transfer(hash: &str, amount_minor: i64, paid_at_ms: i64) -> KhqrTransfer {
    KhqrTransfer {
        hash: hash.to_owned(),
        amount_minor: Some(amount_minor),
        currency: Some("USD".to_owned()),
        to_account_id: Some(KHQR_ACCOUNT.to_owned()),
        paid_at_ms: Some(paid_at_ms),
    }
}

fn credited(amount: i64) -> Vec<(String, String, i64)> {
    vec![
        ("clearing:bakong".to_owned(), "debit".to_owned(), amount),
        ("revenue".to_owned(), "credit".to_owned(), amount),
    ]
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn creates_a_khqr_payment_with_a_scannable_qr(pool: PgPool) {
    let app = TestApp::new(pool);

    let (status, body) = app.create_khqr_payment("order-1", 500_000, "KHR").await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["provider"], "bakong");
    assert_eq!(body["status"], "pending");
    let qr = body["qr"].as_str().unwrap();
    let decoded = khqr_core::decode(qr).unwrap();
    assert_eq!(decoded.transaction_amount.as_deref(), Some("5000"));
    assert_eq!(body["md5"], khqr_core::md5(qr));
    assert!(body["expires_at"].as_str().is_some());

    let id = body["id"].as_str().unwrap();
    let (status, fetched) = app
        .send(
            Request::get(format!("/payments/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["qr"], body["qr"]);
    assert_eq!(fetched["md5"], body["md5"]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn rejects_fractional_riel_and_keys_reused_for_a_different_request(pool: PgPool) {
    let app = TestApp::new(pool);

    let (fractional, _) = app.create_khqr_payment("order-1", 50_070, "KHR").await;
    new_khqr(&app, "order-2").await;
    let (reused, _) = app.create_khqr_payment("order-2", 2000, "USD").await;

    assert_eq!(fractional, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reused, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn paid_qr_settles_into_the_bakong_clearing_account(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.pay(&payment.md5, transfer("hash-1", 1000, unix_now_ms()));

    assert_eq!(app.poll().await, 1);

    assert_eq!(app.status_of(&payment.id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&payment.id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn unpaid_qr_stays_pending_and_backs_off(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;

    assert_eq!(app.poll().await, 1);
    assert_eq!(
        app.poll().await,
        0,
        "not due again until the backoff passes"
    );

    assert_eq!(app.status_of(&payment.id).await, "pending");
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn unpaid_qr_expires_only_after_the_grace_period(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;

    app.expire(&payment.id, "1 second").await;
    app.poll().await;
    assert_eq!(
        app.status_of(&payment.id).await,
        "pending",
        "Bakong may still be indexing a last-second payment"
    );

    app.expire(&payment.id, "3 minutes").await;
    app.poll().await;
    assert_eq!(app.status_of(&payment.id).await, "expired");
    assert!(app.ledger_lines_for(&payment.id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn payment_made_before_expiry_but_seen_after_is_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.expire(&payment.id, "10 seconds").await;
    app.pay(
        &payment.md5,
        transfer("hash-1", 1000, unix_now_ms() - 20_000),
    );

    app.poll().await;

    assert_eq!(app.status_of(&payment.id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&payment.id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn small_clock_skew_past_expiry_is_still_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.expire(&payment.id, "3 minutes").await;
    app.pay(
        &payment.md5,
        transfer("hash-1", 1000, unix_now_ms() - 150_000),
    );

    app.poll().await;

    assert_eq!(app.status_of(&payment.id).await, "succeeded");
    assert!(app.review_flags_for(&payment.id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn payment_made_after_expiry_is_flagged_not_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.expire(&payment.id, "5 minutes").await;
    app.pay(&payment.md5, transfer("hash-1", 1000, unix_now_ms()));

    app.poll().await;

    assert_eq!(app.status_of(&payment.id).await, "expired");
    assert!(app.ledger_lines_for(&payment.id).await.is_empty());
    assert_eq!(
        app.review_flags_for(&payment.id).await,
        ["paid_after_expiry"]
    );
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn bakong_outage_never_expires_a_payment(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.bakong.down.store(true, Ordering::SeqCst);
    app.expire(&payment.id, "1 minute").await;

    app.poll().await;

    assert_eq!(app.status_of(&payment.id).await, "pending");
    assert!(app.review_flags_for(&payment.id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn unverifiable_payment_is_flagged_and_polling_stops(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.bakong.down.store(true, Ordering::SeqCst);
    app.expire(&payment.id, "25 hours").await;

    app.poll().await;

    assert_eq!(app.status_of(&payment.id).await, "pending");
    assert_eq!(app.review_flags_for(&payment.id).await, ["unverifiable"]);
    let still_polled: bool =
        sqlx::query_scalar("select next_check_at is not null from payments where id = $1::uuid")
            .bind(&payment.id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(!still_polled);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn mismatched_transfer_is_flagged_not_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let wrong_amount = new_khqr(&app, "order-1").await;
    let wrong_account = new_khqr(&app, "order-2").await;
    app.pay(&wrong_amount.md5, transfer("hash-1", 999, unix_now_ms()));
    app.pay(
        &wrong_account.md5,
        KhqrTransfer {
            to_account_id: Some("someone_else@bank".to_owned()),
            ..transfer("hash-2", 1000, unix_now_ms())
        },
    );
    let missing_currency = new_khqr(&app, "order-3").await;
    app.pay(
        &missing_currency.md5,
        KhqrTransfer {
            currency: None,
            ..transfer("hash-3", 1000, unix_now_ms())
        },
    );

    assert_eq!(app.poll().await, 3);
    assert_eq!(app.poll().await, 0, "polling stops once flagged");

    for payment in [&wrong_amount, &wrong_account, &missing_currency] {
        assert_eq!(app.status_of(&payment.id).await, "pending");
        assert!(app.ledger_lines_for(&payment.id).await.is_empty());
        assert_eq!(app.review_flags_for(&payment.id).await, ["amount_mismatch"]);
    }
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn repeated_and_concurrent_polls_credit_once(pool: PgPool) {
    let app = TestApp::new(pool);
    let payment = new_khqr(&app, "order-1").await;
    app.pay(&payment.md5, transfer("hash-1", 1000, unix_now_ms()));

    let (a, b) = tokio::join!(app.poll(), app.poll());
    app.make_due(&payment.id).await;
    app.poll().await;

    assert_eq!(
        a + b,
        1,
        "a claimed payment is invisible to the other poller"
    );
    assert_eq!(app.ledger_lines_for(&payment.id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn one_bakong_transfer_never_credits_two_payments(pool: PgPool) {
    let app = TestApp::new(pool);
    let first = new_khqr(&app, "order-1").await;
    let second = new_khqr(&app, "order-2").await;
    app.pay(&first.md5, transfer("same-hash", 1000, unix_now_ms()));
    app.pay(&second.md5, transfer("same-hash", 1000, unix_now_ms()));

    app.poll().await;

    let statuses = [
        app.status_of(&first.id).await,
        app.status_of(&second.id).await,
    ];
    let flags = [
        app.review_flags_for(&first.id).await,
        app.review_flags_for(&second.id).await,
    ];
    let ledger_lines =
        app.ledger_lines_for(&first.id).await.len() + app.ledger_lines_for(&second.id).await.len();

    assert_eq!(
        statuses
            .iter()
            .filter(|status| *status == "succeeded")
            .count(),
        1
    );
    assert_eq!(
        flags
            .iter()
            .filter(|flags| **flags == ["duplicate_transfer"])
            .count(),
        1
    );
    assert_eq!(ledger_lines, 2, "exactly one balanced entry");
}
