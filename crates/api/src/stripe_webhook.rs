use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use dual_rail_core::{JournalEntry, Outcome, PaymentMethod, PaymentStatus};
use dual_rail_rails::stripe_webhook::{self, EventKind, PaymentIntent};
use dual_rail_store::events::{self, EventSource};
use dual_rail_store::{ledger, payments};
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest("Stripe-Signature header is required".to_owned()))?;
    stripe_webhook::verify_signature(&body, signature, &state.stripe_webhook_secret, unix_now())
        .map_err(|err| {
            tracing::warn!(%err, "rejected stripe webhook");
            ApiError::BadRequest("invalid signature".to_owned())
        })?;
    let event = stripe_webhook::parse_event(&body).map_err(|err| {
        tracing::error!(%err, "signed stripe webhook could not be parsed");
        ApiError::BadRequest("unreadable event".to_owned())
    })?;

    match event.kind {
        EventKind::PaymentIntentSucceeded(intent) => {
            settle(&state.pool, &event.id, &intent, Outcome::Succeeded).await?;
        }
        EventKind::PaymentIntentCanceled(intent) => {
            settle(&state.pool, &event.id, &intent, Outcome::Failed).await?;
        }
        // Not terminal at Stripe: the customer can retry the same intent and still succeed.
        EventKind::PaymentIntentPaymentFailed(intent) => {
            tracing::info!(event_id = %event.id, payment_intent = %intent.id, "card attempt failed, payment stays pending");
        }
        EventKind::Ignored(kind) => {
            tracing::debug!(event_id = %event.id, %kind, "ignored stripe event");
        }
    }
    Ok(StatusCode::OK)
}

async fn settle(
    pool: &PgPool,
    event_id: &str,
    intent: &PaymentIntent,
    outcome: Outcome,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !events::record(&mut tx, EventSource::Stripe, event_id).await? {
        tracing::info!(%event_id, "duplicate stripe event");
        return Ok(());
    }

    let payment_id = intent
        .metadata
        .get("payment_id")
        .and_then(|id| id.parse::<Uuid>().ok());
    let payment = match payment_id {
        Some(id) => payments::lock(&mut tx, id).await?,
        None => None,
    };
    let Some(payment) = payment.filter(|payment| {
        payment.method == PaymentMethod::Card
            && payment
                .provider_ref
                .as_deref()
                .is_none_or(|provider_ref| provider_ref == intent.id)
    }) else {
        tracing::warn!(%event_id, payment_intent = %intent.id, "stripe event does not match a card payment");
        return tx.commit().await;
    };

    let next = match payment.status.transition(outcome) {
        Ok(next) => next,
        Err(err) => {
            tracing::info!(%event_id, payment_id = %payment.id, %err, "late stripe event ignored");
            return tx.commit().await;
        }
    };

    if next == PaymentStatus::Succeeded
        && (intent.amount_received != payment.amount.amount_minor()
            || !intent
                .currency
                .eq_ignore_ascii_case(payment.amount.currency().as_str()))
    {
        tracing::error!(
            %event_id,
            payment_id = %payment.id,
            expected = payment.amount.amount_minor(),
            received = intent.amount_received,
            currency = %intent.currency,
            "stripe amount does not match payment, left pending for reconciliation"
        );
        return tx.commit().await;
    }

    if payments::transition(&mut tx, payment.id, next, &intent.id).await?
        && next == PaymentStatus::Succeeded
    {
        let entry = JournalEntry::for_successful_payment(payment.method, payment.amount);
        ledger::insert_entry(&mut tx, payment.id, &entry).await?;
    }
    tx.commit().await?;
    tracing::info!(%event_id, payment_id = %payment.id, status = %next, "card payment settled");
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
