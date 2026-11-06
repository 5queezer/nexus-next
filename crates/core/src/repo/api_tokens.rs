use serde::Serialize;
use sqlx::SqliteConnection;

use super::id_string;
use crate::error::Result;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ApiToken {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    #[serde(skip)]
    pub token_hash: String,
    pub name: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

pub async fn find_by_hash(
    conn: &mut SqliteConnection,
    token_hash: &str,
) -> Result<Option<ApiToken>> {
    Ok(
        sqlx::query_as("SELECT * FROM api_tokens WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(conn)
            .await?,
    )
}

pub async fn current(conn: &mut SqliteConnection) -> Result<Option<ApiToken>> {
    Ok(
        sqlx::query_as("SELECT * FROM api_tokens ORDER BY id LIMIT 1")
            .fetch_optional(conn)
            .await?,
    )
}

/// Port of `createApiToken`: there is one token at a time, so this replaces any existing one. Run
/// inside `Db::begin_write`.
pub async fn replace(
    conn: &mut SqliteConnection,
    token_hash: &str,
    name: &str,
) -> Result<ApiToken> {
    delete_all(&mut *conn).await?;
    Ok(
        sqlx::query_as("INSERT INTO api_tokens (token_hash, name) VALUES (?, ?) RETURNING *")
            .bind(token_hash)
            .bind(name)
            .fetch_one(conn)
            .await?,
    )
}

pub async fn delete_all(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query("DELETE FROM api_tokens").execute(conn).await?;
    Ok(())
}

pub async fn touch_last_used(conn: &mut SqliteConnection, id: i64) -> Result<()> {
    sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
        .bind(Timestamp::now())
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}
