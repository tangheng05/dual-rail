mod common;

use axum::http::StatusCode;
use common::{TestApp, WEBHOOK_SECRET, payment_intent_event, unix_now};
use dual_rail_rails::stripe_webhook::signature_header;
use serde_json::json;
use sqlx::PgPool;

async fn new_payment(app: &TestApp) -> String {
    let (status, body) = app.create_card_payment("order-1", 1000).await;
    assert_eq!(status, StatusCode::CREATED);
    body["id"].as_str().unwrap().to_owned()
}

fn credited(amount: i64) -> Vec<(String, String, i64)> {
    vec![
        ("clearing:stripe".to_owned(), "debit".to_owned(), amount),
        ("revenue".to_owned(), "credit".to_owned(), amount),
    ]
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn succeeded_event_settles_the_payment_and_writes_one_balanced_entry(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    let event = payment_intent_event("evt_1", "payment_intent.succeeded", &id, 1000);
    assert_eq!(app.deliver(&event).await, StatusCode::OK);

    assert_eq!(app.status_of(&id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn replaying_the_same_event_five_times_credits_once(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;
    let event = payment_intent_event("evt_1", "payment_intent.succeeded", &id, 1000);

    for _ in 0..5 {
        assert_eq!(app.deliver(&event).await, StatusCode::OK);
    }

    assert_eq!(app.status_of(&id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn concurrent_deliveries_credit_once(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;
    let first = payment_intent_event("evt_1", "payment_intent.succeeded", &id, 1000);
    let second = payment_intent_event("evt_2", "payment_intent.succeeded", &id, 1000);

    let (a, b) = tokio::join!(app.deliver(&first), app.deliver(&second));

    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    assert_eq!(app.ledger_lines_for(&id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn late_cancel_after_success_is_ignored(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.succeeded",
        &id,
        1000,
    ))
    .await;
    let status = app
        .deliver(&payment_intent_event(
            "evt_2",
            "payment_intent.canceled",
            &id,
            0,
        ))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.status_of(&id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn failed_attempt_keeps_the_payment_open_for_a_retry(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.payment_failed",
        &id,
        0,
    ))
    .await;
    assert_eq!(app.status_of(&id).await, "pending");

    app.deliver(&payment_intent_event(
        "evt_2",
        "payment_intent.succeeded",
        &id,
        1000,
    ))
    .await;
    assert_eq!(app.status_of(&id).await, "succeeded");
    assert_eq!(app.ledger_lines_for(&id).await, credited(1000));
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn canceled_intent_fails_the_payment_without_ledger_entries(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.canceled",
        &id,
        0,
    ))
    .await;

    assert_eq!(app.status_of(&id).await, "failed");
    assert!(app.ledger_lines_for(&id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn bad_signature_is_rejected_and_changes_nothing(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;
    let payload = payment_intent_event("evt_1", "payment_intent.succeeded", &id, 1000).to_string();

    let forged = signature_header(payload.as_bytes(), "whsec_attacker", unix_now());
    let stale = signature_header(payload.as_bytes(), WEBHOOK_SECRET, unix_now() - 3600);
    for signature in [forged.as_str(), stale.as_str(), "garbage"] {
        assert_eq!(
            app.deliver_raw(payload.clone(), signature).await,
            StatusCode::BAD_REQUEST
        );
    }

    assert_eq!(app.status_of(&id).await, "pending");
    assert!(app.ledger_lines_for(&id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn amount_mismatch_is_not_credited(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    app.deliver(&payment_intent_event(
        "evt_1",
        "payment_intent.succeeded",
        &id,
        999,
    ))
    .await;

    assert_eq!(app.status_of(&id).await, "pending");
    assert!(app.ledger_lines_for(&id).await.is_empty());
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn events_for_other_integrations_are_acknowledged_and_ignored(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;

    let foreign = json!({
        "id": "evt_1",
        "type": "payment_intent.succeeded",
        "data": { "object": { "id": "pi_other", "amount_received": 1000, "currency": "usd", "metadata": {} } }
    });
    let unrelated = json!({ "id": "evt_2", "type": "customer.created", "data": { "object": {} } });

    assert_eq!(app.deliver(&foreign).await, StatusCode::OK);
    assert_eq!(app.deliver(&unrelated).await, StatusCode::OK);
    assert_eq!(app.status_of(&id).await, "pending");
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn an_event_from_the_other_mode_never_settles_a_payment(pool: PgPool) {
    let app = TestApp::new(pool);
    let id = new_payment(&app).await;
    let mut live_event = payment_intent_event("evt_1", "payment_intent.succeeded", &id, 1000);
    live_event["livemode"] = json!(true);

    assert_eq!(app.deliver(&live_event).await, StatusCode::OK);

    assert_eq!(app.status_of(&id).await, "pending");
    assert!(app.ledger_lines_for(&id).await.is_empty());
}
