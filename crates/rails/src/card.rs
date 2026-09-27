use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use dual_rail_core::{Currency, Money};
use stripe::{Client, ClientBuilder, IdempotencyKey, RequestStrategy, StripeRequest};
use stripe_core::payment_intent::CreatePaymentIntent;
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
    #[error("stripe returned a payment intent without a client secret")]
    MissingClientSecret,
}

#[async_trait]
pub trait CardGateway: Send + Sync {
    async fn create_payment_intent(
        &self,
        request: PaymentIntentRequest,
    ) -> Result<CreatedPaymentIntent, CardGatewayError>;
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
            .map_err(|err| CardGatewayError::Provider(err.to_string()))?;

        Ok(CreatedPaymentIntent {
            id: intent.id.to_string(),
            client_secret: intent
                .client_secret
                .ok_or(CardGatewayError::MissingClientSecret)?,
        })
    }
}

fn stripe_currency(currency: Currency) -> stripe_types::Currency {
    match currency {
        Currency::Usd => stripe_types::Currency::USD,
        Currency::Khr => stripe_types::Currency::KHR,
    }
}
