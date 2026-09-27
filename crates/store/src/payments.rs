use dual_rail_core::{Money, PaymentMethod, PaymentStatus, Provider};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::decode_error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    pub id: Uuid,
    pub method: PaymentMethod,
    pub provider: Provider,
    pub status: PaymentStatus,
    pub amount: Money,
    pub provider_ref: Option<String>,
    pub khqr_payload: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
}

struct PaymentRow {
    id: Uuid,
    method: String,
    provider: String,
    status: String,
    amount_minor: i64,
    currency: String,
    provider_ref: Option<String>,
    khqr_payload: Option<String>,
    expires_at: Option<OffsetDateTime>,
}

impl TryFrom<PaymentRow> for Payment {
    type Error = sqlx::Error;

    fn try_from(row: PaymentRow) -> Result<Self, Self::Error> {
        let currency = row.currency.parse().map_err(decode_error)?;
        Ok(Self {
            id: row.id,
            method: row.method.parse().map_err(decode_error)?,
            provider: row.provider.parse().map_err(decode_error)?,
            status: row.status.parse().map_err(decode_error)?,
            amount: Money::new(row.amount_minor, currency).map_err(decode_error)?,
            provider_ref: row.provider_ref,
            khqr_payload: row.khqr_payload,
            expires_at: row.expires_at,
        })
    }
}

pub struct NewKhqrPayment<'a> {
    pub id: Uuid,
    pub amount: Money,
    pub idempotency_key: &'a str,
    pub payload: &'a str,
    pub md5: &'a str,
    pub expires_at: OffsetDateTime,
}

/// A pending KHQR payment claimed for one Bakong check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueKhqr {
    pub id: Uuid,
    pub md5: String,
    pub expires_at: OffsetDateTime,
    pub check_attempts: i32,
    pub past_expiry: bool,
    /// Past expiry plus the grace that lets Bakong index a last-second payment.
    pub past_final_check: bool,
    pub past_verification_deadline: bool,
}

/// Returns None when the idempotency key is already taken.
pub async fn insert_pending(
    pool: &PgPool,
    method: PaymentMethod,
    provider: Provider,
    amount: Money,
    idempotency_key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "insert into payments (method, provider, status, amount_minor, currency, idempotency_key)
         values ($1, $2, 'pending', $3, $4, $5)
         on conflict (idempotency_key) do nothing
         returning id",
        method.as_str(),
        provider.as_str(),
        amount.amount_minor(),
        amount.currency().as_str(),
        idempotency_key,
    )
    .fetch_optional(pool)
    .await
}

/// Returns false when the idempotency key is already taken.
pub async fn insert_pending_khqr(
    pool: &PgPool,
    payment: &NewKhqrPayment<'_>,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query!(
        "insert into payments (id, method, provider, status, amount_minor, currency,
                               idempotency_key, provider_ref, khqr_payload, expires_at, next_check_at)
         values ($1, 'khqr', 'bakong', 'pending', $2, $3, $4, $5, $6, $7, now())
         on conflict (idempotency_key) do nothing",
        payment.id,
        payment.amount.amount_minor(),
        payment.amount.currency().as_str(),
        payment.idempotency_key,
        payment.md5,
        payment.payload,
        payment.expires_at,
    )
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected() == 1)
}

pub async fn set_provider_ref(
    pool: &PgPool,
    id: Uuid,
    provider_ref: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "update payments set provider_ref = $2, updated_at = now()
         where id = $1 and provider_ref is null",
        id,
        provider_ref,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Payment>, sqlx::Error> {
    sqlx::query_as!(
        PaymentRow,
        "select id, method, provider, status, amount_minor, currency, provider_ref,
                khqr_payload, expires_at
         from payments where id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?
    .map(Payment::try_from)
    .transpose()
}

pub async fn lock(conn: &mut PgConnection, id: Uuid) -> Result<Option<Payment>, sqlx::Error> {
    sqlx::query_as!(
        PaymentRow,
        "select id, method, provider, status, amount_minor, currency, provider_ref,
                khqr_payload, expires_at
         from payments where id = $1 for update",
        id,
    )
    .fetch_optional(conn)
    .await?
    .map(Payment::try_from)
    .transpose()
}

/// Moves a pending payment to `to` and stops any polling. Returns false if it was
/// no longer pending.
pub async fn transition(
    conn: &mut PgConnection,
    id: Uuid,
    to: PaymentStatus,
    provider_ref: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "update payments
         set status = $2, provider_ref = coalesce(provider_ref, $3), next_check_at = null,
             updated_at = now()
         where id = $1 and status = 'pending'",
        id,
        to.as_str(),
        provider_ref,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Claims due KHQR payments and pushes their next check out by `lease_secs`, so a
/// crashed poller's claims come back and other instances skip them meanwhile.
pub async fn claim_due_khqr(
    pool: &PgPool,
    limit: i64,
    lease_secs: f64,
    expiry_grace_secs: f64,
) -> Result<Vec<DueKhqr>, sqlx::Error> {
    sqlx::query_as!(
        DueKhqr,
        r#"update payments
           set next_check_at = now() + $2 * interval '1 second'
           where id in (
               select id from payments
               where provider = 'bakong' and status = 'pending' and next_check_at <= now()
               order by next_check_at
               limit $1
               for update skip locked
           )
           returning id,
                     provider_ref as "md5!",
                     expires_at as "expires_at!",
                     check_attempts,
                     now() >= expires_at as "past_expiry!",
                     now() >= expires_at + $3 * interval '1 second' as "past_final_check!",
                     now() >= expires_at + interval '24 hours' as "past_verification_deadline!""#,
        limit,
        lease_secs,
        expiry_grace_secs,
    )
    .fetch_all(pool)
    .await
}

/// Schedules the next check `delay_secs` from now, but never later than the
/// expiry while the QR is still live, so the final check lands on time.
pub async fn reschedule(pool: &PgPool, id: Uuid, delay_secs: f64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "update payments
         set next_check_at = case
                 when now() < expires_at then least(now() + $2 * interval '1 second', expires_at)
                 else now() + $2 * interval '1 second'
             end,
             check_attempts = check_attempts + 1
         where id = $1 and status = 'pending'",
        id,
        delay_secs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn stop_polling(conn: &mut PgConnection, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "update payments set next_check_at = null, updated_at = now() where id = $1",
        id
    )
    .execute(conn)
    .await?;
    Ok(())
}
