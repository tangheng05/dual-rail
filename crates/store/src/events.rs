use sqlx::PgConnection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Stripe,
    Bakong,
}

impl EventSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Bakong => "bakong",
        }
    }
}

/// Returns false when the event was already recorded, i.e. it is a redelivery.
pub async fn record(
    conn: &mut PgConnection,
    source: EventSource,
    event_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "insert into processed_events (provider, event_id) values ($1, $2) on conflict do nothing",
        source.as_str(),
        event_id,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}
