//! The owner's approvals of CV text. Nothing reachable from MCP or voice writes here except on the
//! owner's behalf: see SPEC section 9a. A revision is approved or a draft by what this table holds,
//! never by anything stored on the revision or passed by a caller.

use nexus_tailor::Facts;
use sqlx::SqliteConnection;

use crate::blob::sha256_hex;
use crate::error::Result;
use crate::repo::cv_revisions::Variant;
use crate::time::Timestamp;

/// A hash of what a CV can render. So `version`, `locked_at` or a vocabulary edit invalidate nothing,
/// and any changed fact invalidates every approval built on it. `Facts` is typed, so the JSON's key
/// order is fixed.
#[must_use]
pub fn facts_sha256(facts: &Facts) -> String {
    let renderable = serde_json::to_vec(&facts.renderable()).expect("facts serialize");
    sha256_hex(&renderable)
}

pub async fn is_approved(
    conn: &mut SqliteConnection,
    source_sha256: &str,
    facts_sha256: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cv_approvals WHERE source_sha256 = ? AND facts_sha256 = ?)",
    )
    .bind(source_sha256)
    .bind(facts_sha256)
    .fetch_one(conn)
    .await?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval<'a> {
    pub source_sha256: &'a str,
    pub facts_sha256: &'a str,
    pub rendered_text: &'a str,
    pub variant_id: i64,
    pub revision_number: i64,
    /// `cli`, `dashboard` or `lock`. Who the owner was acting as, set by the path, never by a caller.
    pub approved_by: &'a str,
}

/// Run inside `Db::begin_write`. Approving what is approved already changes nothing.
pub async fn record(conn: &mut SqliteConnection, new: &NewApproval<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cv_approvals (source_sha256, facts_sha256, rendered_text, variant_id,
             revision_number, approved_by, approved_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (source_sha256, facts_sha256) DO NOTHING",
    )
    .bind(new.source_sha256)
    .bind(new.facts_sha256)
    .bind(new.rendered_text)
    .bind(new.variant_id)
    .bind(new.revision_number)
    .bind(new.approved_by)
    .bind(Timestamp::now())
    .execute(conn)
    .await?;
    Ok(())
}

/// What the owner last approved of this variant, and at which revision, whichever facts it was
/// rendered from: a diff against the parent would let a chain of small drafts hide what changed.
pub async fn last_approved(
    conn: &mut SqliteConnection,
    variant: &Variant,
) -> Result<Option<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT a.rendered_text, r.number
         FROM cv_revisions r JOIN cv_approvals a ON a.source_sha256 = r.source_sha256
         WHERE r.variant_id = ?
         ORDER BY r.number DESC, a.id DESC
         LIMIT 1",
    )
    .bind(variant.id)
    .fetch_optional(conn)
    .await?)
}

/// A variant whose newest revision the owner hasn't approved under today's facts.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Awaiting {
    pub name: String,
    pub application_id: Option<i64>,
    pub number: i64,
    pub origin: String,
    pub summary: String,
    pub created_at: Timestamp,
}

/// `default` first, then by name.
pub async fn awaiting(conn: &mut SqliteConnection, facts_sha256: &str) -> Result<Vec<Awaiting>> {
    Ok(sqlx::query_as(
        "SELECT v.name, v.application_id, r.number, r.origin, r.summary, r.created_at
         FROM cv_variants v JOIN cv_revisions r ON r.id = v.head_revision_id
         WHERE NOT EXISTS (
             SELECT 1 FROM cv_approvals a
             WHERE a.source_sha256 = r.source_sha256 AND a.facts_sha256 = ?
         )
         ORDER BY v.name <> 'default', v.name",
    )
    .bind(facts_sha256)
    .fetch_all(conn)
    .await?)
}
