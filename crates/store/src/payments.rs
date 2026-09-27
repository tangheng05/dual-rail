use dual_rail_core::{Money, PaymentMethod, PaymentStatus};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::decode_error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    pub id: Uuid,
    pub method: PaymentMethod,
    pub status: PaymentStatus,
    pub amount: Money,
    pub provider_ref: Option<String>,
}

struct PaymentRow {
    id: Uuid,
    method: String,
    status: String,
    amount_minor: i64,
    currency: String,
    provider_ref: Option<String>,
}

impl TryFrom<PaymentRow> for Payment {
    type Error = sqlx::Error;

    fn try_from(row: PaymentRow) -> Result<Self, Self::Error> {
        let currency = row.currency.parse().map_err(decode_error)?;
        Ok(Self {
            id: row.id,
            method: row.method.parse().map_err(decode_error)?,
            status: row.status.parse().map_err(decode_error)?,
            amount: Money::new(row.amount_minor, currency).map_err(decode_error)?,
            provider_ref: row.provider_ref,
        })
    }
}

/// Returns None when the idempotency key is already taken.
pub async fn insert_pending(
    pool: &PgPool,
    method: PaymentMethod,
    amount: Money,
    idempotency_key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "insert into payments (method, status, amount_minor, currency, idempotency_key)
         values ($1, 'pending', $2, $3, $4)
         on conflict (idempotency_key) do nothing
         returning id",
        method.as_str(),
        amount.amount_minor(),
        amount.currency().as_str(),
        idempotency_key,
    )
    .fetch_optional(pool)
    .await
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
        "select id, method, status, amount_minor, currency, provider_ref
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
        "select id, method, status, amount_minor, currency, provider_ref
         from payments where id = $1 for update",
        id,
    )
    .fetch_optional(conn)
    .await?
    .map(Payment::try_from)
    .transpose()
}

/// Moves a pending payment to `to`. Returns false if it was no longer pending.
pub async fn transition(
    conn: &mut PgConnection,
    id: Uuid,
    to: PaymentStatus,
    provider_ref: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "update payments
         set status = $2, provider_ref = coalesce(provider_ref, $3), updated_at = now()
         where id = $1 and status = 'pending'",
        id,
        to.as_str(),
        provider_ref,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}
