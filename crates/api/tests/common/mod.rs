#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use dual_rail_api::{AppState, HttpSettings, hash_api_key};
use dual_rail_rails::card::{
    CardGateway, CardGatewayError, CreatedPaymentIntent, IntentSnapshot, PaymentIntentRequest,
};
use dual_rail_rails::khqr::{
    KhqrIssuer, KhqrStatus, KhqrTransfer, KhqrVerifier, MerchantAccount, VerifierError,
};
use dual_rail_rails::stripe_webhook::signature_header;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

pub const WEBHOOK_SECRET: &str = "whsec_test";
pub const KHQR_ACCOUNT: &str = "dual_rail@devb";
pub const API_KEY: &str = "drk_0000000000000000000000000000000000000000000000000000000000000001";
pub const CLIENT_TOKEN_SECRET: &[u8] = b"a-test-client-token-secret-32-bytes!";

#[derive(Default)]
pub struct FakeCards {
    pub requests: Mutex<Vec<PaymentIntentRequest>>,
    pub secret_lookups: AtomicUsize,
    pub fail: AtomicBool,
    pub reject: bool,
    pub in_progress: AtomicBool,
    pub intents: Mutex<HashMap<String, IntentSnapshot>>,
    /// Makes Stripe slow, to push a request past the HTTP timeout.
    pub delay: Mutex<Option<Duration>>,
}

impl FakeCards {
    pub fn failing() -> Self {
        Self {
            fail: AtomicBool::new(true),
            ..Self::default()
        }
    }

    pub fn creates(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl CardGateway for FakeCards {
    async fn create_payment_intent(
        &self,
        request: PaymentIntentRequest,
    ) -> Result<CreatedPaymentIntent, CardGatewayError> {
        let payment_id = request.payment_id;
        self.requests.lock().unwrap().push(request);
        let delay = *self.delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if self.in_progress.load(Ordering::SeqCst) {
            return Err(CardGatewayError::InProgress);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(CardGatewayError::Provider("stripe is down".to_owned()));
        }
        if self.reject {
            return Err(CardGatewayError::Rejected(
                "Amount must be no more than ៛999,999.99".to_owned(),
            ));
        }
        let id = format!("pi_{}", payment_id.simple());
        self.intents
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_insert_with(|| IntentSnapshot {
                id: id.clone(),
                status: "requires_payment_method".to_owned(),
                amount_received: 0,
                currency: "usd".to_owned(),
                payment_id: Some(payment_id.to_string()),
            });
        Ok(CreatedPaymentIntent {
            client_secret: format!("{id}_secret_test"),
            id,
        })
    }

    async fn payment_intent(
        &self,
        payment_intent_id: &str,
    ) -> Result<Option<IntentSnapshot>, CardGatewayError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(CardGatewayError::Provider("stripe is down".to_owned()));
        }
        Ok(self.intents.lock().unwrap().get(payment_intent_id).cloned())
    }

    async fn succeeded_intents(
        &self,
        _from_unix: i64,
        _to_unix: i64,
    ) -> Result<Vec<IntentSnapshot>, CardGatewayError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(CardGatewayError::Provider("stripe is down".to_owned()));
        }
        Ok(self
            .intents
            .lock()
            .unwrap()
            .values()
            .filter(|intent| intent.succeeded())
            .cloned()
            .collect())
    }

    async fn client_secret(&self, payment_intent_id: &str) -> Result<String, CardGatewayError> {
        self.secret_lookups.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(CardGatewayError::Provider("stripe is down".to_owned()));
        }
        Ok(format!("{payment_intent_id}_secret_test"))
    }
}

#[derive(Default)]
pub struct FakeBakong {
    pub paid: Mutex<HashMap<String, KhqrTransfer>>,
    pub down: AtomicBool,
    pub calls: AtomicUsize,
}

#[async_trait]
impl KhqrVerifier for FakeBakong {
    async fn check(&self, md5s: &[String]) -> Result<Vec<KhqrStatus>, VerifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(VerifierError("bakong returned http 403".to_owned()));
        }
        let paid = self.paid.lock().unwrap();
        Ok(md5s
            .iter()
            .map(|md5| {
                paid.get(md5)
                    .cloned()
                    .map_or(KhqrStatus::Unpaid, KhqrStatus::Paid)
            })
            .collect())
    }
}

pub struct TestApp {
    pub router: Router,
    pub state: AppState,
    pub cards: Arc<FakeCards>,
    pub bakong: Arc<FakeBakong>,
    pub pool: PgPool,
}

impl TestApp {
    pub async fn new(pool: PgPool) -> Self {
        Self::with_cards(pool, FakeCards::default()).await
    }

    pub async fn with_cards(pool: PgPool, cards: FakeCards) -> Self {
        Self::configured(pool, cards, HttpSettings::default(), false).await
    }

    pub async fn demo(pool: PgPool) -> Self {
        Self::configured(pool, FakeCards::default(), HttpSettings::default(), true).await
    }

    pub async fn configured(
        pool: PgPool,
        cards: FakeCards,
        http: HttpSettings,
        demo_mode: bool,
    ) -> Self {
        dual_rail_store::api_keys::insert(&pool, "tests", &API_KEY[..12], &hash_api_key(API_KEY))
            .await
            .unwrap();
        let cards = Arc::new(cards);
        let bakong = Arc::new(FakeBakong::default());
        let state = AppState {
            pool: pool.clone(),
            cards: cards.clone(),
            stripe_webhook_secret: WEBHOOK_SECRET.into(),
            stripe_livemode: false,
            stripe_publishable_key: Some("pk_test_demo".into()),
            khqr: Arc::new(KhqrIssuer::new(MerchantAccount {
                account_id: KHQR_ACCOUNT.to_owned(),
                merchant_name: "Dual Rail".to_owned(),
                merchant_city: "Phnom Penh".to_owned(),
                merchant_id: None,
                acquiring_bank: None,
            })),
            verifier: bakong.clone(),
            khqr_ttl: Duration::from_secs(300),
            http,
            client_token_secret: CLIENT_TOKEN_SECRET.into(),
            demo_mode,
        };
        Self {
            router: dual_rail_api::app(state.clone()),
            state,
            cards,
            bakong,
            pool,
        }
    }

    pub async fn create_khqr_payment(
        &self,
        key: &str,
        amount_minor: i64,
        currency: &str,
    ) -> (StatusCode, Value) {
        self.send(create_payment_request(
            Some(key),
            json!({ "method": "khqr", "amount_minor": amount_minor, "currency": currency }),
        ))
        .await
    }

    pub fn pay(&self, md5: &str, transfer: KhqrTransfer) {
        self.bakong
            .paid
            .lock()
            .unwrap()
            .insert(md5.to_owned(), transfer);
    }

    pub async fn poll(&self) -> usize {
        dual_rail_api::poll_once(&self.state).await.unwrap()
    }

    pub async fn make_due(&self, payment_id: &str) {
        sqlx::query("update payments set next_check_at = now() where id = $1::uuid")
            .bind(payment_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    pub async fn expire(&self, payment_id: &str, ago: &str) {
        sqlx::query(
            "update payments set expires_at = now() - $2::interval, next_check_at = now()
             where id = $1::uuid",
        )
        .bind(payment_id)
        .bind(ago)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Marks the fake Stripe intent for `payment_id` as succeeded with `amount_received`.
    pub fn stripe_succeeds(&self, payment_id: &str, amount_received: i64) {
        let id = format!("pi_{}", payment_id.replace('-', ""));
        let mut intents = self.cards.intents.lock().unwrap();
        let intent = intents.get_mut(&id).expect("intent was created");
        intent.status = "succeeded".to_owned();
        intent.amount_received = amount_received;
    }

    pub async fn reconcile_today(&self) -> dual_rail_api::RunSummary {
        let offset = time::macros::offset!(+7);
        let today = time::OffsetDateTime::now_utc().to_offset(offset).date();
        dual_rail_api::reconcile(&self.state, today, offset)
            .await
            .unwrap()
            .expect("no other run in progress")
    }

    /// Everything reconciliation must never change.
    pub async fn money_state(&self) -> Vec<(String, String, i64)> {
        sqlx::query_as(
            "select p.id::text, p.status, coalesce(sum(l.amount_minor), 0)::bigint
             from payments p
             left join journal_entries j on j.payment_id = p.id
             left join ledger_lines l on l.journal_entry_id = j.id
             group by p.id, p.status order by p.id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    pub async fn review_flags_for(&self, payment_id: &str) -> Vec<String> {
        sqlx::query_scalar(
            "select reason from review_flags where payment_id = $1::uuid order by reason",
        )
        .bind(payment_id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    pub async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let (status, _, body) = self.send_full(request).await;
        (status, body)
    }

    pub async fn send_raw(&self, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    pub async fn send_full(&self, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, body)
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

/// A GET the merchant makes with its API key.
pub fn api_get(path: impl AsRef<str>) -> Request<Body> {
    Request::get(path.as_ref())
        .header("authorization", format!("Bearer {API_KEY}"))
        .body(Body::empty())
        .unwrap()
}

pub fn create_payment_request(key: Option<&str>, body: Value) -> Request<Body> {
    let mut request = Request::post("/payments")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {API_KEY}"));
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

pub fn unix_now_ms() -> i64 {
    unix_now() * 1000
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
