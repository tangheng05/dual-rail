use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    Now,
    Already,
    NotFound,
}

pub async fn insert(
    pool: &PgPool,
    name: &str,
    prefix: &str,
    key_hash: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar!(
        "insert into api_keys (name, prefix, key_hash) values ($1, $2, $3) returning id",
        name,
        prefix,
        key_hash,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_active(pool: &PgPool, key_hash: &str) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        "select id from api_keys where key_hash = $1 and revoked_at is null",
        key_hash,
    )
    .fetch_optional(pool)
    .await
}

/// Records use at most once a minute, so authentication isn't a write per request.
pub async fn touch(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "update api_keys set last_used_at = now()
         where id = $1 and (last_used_at is null or last_used_at < now() - interval '1 minute')",
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list(pool: &PgPool) -> Result<Vec<ApiKey>, sqlx::Error> {
    sqlx::query_as!(
        ApiKey,
        "select id, name, prefix, created_at, last_used_at, revoked_at
         from api_keys order by created_at",
    )
    .fetch_all(pool)
    .await
}

pub async fn revoke(pool: &PgPool, id: Uuid) -> Result<Revoked, sqlx::Error> {
    let updated = sqlx::query!(
        "update api_keys set revoked_at = now() where id = $1 and revoked_at is null",
        id,
    )
    .execute(pool)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(Revoked::Now);
    }

    let exists = sqlx::query_scalar!(
        r#"select exists (select 1 from api_keys where id = $1) as "exists!""#,
        id
    )
    .fetch_one(pool)
    .await?;
    Ok(if exists {
        Revoked::Already
    } else {
        Revoked::NotFound
    })
}
