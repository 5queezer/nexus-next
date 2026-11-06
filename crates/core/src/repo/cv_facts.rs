//! The hard facts of a CV. MCP can write them only through `upsert_cv_profile`, and only while they
//! are unlocked. The functions here are the owner's paths: `nexus cv-facts` now, the dashboard later.

use std::collections::BTreeMap;

use nexus_tailor::{Contact, Education, Employment, Facts, Language, Metric};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
pub(crate) async fn record_history(
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

/// One fact field that differs from the last locked state. `old` and `new` are quoted and escaped, so
/// that an agent's text can't rewrite the owner's terminal, and `(none)` when there is no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactChange {
    pub field: String,
    pub old: String,
    pub new: String,
}

/// What a lock would confirm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockPreview {
    pub changes: Vec<FactChange>,
    /// Tokens the profile's wording would add to `allowed_tokens`.
    pub added: Vec<String>,
}

impl LockPreview {
    pub fn needs_confirmation(&self) -> bool {
        !self.changes.is_empty() || !self.added.is_empty()
    }
}

/// Every value in a fact as `path -> text`. An entry's path holds its position and its name, because
/// IDs can repeat. Empty values are left out: they read the same as no value.
fn flatten(path: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    let child = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                flatten(&child(key), value, out);
            }
        }
        Value::Array(items) if items.iter().any(Value::is_object) => {
            for (index, item) in items.iter().enumerate() {
                let name = ["id", "key", "name"]
                    .iter()
                    .find_map(|key| item[key].as_str())
                    .unwrap_or_default();
                flatten(&format!("{path}[{index}:{name}]"), item, out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            let texts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            out.insert(path.to_owned(), format!("{texts:?}"));
        }
        Value::String(text) if !text.is_empty() => {
            out.insert(path.to_owned(), format!("{text:?}"));
        }
        _ => {}
    }
}

fn fact_changes(old: &Facts, new: &Facts) -> Vec<FactChange> {
    let flat = |facts: &Facts| {
        let mut out = BTreeMap::new();
        let value = serde_json::to_value(facts).expect("facts serialize");
        flatten("", &value, &mut out);
        out
    };
    let (old, new) = (flat(old), flat(new));
    let none = "(none)".to_owned();
    let mut fields: Vec<&String> = old.keys().chain(new.keys()).collect();
    fields.sort_unstable();
    fields.dedup();
    fields
        .into_iter()
        .filter(|field| old.get(*field) != new.get(*field))
        .map(|field| FactChange {
            field: field.clone(),
            old: old.get(field).unwrap_or(&none).clone(),
            new: new.get(field).unwrap_or(&none).clone(),
        })
        .collect()
}

/// The facts as they last stood locked: an unlock records the state before it. Empty before the first
/// lock.
async fn last_locked(conn: &mut SqliteConnection) -> Result<Facts> {
    let state: Option<Json<Facts>> = sqlx::query_scalar(
        "SELECT state FROM cv_facts_history WHERE change = 'unlock' ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(conn)
    .await?;
    Ok(state.map_or_else(Facts::default, |state| state.0))
}

/// Refuses what can't be locked, and moves tokens from the profile's wording into `facts`.
async fn derive_tokens(
    conn: &mut SqliteConnection,
    state: &FactsState,
) -> Result<(Facts, Vec<String>)> {
    if state.is_locked() {
        return Err(Error::CvFacts("the facts are already locked"));
    }
    if state.facts.name.trim().is_empty() {
        return Err(Error::CvFacts(
            "there is nothing to lock: no profile has set the name yet",
        ));
    }
    let mut facts = state.facts.clone();
    let mut added = Vec::new();
    for (_, text) in cv::stored_wording(conn).await? {
        for token in nexus_tailor::unverified_numbers(&text, &facts) {
            facts.allowed_tokens.push(token.clone());
            added.push(token);
        }
        for url in nexus_tailor::unverified_urls(&text, &facts) {
            facts.allowed_tokens.push(url.clone());
            added.push(url);
        }
    }
    Ok((facts, added))
}

/// What locking now would confirm, compared with the last locked state. Writes nothing.
pub async fn lock_preview(conn: &mut SqliteConnection) -> Result<LockPreview> {
    let state = get(&mut *conn).await?;
    let (_, added) = derive_tokens(&mut *conn, &state).await?;
    let changes = fact_changes(&last_locked(conn).await?, &state.facts);
    Ok(LockPreview { changes, added })
}

/// Locks the facts. Numbers and URLs in the profile's wording join `allowed_tokens`: locking is the
/// owner confirming them, so show them [`lock_preview`] first. Run inside `Db::begin_write`.
pub async fn lock(
    conn: &mut SqliteConnection,
    changed_by: &str,
) -> Result<(FactsState, Vec<String>)> {
    let state = get(&mut *conn).await?;
    let (facts, added) = derive_tokens(&mut *conn, &state).await?;
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
