use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dual_rail_core::{Money, PaymentMethod};
use dual_rail_rails::card::PaymentIntentRequest;
use dual_rail_store::payments::{self, Payment};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
const MAX_DESCRIPTION_LEN: usize = 1000;

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
    status: &'static str,
    amount_minor: i64,
    currency: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
}

impl From<Payment> for PaymentResponse {
    fn from(payment: Payment) -> Self {
        Self {
            id: payment.id,
            method: payment.method.as_str(),
            status: payment.status.as_str(),
            amount_minor: payment.amount.amount_minor(),
            currency: payment.amount.currency().as_str(),
            client_secret: None,
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
    if method == PaymentMethod::Khqr {
        return Err(ApiError::Unprocessable(
            "khqr payments are not available yet".to_owned(),
        ));
    }

    let Some(payment_id) =
        payments::insert_pending(&state.pool, method, amount, idempotency_key).await?
    else {
        return Err(ApiError::Conflict(
            "Idempotency-Key has already been used".to_owned(),
        ));
    };

    let intent = state
        .cards
        .create_payment_intent(PaymentIntentRequest {
            payment_id,
            amount,
            description: body.description,
        })
        .await
        .map_err(|err| {
            tracing::error!(%payment_id, %err, "could not create stripe payment intent");
            ApiError::BadGateway
        })?;
    payments::set_provider_ref(&state.pool, payment_id, &intent.id).await?;
    tracing::info!(%payment_id, payment_intent = %intent.id, "card payment created");

    Ok((
        StatusCode::CREATED,
        Json(PaymentResponse {
            id: payment_id,
            method: method.as_str(),
            status: "pending",
            amount_minor: amount.amount_minor(),
            currency: amount.currency().as_str(),
            client_secret: Some(intent.client_secret),
        }),
    ))
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
