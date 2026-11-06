//! The owner's approvals of CV text. Nothing reachable from MCP or voice writes here except on the
//! owner's behalf: see SPEC section 9a. A revision is approved or a draft by what this table holds,
//! never by anything stored on the revision or passed by a caller.

use nexus_tailor::Facts;
use sqlx::{AssertSqlSafe, SqliteConnection};

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

/// SQL saying whether a source is approved in a variant, under these facts. An approval the owner has
/// since replaced in a variant stops counting there, so the text must be the variant's newest approved
/// one. A variant with no approval of its own takes text from elsewhere, as long as that hasn't been
/// replaced where it was approved: a new variant can't bring back what the owner corrected. Its
/// arguments are SQL written in this file, never data.
fn approved_sql(variant: &str, source: &str, facts: &str) -> String {
    format!(
        "COALESCE(
            {source} = (SELECT own.source_sha256 FROM cv_approvals own
                WHERE own.variant_id = {variant} AND own.facts_sha256 = {facts}
                ORDER BY own.id DESC LIMIT 1),
            EXISTS (SELECT 1 FROM cv_approvals a
                WHERE a.source_sha256 = {source} AND a.facts_sha256 = {facts}
                AND a.id = (SELECT max(b.id) FROM cv_approvals b
                    WHERE b.variant_id = a.variant_id AND b.facts_sha256 = {facts}))
        )"
    )
}

/// `variant_id` is none for a variant that doesn't exist yet.
pub async fn is_approved(
    conn: &mut SqliteConnection,
    variant_id: Option<i64>,
    source_sha256: &str,
    facts_sha256: &str,
) -> Result<bool> {
    let sql = format!("SELECT {}", approved_sql("?1", "?2", "?3"));
    Ok(sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(variant_id)
        .bind(source_sha256)
        .bind(facts_sha256)
        .fetch_one(conn)
        .await?)
}

/// Who made a variant's newest approval under these facts, if it has one: a lock doesn't speak over
/// the owner's own.
pub async fn newest_approved_by(
    conn: &mut SqliteConnection,
    variant_id: i64,
    facts_sha256: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT approved_by FROM cv_approvals WHERE variant_id = ? AND facts_sha256 = ?
         ORDER BY id DESC LIMIT 1",
    )
    .bind(variant_id)
    .bind(facts_sha256)
    .fetch_optional(conn)
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

/// Run inside `Db::begin_write`. Approving what is the variant's newest approved text changes nothing.
/// Approving what the owner replaced there makes it the newest again.
pub async fn record(conn: &mut SqliteConnection, new: &NewApproval<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cv_approvals (source_sha256, facts_sha256, rendered_text, variant_id,
             revision_number, approved_by, approved_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
         WHERE COALESCE(?1 <> (SELECT source_sha256 FROM cv_approvals
             WHERE variant_id = ?4 AND facts_sha256 = ?2 ORDER BY id DESC LIMIT 1), 1)",
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

/// What a variant's next draft is read against, and at which revision: its newest approval under these
/// facts, or for a variant with none of its own, the newest approval of a text it holds. By approval,
/// not revision number: a revert to older text must not hide the correction it undoes. Under other
/// facts it is no baseline, since a line the owner has corrected would read as unchanged.
pub async fn last_approved(
    conn: &mut SqliteConnection,
    variant: &Variant,
    facts_sha256: &str,
) -> Result<Option<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT a.rendered_text, r.number
         FROM cv_revisions r JOIN cv_approvals a ON a.source_sha256 = r.source_sha256
         WHERE r.variant_id = ?1 AND a.facts_sha256 = ?2
         ORDER BY a.variant_id = ?1 DESC, a.id DESC, r.number DESC
         LIMIT 1",
    )
    .bind(variant.id)
    .bind(facts_sha256)
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
    let sql = format!(
        "SELECT v.name, v.application_id, r.number, r.origin, r.summary, r.created_at
         FROM cv_variants v JOIN cv_revisions r ON r.id = v.head_revision_id
         WHERE NOT {}
         ORDER BY v.name <> 'default', v.name",
        approved_sql("v.id", "r.source_sha256", "?1")
    );
    Ok(sqlx::query_as(AssertSqlSafe(sql))
        .bind(facts_sha256)
        .fetch_all(conn)
        .await?)
}
