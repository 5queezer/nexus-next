//! The hard facts of a CV. MCP can write them only through `upsert_cv_profile`, and only while they
//! are unlocked. The functions here are the owner's paths: `nexus cv-facts` now, the dashboard later.

use nexus_tailor::{Contact, Education, Employment, Facts, Language, Metric};
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;
use sqlx::types::Json;

use super::cv;
use crate::error::{Error, Result};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactsState {
    #[serde(flatten)]
    pub facts: Facts,
    pub version: i64,
    pub locked_at: Option<Timestamp>,
    pub updated_at: Timestamp,
}

impl FactsState {
    pub fn is_locked(&self) -> bool {
        self.locked_at.is_some()
    }
}

#[derive(sqlx::FromRow)]
struct FactsRow {
    name: String,
    contact: Json<Contact>,
    employment: Json<Vec<Employment>>,
    education: Json<Vec<Education>>,
    languages: Json<Vec<Language>>,
    metrics: Json<Vec<Metric>>,
    allowed_tokens: Json<Vec<String>>,
    banned_phrases: Json<Vec<String>>,
    vocabulary: Json<Vec<String>>,
    version: i64,
    locked_at: Option<Timestamp>,
    updated_at: Timestamp,
}

pub async fn get(conn: &mut SqliteConnection) -> Result<FactsState> {
    let row: FactsRow = sqlx::query_as("SELECT * FROM cv_facts")
        .fetch_one(conn)
        .await?;
    Ok(FactsState {
        facts: Facts {
            name: row.name,
            contact: row.contact.0,
            employment: row.employment.0,
            education: row.education.0,
            languages: row.languages.0,
            metrics: row.metrics.0,
            allowed_tokens: row.allowed_tokens.0,
            banned_phrases: row.banned_phrases.0,
            vocabulary: row.vocabulary.0,
        },
        version: row.version,
        locked_at: row.locked_at,
        updated_at: row.updated_at,
    })
}

/// Saves the state as it is now, before a change.
async fn record_history(
    conn: &mut SqliteConnection,
    change: &str,
    changed_by: &str,
    now: &Timestamp,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cv_facts_history (version, state, change, changed_by, changed_at)
         SELECT version, json_object(
                 'name', name, 'contact', json(contact), 'employment', json(employment),
                 'education', json(education), 'languages', json(languages),
                 'metrics', json(metrics), 'allowedTokens', json(allowed_tokens),
                 'bannedPhrases', json(banned_phrases), 'vocabulary', json(vocabulary),
                 'lockedAt', locked_at
             ), ?, ?, ?
         FROM cv_facts",
    )
    .bind(change)
    .bind(changed_by)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// The fields the profile can't express. Those left out stay as they are. Anything else in the file
/// is refused: employment, education, name and contact come from the profile while unlocked.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerFacts {
    pub languages: Option<Vec<Language>>,
    pub metrics: Option<Vec<Metric>>,
    pub allowed_tokens: Option<Vec<String>>,
    pub banned_phrases: Option<Vec<String>>,
    pub vocabulary: Option<Vec<String>>,
}

/// An owner's edit, locked or not. Run inside `Db::begin_write`.
pub async fn set_owner_facts(
    conn: &mut SqliteConnection,
    fields: &OwnerFacts,
    changed_by: &str,
) -> Result<FactsState> {
    let now = Timestamp::now();
    record_history(&mut *conn, "set", changed_by, &now).await?;
    sqlx::query(
        "UPDATE cv_facts SET
             languages = coalesce(?, languages), metrics = coalesce(?, metrics),
             allowed_tokens = coalesce(?, allowed_tokens),
             banned_phrases = coalesce(?, banned_phrases), vocabulary = coalesce(?, vocabulary),
             version = version + 1, updated_at = ?",
    )
    .bind(fields.languages.as_ref().map(Json))
    .bind(fields.metrics.as_ref().map(Json))
    .bind(fields.allowed_tokens.as_ref().map(Json))
    .bind(fields.banned_phrases.as_ref().map(Json))
    .bind(fields.vocabulary.as_ref().map(Json))
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    get(conn).await
}

/// Locks the facts. Numbers in the profile's wording join `allowed_tokens`: locking is the owner
/// confirming them, so show them `added`. Run inside `Db::begin_write`.
pub async fn lock(
    conn: &mut SqliteConnection,
    changed_by: &str,
) -> Result<(FactsState, Vec<String>)> {
    let state = get(&mut *conn).await?;
    if state.is_locked() {
        return Err(Error::CvFacts("the facts are already locked"));
    }
    if state.facts.name.trim().is_empty() {
        return Err(Error::CvFacts(
            "there is nothing to lock: no profile has set the name yet",
        ));
    }
    let mut facts = state.facts;
    let mut added = Vec::new();
    for (_, text) in cv::stored_wording(&mut *conn).await? {
        for token in nexus_tailor::unverified_numbers(&text, &facts) {
            facts.allowed_tokens.push(token.clone());
            added.push(token);
        }
    }
    let now = Timestamp::now();
    record_history(&mut *conn, "lock", changed_by, &now).await?;
    sqlx::query(
        "UPDATE cv_facts SET allowed_tokens = ?, locked_at = ?, version = version + 1,
             updated_at = ?",
    )
    .bind(Json(&facts.allowed_tokens))
    .bind(&now)
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    Ok((get(conn).await?, added))
}

/// Run inside `Db::begin_write`.
pub async fn unlock(conn: &mut SqliteConnection, changed_by: &str) -> Result<FactsState> {
    if !get(&mut *conn).await?.is_locked() {
        return Err(Error::CvFacts("the facts are not locked"));
    }
    let now = Timestamp::now();
    record_history(&mut *conn, "unlock", changed_by, &now).await?;
    sqlx::query("UPDATE cv_facts SET locked_at = NULL, version = version + 1, updated_at = ?")
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    get(conn).await
}
