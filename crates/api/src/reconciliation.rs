use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::Context;
use dual_rail_core::{
    MismatchKind, PaymentMethod, PaymentStatus, Provider, ProviderView, check_expired,
    check_succeeded,
};
use dual_rail_rails::khqr::KhqrStatus;
use dual_rail_store::payments::{self, Payment};
use dual_rail_store::reconciliation::{self as runs, RunStatus};
use dual_rail_store::reviews::{self, ReviewReason};
use serde::Serialize;
use serde_json::json;
use time::{Date, OffsetDateTime, UtcOffset};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::AppState;

const SCHEDULER_TICK: Duration = Duration::from_secs(300);
const RUN_AFTER_LOCAL_HOUR: u8 = 1;
const STRIPE_LOOKBACK: time::Duration = time::Duration::days(3);
const STALE_AFTER: time::Duration = time::Duration::hours(24);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_ref: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub run_id: Uuid,
    pub run_date: Date,
    pub status: &'static str,
    pub counts: BTreeMap<&'static str, usize>,
    pub mismatches: Vec<Mismatch>,
}

/// The UTC instants bounding local calendar day `date` at `offset`.
pub fn local_day_window(date: Date, offset: UtcOffset) -> (OffsetDateTime, OffsetDateTime) {
    let start = date.midnight().assume_offset(offset);
    (start, start + time::Duration::days(1))
}

/// Reconciles `date`. Returns None when another run for that date is in progress.
/// Only reports: never changes a payment or the ledger.
pub async fn reconcile(
    state: &AppState,
    date: Date,
    offset: UtcOffset,
) -> anyhow::Result<Option<RunSummary>> {
    let Some(run_id) = runs::claim(&state.pool, date).await? else {
        return Ok(None);
    };

    let (from, to) = local_day_window(date, offset);
    let (mismatches, checked) = match gather(state, from, to).await {
        Ok(result) => result,
        Err(err) => {
            tracing::error!(%run_id, %date, err = %format!("{err:#}"), "reconciliation failed");
            runs::finish(
                &state.pool,
                run_id,
                RunStatus::Failed,
                json!([]),
                json!({ "error": format!("{err:#}") }),
            )
            .await?;
            return Err(err);
        }
    };

    let mut counts = BTreeMap::new();
    for mismatch in &mismatches {
        *counts.entry(mismatch.kind).or_insert(0) += 1;
    }
    let status = if counts.contains_key(MismatchKind::Unverified.as_str()) {
        RunStatus::Incomplete
    } else {
        RunStatus::Completed
    };
    let summary = json!({
        "window": { "from": from.to_string(), "to": to.to_string() },
        "checked": checked,
        "counts": counts,
        "open_review_flags": reviews::count_open(&state.pool).await?,
        "double_payment_check": "not_available",
    });
    runs::finish(
        &state.pool,
        run_id,
        status,
        serde_json::to_value(&mismatches)?,
        summary,
    )
    .await?;

    tracing::info!(%run_id, %date, status = status.as_str(), mismatches = mismatches.len(), ?counts, "reconciliation finished");
    Ok(Some(RunSummary {
        run_id,
        run_date: date,
        status: status.as_str(),
        counts,
        mismatches,
    }))
}

pub async fn run_scheduler(state: AppState, offset: UtcOffset, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(SCHEDULER_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let now = OffsetDateTime::now_utc().to_offset(offset);
        if now.hour() < RUN_AFTER_LOCAL_HOUR {
            continue;
        }
        let Some(yesterday) = now.date().previous_day() else {
            continue;
        };
        match runs::has_finished_run(&state.pool, yesterday).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => {
                tracing::error!(%err, "could not check reconciliation runs");
                continue;
            }
        }
        if let Err(err) = reconcile(&state, yesterday, offset).await {
            tracing::error!(err = %format!("{err:#}"), %yesterday, "scheduled reconciliation failed");
        }
    }
}

async fn gather(
    state: &AppState,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> anyhow::Result<(Vec<Mismatch>, serde_json::Value)> {
    let pool = &state.pool;
    let mut found = Vec::new();

    for payment_id in runs::succeeded_without_entry(pool, from, to).await? {
        found.push(internal(
            MismatchKind::MissingInLedger,
            payment_id,
            "succeeded payment has no journal entry",
        ));
    }
    for payment_id in runs::entries_without_success(pool, from, to).await? {
        found.push(internal(
            MismatchKind::LedgerWithoutPayment,
            payment_id,
            "journal entry for a payment that is not succeeded",
        ));
    }
    for payment_id in runs::unbalanced_entries(pool, from, to).await? {
        found.push(internal(
            MismatchKind::UnbalancedEntry,
            payment_id,
            "journal entry does not balance or does not match its payment",
        ));
    }

    let succeeded = payments::settled_between(pool, PaymentStatus::Succeeded, from, to).await?;
    let expired = payments::settled_between(pool, PaymentStatus::Expired, from, to).await?;
    let (cards, khqrs): (Vec<&Payment>, Vec<&Payment>) = succeeded
        .iter()
        .partition(|payment| payment.provider == Provider::Stripe);
    let expired_khqrs: Vec<&Payment> = expired
        .iter()
        .filter(|payment| payment.method == PaymentMethod::Khqr)
        .collect();

    for payment in &cards {
        let view = match payment.provider_ref.as_deref() {
            None => ProviderView::Missing,
            Some(intent_id) => match state.cards.payment_intent(intent_id).await {
                Ok(Some(intent)) if intent.succeeded() => ProviderView::Paid {
                    amount_minor: Some(intent.amount_received),
                    currency: Some(intent.currency),
                },
                Ok(Some(intent)) => ProviderView::NotPaid {
                    status: intent.status,
                },
                Ok(None) => ProviderView::Missing,
                Err(err) => ProviderView::Unknown {
                    reason: err.to_string(),
                },
            },
        };
        push_check(&mut found, payment, check_succeeded(payment.amount, &view));
    }

    let khqr_views = bakong_views(state, &khqrs).await;
    for (payment, view) in khqrs.iter().zip(&khqr_views) {
        push_check(&mut found, payment, check_succeeded(payment.amount, view));
    }

    let expired_views = bakong_views(state, &expired_khqrs).await;
    for (payment, view) in expired_khqrs.iter().zip(&expired_views) {
        let result = check_expired(view);
        if let Some((MismatchKind::PaidAfterExpiry, _)) = &result {
            let mut conn = pool.acquire().await?;
            reviews::flag(
                &mut conn,
                payment.id,
                ReviewReason::PaidAfterExpiry,
                json!({ "source": "reconciliation", "md5": payment.provider_ref }),
            )
            .await?;
        }
        push_check(&mut found, payment, result);
    }

    let stripe_intents = lost_stripe_webhooks(state, from, to, &mut found).await?;

    for stale in runs::stale_pending(pool, to - STALE_AFTER).await? {
        found.push(Mismatch {
            kind: MismatchKind::StalePending.as_str(),
            payment_id: Some(stale.id),
            provider: stale
                .provider
                .parse::<Provider>()
                .ok()
                .map(Provider::as_str),
            provider_ref: stale.provider_ref,
            detail: format!("pending since {}", stale.created_at),
        });
    }

    let checked = json!({
        "succeeded_card": cards.len(),
        "succeeded_khqr": khqrs.len(),
        "expired_khqr": expired_khqrs.len(),
        "stripe_succeeded_intents": stripe_intents,
    });
    Ok((found, checked))
}

/// Finds Stripe intents that succeeded while our payment did not, i.e. a webhook
/// we never processed. Returns how many succeeded intents were examined.
async fn lost_stripe_webhooks(
    state: &AppState,
    from: OffsetDateTime,
    to: OffsetDateTime,
    found: &mut Vec<Mismatch>,
) -> anyhow::Result<usize> {
    let intents = match state
        .cards
        .succeeded_intents(
            (from - STRIPE_LOOKBACK).unix_timestamp(),
            to.unix_timestamp(),
        )
        .await
    {
        Ok(intents) => intents,
        Err(err) => {
            found.push(Mismatch {
                kind: MismatchKind::Unverified.as_str(),
                payment_id: None,
                provider: Some(Provider::Stripe.as_str()),
                provider_ref: None,
                detail: format!("could not list stripe payment intents: {err}"),
            });
            return Ok(0);
        }
    };

    let ours: Vec<(Uuid, &str)> = intents
        .iter()
        .filter_map(|intent| {
            let payment_id = intent.payment_id.as_deref()?.parse().ok()?;
            Some((payment_id, intent.id.as_str()))
        })
        .collect();
    let ids: Vec<Uuid> = ours.iter().map(|(payment_id, _)| *payment_id).collect();
    let payments: HashMap<Uuid, Payment> = payments::find_many(&state.pool, &ids)
        .await
        .context("loading payments for stripe intents")?
        .into_iter()
        .map(|payment| (payment.id, payment))
        .collect();

    for (payment_id, intent_id) in ours {
        let detail = match payments.get(&payment_id) {
            Some(payment) if payment.status == PaymentStatus::Succeeded => continue,
            Some(payment) => format!("stripe intent succeeded but payment is {}", payment.status),
            None => "stripe intent succeeded for a payment we have no record of".to_owned(),
        };
        found.push(Mismatch {
            kind: MismatchKind::MissingInLedger.as_str(),
            payment_id: Some(payment_id),
            provider: Some(Provider::Stripe.as_str()),
            provider_ref: Some(intent_id.to_owned()),
            detail,
        });
    }
    Ok(intents.len())
}

async fn bakong_views(state: &AppState, payments: &[&Payment]) -> Vec<ProviderView> {
    let md5s: Vec<String> = payments
        .iter()
        .map(|payment| payment.provider_ref.clone().unwrap_or_default())
        .collect();
    if md5s.is_empty() {
        return Vec::new();
    }
    match state.verifier.check(&md5s).await {
        Ok(statuses) if statuses.len() == md5s.len() => statuses
            .into_iter()
            .map(|status| match status {
                KhqrStatus::Unpaid => ProviderView::Missing,
                KhqrStatus::Paid(transfer) => ProviderView::Paid {
                    amount_minor: transfer.amount_minor,
                    currency: transfer.currency,
                },
                KhqrStatus::Unreadable(reason) => ProviderView::Unknown { reason },
            })
            .collect(),
        Ok(_) => unknown_for_all(
            md5s.len(),
            "bakong answered for a different number of payments",
        ),
        Err(err) => unknown_for_all(md5s.len(), &err.to_string()),
    }
}

fn unknown_for_all(count: usize, reason: &str) -> Vec<ProviderView> {
    vec![
        ProviderView::Unknown {
            reason: reason.to_owned()
        };
        count
    ]
}

fn push_check(
    found: &mut Vec<Mismatch>,
    payment: &Payment,
    result: Option<(MismatchKind, String)>,
) {
    if let Some((kind, detail)) = result {
        found.push(Mismatch {
            kind: kind.as_str(),
            payment_id: Some(payment.id),
            provider: Some(payment.provider.as_str()),
            provider_ref: payment.provider_ref.clone(),
            detail,
        });
    }
}

fn internal(kind: MismatchKind, payment_id: Uuid, detail: &str) -> Mismatch {
    Mismatch {
        kind: kind.as_str(),
        payment_id: Some(payment_id),
        provider: None,
        provider_ref: None,
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use time::macros::{date, datetime, offset};

    use super::*;

    #[test]
    fn local_day_window_is_the_local_calendar_day_in_utc() {
        let (from, to) = local_day_window(date!(2026 - 09 - 26), offset!(+7));
        assert_eq!(from, datetime!(2026-09-25 17:00 UTC));
        assert_eq!(to, datetime!(2026-09-26 17:00 UTC));
        let late_evening_local = datetime!(2026-09-26 23:30 +7);
        assert!(from <= late_evening_local && late_evening_local < to);
    }
}
