use dual_rail_core::Provider;
use sqlx::PgConnection;

/// Returns false when the event was already recorded, i.e. it is a redelivery.
pub async fn record(
    conn: &mut PgConnection,
    provider: Provider,
    event_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "insert into processed_events (provider, event_id) values ($1, $2) on conflict do nothing",
        provider.as_str(),
        event_id,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}
