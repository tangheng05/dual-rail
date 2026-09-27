use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use dual_rail_core::{Currency, Money};
use stripe::{Client, ClientBuilder, IdempotencyKey, RequestStrategy, StripeError, StripeRequest};
use stripe_core::payment_intent::{CreatePaymentIntent, RetrievePaymentIntent};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentIntentRequest {
    pub payment_id: Uuid,
    pub amount: Money,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPaymentIntent {
    pub id: String,
    pub client_secret: String,
}

#[derive(Debug, Error)]
pub enum CardGatewayError {
    #[error("stripe request failed: {0}")]
    Provider(String),
    /// Stripe refused the request itself, so retrying cannot help.
    #[error("stripe rejected the payment: {0}")]
    Rejected(String),
    /// Another request with the same idempotency key is still running at Stripe.
    #[error("stripe is still processing a request with this idempotency key")]
    InProgress,
    #[error("stripe returned a payment intent without a client secret")]
    MissingClientSecret,
}

#[async_trait]
pub trait CardGateway: Send + Sync {
    async fn create_payment_intent(
        &self,
        request: PaymentIntentRequest,
    ) -> Result<CreatedPaymentIntent, CardGatewayError>;

    async fn client_secret(&self, payment_intent_id: &str) -> Result<String, CardGatewayError>;
}

pub struct StripeGateway {
    client: Client,
}

impl StripeGateway {
    pub fn new(secret_key: &str) -> Result<Self, CardGatewayError> {
        let client = ClientBuilder::new(secret_key)
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|err| CardGatewayError::Provider(err.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl CardGateway for StripeGateway {
    async fn create_payment_intent(
        &self,
        request: PaymentIntentRequest,
    ) -> Result<CreatedPaymentIntent, CardGatewayError> {
        // Keyed by our payment id, so a retried create can never open a second intent.
        let idempotency_key =
            IdempotencyKey::new(format!("dual-rail-payment-{}", request.payment_id))
                .map_err(|err| CardGatewayError::Provider(err.to_string()))?;

        let mut create = CreatePaymentIntent::new(
            request.amount.amount_minor(),
            stripe_currency(request.amount.currency()),
        )
        .payment_method_types(vec!["card".to_owned()])
        .metadata(HashMap::from([(
            "payment_id".to_owned(),
            request.payment_id.to_string(),
        )]));
        if let Some(description) = request.description {
            create = create.description(description);
        }

        let intent = create
            .customize()
            .request_strategy(RequestStrategy::Idempotent(idempotency_key))
            .send(&self.client)
            .await
            .map_err(classify)?;

        Ok(CreatedPaymentIntent {
            id: intent.id.to_string(),
            client_secret: intent
                .client_secret
                .ok_or(CardGatewayError::MissingClientSecret)?,
        })
    }

    async fn client_secret(&self, payment_intent_id: &str) -> Result<String, CardGatewayError> {
        RetrievePaymentIntent::new(payment_intent_id)
            .send(&self.client)
            .await
            .map_err(classify)?
            .client_secret
            .ok_or(CardGatewayError::MissingClientSecret)
    }
}

fn classify(err: StripeError) -> CardGatewayError {
    match err {
        StripeError::Stripe(_, 409) => CardGatewayError::InProgress,
        StripeError::Stripe(errors, status) if (400..500).contains(&status) && status != 429 => {
            CardGatewayError::Rejected(errors.message.unwrap_or_else(|| format!("http {status}")))
        }
        other => CardGatewayError::Provider(other.to_string()),
    }
}

fn stripe_currency(currency: Currency) -> stripe_types::Currency {
    match currency {
        Currency::Usd => stripe_types::Currency::USD,
        Currency::Khr => stripe_types::Currency::KHR,
    }
}
