use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dual_rail_core::{Currency, Money, Outcome, PaymentMethod, PaymentStatus, Provider};
use dual_rail_rails::card::{CardGatewayError, PaymentIntentRequest};
use dual_rail_rails::khqr::IssueError;
use dual_rail_store::payments::{self, NewKhqrPayment, Payment};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
const MAX_DESCRIPTION_LEN: usize = 1000;
// Stripe's published USD card limits: $0.50 to $999,999.99.
const STRIPE_USD_RANGE: std::ops::RangeInclusive<i64> = 50..=99_999_999;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePayment {
    method: String,
    amount_minor: i64,
    currency: String,
    description: Option<String>,
}

#[derive(Serialize)]
pub struct PaymentResponse {
    id: Uuid,
    method: &'static str,
    provider: &'static str,
    status: &'static str,
    amount_minor: i64,
    currency: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    expires_at: Option<OffsetDateTime>,
}

impl From<Payment> for PaymentResponse {
    fn from(payment: Payment) -> Self {
        let is_khqr = payment.method == PaymentMethod::Khqr;
        Self {
            id: payment.id,
            method: payment.method.as_str(),
            provider: payment.provider.as_str(),
            status: payment.status.as_str(),
            amount_minor: payment.amount.amount_minor(),
            currency: payment.amount.currency().as_str(),
            client_secret: None,
            md5: payment.provider_ref.filter(|_| is_khqr),
            qr: payment.khqr_payload,
            expires_at: payment.expires_at,
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreatePayment>, JsonRejection>,
) -> Result<(StatusCode, Json<PaymentResponse>), ApiError> {
    let idempotency_key = idempotency_key(&headers)?;
    let Json(body) = payload.map_err(|rejection| ApiError::Unprocessable(rejection.body_text()))?;
    let method: PaymentMethod = body.method.parse()?;
    let amount = Money::new(body.amount_minor, body.currency.parse()?)?;
    if body
        .description
        .as_ref()
        .is_some_and(|description| description.chars().count() > MAX_DESCRIPTION_LEN)
    {
        return Err(ApiError::Unprocessable(format!(
            "description must be at most {MAX_DESCRIPTION_LEN} characters"
        )));
    }

    let response = match method {
        PaymentMethod::Card => {
            create_card(&state, idempotency_key, amount, body.description).await?
        }
        PaymentMethod::Khqr => create_khqr(&state, idempotency_key, amount).await?,
    };
    Ok((StatusCode::CREATED, Json(response)))
}

async fn create_card(
    state: &AppState,
    idempotency_key: &str,
    amount: Money,
    description: Option<String>,
) -> Result<PaymentResponse, ApiError> {
    let method = PaymentMethod::Card;
    let provider = Provider::for_method(method);
    if amount.currency() == Currency::Usd && !STRIPE_USD_RANGE.contains(&amount.amount_minor()) {
        return Err(ApiError::Unprocessable(
            "card payments in USD must be between 50 and 99999999 minor units".to_owned(),
        ));
    }
    let Some(payment_id) =
        payments::insert_pending(&state.pool, method, provider, amount, idempotency_key).await?
    else {
        return Err(key_already_used());
    };

    let intent = state
        .cards
        .create_payment_intent(PaymentIntentRequest {
            payment_id,
            amount,
            description,
        })
        .await;
    let intent = match intent {
        Ok(intent) => intent,
        Err(CardGatewayError::Rejected(reason)) => {
            tracing::warn!(%payment_id, %reason, "stripe rejected payment intent");
            fail_rejected(state, payment_id).await?;
            return Err(ApiError::Unprocessable(reason));
        }
        Err(err) => {
            tracing::error!(%payment_id, %err, "could not create stripe payment intent");
            return Err(ApiError::BadGateway);
        }
    };
    payments::set_provider_ref(&state.pool, payment_id, &intent.id).await?;
    tracing::info!(%payment_id, payment_intent = %intent.id, "card payment created");

    Ok(PaymentResponse {
        client_secret: Some(intent.client_secret),
        ..pending_response(payment_id, method, provider, amount)
    })
}

/// A request Stripe refused can never be paid, so it must not sit pending.
async fn fail_rejected(state: &AppState, payment_id: Uuid) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    if let Some(payment) = payments::lock(&mut tx, payment_id).await? {
        crate::settlement::apply(&mut tx, &payment, Outcome::Failed, None).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn create_khqr(
    state: &AppState,
    idempotency_key: &str,
    amount: Money,
) -> Result<PaymentResponse, ApiError> {
    let method = PaymentMethod::Khqr;
    let payment_id = Uuid::new_v4();
    let created_at = OffsetDateTime::now_utc();
    let expires_at = created_at + state.khqr_ttl;
    let qr = state
        .khqr
        .issue(payment_id, amount, unix_ms(created_at), unix_ms(expires_at))
        .map_err(|err| match err {
            IssueError::FractionalRiel(_) | IssueError::TooLarge(_) => {
                ApiError::Unprocessable(err.to_string())
            }
            other => {
                tracing::error!(%payment_id, err = %other, "could not issue khqr");
                ApiError::Internal
            }
        })?;

    let inserted = payments::insert_pending_khqr(
        &state.pool,
        &NewKhqrPayment {
            id: payment_id,
            amount,
            idempotency_key,
            payload: &qr.payload,
            md5: &qr.md5,
            expires_at,
        },
    )
    .await?;
    if !inserted {
        return Err(key_already_used());
    }
    tracing::info!(%payment_id, md5 = %qr.md5, "khqr payment created");

    Ok(PaymentResponse {
        qr: Some(qr.payload),
        md5: Some(qr.md5),
        expires_at: Some(expires_at),
        ..pending_response(payment_id, method, Provider::for_method(method), amount)
    })
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PaymentResponse>, ApiError> {
    let payment = payments::find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(payment.into()))
}

fn pending_response(
    id: Uuid,
    method: PaymentMethod,
    provider: Provider,
    amount: Money,
) -> PaymentResponse {
    PaymentResponse {
        id,
        method: method.as_str(),
        provider: provider.as_str(),
        status: PaymentStatus::Pending.as_str(),
        amount_minor: amount.amount_minor(),
        currency: amount.currency().as_str(),
        client_secret: None,
        qr: None,
        md5: None,
        expires_at: None,
    }
}

fn key_already_used() -> ApiError {
    ApiError::Conflict("Idempotency-Key has already been used".to_owned())
}

fn unix_ms(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

fn idempotency_key(headers: &HeaderMap) -> Result<&str, ApiError> {
    let key = headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::BadRequest("Idempotency-Key header is required".to_owned()))?
        .to_str()
        .map_err(|_| ApiError::BadRequest("Idempotency-Key must be ASCII".to_owned()))?;
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(ApiError::BadRequest(format!(
            "Idempotency-Key must be 1 to {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }
    Ok(key)
}
