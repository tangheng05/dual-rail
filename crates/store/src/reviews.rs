use serde_json::Value;
use sqlx::PgConnection;
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
