use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use dual_rail_core::{Currency, Money, Outcome, PaymentMethod, PaymentStatus, Provider};
use dual_rail_rails::card::{CardGatewayError, PaymentIntentRequest};
use dual_rail_rails::khqr::IssueError;
use dual_rail_store::payments::{self, NewKhqrPayment, NewPayment, Payment};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
) -> Result<Response, ApiError> {
    let idempotency_key = idempotency_key(&headers)?;
    let Json(body) = payload.map_err(|rejection| ApiError::Unprocessable(rejection.body_text()))?;
    let method: PaymentMethod = body.method.parse()?;
    let amount = Money::new(body.amount_minor, body.currency.parse()?)?;
    let description = body.description.as_deref();
    if description.is_some_and(|description| description.chars().count() > MAX_DESCRIPTION_LEN) {
        return Err(ApiError::Unprocessable(format!(
            "description must be at most {MAX_DESCRIPTION_LEN} characters"
        )));
    }

    let request_hash = fingerprint(method, amount, description);
    match method {
        PaymentMethod::Card => {
            create_card(&state, idempotency_key, &request_hash, amount, description).await
        }
        PaymentMethod::Khqr => create_khqr(&state, idempotency_key, &request_hash, amount).await,
    }
}

async fn create_card(
    state: &AppState,
    idempotency_key: &str,
    request_hash: &str,
    amount: Money,
    description: Option<&str>,
) -> Result<Response, ApiError> {
    let method = PaymentMethod::Card;
    let provider = Provider::for_method(method);
    if amount.currency() == Currency::Usd && !STRIPE_USD_RANGE.contains(&amount.amount_minor()) {
        return Err(ApiError::Unprocessable(
            "card payments in USD must be between 50 and 99999999 minor units".to_owned(),
        ));
    }
    let new_payment = NewPayment {
        method,
        provider,
        amount,
        idempotency_key,
        request_hash,
        description,
    };
    let Some(payment_id) = payments::insert_pending(&state.pool, &new_payment).await? else {
        return replay(state, idempotency_key, request_hash).await;
    };

    let client_secret = link_card_intent(state, payment_id, amount, description).await?;
    Ok(created(
        PaymentResponse {
            client_secret: Some(client_secret),
            ..pending_response(payment_id, method, provider, amount)
        },
        false,
    ))
}

/// Creates (or, on a retry, re-fetches) the payment's Stripe intent and links it.
/// The Stripe idempotency key is derived from the payment id and the parameters
/// come from the stored request, so a retry can never open a second intent.
async fn link_card_intent(
    state: &AppState,
    payment_id: Uuid,
    amount: Money,
    description: Option<&str>,
) -> Result<String, ApiError> {
    let intent = state
        .cards
        .create_payment_intent(PaymentIntentRequest {
            payment_id,
            amount,
            description: description.map(str::to_owned),
        })
        .await;
    match intent {
        Ok(intent) => {
            payments::set_provider_ref(&state.pool, payment_id, &intent.id).await?;
            tracing::info!(%payment_id, payment_intent = %intent.id, "card payment linked to stripe");
            Ok(intent.client_secret)
        }
        Err(CardGatewayError::Rejected(reason)) => {
            tracing::warn!(%payment_id, %reason, "stripe rejected payment intent");
            fail_rejected(state, payment_id).await?;
            Err(ApiError::Unprocessable(reason))
        }
        Err(CardGatewayError::InProgress) => Err(still_in_progress()),
        Err(err) => {
            tracing::error!(%payment_id, %err, "could not create stripe payment intent");
            Err(ApiError::BadGateway)
        }
    }
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
    request_hash: &str,
    amount: Money,
) -> Result<Response, ApiError> {
    let method = PaymentMethod::Khqr;
    let payment_id = Uuid::new_v4();
    let created_at = whole_millis(OffsetDateTime::now_utc());
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
            request_hash,
            payload: &qr.payload,
            md5: &qr.md5,
            expires_at,
        },
    )
    .await?;
    if !inserted {
        return replay(state, idempotency_key, request_hash).await;
    }
    tracing::info!(%payment_id, md5 = %qr.md5, "khqr payment created");

    Ok(created(
        PaymentResponse {
            qr: Some(qr.payload),
            md5: Some(qr.md5),
            expires_at: Some(expires_at),
            ..pending_response(payment_id, method, Provider::for_method(method), amount)
        },
        false,
    ))
}

/// Answers a repeated Idempotency-Key from the stored payment instead of creating
/// a new one. A card payment still pending gets its client secret again, finishing
/// the Stripe link first if the original request failed before it.
async fn replay(
    state: &AppState,
    idempotency_key: &str,
    request_hash: &str,
) -> Result<Response, ApiError> {
    let payment = payments::find_by_idempotency_key(&state.pool, idempotency_key)
        .await?
        .ok_or(ApiError::Internal)?;
    if payment.request_hash.as_deref() != Some(request_hash) {
        return Err(ApiError::Conflict(
            "Idempotency-Key was already used with a different request".to_owned(),
        ));
    }

    let is_card = payment.method == PaymentMethod::Card;
    if is_card && payment.status == PaymentStatus::Failed && payment.provider_ref.is_none() {
        let rejected =
            ApiError::Unprocessable("the card provider rejected this payment".to_owned());
        return Ok(with_replayed_header(rejected.into_response()));
    }

    let client_secret = if is_card && payment.status == PaymentStatus::Pending {
        Some(match &payment.provider_ref {
            Some(payment_intent) => state.cards.client_secret(payment_intent).await.map_err(
                |err| match err {
                    CardGatewayError::InProgress => still_in_progress(),
                    err => {
                        tracing::error!(payment_id = %payment.id, %err, "could not fetch stripe client secret");
                        ApiError::BadGateway
                    }
                },
            )?,
            None => {
                link_card_intent(
                    state,
                    payment.id,
                    payment.amount,
                    payment.description.as_deref(),
                )
                .await?
            }
        })
    } else {
        None
    };

    tracing::info!(payment_id = %payment.id, "idempotent replay");
    Ok(created(
        PaymentResponse {
            client_secret,
            ..payment.into()
        },
        true,
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

pub async fn qr_svg(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let payload = payments::find(&state.pool, id)
        .await?
        .and_then(|payment| payment.khqr_payload)
        .ok_or(ApiError::NotFound)?;
    let svg = khqr_core::to_svg(&payload).map_err(|err| {
        tracing::error!(payment_id = %id, %err, "could not render khqr");
        ApiError::Internal
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "private, max-age=600"),
        ],
        svg,
    )
        .into_response())
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

fn still_in_progress() -> ApiError {
    ApiError::Conflict(
        "a request with this Idempotency-Key is still in progress, retry shortly".to_owned(),
    )
}

fn created(body: PaymentResponse, replayed: bool) -> Response {
    let response = (StatusCode::CREATED, Json(body)).into_response();
    if replayed {
        with_replayed_header(response)
    } else {
        response
    }
}

fn with_replayed_header(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("idempotent-replayed", HeaderValue::from_static("true"));
    response
}

/// Hashes the parsed request, so key order and whitespace in the JSON body do not
/// make an identical request look different.
fn fingerprint(method: PaymentMethod, amount: Money, description: Option<&str>) -> String {
    let canonical = serde_json::json!([
        method.as_str(),
        amount.amount_minor(),
        amount.currency().as_str(),
        description,
    ]);
    hex::encode(Sha256::digest(canonical.to_string()))
}

/// The QR carries milliseconds and Postgres keeps microseconds, so a clock with
/// nanoseconds would make the QR, the stored row and the response disagree.
fn whole_millis(at: OffsetDateTime) -> OffsetDateTime {
    at.replace_nanosecond(at.nanosecond() / 1_000_000 * 1_000_000)
        .expect("rounding down keeps the nanosecond in range")
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

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn khqr_timestamps_are_whole_milliseconds() {
        let from_a_nanosecond_clock = datetime!(2026-09-27 05:37:36.992_330_72 UTC);

        let stored = whole_millis(from_a_nanosecond_clock);

        assert_eq!(stored, datetime!(2026-09-27 05:37:36.992 UTC));
        assert_eq!(unix_ms(stored), unix_ms(from_a_nanosecond_clock));
    }
}
