use nexus_tailor::Employment;
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;
use sqlx::types::Json;

use super::{applications, cv_facts, id_string, opt_id_string};
use crate::error::{Error, Result};
use crate::time::Timestamp;

/// As clients have always seen it: facts and wording together, whichever table holds them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CvProfile {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    pub name: String,
    pub contact: Json<Value>,
    pub profile: String,
    pub skills: Json<Value>,
    pub experience: Json<Value>,
    pub projects: Json<Value>,
    pub education: Json<Value>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CvProfileInput {
    pub name: String,
    pub contact: Value,
    pub profile: String,
    pub skills: Value,
    pub experience: Value,
    /// `[]` when omitted, on update too, as in `upsertCvProfile`.
    pub projects: Option<Value>,
    pub education: Option<Value>,
}

/// What an experience entry holds that is a fact. The rest of it is wording.
const EXPERIENCE_FACTS: [&str; 4] = ["company", "title", "date", "location"];

/// Splits an experience array into `cv_facts.employment` and what `cv_profile.experience` keeps.
pub(crate) fn split_experience(experience: &Value) -> (Value, Value) {
    let entries = experience.as_array().map_or(&[][..], Vec::as_slice);
    let mut employment = Vec::with_capacity(entries.len());
    let mut wording = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut fact = Map::new();
        let mut kept = Map::new();
        for (key, value) in entry.as_object().into_iter().flatten() {
            if key == "id" {
                fact.insert(key.clone(), value.clone());
                kept.insert(key.clone(), value.clone());
            } else if EXPERIENCE_FACTS.contains(&key.as_str()) {
                fact.insert(key.clone(), value.clone());
            } else {
                kept.insert(key.clone(), value.clone());
            }
        }
        employment.push(Value::Object(fact));
        wording.push(Value::Object(kept));
    }
    (Value::Array(employment), Value::Array(wording))
}

/// `cv_facts.education` from a profile's education array. Old Nexus has no IDs here, so an entry's
/// ID is its position, and `degree` is called `qualification`.
pub(crate) fn split_education(education: &Value) -> Value {
    let entries = education.as_array().map_or(&[][..], Vec::as_slice);
    let split = entries.iter().enumerate().map(|(index, entry)| {
        let mut fact = Map::new();
        fact.insert("id".to_owned(), format!("edu-{}", index + 1).into());
        for (key, value) in entry.as_object().into_iter().flatten() {
            let key = if key == "degree" {
                "qualification"
            } else {
                key
            };
            fact.insert(key.to_owned(), value.clone());
        }
        Value::Object(fact)
    });
    Value::Array(split.collect())
}

fn merge_experience(wording: &Value, employment: &Value) -> Value {
    let entries = wording.as_array().map_or(&[][..], Vec::as_slice);
    let facts = employment.as_array().map_or(&[][..], Vec::as_slice);
    let merged = entries.iter().enumerate().map(|(index, entry)| {
        let id = &entry["id"];
        // By position first: only that tells apart two entries sharing an ID, which old Nexus allowed.
        let fact = facts
            .get(index)
            .filter(|fact| &fact["id"] == id)
            .or_else(|| facts.iter().find(|fact| &fact["id"] == id));
        let mut out = Map::new();
        out.insert("id".to_owned(), id.clone());
        for key in EXPERIENCE_FACTS {
            if let Some(value) = fact.and_then(|fact| fact.get(key)) {
                out.insert(key.to_owned(), value.clone());
            }
        }
        for (key, value) in entry.as_object().into_iter().flatten() {
            if key != "id" {
                out.insert(key.clone(), value.clone());
            }
        }
        Value::Object(out)
    });
    Value::Array(merged.collect())
}

fn merge_education(education: &Value) -> Value {
    let entries = education.as_array().map_or(&[][..], Vec::as_slice);
    let merged = entries.iter().map(|entry| {
        let mut out = Map::new();
        for (key, value) in entry.as_object().into_iter().flatten() {
            match key.as_str() {
                "id" => {}
                "qualification" => {
                    out.insert("degree".to_owned(), value.clone());
                }
                _ => {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        Value::Object(out)
    });
    Value::Array(merged.collect())
}

#[derive(sqlx::FromRow)]
struct ProfileRow {
    id: i64,
    name: String,
    contact: Json<Value>,
    profile: String,
    skills: Json<Value>,
    experience: Json<Value>,
    employment: Json<Value>,
    projects: Json<Value>,
    education: Json<Value>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

pub async fn get_profile(conn: &mut SqliteConnection) -> Result<Option<CvProfile>> {
    let row: Option<ProfileRow> = sqlx::query_as(
        "SELECT p.id, f.name, f.contact, p.profile, p.skills, p.experience, f.employment,
             p.projects, f.education, p.created_at, p.updated_at
         FROM cv_profile p, cv_facts f",
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|row| CvProfile {
        id: row.id,
        name: row.name,
        contact: row.contact,
        profile: row.profile,
        skills: row.skills,
        experience: Json(merge_experience(&row.experience.0, &row.employment.0)),
        projects: row.projects,
        education: Json(merge_education(&row.education.0)),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

fn strings_in<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings_in(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings_in(item, out)),
        _ => {}
    }
}

/// Every piece of a profile's wording, with the field it sits in.
fn wording_pieces(
    profile: &str,
    skills: &Value,
    experience: &Value,
    projects: &Value,
) -> Vec<(String, String)> {
    let mut pieces = vec![("profile".to_owned(), profile.to_owned())];
    let mut add = |field: String, value: &Value| {
        let mut strings = Vec::new();
        strings_in(value, &mut strings);
        pieces.extend(strings.into_iter().map(|s| (field.clone(), s.to_owned())));
    };
    add("skills".to_owned(), skills);
    for entry in experience.as_array().map_or(&[][..], Vec::as_slice) {
        let id = entry["id"].as_str().unwrap_or_default();
        add(format!("experience.{id}.bullets"), &entry["bullets"]);
    }
    add("projects".to_owned(), projects);
    pieces
}

/// The wording of the stored profile, if any: what locking the facts confirms.
pub(crate) async fn stored_wording(conn: &mut SqliteConnection) -> Result<Vec<(String, String)>> {
    let row: Option<(String, Json<Value>, Json<Value>, Json<Value>)> =
        sqlx::query_as("SELECT profile, skills, experience, projects FROM cv_profile")
            .fetch_optional(conn)
            .await?;
    Ok(
        row.map_or_else(Vec::new, |(profile, skills, experience, projects)| {
            wording_pieces(&profile, &skills.0, &experience.0, &projects.0)
        }),
    )
}

/// Why a profile can't be written over locked facts, per SPEC section 9a.
async fn locked_changes(
    conn: &mut SqliteConnection,
    state: &cv_facts::FactsState,
    input: &CvProfileInput,
    employment: &Value,
    wording: &Value,
    education: &Value,
) -> Result<(Vec<String>, Vec<String>)> {
    let (contact, stored_education): (Json<Value>, Json<Value>) =
        sqlx::query_as("SELECT contact, education FROM cv_facts")
            .fetch_one(conn)
            .await?;
    let mut fields = Vec::new();
    if input.name != state.facts.name {
        fields.push("name".to_owned());
    }
    if input.contact != contact.0 {
        fields.push("contact".to_owned());
    }
    if *education != stored_education.0 {
        fields.push("education".to_owned());
    }
    for entry in employment.as_array().map_or(&[][..], Vec::as_slice) {
        let given: Employment = serde_json::from_value(entry.clone()).unwrap_or_default();
        let Some(fact) = state.facts.employment.iter().find(|e| e.id == given.id) else {
            fields.push(format!("experience.{}", given.id));
            continue;
        };
        for (key, a, b) in [
            ("company", &given.company, &fact.company),
            ("title", &given.title, &fact.title),
            ("date", &given.date, &fact.date),
            ("location", &given.location, &fact.location),
        ] {
            if a != b {
                fields.push(format!("experience.{}.{key}", given.id));
            }
        }
    }
    let empty = Value::Array(Vec::new());
    let mut tokens = Vec::new();
    let pieces = wording_pieces(
        &input.profile,
        &input.skills,
        wording,
        input.projects.as_ref().unwrap_or(&empty),
    );
    for (field, text) in pieces {
        let found = nexus_tailor::unverified_numbers(&text, &state.facts);
        if !found.is_empty() && !fields.contains(&field) {
            fields.push(field);
        }
        for token in found {
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }
    }
    Ok((fields, tokens))
}

/// While the facts are unlocked, writes facts and wording as old Nexus's `upsertCvProfile` did.
/// While they are locked, writes wording only, or refuses with [`Error::CvFactsLocked`] and writes
/// nothing. Never records `cv_facts_history`: that belongs to the owner's paths. Run inside
/// `Db::begin_write`.
pub async fn upsert_profile(
    conn: &mut SqliteConnection,
    input: &CvProfileInput,
) -> Result<CvProfile> {
    let empty = Value::Array(Vec::new());
    let now = Timestamp::now();
    let (employment, wording) = split_experience(&input.experience);
    let education = split_education(input.education.as_ref().unwrap_or(&empty));
    let state = cv_facts::get(&mut *conn).await?;
    if state.is_locked() {
        let (fields, tokens) =
            locked_changes(&mut *conn, &state, input, &employment, &wording, &education).await?;
        if !fields.is_empty() {
            return Err(Error::CvFactsLocked { fields, tokens });
        }
    } else {
        sqlx::query(
            "UPDATE cv_facts SET
                 version = version + (name IS NOT ?1 OR contact IS NOT ?2 OR employment IS NOT ?3
                     OR education IS NOT ?4),
                 name = ?1, contact = ?2, employment = ?3, education = ?4, updated_at = ?5
             WHERE locked_at IS NULL",
        )
        .bind(&input.name)
        .bind(Json(&input.contact))
        .bind(Json(&employment))
        .bind(Json(&education))
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        "INSERT INTO cv_profile (profile, skills, experience, projects, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (singleton) DO UPDATE SET
             profile = ?1, skills = ?2, experience = ?3, projects = ?4, updated_at = ?5",
    )
    .bind(&input.profile)
    .bind(Json(&input.skills))
    .bind(Json(&wording))
    .bind(Json(input.projects.as_ref().unwrap_or(&empty)))
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    get_profile(conn).await?.ok_or(Error::NotFound)
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct CvPatch {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    #[serde(serialize_with = "id_string")]
    pub application_id: i64,
    pub profile_override: Option<String>,
    pub experience_ids: Json<Value>,
    pub skill_categories: Json<Value>,
    pub include_projects: bool,
    pub include_education: bool,
    #[serde(serialize_with = "opt_id_string")]
    pub document_id: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CvPatchInput {
    pub profile_override: Option<String>,
    pub experience_ids: Vec<String>,
    pub skill_categories: Vec<String>,
    pub include_projects: Option<bool>,
    pub include_education: Option<bool>,
}

pub async fn get_patch(
    conn: &mut SqliteConnection,
    application_id: i64,
) -> Result<Option<CvPatch>> {
    Ok(
        sqlx::query_as("SELECT * FROM cv_patches WHERE application_id = ?")
            .bind(application_id)
            .fetch_optional(conn)
            .await?,
    )
}

pub async fn upsert_patch(
    conn: &mut SqliteConnection,
    application_id: i64,
    input: &CvPatchInput,
) -> Result<CvPatch> {
    if !applications::exists(&mut *conn, application_id).await? {
        return Err(Error::NotFound);
    }
    Ok(sqlx::query_as(
        "INSERT INTO cv_patches (application_id, profile_override, experience_ids,
             skill_categories, include_projects, include_education, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (application_id) DO UPDATE SET
             profile_override = ?2, experience_ids = ?3, skill_categories = ?4,
             include_projects = ?5, include_education = ?6, updated_at = ?7
         RETURNING *",
    )
    .bind(application_id)
    .bind(&input.profile_override)
    .bind(Json(&input.experience_ids))
    .bind(Json(&input.skill_categories))
    .bind(input.include_projects.unwrap_or(false))
    .bind(input.include_education.unwrap_or(true))
    .bind(Timestamp::now())
    .fetch_one(conn)
    .await?)
}

/// Port of `setCvPatchDocumentId`: both the patch and any given document must exist.
pub async fn set_patch_document_id(
    conn: &mut SqliteConnection,
    patch_id: i64,
    document_id: Option<i64>,
) -> Result<()> {
    if let Some(document_id) = document_id {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM documents WHERE id = ?")
            .bind(document_id)
            .fetch_optional(&mut *conn)
            .await?;
        if found.is_none() {
            return Err(Error::NotFound);
        }
    }
    let done = sqlx::query("UPDATE cv_patches SET document_id = ?, updated_at = ? WHERE id = ?")
        .bind(document_id)
        .bind(Timestamp::now())
        .bind(patch_id)
        .execute(conn)
        .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}
