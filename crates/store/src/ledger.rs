use dual_rail_core::JournalEntry;
use sqlx::PgConnection;
use uuid::Uuid;

pub async fn insert_entry(
    conn: &mut PgConnection,
    payment_id: Uuid,
    entry: &JournalEntry,
) -> Result<Uuid, sqlx::Error> {
    let entry_id = sqlx::query_scalar!(
        "insert into journal_entries (payment_id) values ($1) returning id",
        payment_id,
    )
    .fetch_one(&mut *conn)
    .await?;

    for line in entry.lines() {
        let inserted = sqlx::query!(
            "insert into ledger_lines (journal_entry_id, account_id, direction, amount_minor, currency)
             select $1, id, $3, $4, $5 from ledger_accounts where code = $2",
            entry_id,
            line.account.code(),
            line.direction.as_str(),
            line.amount.amount_minor(),
            line.amount.currency().as_str(),
        )
        .execute(&mut *conn)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
    }

    Ok(entry_id)
}
