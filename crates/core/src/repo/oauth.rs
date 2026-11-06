//! Storage for the OAuth 2.1 tables. PKCE checks, token generation and hashing are the caller's.

use sqlx::SqliteConnection;
use sqlx::types::Json;

use crate::error::Result;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthClient {
    pub id: i64,
    pub client_id: String,
    pub client_secret_hash: Option<String>,
    pub client_name: Option<String>,
    pub redirect_uris: Json<Vec<String>>,
    pub grant_types: Json<Vec<String>>,
    pub response_types: Json<Vec<String>>,
    pub token_endpoint_auth: String,
    pub client_secret_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOAuthClient {
    pub client_id: String,
    pub client_secret_hash: Option<String>,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth: String,
}

pub async fn insert_client(
    conn: &mut SqliteConnection,
    client: &NewOAuthClient,
) -> Result<OAuthClient> {
    Ok(sqlx::query_as(
        "INSERT INTO oauth_clients (client_id, client_secret_hash, client_name, redirect_uris,
             grant_types, response_types, token_endpoint_auth)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(&client.client_id)
    .bind(&client.client_secret_hash)
    .bind(&client.client_name)
    .bind(Json(&client.redirect_uris))
    .bind(Json(&client.grant_types))
    .bind(Json(&client.response_types))
    .bind(&client.token_endpoint_auth)
    .fetch_one(conn)
    .await?)
}

pub async fn find_client(
    conn: &mut SqliteConnection,
    client_id: &str,
) -> Result<Option<OAuthClient>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_clients WHERE client_id = ?")
            .bind(client_id)
            .fetch_optional(conn)
            .await?,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthAuthCode {
    pub id: i64,
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scopes: Json<Vec<String>>,
    pub sensitive_consent_version: i64,
    pub expires_at: Timestamp,
    pub used: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuthCode {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scopes: Vec<String>,
    pub sensitive_consent_version: i64,
    pub expires_at: Timestamp,
}

pub async fn insert_auth_code(conn: &mut SqliteConnection, code: &NewAuthCode) -> Result<()> {
    sqlx::query(
        "INSERT INTO oauth_auth_codes (code, client_id, redirect_uri, code_challenge, scopes,
             sensitive_consent_version, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&code.code)
    .bind(&code.client_id)
    .bind(&code.redirect_uri)
    .bind(&code.code_challenge)
    .bind(Json(&code.scopes))
    .bind(code.sensitive_consent_version)
    .bind(&code.expires_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Atomically marks a code used, as `exchangeAuthCode` does. Returns `None` if it is unknown,
/// expired, already used, or issued to another client or redirect URI.
pub async fn consume_auth_code(
    conn: &mut SqliteConnection,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    now: &Timestamp,
) -> Result<Option<OAuthAuthCode>> {
    Ok(sqlx::query_as(
        "UPDATE oauth_auth_codes SET used = 1
         WHERE code = ? AND used = 0 AND expires_at > ? AND client_id = ? AND redirect_uri = ?
         RETURNING *",
    )
    .bind(code)
    .bind(now)
    .bind(client_id)
    .bind(redirect_uri)
    .fetch_optional(conn)
    .await?)
}

/// The code row with this stored hash, in any state. Token issuance goes through
/// `consume_auth_code`; this is how a replay of an already used code is recognised.
pub async fn find_auth_code(
    conn: &mut SqliteConnection,
    code: &str,
) -> Result<Option<OAuthAuthCode>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_auth_codes WHERE code = ?")
            .bind(code)
            .fetch_optional(conn)
            .await?,
    )
}

/// A stored access or refresh token; the two tables have the same shape, except that only refresh
/// tokens have `rotated_at`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthToken {
    pub id: i64,
    pub token_hash: String,
    pub client_id: String,
    pub scopes: Json<Vec<String>>,
    pub sensitive_consent_version: i64,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
    pub grant_id: String,
    #[sqlx(default)]
    pub rotated_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOAuthToken {
    pub token_hash: String,
    pub grant_id: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub sensitive_consent_version: i64,
    pub expires_at: Timestamp,
}

pub async fn insert_access_token(conn: &mut SqliteConnection, token: &NewOAuthToken) -> Result<()> {
    insert_token(
        conn,
        token,
        "INSERT INTO oauth_access_tokens (token_hash, grant_id, client_id, scopes,
        sensitive_consent_version, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .await
}

pub async fn insert_refresh_token(
    conn: &mut SqliteConnection,
    token: &NewOAuthToken,
) -> Result<()> {
    insert_token(
        conn,
        token,
        "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, client_id, scopes,
        sensitive_consent_version, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .await
}

async fn insert_token(
    conn: &mut SqliteConnection,
    token: &NewOAuthToken,
    sql: &'static str,
) -> Result<()> {
    sqlx::query(sql)
        .bind(&token.token_hash)
        .bind(&token.grant_id)
        .bind(&token.client_id)
        .bind(Json(&token.scopes))
        .bind(token.sensitive_consent_version)
        .bind(&token.expires_at)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn find_access_token(
    conn: &mut SqliteConnection,
    token_hash: &str,
) -> Result<Option<OAuthToken>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_access_tokens WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(conn)
            .await?,
    )
}

pub async fn find_refresh_token(
    conn: &mut SqliteConnection,
    token_hash: &str,
) -> Result<Option<OAuthToken>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_refresh_tokens WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(conn)
            .await?,
    )
}

/// Retires every live refresh token of a grant, so a grant has at most one live token. Retired
/// rows stay until they expire, for replay detection.
pub async fn retire_refresh_tokens(
    conn: &mut SqliteConnection,
    grant_id: &str,
    now: &Timestamp,
) -> Result<()> {
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET rotated_at = ? WHERE grant_id = ? AND rotated_at IS NULL",
    )
    .bind(now)
    .bind(grant_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// How many rows a revocation deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revoked {
    pub access_tokens: u64,
    pub refresh_tokens: u64,
}

/// Deletes every access and refresh token descending from one authorization.
pub async fn revoke_grant(conn: &mut SqliteConnection, grant_id: &str) -> Result<Revoked> {
    let access = sqlx::query("DELETE FROM oauth_access_tokens WHERE grant_id = ?")
        .bind(grant_id)
        .execute(&mut *conn)
        .await?;
    let refresh = sqlx::query("DELETE FROM oauth_refresh_tokens WHERE grant_id = ?")
        .bind(grant_id)
        .execute(conn)
        .await?;
    Ok(Revoked {
        access_tokens: access.rows_affected(),
        refresh_tokens: refresh.rows_affected(),
    })
}

/// Deletes every code and token of every client. Client registrations stay. Run inside
/// `Db::begin_write`.
pub async fn revoke_all(conn: &mut SqliteConnection) -> Result<Revoked> {
    sqlx::query("DELETE FROM oauth_auth_codes")
        .execute(&mut *conn)
        .await?;
    let access = sqlx::query("DELETE FROM oauth_access_tokens")
        .execute(&mut *conn)
        .await?;
    let refresh = sqlx::query("DELETE FROM oauth_refresh_tokens")
        .execute(conn)
        .await?;
    Ok(Revoked {
        access_tokens: access.rows_affected(),
        refresh_tokens: refresh.rows_affected(),
    })
}

pub async fn count_clients(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients")
        .fetch_one(conn)
        .await?)
}

/// Deletes the oldest client created at or before `cutoff` that holds no code or token, making room
/// at the client cap. False if every older client is in use. Run after `prune_expired`.
pub async fn evict_oldest_unused_client(
    conn: &mut SqliteConnection,
    cutoff: &Timestamp,
) -> Result<bool> {
    let done = sqlx::query(
        "DELETE FROM oauth_clients WHERE id = (
             SELECT c.id FROM oauth_clients c
             WHERE c.created_at <= ?
               AND NOT EXISTS (SELECT 1 FROM oauth_auth_codes t WHERE t.client_id = c.client_id)
               AND NOT EXISTS (SELECT 1 FROM oauth_access_tokens t WHERE t.client_id = c.client_id)
               AND NOT EXISTS (SELECT 1 FROM oauth_refresh_tokens t WHERE t.client_id = c.client_id)
             ORDER BY c.created_at, c.id LIMIT 1
         )",
    )
    .bind(cutoff)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Deletes expired tokens and unused codes. A used code stays until `used_code_cutoff`, because
/// presenting it again revokes its grant, and that must work for as long as the grant's tokens live.
pub async fn prune_expired(
    conn: &mut SqliteConnection,
    now: &Timestamp,
    used_code_cutoff: &Timestamp,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM oauth_auth_codes
         WHERE (used = 0 AND expires_at <= ?) OR (used = 1 AND expires_at <= ?)",
    )
    .bind(now)
    .bind(used_code_cutoff)
    .execute(&mut *conn)
    .await?;
    for sql in [
        "DELETE FROM oauth_access_tokens WHERE expires_at <= ?",
        "DELETE FROM oauth_refresh_tokens WHERE expires_at <= ?",
    ] {
        sqlx::query(sql).bind(now).execute(&mut *conn).await?;
    }
    Ok(())
}
