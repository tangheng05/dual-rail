#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use dual_rail_api::AppState;
use dual_rail_rails::card::{
    CardGateway, CardGatewayError, CreatedPaymentIntent, PaymentIntentRequest,
};
use dual_rail_rails::stripe_webhook::signature_header;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

pub const WEBHOOK_SECRET: &str = "whsec_test";

#[derive(Default)]
pub struct FakeCards {
    pub requests: Mutex<Vec<PaymentIntentRequest>>,
    pub fail: bool,
}

#[async_trait]
impl CardGateway for FakeCards {
    async fn create_payment_intent(
        &self,
        request: PaymentIntentRequest,
    ) -> Result<CreatedPaymentIntent, CardGatewayError> {
        let payment_id = request.payment_id;
        self.requests.lock().unwrap().push(request);
        if self.fail {
            return Err(CardGatewayError::Provider("stripe is down".to_owned()));
        }
        Ok(CreatedPaymentIntent {
            id: format!("pi_{}", payment_id.simple()),
            client_secret: format!("pi_{}_secret_test", payment_id.simple()),
        })
    }
}

pub struct TestApp {
    pub router: Router,
    pub cards: Arc<FakeCards>,
    pub pool: PgPool,
}

impl TestApp {
    pub fn new(pool: PgPool) -> Self {
        Self::with_cards(pool, FakeCards::default())
    }

    pub fn with_cards(pool: PgPool, cards: FakeCards) -> Self {
        let cards = Arc::new(cards);
        let router = dual_rail_api::app(AppState {
            pool: pool.clone(),
            cards: cards.clone(),
            stripe_webhook_secret: WEBHOOK_SECRET.into(),
        });
        Self {
            router,
            cards,
            pool,
        }
    }

    pub async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    pub async fn create_card_payment(&self, key: &str, amount_minor: i64) -> (StatusCode, Value) {
        self.send(create_payment_request(
            Some(key),
            json!({ "method": "card", "amount_minor": amount_minor, "currency": "USD" }),
        ))
        .await
    }

    pub async fn deliver(&self, event: &Value) -> StatusCode {
        let payload = event.to_string();
        let signature = signature_header(payload.as_bytes(), WEBHOOK_SECRET, unix_now());
        self.deliver_raw(payload, &signature).await
    }

    pub async fn deliver_raw(&self, payload: String, signature: &str) -> StatusCode {
        let request = Request::post("/webhooks/stripe")
            .header("stripe-signature", signature)
            .header("content-type", "application/json")
            .body(Body::from(payload))
            .unwrap();
        self.send(request).await.0
    }

    pub async fn status_of(&self, payment_id: &str) -> String {
        sqlx::query_scalar("select status from payments where id = $1::uuid")
            .bind(payment_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn ledger_lines_for(&self, payment_id: &str) -> Vec<(String, String, i64)> {
        sqlx::query_as(
            "select a.code, l.direction, l.amount_minor
             from ledger_lines l
             join journal_entries j on j.id = l.journal_entry_id
             join ledger_accounts a on a.id = l.account_id
             where j.payment_id = $1::uuid
             order by l.direction desc",
        )
        .bind(payment_id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

pub fn create_payment_request(key: Option<&str>, body: Value) -> Request<Body> {
    let mut request = Request::post("/payments").header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.body(Body::from(body.to_string())).unwrap()
}

pub fn payment_intent_event(
    event_id: &str,
    kind: &str,
    payment_id: &str,
    amount_received: i64,
) -> Value {
    json!({
        "id": event_id,
        "object": "event",
        "type": kind,
        "data": { "object": {
            "id": format!("pi_{}", payment_id.replace('-', "")),
            "object": "payment_intent",
            "amount_received": amount_received,
            "currency": "usd",
            "metadata": { "payment_id": payment_id }
        }}
    })
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
