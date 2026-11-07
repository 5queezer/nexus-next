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

/// SQL saying whether some variant has replaced a source: it held that text, or started from it with a
/// change in the same call, and the owner has since approved another there. A lock moving between two
/// selections replaces nothing, as nobody corrected anything. Under any facts, since a correction doesn't
/// stop counting when a fact comes back. Nothing tells a correction from tailoring, so approving a
/// changed text counts either way. A variant that approves the text again has taken it back. Its argument
/// is SQL written in this file.
fn replaced_sql(source: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM cv_approvals newest
            JOIN cv_revisions held ON held.variant_id = newest.variant_id
                AND held.number <= newest.revision_number
            LEFT JOIN cv_revisions parent ON parent.id = held.parent_id
            WHERE {source} IN (held.source_sha256, parent.source_sha256)
            AND newest.source_sha256 <> {source} AND newest.approved_by <> 'lock'
            AND newest.id = (SELECT max(n.id) FROM cv_approvals n
                WHERE n.variant_id = newest.variant_id))"
    )
}

/// Whether some variant has replaced this source. The lock asks too: its approval of the owner's wording
/// doesn't speak over a correction made since.
pub async fn is_replaced(conn: &mut SqliteConnection, source_sha256: &str) -> Result<bool> {
    let sql = format!("SELECT {}", replaced_sql("?1"));
    Ok(sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(source_sha256)
        .fetch_one(conn)
        .await?)
}

/// SQL saying whether a source is approved in a variant, under these facts. A variant's newest approval
/// decides, under whichever facts it was made: the text is approved only if that row's source and
/// facts both match, so an approval the owner has since replaced stops counting, even if the facts
/// come back to what they were. A lock's own row stops counting once any variant has replaced its text,
/// as nobody tapped it and a lock doesn't speak over a correction: a tap there writes a row of its own. A
/// row the owner tapped still stands. A variant with no approval of its own takes text from elsewhere, as
/// long as that is still the newest approval where it was made and no variant has replaced it: the
/// owner's decision is that a correction counts everywhere, so a new variant can't bring back what they
/// corrected in any of them. Its arguments are SQL written in this file, never data.
fn approved_sql(variant: &str, source: &str, facts: &str) -> String {
    let replaced = replaced_sql(source);
    format!(
        "COALESCE(
            (SELECT own.source_sha256 = {source} AND own.facts_sha256 = {facts}
                AND NOT (own.approved_by = 'lock' AND {replaced})
                FROM cv_approvals own
                WHERE own.variant_id = {variant}
                ORDER BY own.id DESC LIMIT 1),
            EXISTS (SELECT 1 FROM cv_approvals a
                WHERE a.source_sha256 = {source} AND a.facts_sha256 = {facts}
                AND a.id = (SELECT max(b.id) FROM cv_approvals b WHERE b.variant_id = a.variant_id))
            AND NOT {replaced}
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

/// Who made a variant's newest approval, under whichever facts, if it has one: a lock doesn't speak over
/// the owner's own.
pub async fn newest_approved_by(
    conn: &mut SqliteConnection,
    variant_id: i64,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT approved_by FROM cv_approvals WHERE variant_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(variant_id)
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

/// Run inside `Db::begin_write`. Approving what is the variant's newest approval, under these same
/// facts, changes nothing, unless that row is a lock's and this the owner's tap, which then stands where
/// the lock's wouldn't. Anything else becomes the newest, which is what decides.
pub async fn record(conn: &mut SqliteConnection, new: &NewApproval<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO cv_approvals (source_sha256, facts_sha256, rendered_text, variant_id,
             revision_number, approved_by, approved_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
         WHERE NOT COALESCE((SELECT source_sha256 = ?1 AND facts_sha256 = ?2
                 AND (approved_by <> 'lock' OR ?6 = 'lock') FROM cv_approvals
             WHERE variant_id = ?4 ORDER BY id DESC LIMIT 1), 0)",
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

/// What a draft is read against. See [`last_approved`]. It says nothing of what is approved:
/// [`is_approved`] alone says that.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Baseline {
    pub rendered_text: String,
    /// The revision holding that text: in the variant it started from, if `started_from`.
    pub revision: i64,
    pub revision_id: i64,
    /// The approval is this variant's own, not another's text it holds.
    pub own: bool,
    /// `cli`, `dashboard` or `lock`.
    pub approved_by: String,
    /// The text is what this variant started from in another, and it never held it.
    pub started_from: bool,
    /// Another variant has replaced this text since, whoever's approval this is, so a line the owner
    /// corrected there reads here as
    /// unchanged. The owner accepted that, as long as the review says so plainly.
    pub replaced: bool,
}

/// What a variant's next draft is read against, at which revision, and whether the approval is its own.
/// Its newest approval, if that was made under these facts: under others it is no baseline, since a
/// line the owner has corrected would read as unchanged. A variant with none of its own reads against the
/// newest approved text it holds or started from, under these facts: first one that still stands, then
/// one replaced since, which says so, and among those one other than its head, which would read as
/// nothing changed. By approval, not revision number: a revert to older text must not hide the
/// correction it undoes.
pub async fn last_approved(
    conn: &mut SqliteConnection,
    variant: &Variant,
    facts_sha256: &str,
) -> Result<Option<Baseline>> {
    let replaced = replaced_sql("a.source_sha256");
    let sql = format!(
        "WITH held (source_sha256, id, number, started_from) AS (
            SELECT source_sha256, id, number, 0 FROM cv_revisions WHERE variant_id = ?1
            UNION ALL
            SELECT p.source_sha256, p.id, p.number, 1
            FROM cv_revisions r JOIN cv_revisions p ON p.id = r.parent_id
            WHERE r.variant_id = ?1 AND p.variant_id <> ?1
         )
         SELECT a.rendered_text, h.number AS revision, h.id AS revision_id, a.variant_id = ?1 AS own,
             a.approved_by, h.started_from, {replaced} AS replaced
         FROM held h JOIN cv_approvals a ON a.source_sha256 = h.source_sha256
         WHERE a.facts_sha256 = ?2
         AND (a.id = (SELECT max(b.id) FROM cv_approvals b WHERE b.variant_id = a.variant_id)
             OR (a.variant_id <> ?1 AND {replaced}))
         AND (a.variant_id = ?1
             OR NOT EXISTS (SELECT 1 FROM cv_approvals own WHERE own.variant_id = ?1))
         ORDER BY replaced,
             a.source_sha256 = (SELECT r.source_sha256 FROM cv_revisions r
                 JOIN cv_variants v ON v.head_revision_id = r.id WHERE v.id = ?1),
             a.id DESC, h.started_from, h.number DESC
         LIMIT 1"
    );
    Ok(sqlx::query_as(AssertSqlSafe(sql))
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
