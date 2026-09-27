use std::time::Duration;

use dual_rail_core::{Outcome, Provider};
use dual_rail_rails::khqr::{KhqrStatus, KhqrTransfer};
use dual_rail_store::events;
use dual_rail_store::payments::{self, DueKhqr};
use dual_rail_store::reviews::{self, ReviewReason};
use serde_json::json;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::{AppState, settlement};

const BATCH: i64 = 50;
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
// Longer than CHECK_TIMEOUT, so a batch is never re-claimed while still in flight.
const LEASE_SECS: f64 = 90.0;
// Lets Bakong index a payment made in the last seconds before the QR expired.
const EXPIRY_GRACE_SECS: f64 = 120.0;
// Wallets refuse expired QRs by their own clock, so a transfer stamped slightly
// after our expiry is skew between clocks, not a late payment.
const PAID_AT_TOLERANCE_MS: i128 = 60_000;
const LIVE_DELAYS_SECS: [f64; 3] = [2.0, 5.0, 10.0];
const EXPIRED_DELAY_SECS: f64 = 60.0;

/// `interval` bounds how often Bakong is called: at most one batch per tick.
pub async fn run(state: AppState, interval: Duration, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        if let Err(err) = poll_once(&state).await {
            tracing::error!(%err, "khqr poll failed");
        }
    }
}

/// Checks one batch of due KHQR payments against Bakong. Returns how many were checked.
pub async fn poll_once(state: &AppState) -> Result<usize, sqlx::Error> {
    let due = payments::claim_due_khqr(&state.pool, BATCH, LEASE_SECS, EXPIRY_GRACE_SECS).await?;
    if due.is_empty() {
        return Ok(0);
    }

    let md5s: Vec<String> = due.iter().map(|payment| payment.md5.clone()).collect();
    let answer = tokio::time::timeout(CHECK_TIMEOUT, state.verifier.check(&md5s)).await;
    let failure = match answer {
        Ok(Ok(statuses)) if statuses.len() == due.len() => {
            for (payment, status) in due.iter().zip(statuses) {
                resolve(state, payment, status).await?;
            }
            return Ok(due.len());
        }
        Ok(Ok(_)) => "bakong answered for a different number of payments".to_owned(),
        Ok(Err(err)) => err.to_string(),
        Err(_) => format!("bakong did not answer within {CHECK_TIMEOUT:?}"),
    };

    tracing::warn!(err = %failure, count = due.len(), "khqr check failed, will retry");
    for payment in &due {
        unverified(state, payment).await?;
    }
    Ok(due.len())
}

pub fn next_delay_secs(check_attempts: i32, past_final_check: bool) -> f64 {
    if past_final_check {
        return EXPIRED_DELAY_SECS;
    }
    let index = usize::try_from(check_attempts).unwrap_or(0);
    LIVE_DELAYS_SECS[index.min(LIVE_DELAYS_SECS.len() - 1)]
}

async fn resolve(state: &AppState, due: &DueKhqr, status: KhqrStatus) -> Result<(), sqlx::Error> {
    match status {
        KhqrStatus::Unpaid if due.past_final_check => {
            // Only a successful answer after expiry plus grace may expire a payment.
            let mut tx = state.pool.begin().await?;
            if let Some(payment) = payments::lock(&mut tx, due.id).await? {
                settlement::apply(&mut tx, &payment, Outcome::Expired, Some(&due.md5)).await?;
            }
            tx.commit().await
        }
        KhqrStatus::Unpaid => {
            let delay = next_delay_secs(due.check_attempts, false);
            payments::reschedule(&state.pool, due.id, delay).await
        }
        KhqrStatus::Paid(transfer) => paid(state, due, &transfer).await,
        KhqrStatus::Unreadable(reason) => {
            tracing::warn!(payment_id = %due.id, %reason, "unreadable bakong answer, will retry");
            unverified(state, due).await
        }
    }
}

async fn paid(state: &AppState, due: &DueKhqr, transfer: &KhqrTransfer) -> Result<(), sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let Some(payment) = payments::lock(&mut tx, due.id).await? else {
        return tx.commit().await;
    };
    if payment.status.transition(Outcome::Succeeded).is_err() {
        return tx.commit().await;
    }

    let details = json!({
        "md5": due.md5,
        "hash": transfer.hash,
        "amount_minor": transfer.amount_minor,
        "currency": transfer.currency,
        "to_account_id": transfer.to_account_id,
        "paid_at_ms": transfer.paid_at_ms,
    });

    if !events::record(&mut tx, Provider::Bakong, &transfer.hash).await? {
        tracing::error!(payment_id = %payment.id, hash = %transfer.hash, "bakong transfer already credited to another payment");
        reviews::flag(
            &mut tx,
            payment.id,
            ReviewReason::DuplicateTransfer,
            details,
        )
        .await?;
        payments::stop_polling(&mut tx, payment.id).await?;
        return tx.commit().await;
    }

    // Missing fields count as a mismatch: crediting needs positive evidence.
    let matches = match (
        transfer.amount_minor,
        transfer.currency.as_deref(),
        transfer.to_account_id.as_deref(),
    ) {
        (Some(amount_minor), Some(currency), Some(account)) => {
            settlement::amount_matches(&payment, amount_minor, currency)
                && account == state.khqr.account_id()
        }
        _ => false,
    };
    if !matches {
        tracing::error!(payment_id = %payment.id, "bakong transfer does not match payment, flagged for review");
        reviews::flag(&mut tx, payment.id, ReviewReason::AmountMismatch, details).await?;
        payments::stop_polling(&mut tx, payment.id).await?;
        return tx.commit().await;
    }

    let expires_at_ms = due.expires_at.unix_timestamp_nanos() / 1_000_000;
    let on_time = transfer.paid_at_ms.map_or(!due.past_expiry, |paid_at| {
        i128::from(paid_at) <= expires_at_ms + PAID_AT_TOLERANCE_MS
    });
    if on_time {
        settlement::apply(&mut tx, &payment, Outcome::Succeeded, Some(&due.md5)).await?;
    } else {
        tracing::error!(payment_id = %payment.id, "bakong reports payment after expiry, not credited");
        settlement::apply(&mut tx, &payment, Outcome::Expired, Some(&due.md5)).await?;
        reviews::flag(&mut tx, payment.id, ReviewReason::PaidAfterExpiry, details).await?;
    }
    tx.commit().await
}

async fn unverified(state: &AppState, due: &DueKhqr) -> Result<(), sqlx::Error> {
    if !due.past_verification_deadline {
        let delay = next_delay_secs(due.check_attempts, due.past_final_check);
        return payments::reschedule(&state.pool, due.id, delay).await;
    }

    tracing::error!(payment_id = %due.id, "khqr payment could not be verified before the deadline");
    let mut tx = state.pool.begin().await?;
    reviews::flag(
        &mut tx,
        due.id,
        ReviewReason::Unverifiable,
        json!({ "md5": due.md5, "check_attempts": due.check_attempts }),
    )
    .await?;
    payments::stop_polling(&mut tx, due.id).await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_to_a_cap_while_live_and_slows_after_expiry() {
        let live: Vec<f64> = (0..5)
            .map(|attempt| next_delay_secs(attempt, false))
            .collect();
        assert_eq!(live, [2.0, 5.0, 10.0, 10.0, 10.0]);
        assert_eq!(next_delay_secs(0, true), EXPIRED_DELAY_SECS);
    }
}
