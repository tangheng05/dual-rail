use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReason {
    AmountMismatch,
    PaidAfterExpiry,
    Unverifiable,
    DuplicateTransfer,
}

impl ReviewReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::AmountMismatch => "amount_mismatch",
            Self::PaidAfterExpiry => "paid_after_expiry",
            Self::Unverifiable => "unverifiable",
            Self::DuplicateTransfer => "duplicate_transfer",
        }
    }
}

pub async fn flag(
    conn: &mut PgConnection,
    payment_id: Uuid,
    reason: ReviewReason,
    details: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "insert into review_flags (payment_id, reason, details) values ($1, $2, $3)
         on conflict (payment_id, reason) do nothing",
        payment_id,
        reason.as_str(),
        details,
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewFlag {
    pub id: Uuid,
    pub payment_id: Uuid,
    pub reason: String,
    pub details: Value,
    pub created_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Now,
    Already,
    NotFound,
}

/// Oldest first. Resolved flags are included only when `include_resolved` is set.
pub async fn list(pool: &PgPool, include_resolved: bool) -> Result<Vec<ReviewFlag>, sqlx::Error> {
    sqlx::query_as!(
        ReviewFlag,
        "select id, payment_id, reason, details, created_at, resolved_at, resolution
         from review_flags
         where $1 or resolved_at is null
         order by created_at",
        include_resolved,
    )
    .fetch_all(pool)
    .await
}

pub async fn count_open(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select count(*) as "count!" from review_flags where resolved_at is null"#
    )
    .fetch_one(pool)
    .await
}

/// Records how a flag was settled. A resolution is written once and never
/// replaced, so the note stays an honest record of the decision.
pub async fn resolve(pool: &PgPool, id: Uuid, note: &str) -> Result<Resolved, sqlx::Error> {
    let updated = sqlx::query!(
        "update review_flags set resolved_at = now(), resolution = $2
         where id = $1 and resolved_at is null",
        id,
        note,
    )
    .execute(pool)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(Resolved::Now);
    }

    let exists = sqlx::query_scalar!(
        r#"select exists (select 1 from review_flags where id = $1) as "exists!""#,
        id
    )
    .fetch_one(pool)
    .await?;
    Ok(if exists {
        Resolved::Already
    } else {
        Resolved::NotFound
    })
}
