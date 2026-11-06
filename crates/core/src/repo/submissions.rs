use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqliteConnection;
use sqlx::types::Json;

use super::{applications, id_string};
use crate::error::{Error, Result};
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    #[serde(serialize_with = "id_string")]
    pub application_id: i64,
    pub idempotency_key: String,
    pub request_hash: String,
    pub submitted_at: Timestamp,
    pub application_url: Option<String>,
    pub ats_name: Option<String>,
    pub requisition_id: Option<String>,
    pub language: Option<String>,
    pub answers: Json<Value>,
    pub policy: Json<Value>,
    pub candidate_salary_min: Option<i64>,
    pub candidate_salary_max: Option<i64>,
    pub candidate_salary_currency: Option<String>,
    pub candidate_salary_period: Option<String>,
    pub candidate_salary_type: Option<String>,
    pub candidate_salary_flexible: bool,
    pub document_ids: Json<Value>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSubmission {
    pub application_id: i64,
    pub idempotency_key: String,
    pub request_hash: String,
    pub submitted_at: Timestamp,
    pub application_url: Option<String>,
    pub ats_name: Option<String>,
    pub requisition_id: Option<String>,
    pub language: Option<String>,
    pub answers: Value,
    pub policy: Value,
    pub candidate_salary_min: Option<i64>,
    pub candidate_salary_max: Option<i64>,
    pub candidate_salary_currency: Option<String>,
    pub candidate_salary_period: Option<String>,
    pub candidate_salary_type: Option<String>,
    pub candidate_salary_flexible: bool,
    pub document_ids: Vec<String>,
}

/// Inserts the row only. Claiming documents, updating the application and writing the event are
/// the caller's, inside one `Db::begin_write`.
pub async fn insert(conn: &mut SqliteConnection, new: &NewSubmission) -> Result<Submission> {
    Ok(sqlx::query_as(
        "INSERT INTO application_submissions (application_id, idempotency_key, request_hash,
             submitted_at, application_url, ats_name, requisition_id, language, answers, policy,
             candidate_salary_min, candidate_salary_max, candidate_salary_currency,
             candidate_salary_period, candidate_salary_type, candidate_salary_flexible,
             document_ids)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(new.application_id)
    .bind(&new.idempotency_key)
    .bind(&new.request_hash)
    .bind(&new.submitted_at)
    .bind(&new.application_url)
    .bind(&new.ats_name)
    .bind(&new.requisition_id)
    .bind(&new.language)
    .bind(Json(&new.answers))
    .bind(Json(&new.policy))
    .bind(new.candidate_salary_min)
    .bind(new.candidate_salary_max)
    .bind(&new.candidate_salary_currency)
    .bind(&new.candidate_salary_period)
    .bind(&new.candidate_salary_type)
    .bind(new.candidate_salary_flexible)
    .bind(Json(&new.document_ids))
    .fetch_one(conn)
    .await?)
}

pub async fn get(conn: &mut SqliteConnection, id: i64) -> Result<Option<Submission>> {
    Ok(
        sqlx::query_as("SELECT * FROM application_submissions WHERE id = ?")
            .bind(id)
            .fetch_optional(conn)
            .await?,
    )
}

pub async fn find_by_idempotency_key(
    conn: &mut SqliteConnection,
    key: &str,
) -> Result<Option<Submission>> {
    Ok(
        sqlx::query_as("SELECT * FROM application_submissions WHERE idempotency_key = ?")
            .bind(key)
            .fetch_optional(conn)
            .await?,
    )
}

/// Newest first. Answers read as `[]` unless requested, as in `listApplicationSubmissions`.
pub async fn list_for_application(
    conn: &mut SqliteConnection,
    application_id: i64,
    include_answers: bool,
) -> Result<Vec<Submission>> {
    if !applications::exists(&mut *conn, application_id).await? {
        return Err(Error::NotFound);
    }
    let rows = sqlx::query(
        "SELECT * FROM application_submissions WHERE application_id = ?
         ORDER BY submitted_at DESC, id DESC",
    )
    .bind(application_id)
    .fetch_all(conn)
    .await?;
    let mut rows: Vec<Submission> = super::decode_rows(rows);
    if !include_answers {
        for row in &mut rows {
            row.answers = Json(json!([]));
        }
    }
    Ok(rows)
}

pub async fn list(conn: &mut SqliteConnection) -> Result<Vec<Submission>> {
    Ok(
        sqlx::query_as("SELECT * FROM application_submissions ORDER BY submitted_at DESC, id DESC")
            .fetch_all(conn)
            .await?,
    )
}

pub async fn count_for_application(
    conn: &mut SqliteConnection,
    application_id: i64,
) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM application_submissions WHERE application_id = ?")
            .bind(application_id)
            .fetch_one(conn)
            .await?,
    )
}
