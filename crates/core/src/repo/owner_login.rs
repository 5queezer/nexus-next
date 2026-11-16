//! The owner's browser sessions and passkeys. Only hashes of session ids are stored.

use serde::Serialize;
use sqlx::SqliteConnection;

use crate::error::Result;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct OwnerSession {
    #[serde(skip)]
    pub id_hash: String,
    #[serde(skip)]
    pub passphrase_tag: String,
    pub remembered: bool,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Stores a new session and keeps at most `max` live ones, ending the oldest first and any made
/// under another passphrase. Run inside `Db::begin_write`.
pub async fn insert_session(
    conn: &mut SqliteConnection,
    id_hash: &str,
    passphrase_tag: &str,
    remembered: bool,
    expires_at: &Timestamp,
    max: i64,
) -> Result<()> {
    let now = Timestamp::now();
    sqlx::query("DELETE FROM owner_sessions WHERE expires_at <= ? OR passphrase_tag != ?")
        .bind(now.as_str())
        .bind(passphrase_tag)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM owner_sessions WHERE id_hash IN
         (SELECT id_hash FROM owner_sessions ORDER BY created_at DESC, id_hash LIMIT -1 OFFSET ?)",
    )
    .bind(max - 1)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO owner_sessions (id_hash, passphrase_tag, remembered, expires_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(id_hash)
    .bind(passphrase_tag)
    .bind(remembered)
    .bind(expires_at.as_str())
    .execute(conn)
    .await?;
    Ok(())
}

/// The session with this hash, if it has not expired and was made under this passphrase.
pub async fn live_session(
    conn: &mut SqliteConnection,
    id_hash: &str,
    passphrase_tag: &str,
) -> Result<Option<OwnerSession>> {
    Ok(sqlx::query_as(
        "SELECT * FROM owner_sessions WHERE id_hash = ? AND passphrase_tag = ? AND expires_at > ?",
    )
    .bind(id_hash)
    .bind(passphrase_tag)
    .bind(Timestamp::now().as_str())
    .fetch_optional(conn)
    .await?)
}

pub async fn live_sessions(conn: &mut SqliteConnection) -> Result<Vec<OwnerSession>> {
    Ok(sqlx::query_as(
        "SELECT * FROM owner_sessions WHERE expires_at > ? ORDER BY created_at, id_hash",
    )
    .bind(Timestamp::now().as_str())
    .fetch_all(conn)
    .await?)
}

pub async fn delete_session(conn: &mut SqliteConnection, id_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM owner_sessions WHERE id_hash = ?")
        .bind(id_hash)
        .execute(conn)
        .await?;
    Ok(())
}

/// Ends every session. Returns how many were live.
pub async fn delete_all_sessions(conn: &mut SqliteConnection) -> Result<u64> {
    let live = sqlx::query("DELETE FROM owner_sessions WHERE expires_at > ?")
        .bind(Timestamp::now().as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM owner_sessions")
        .execute(conn)
        .await?;
    Ok(live)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct OwnerPasskey {
    pub credential_id: String,
    pub name: String,
    #[serde(skip)]
    pub passkey: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

pub async fn passkeys(conn: &mut SqliteConnection) -> Result<Vec<OwnerPasskey>> {
    Ok(
        sqlx::query_as("SELECT * FROM owner_passkeys ORDER BY created_at, credential_id")
            .fetch_all(conn)
            .await?,
    )
}

pub async fn insert_passkey(
    conn: &mut SqliteConnection,
    credential_id: &str,
    name: &str,
    passkey: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO owner_passkeys (credential_id, name, passkey) VALUES (?, ?, ?)")
        .bind(credential_id)
        .bind(name)
        .bind(passkey)
        .execute(conn)
        .await?;
    Ok(())
}

/// Records a login with this passkey, and its new counter and backup state when they changed.
pub async fn passkey_used(
    conn: &mut SqliteConnection,
    credential_id: &str,
    passkey: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE owner_passkeys SET last_used_at = ?, passkey = coalesce(?, passkey)
         WHERE credential_id = ?",
    )
    .bind(Timestamp::now().as_str())
    .bind(passkey)
    .bind(credential_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Returns whether a passkey with this id existed.
pub async fn delete_passkey(conn: &mut SqliteConnection, credential_id: &str) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM owner_passkeys WHERE credential_id = ?")
            .bind(credential_id)
            .execute(conn)
            .await?
            .rows_affected()
            == 1,
    )
}
