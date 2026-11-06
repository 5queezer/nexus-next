//! CV variants and their immutable revisions. See SPEC section 9a.

use serde_json::Value;
use sqlx::SqliteConnection;
use sqlx::types::Json;

use crate::blob::sha256_hex;
use crate::error::Result;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Variant {
    pub id: i64,
    pub name: String,
    pub application_id: Option<i64>,
    pub head_revision_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Revision {
    pub id: i64,
    pub variant_id: i64,
    pub number: i64,
    pub parent_id: Option<i64>,
    pub source: String,
    pub source_sha256: String,
    pub facts_version: i64,
    pub origin: String,
    pub summary: String,
    pub warnings: Json<Value>,
    pub created_at: Timestamp,
}

pub async fn get_variant(conn: &mut SqliteConnection, name: &str) -> Result<Option<Variant>> {
    Ok(sqlx::query_as(
        "SELECT id, name, application_id, head_revision_id FROM cv_variants WHERE name = ?",
    )
    .bind(name)
    .fetch_optional(conn)
    .await?)
}

/// A variant with its head, for the workspace's list. `head_number` is 0 with nothing stored.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct VariantSummary {
    pub name: String,
    pub application_id: Option<i64>,
    pub head_number: i64,
    pub head_origin: Option<String>,
    pub head_created_at: Option<Timestamp>,
}

/// `default` first, then the others by name.
pub async fn list_variants(conn: &mut SqliteConnection) -> Result<Vec<VariantSummary>> {
    Ok(sqlx::query_as(
        "SELECT v.name, v.application_id, coalesce(r.number, 0) AS head_number,
             r.origin AS head_origin, r.created_at AS head_created_at
         FROM cv_variants v LEFT JOIN cv_revisions r ON r.id = v.head_revision_id
         ORDER BY v.name <> 'default', v.name",
    )
    .fetch_all(conn)
    .await?)
}

pub async fn create_variant(
    conn: &mut SqliteConnection,
    name: &str,
    application_id: Option<i64>,
) -> Result<Variant> {
    Ok(sqlx::query_as(
        "INSERT INTO cv_variants (name, application_id) VALUES (?, ?)
         RETURNING id, name, application_id, head_revision_id",
    )
    .bind(name)
    .bind(application_id)
    .fetch_one(conn)
    .await?)
}

pub async fn get_by_id(conn: &mut SqliteConnection, id: i64) -> Result<Option<Revision>> {
    Ok(sqlx::query_as("SELECT * FROM cv_revisions WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

pub async fn get_by_number(
    conn: &mut SqliteConnection,
    variant_id: i64,
    number: i64,
) -> Result<Option<Revision>> {
    Ok(
        sqlx::query_as("SELECT * FROM cv_revisions WHERE variant_id = ? AND number = ?")
            .bind(variant_id)
            .bind(number)
            .fetch_optional(conn)
            .await?,
    )
}

/// The variant's newest revision. Read fresh: a caller's `Variant` may be out of date.
pub async fn head(conn: &mut SqliteConnection, variant_id: i64) -> Result<Option<Revision>> {
    Ok(sqlx::query_as(
        "SELECT r.* FROM cv_revisions r JOIN cv_variants v ON v.head_revision_id = r.id
         WHERE v.id = ?",
    )
    .bind(variant_id)
    .fetch_optional(conn)
    .await?)
}

/// Newest first.
pub async fn list(
    conn: &mut SqliteConnection,
    variant_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Revision>> {
    Ok(sqlx::query_as(
        "SELECT * FROM cv_revisions WHERE variant_id = ? ORDER BY number DESC LIMIT ? OFFSET ?",
    )
    .bind(variant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(conn)
    .await?)
}

pub async fn count(conn: &mut SqliteConnection, variant_id: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM cv_revisions WHERE variant_id = ?")
            .bind(variant_id)
            .fetch_one(conn)
            .await?,
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewRevision<'a> {
    pub parent_id: Option<i64>,
    pub source: &'a str,
    pub facts_version: i64,
    pub origin: &'a str,
    pub summary: &'a str,
    pub warnings: &'a [String],
}

/// Adds a revision after the variant's head and moves the head to it. Run inside `Db::begin_write`.
pub async fn append(
    conn: &mut SqliteConnection,
    variant_id: i64,
    new: &NewRevision<'_>,
) -> Result<Revision> {
    let revision: Revision = sqlx::query_as(
        "INSERT INTO cv_revisions (variant_id, number, parent_id, source, source_sha256,
             facts_version, origin, summary, warnings)
         VALUES (?1, (SELECT coalesce(max(number), 0) + 1 FROM cv_revisions WHERE variant_id = ?1),
             ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING *",
    )
    .bind(variant_id)
    .bind(new.parent_id)
    .bind(new.source)
    .bind(sha256_hex(new.source.as_bytes()))
    .bind(new.facts_version)
    .bind(new.origin)
    .bind(new.summary)
    .bind(Json(new.warnings))
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("UPDATE cv_variants SET head_revision_id = ? WHERE id = ?")
        .bind(revision.id)
        .bind(variant_id)
        .execute(conn)
        .await?;
    Ok(revision)
}
