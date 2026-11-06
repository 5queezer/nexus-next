use serde::Serialize;
use serde_json::Value;
use sqlx::SqliteConnection;
use sqlx::types::Json;

use super::{applications, id_string, opt_id_string};
use crate::error::{Error, Result};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
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

const PROFILE_COLUMNS: &str =
    "id, name, contact, profile, skills, experience, projects, education, created_at, updated_at";

pub async fn get_profile(conn: &mut SqliteConnection) -> Result<Option<CvProfile>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PROFILE_COLUMNS} FROM cv_profile"
    )))
    .fetch_optional(conn)
    .await?)
}

pub async fn upsert_profile(
    conn: &mut SqliteConnection,
    input: &CvProfileInput,
) -> Result<CvProfile> {
    let empty = Value::Array(Vec::new());
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO cv_profile (name, contact, profile, skills, experience, projects, education,
             updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (singleton) DO UPDATE SET
             name = ?1, contact = ?2, profile = ?3, skills = ?4, experience = ?5, projects = ?6,
             education = ?7, updated_at = ?8
         RETURNING {PROFILE_COLUMNS}"
    )))
    .bind(&input.name)
    .bind(Json(&input.contact))
    .bind(&input.profile)
    .bind(Json(&input.skills))
    .bind(Json(&input.experience))
    .bind(Json(input.projects.as_ref().unwrap_or(&empty)))
    .bind(Json(input.education.as_ref().unwrap_or(&empty)))
    .bind(Timestamp::now())
    .fetch_one(conn)
    .await?)
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
