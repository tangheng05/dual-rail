use serde_json::Value;
use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Incomplete,
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalePayment {
    pub id: Uuid,
    pub provider: String,
    pub provider_ref: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Starts a run for `run_date`, or returns None while another one is running.
/// A run left `running` for over an hour is treated as crashed and failed first.
pub async fn claim(pool: &PgPool, run_date: Date) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query!(
        r#"update reconciliation_runs
           set status = 'failed', finished_at = now(),
               summary = summary || '{"error": "abandoned while running"}'
           where run_date = $1 and status = 'running' and created_at < now() - interval '1 hour'"#,
        run_date,
    )
    .execute(pool)
    .await?;

    sqlx::query_scalar!(
        "insert into reconciliation_runs (run_date, status) values ($1, 'running')
         on conflict (run_date) where status = 'running' do nothing
         returning id",
        run_date,
    )
    .fetch_optional(pool)
    .await
}

pub async fn finish(
    pool: &PgPool,
    run_id: Uuid,
    status: RunStatus,
    mismatches: Value,
    summary: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "update reconciliation_runs
         set status = $2, mismatches = $3, summary = $4, finished_at = now()
         where id = $1",
        run_id,
        status.as_str(),
        mismatches,
        summary,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether the scheduler can leave `run_date` alone: it has a completed run, or an
/// incomplete one from the last hour (retried hourly while providers recover).
pub async fn has_finished_run(pool: &PgPool, run_date: Date) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"select exists (
               select 1 from reconciliation_runs
               where run_date = $1
                 and (status = 'completed'
                      or (status = 'incomplete' and finished_at > now() - interval '1 hour'))
           ) as "exists!""#,
        run_date,
    )
    .fetch_one(pool)
    .await
}

/// Succeeded payments settled in `[from, to)` that have no journal entry.
pub async fn succeeded_without_entry(
    pool: &PgPool,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "select p.id from payments p
         where p.status = 'succeeded' and p.settled_at >= $1 and p.settled_at < $2
           and not exists (select 1 from journal_entries j where j.payment_id = p.id)",
        from,
        to,
    )
    .fetch_all(pool)
    .await
}

/// Journal entries written in `[from, to)` for payments that are not succeeded.
pub async fn entries_without_success(
    pool: &PgPool,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "select j.payment_id from journal_entries j
         join payments p on p.id = j.payment_id
         where j.created_at >= $1 and j.created_at < $2 and p.status <> 'succeeded'",
        from,
        to,
    )
    .fetch_all(pool)
    .await
}

/// Journal entries written in `[from, to)` whose debits and credits differ, or
/// whose amount or currency differs from their payment.
pub async fn unbalanced_entries(
    pool: &PgPool,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "select j.payment_id from journal_entries j
         join payments p on p.id = j.payment_id
         join ledger_lines l on l.journal_entry_id = j.id
         where j.created_at >= $1 and j.created_at < $2
         group by j.id, j.payment_id, p.amount_minor, p.currency
         having sum(case when l.direction = 'debit' then l.amount_minor else 0 end)
                    <> sum(case when l.direction = 'credit' then l.amount_minor else 0 end)
             or sum(case when l.direction = 'debit' then l.amount_minor else 0 end) <> p.amount_minor
             or bool_or(l.currency <> p.currency)",
        from,
        to,
    )
    .fetch_all(pool)
    .await
}

pub async fn stale_pending(
    pool: &PgPool,
    created_before: OffsetDateTime,
) -> Result<Vec<StalePayment>, sqlx::Error> {
    sqlx::query_as!(
        StalePayment,
        "select id, provider, provider_ref, created_at from payments
         where status = 'pending' and created_at < $1
         order by created_at",
        created_before,
    )
    .fetch_all(pool)
    .await
}
