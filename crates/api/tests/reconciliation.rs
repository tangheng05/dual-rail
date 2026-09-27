mod common;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use common::{KHQR_ACCOUNT, TestApp, payment_intent_event, unix_now_ms};
use dual_rail_api::RunSummary;
use dual_rail_rails::khqr::KhqrTransfer;
use sqlx::PgPool;

fn kinds(summary: &RunSummary) -> Vec<(&str, Option<String>)> {
    let mut kinds: Vec<_> = summary
        .mismatches
        .iter()
        .map(|mismatch| (mismatch.kind, mismatch.payment_id.map(|id| id.to_string())))
        .collect();
    kinds.sort();
    kinds
}

async fn card_payment(app: &TestApp, key: &str) -> String {
    let (status, body) = app.create_card_payment(key, 1000).await;
    assert_eq!(status, StatusCode::CREATED);
    body["id"].as_str().unwrap().to_owned()
}

async fn settled_card_payment(app: &TestApp, key: &str) -> String {
    let id = card_payment(app, key).await;
    app.stripe_succeeds(&id, 1000);
    let event = payment_intent_event(&format!("evt_{key}"), "payment_intent.succeeded", &id, 1000);
    assert_eq!(app.deliver(&event).await, StatusCode::OK);
    id
}

fn transfer(hash: &str) -> KhqrTransfer {
    KhqrTransfer {
        hash: hash.to_owned(),
        amount_minor: Some(1000),
        currency: Some("USD".to_owned()),
        to_account_id: Some(KHQR_ACCOUNT.to_owned()),
        paid_at_ms: Some(unix_now_ms()),
    }
}

async fn khqr_payment(app: &TestApp, key: &str) -> (String, String) {
    let (status, body) = app.create_khqr_payment(key, 1000, "USD").await;
    assert_eq!(status, StatusCode::CREATED);
    (
        body["id"].as_str().unwrap().to_owned(),
        body["md5"].as_str().unwrap().to_owned(),
    )
}

async fn settled_khqr_payment(app: &TestApp, key: &str) -> (String, String) {
    let (id, md5) = khqr_payment(app, key).await;
    app.pay(&md5, transfer(&format!("hash-{key}")));
    app.poll().await;
    assert_eq!(app.status_of(&id).await, "succeeded");
    (id, md5)
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn clean_day_completes_with_no_mismatches(pool: PgPool) {
    let app = TestApp::new(pool);
    settled_card_payment(&app, "card").await;
    settled_khqr_payment(&app, "khqr").await;
    let before = app.money_state().await;

    let summary = app.reconcile_today().await;

    assert_eq!(summary.status, "completed");
    assert_eq!(kinds(&summary), []);
    assert_eq!(
        app.money_state().await,
        before,
        "reconciliation never changes money"
    );
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn lost_stripe_webhook_is_missing_in_ledger(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = card_payment(&app, "card").await;
    app.stripe_succeeds(&id, 1000);
    let before = app.money_state().await;

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("missing_in_ledger", Some(id.clone()))]);
    assert_eq!(app.status_of(&id).await, "pending", "reported, not fixed");
    assert_eq!(app.money_state().await, before);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn success_stripe_does_not_confirm_is_missing_at_provider(pool: PgPool) {
    let app = TestApp::new(pool);
    let unconfirmed = card_payment(&app, "unconfirmed").await;
    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.succeeded",
        &unconfirmed,
        1000,
    ))
    .await;
    let vanished = settled_card_payment(&app, "vanished").await;
    app.cards
        .intents
        .lock()
        .unwrap()
        .remove(&format!("pi_{}", vanished.replace('-', "")));

    let summary = app.reconcile_today().await;

    let mut expected = vec![
        ("missing_at_provider", Some(unconfirmed)),
        ("missing_at_provider", Some(vanished)),
    ];
    expected.sort();
    assert_eq!(kinds(&summary), expected);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn stripe_amount_differing_from_ledger_is_an_amount_mismatch(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = settled_card_payment(&app, "card").await;
    app.stripe_succeeds(&id, 999);

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("amount_mismatch", Some(id))]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn khqr_bakong_no_longer_confirms_is_missing_at_provider(pool: PgPool) {
    let app = TestApp::new(pool);
    let (id, md5) = settled_khqr_payment(&app, "khqr").await;
    app.bakong.paid.lock().unwrap().remove(&md5);

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("missing_at_provider", Some(id))]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn khqr_paid_after_we_expired_it_is_flagged_not_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let (id, md5) = khqr_payment(&app, "khqr").await;
    app.expire(&id, "3 minutes").await;
    app.poll().await;
    assert_eq!(app.status_of(&id).await, "expired");
    app.pay(&md5, transfer("late-hash"));
    let before = app.money_state().await;

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("paid_after_expiry", Some(id.clone()))]);
    assert_eq!(app.review_flags_for(&id).await, ["paid_after_expiry"]);
    assert_eq!(app.money_state().await, before);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn payment_pending_for_days_is_stale(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = card_payment(&app, "card").await;
    sqlx::query("update payments set created_at = now() - interval '3 days' where id = $1::uuid")
        .bind(&id)
        .execute(&app.pool)
        .await
        .unwrap();

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("stale_pending", Some(id))]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn tampered_ledger_is_an_unbalanced_entry(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = settled_card_payment(&app, "card").await;
    sqlx::raw_sql(
        "alter table ledger_lines disable trigger ledger_lines_append_only;
         update ledger_lines set amount_minor = 1 where direction = 'credit';
         alter table ledger_lines enable trigger ledger_lines_append_only;",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let summary = app.reconcile_today().await;

    assert_eq!(kinds(&summary), [("unbalanced_entry", Some(id))]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn provider_outage_makes_the_run_incomplete(pool: PgPool) {
    let app = TestApp::new(pool);
    let (id, _) = settled_khqr_payment(&app, "khqr").await;
    app.bakong.down.store(true, Ordering::SeqCst);

    let summary = app.reconcile_today().await;

    assert_eq!(summary.status, "incomplete");
    assert_eq!(kinds(&summary), [("unverified", Some(id))]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn finished_day_is_skipped_by_the_scheduler_but_can_be_rerun_manually(pool: PgPool) {
    let app = TestApp::new(pool);
    let first = app.reconcile_today().await;

    assert!(
        dual_rail_store::reconciliation::has_finished_run(&app.pool, first.run_date)
            .await
            .unwrap()
    );
    let rerun = app.reconcile_today().await;
    assert_ne!(rerun.run_id, first.run_id);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn only_one_run_per_day_can_be_in_progress(pool: PgPool) {
    let date = time::macros::date!(2026 - 09 - 26);

    let first = dual_rail_store::reconciliation::claim(&pool, date)
        .await
        .unwrap();
    let second = dual_rail_store::reconciliation::claim(&pool, date)
        .await
        .unwrap();

    assert!(first.is_some());
    assert_eq!(second, None);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn incomplete_day_is_retried_hourly(pool: PgPool) {
    let app = TestApp::new(pool);
    settled_khqr_payment(&app, "khqr").await;
    app.bakong.down.store(true, Ordering::SeqCst);
    let run = app.reconcile_today().await;
    let done = || dual_rail_store::reconciliation::has_finished_run(&app.pool, run.run_date);

    assert_eq!(run.status, "incomplete");
    assert!(done().await.unwrap(), "not retried within the hour");
    sqlx::query("update reconciliation_runs set finished_at = now() - interval '2 hours'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(!done().await.unwrap(), "retried once the hour has passed");
}
