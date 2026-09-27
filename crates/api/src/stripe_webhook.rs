use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use dual_rail_core::{Outcome, Provider};
use dual_rail_rails::stripe_webhook::{self, EventKind, PaymentIntent};
use dual_rail_store::reviews::{self, ReviewReason};
use dual_rail_store::{events, payments};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::{AppState, settlement};

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
    if !events::record(&mut tx, Provider::Stripe, event_id).await? {
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
        payment.provider == Provider::Stripe
            && payment
                .provider_ref
                .as_deref()
                .is_none_or(|provider_ref| provider_ref == intent.id)
    }) else {
        tracing::warn!(%event_id, payment_intent = %intent.id, "stripe event does not match a stripe payment");
        return tx.commit().await;
    };

    if outcome == Outcome::Succeeded
        && payment.status.transition(outcome).is_ok()
        && !settlement::amount_matches(&payment, intent.amount_received, &intent.currency)
    {
        tracing::error!(%event_id, payment_id = %payment.id, "stripe amount does not match payment, flagged for review");
        reviews::flag(
            &mut tx,
            payment.id,
            ReviewReason::AmountMismatch,
            json!({
                "event_id": event_id,
                "payment_intent": intent.id,
                "amount_received": intent.amount_received,
                "currency": intent.currency,
            }),
        )
        .await?;
        return tx.commit().await;
    }

    settlement::apply(&mut tx, &payment, outcome, Some(&intent.id)).await?;
    tx.commit().await
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
