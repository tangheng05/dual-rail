use dual_rail_core::{JournalEntry, Outcome, PaymentStatus};
use dual_rail_store::ledger;
use dual_rail_store::payments::{self, Payment};
use sqlx::PgConnection;

/// Applies `outcome` to a payment the caller has locked, writing the ledger entry
/// in the same transaction on success. Returns false for a late or duplicate
/// signal on a payment that is no longer pending.
pub async fn apply(
    conn: &mut PgConnection,
    payment: &Payment,
    outcome: Outcome,
    provider_ref: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let next = match payment.status.transition(outcome) {
        Ok(next) => next,
        Err(err) => {
            tracing::info!(payment_id = %payment.id, %err, "late settlement signal ignored");
            return Ok(false);
        }
    };

    let moved = payments::transition(conn, payment.id, next, provider_ref).await?;
    if moved && next == PaymentStatus::Succeeded {
        let entry = JournalEntry::for_successful_payment(payment.provider, payment.amount);
        ledger::insert_entry(conn, payment.id, &entry).await?;
    }
    if moved {
        tracing::info!(payment_id = %payment.id, provider = %payment.provider, status = %next, "payment settled");
    }
    Ok(moved)
}

pub fn amount_matches(payment: &Payment, amount_minor: i64, currency: &str) -> bool {
    amount_minor == payment.amount.amount_minor()
        && currency.eq_ignore_ascii_case(payment.amount.currency().as_str())
}
