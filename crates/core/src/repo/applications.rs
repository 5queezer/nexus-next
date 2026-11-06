use serde::Serialize;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};

use super::{ApplicationRef, Page, id_string, partial_fields};
use crate::error::{Error, Result};
use crate::time::Timestamp;

const NOTES_MAX_LEN: i64 = 10_000;

/// A nullable JSON column that old Nexus exposes as an array, reading anything else as `[]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct JsonList(pub Vec<Value>);

impl From<Option<String>> for JsonList {
    fn from(text: Option<String>) -> Self {
        match text.and_then(|t| serde_json::from_str(&t).ok()) {
            Some(Value::Array(items)) => Self(items),
            _ => Self::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Application {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    pub company: String,
    pub role: String,
    pub status: String,
    pub applied_at: Option<Timestamp>,
    pub last_contact: Option<Timestamp>,
    pub notes: Option<String>,
    pub follow_up_at: Option<Timestamp>,
    pub job_description: Option<String>,
    pub source: Option<String>,
    pub remote: bool,
    pub salary_min: Option<i64>,
    pub salary_max: Option<i64>,
    pub rating: Option<i64>,
    pub job_url: Option<String>,
    pub canonical_job_url: Option<String>,
    pub resume_id: Option<String>,
    pub company_size: Option<String>,
    pub salary_band_mentioned: bool,
    pub triage_quality: Option<i64>,
    pub triage_reason: Option<String>,
    pub incoming_source: Option<String>,
    pub auto_rejected: bool,
    pub auto_reject_reason: Option<String>,
    pub archived_at: Option<Timestamp>,
    pub work_mode: Option<String>,
    #[sqlx(try_from = "Option<String>")]
    pub eligible_countries: JsonList,
    #[sqlx(try_from = "Option<String>")]
    pub primary_locations: JsonList,
    pub office_days_min: Option<i64>,
    pub travel_percent: Option<i64>,
    pub visa_sponsorship: Option<bool>,
    pub right_to_work_required: Option<bool>,
    pub timezone_overlap: Option<String>,
    pub salary_currency: Option<String>,
    pub salary_period: Option<String>,
    pub salary_type: Option<String>,
    pub ats_name: Option<String>,
    pub requisition_id: Option<String>,
    pub job_captured_at: Option<Timestamp>,
    pub job_verified_at: Option<Timestamp>,
    pub job_posted_at: Option<Timestamp>,
    pub job_closed_at: Option<Timestamp>,
    pub job_content_hash: Option<String>,
    pub job_liveness: Option<String>,
    pub job_summary: Option<String>,
    pub current_stage: Option<String>,
    pub next_action: Option<String>,
    pub event_version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

partial_fields! {
    /// Writable application columns, used for both create and update.
    ApplicationFields {
        company: String,
        role: String,
        status: String,
        applied_at: Option<Timestamp>,
        last_contact: Option<Timestamp>,
        notes: Option<String>,
        follow_up_at: Option<Timestamp>,
        job_description: Option<String>,
        source: Option<String>,
        remote: bool,
        salary_min: Option<i64>,
        salary_max: Option<i64>,
        rating: Option<i64>,
        job_url: Option<String>,
        canonical_job_url: Option<String>,
        resume_id: Option<String>,
        company_size: Option<String>,
        salary_band_mentioned: bool,
        triage_quality: Option<i64>,
        triage_reason: Option<String>,
        incoming_source: Option<String>,
        auto_rejected: bool,
        auto_reject_reason: Option<String>,
        archived_at: Option<Timestamp>,
        work_mode: Option<String>,
        eligible_countries: Json<Vec<String>>,
        primary_locations: Json<Vec<String>>,
        office_days_min: Option<i64>,
        travel_percent: Option<i64>,
        visa_sponsorship: Option<bool>,
        right_to_work_required: Option<bool>,
        timezone_overlap: Option<String>,
        salary_currency: Option<String>,
        salary_period: Option<String>,
        salary_type: Option<String>,
        ats_name: Option<String>,
        requisition_id: Option<String>,
        job_captured_at: Option<Timestamp>,
        job_verified_at: Option<Timestamp>,
        job_posted_at: Option<Timestamp>,
        job_closed_at: Option<Timestamp>,
        job_content_hash: Option<String>,
        job_liveness: Option<String>,
        job_summary: Option<String>,
        current_stage: Option<String>,
        next_action: Option<String>,
    }
}

/// Optimistic-concurrency condition on an update.
#[derive(Debug, Clone, Default)]
pub enum Guard {
    #[default]
    None,
    UpdatedAt(Timestamp),
    EventVersion(i64),
}

/// Port of `normalizeStatus`: unknown values read as `applied`.
pub fn normalize_status(status: &str) -> String {
    let normalized = status.trim().to_lowercase();
    match normalized.as_str() {
        "inbound" | "applied" | "interview" | "offer" | "rejected" => normalized,
        "ghost" => "rejected".to_owned(),
        _ => "applied".to_owned(),
    }
}

/// Port of `resolveAppliedAtForCreate`: an omitted date defaults to now only for a status that
/// means the application was already sent.
pub fn resolve_applied_at_for_create(status: &str, value: Option<Timestamp>) -> Option<Timestamp> {
    value.or_else(|| {
        matches!(status, "applied" | "interview" | "offer" | "rejected").then(Timestamp::now)
    })
}

/// Port of the length check in `validateApplicationSummary`, which counts UTF-16 code units.
pub fn check_notes_length(notes: &str) -> Result<()> {
    if notes.encode_utf16().count() as i64 > NOTES_MAX_LEN {
        return Err(Error::NotesTooLong);
    }
    Ok(())
}

fn normalized(mut application: Application) -> Application {
    application.status = normalize_status(&application.status);
    application
}

pub async fn get(conn: &mut SqliteConnection, id: i64) -> Result<Option<Application>> {
    let row = sqlx::query_as("SELECT * FROM applications WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(normalized))
}

pub async fn exists(conn: &mut SqliteConnection, id: i64) -> Result<bool> {
    let row: Option<i64> = sqlx::query_scalar("SELECT 1 FROM applications WHERE id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(row.is_some())
}

pub async fn find_by_canonical_job_url(
    conn: &mut SqliteConnection,
    canonical_job_url: &str,
) -> Result<Option<Application>> {
    let row = sqlx::query_as("SELECT * FROM applications WHERE canonical_job_url = ?")
        .bind(canonical_job_url)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(normalized))
}

pub async fn list(conn: &mut SqliteConnection) -> Result<Vec<Application>> {
    let rows: Vec<Application> =
        sqlx::query_as("SELECT * FROM applications ORDER BY created_at DESC, id DESC")
            .fetch_all(conn)
            .await?;
    Ok(rows.into_iter().map(normalized).collect())
}

pub async fn list_page(
    conn: &mut SqliteConnection,
    page: Option<i64>,
    page_size: Option<i64>,
) -> Result<Page<Application>> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(10).clamp(1, 100);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM applications")
        .fetch_one(&mut *conn)
        .await?;
    let rows: Vec<Application> = sqlx::query_as(
        "SELECT * FROM applications ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
    )
    .bind(page_size)
    .bind((page - 1) * page_size)
    .fetch_all(conn)
    .await?;
    Ok(Page {
        data: rows.into_iter().map(normalized).collect(),
        total,
        page,
        page_size,
        total_pages: (total + page_size - 1) / page_size,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    Company,
    Role,
    Notes,
    JobDescription,
}

#[derive(Debug, Clone, Default)]
pub struct ApplicationFilter {
    /// Matched against the stored status, before normalization, as in old Nexus.
    pub status: Vec<String>,
    pub rating_gte: Option<i64>,
    pub triage_quality_gte: Option<i64>,
    pub search: Option<String>,
    /// Defaults to all four fields.
    pub search_fields: Option<Vec<SearchField>>,
    pub remote: Option<bool>,
    /// A column name in camelCase, `-` prefixed for descending. Unknown names use the default order.
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<i64>,
}

fn sort_column(field: &str) -> Option<&'static str> {
    Some(match field {
        "createdAt" => "created_at",
        "updatedAt" => "updated_at",
        "company" => "company",
        "role" => "role",
        "status" => "status",
        "rating" => "rating",
        "salaryMin" => "salary_min",
        "salaryMax" => "salary_max",
        "appliedAt" => "applied_at",
        "lastContact" => "last_contact",
        "triageQuality" => "triage_quality",
        _ => return None,
    })
}

/// NULLs sort as Postgres does by default (last ascending, first descending), the reverse of
/// SQLite's default.
fn order_by(sort: Option<&str>) -> String {
    let sort = sort.and_then(|sort| {
        let (field, desc) = match sort.strip_prefix('-') {
            Some(field) => (field, true),
            None => (sort, false),
        };
        sort_column(field).map(|column| (column, desc))
    });
    match sort {
        Some((column, true)) => format!("{column} DESC NULLS FIRST, id DESC"),
        Some((column, false)) => format!("{column} ASC NULLS LAST, id ASC"),
        None => "created_at DESC, id DESC".to_owned(),
    }
}

fn push_filter(qb: &mut QueryBuilder<Sqlite>, filter: &ApplicationFilter) {
    qb.push(" WHERE 1 = 1");
    if !filter.status.is_empty() {
        qb.push(" AND status IN (");
        let mut statuses = qb.separated(", ");
        for status in &filter.status {
            statuses.push_bind(status.clone());
        }
        qb.push(")");
    }
    if let Some(rating) = filter.rating_gte {
        qb.push(" AND rating >= ").push_bind(rating);
    }
    if let Some(quality) = filter.triage_quality_gte {
        qb.push(" AND triage_quality >= ").push_bind(quality);
    }
    if let Some(remote) = filter.remote {
        qb.push(" AND remote = ").push_bind(remote);
    }
    if let Some(term) = &filter.search {
        let fields = filter.search_fields.as_deref().unwrap_or(&[
            SearchField::Company,
            SearchField::Role,
            SearchField::Notes,
            SearchField::JobDescription,
        ]);
        qb.push(" AND (0");
        for field in fields {
            let column = match field {
                SearchField::Company => "company",
                SearchField::Role => "role",
                SearchField::Notes => "notes",
                SearchField::JobDescription => "job_description",
            };
            qb.push(format!(" OR instr(lower({column}), lower("))
                .push_bind(term.clone())
                .push(")) > 0");
        }
        qb.push(")");
    }
}

/// Port of `listApplicationsFiltered`. Case-insensitive search covers ASCII only, unlike Postgres
/// `ILIKE`: SQLite's `lower()` leaves letters such as `Ü` unchanged.
pub async fn list_filtered(
    conn: &mut SqliteConnection,
    filter: &ApplicationFilter,
) -> Result<Vec<Application>> {
    if let Some(cursor) = filter.cursor {
        let mut qb = QueryBuilder::new("SELECT 1 FROM applications");
        push_filter(&mut qb, filter);
        qb.push(" AND id = ").push_bind(cursor);
        let found: Option<i64> = qb.build_query_scalar().fetch_optional(&mut *conn).await?;
        if found.is_none() {
            return Err(Error::ApplicationCursorInvalid);
        }
    }

    let mut qb = QueryBuilder::new(format!(
        "WITH ranked AS (SELECT *, ROW_NUMBER() OVER (ORDER BY {}) AS row_rank FROM applications",
        order_by(filter.sort.as_deref())
    ));
    push_filter(&mut qb, filter);
    qb.push(") SELECT * FROM ranked");
    if let Some(cursor) = filter.cursor {
        qb.push(" WHERE row_rank > (SELECT row_rank FROM ranked WHERE id = ")
            .push_bind(cursor)
            .push(")");
    }
    qb.push(" ORDER BY row_rank LIMIT ")
        .push_bind(filter.limit.unwrap_or(-1));
    let rows: Vec<Application> = qb.build_query_as().fetch_all(conn).await?;
    Ok(rows.into_iter().map(normalized).collect())
}

/// Port of `createApplication`: `fields.company` and `fields.role` are overwritten by the required
/// arguments, the status is normalized, and `applied_at` resolves per
/// [`resolve_applied_at_for_create`].
pub async fn create(
    conn: &mut SqliteConnection,
    company: &str,
    role: &str,
    mut fields: ApplicationFields,
) -> Result<Application> {
    if let Some(Some(notes)) = &fields.notes {
        check_notes_length(notes)?;
    }
    let status = fields.status.take().unwrap_or_else(|| "inbound".to_owned());
    let applied_at = resolve_applied_at_for_create(&status, fields.applied_at.take().flatten());
    fields.status = Some(normalize_status(&status));
    fields.applied_at = Some(applied_at);
    fields.company = Some(company.to_owned());
    fields.role = Some(role.to_owned());
    let has_canonical_url = matches!(fields.canonical_job_url, Some(Some(_)));

    let now = Timestamp::now();
    let mut qb = QueryBuilder::new("INSERT INTO applications (created_at, updated_at");
    fields.push_columns(&mut qb);
    qb.push(") VALUES (")
        .push_bind(now.clone())
        .push(", ")
        .push_bind(now);
    fields.push_values(&mut qb);
    qb.push(") RETURNING *");
    match qb
        .build_query_as()
        .fetch_one(conn)
        .await
        .map_err(Error::from)
    {
        Ok(row) => Ok(normalized(row)),
        Err(e) if has_canonical_url && e.is_unique_violation() => {
            Err(Error::CanonicalJobUrlConflict)
        }
        Err(e) => Err(e),
    }
}

/// Port of `updateApplication`: every update bumps `event_version` and `updated_at`, and a `notes`
/// value equal to the stored one is ignored rather than validated. Lifecycle fields are the
/// caller's to reject. Run inside `Db::begin_write` when `notes` is set.
pub async fn update(
    conn: &mut SqliteConnection,
    id: i64,
    mut fields: ApplicationFields,
    guard: Guard,
) -> Result<Application> {
    if let Some(new_notes) = &fields.notes {
        let current: Option<Option<String>> =
            sqlx::query_scalar("SELECT notes FROM applications WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        let current = current.ok_or(Error::NotFound)?;
        if current == *new_notes {
            fields.notes = None;
        } else if let Some(notes) = new_notes {
            check_notes_length(notes)?;
        }
    }
    if let Some(status) = &fields.status {
        fields.status = Some(normalize_status(status));
    }

    let mut qb = QueryBuilder::new("UPDATE applications SET event_version = event_version + 1");
    qb.push(", updated_at = ").push_bind(Timestamp::now());
    fields.push_assignments(&mut qb);
    qb.push(" WHERE id = ").push_bind(id);
    match &guard {
        Guard::None => {}
        Guard::UpdatedAt(expected) => {
            qb.push(" AND updated_at = ").push_bind(expected.clone());
        }
        Guard::EventVersion(expected) => {
            qb.push(" AND event_version = ").push_bind(*expected);
        }
    }
    qb.push(" RETURNING *");
    let row: Option<Application> = match qb.build_query_as().fetch_optional(&mut *conn).await {
        Ok(row) => row,
        Err(e) => {
            let e = Error::from(e);
            return Err(if e.is_unique_violation() {
                Error::CanonicalJobUrlConflict
            } else {
                e
            });
        }
    };
    match row {
        Some(row) => Ok(normalized(row)),
        None if matches!(guard, Guard::None) || !exists(conn, id).await? => Err(Error::NotFound),
        None => Err(Error::Conflict),
    }
}

/// Port of the update half of `appendApplicationNote`: appends with a blank line, keeping the total
/// within the limit in code points. Inserting the event is the caller's.
pub async fn append_note(
    conn: &mut SqliteConnection,
    id: i64,
    note: &str,
    expected_updated_at: Option<&Timestamp>,
) -> Result<Application> {
    let row: Option<Application> = sqlx::query_as(
        "UPDATE applications
         SET notes = CASE WHEN notes IS NULL OR notes = '' THEN ?1
                          ELSE notes || char(10, 10) || ?1 END,
             updated_at = ?2,
             event_version = event_version + 1
         WHERE id = ?3
           AND (?4 IS NULL OR updated_at = ?4)
           AND length(CASE WHEN notes IS NULL OR notes = '' THEN ?1
                           ELSE notes || char(10, 10) || ?1 END) <= ?5
         RETURNING *",
    )
    .bind(note)
    .bind(Timestamp::now())
    .bind(id)
    .bind(expected_updated_at.map(Timestamp::as_str))
    .bind(NOTES_MAX_LEN)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = row {
        return Ok(normalized(row));
    }
    let current: Option<Timestamp> =
        sqlx::query_scalar("SELECT updated_at FROM applications WHERE id = ?")
            .bind(id)
            .fetch_optional(conn)
            .await?;
    match (current, expected_updated_at) {
        (None, _) => Err(Error::NotFound),
        (Some(current), Some(expected)) if current != *expected => Err(Error::Conflict),
        _ => Err(Error::NotesTooLong),
    }
}

/// Bumps `event_version` and `updated_at` with no other change, as `createApplicationEvent` does.
pub async fn bump_event_version(conn: &mut SqliteConnection, id: i64) -> Result<()> {
    let done = sqlx::query(
        "UPDATE applications SET event_version = event_version + 1, updated_at = ? WHERE id = ?",
    )
    .bind(Timestamp::now())
    .bind(id)
    .execute(conn)
    .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}

/// Port of `deleteApplication`: documents claimed by the application's submissions become
/// historical and unlinked, then the row goes and its children cascade. Documents merely linked to
/// the application survive, unlinked. Run inside `Db::begin_write`.
pub async fn delete(conn: &mut SqliteConnection, id: i64) -> Result<()> {
    if !exists(&mut *conn, id).await? {
        return Err(Error::NotFound);
    }
    sqlx::query(
        "UPDATE documents SET state = 'historical', submission_id = NULL
         WHERE submission_id IN (SELECT id FROM application_submissions WHERE application_id = ?)",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM applications WHERE id = ?")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Summaries of the given applications, for embedding in event records.
pub async fn refs(conn: &mut SqliteConnection, ids: &[i64]) -> Result<Vec<ApplicationRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb = QueryBuilder::new("SELECT id, company, role FROM applications WHERE id IN (");
    let mut list = qb.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    qb.push(")");
    Ok(qb.build_query_as().fetch_all(conn).await?)
}

/// Rows the submission conflict check reads: every application's identifying fields.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ApplicationIdentity {
    pub id: i64,
    pub company: String,
    pub status: String,
    pub requisition_id: Option<String>,
    pub ats_name: Option<String>,
}

pub async fn list_identities(conn: &mut SqliteConnection) -> Result<Vec<ApplicationIdentity>> {
    let rows: Vec<ApplicationIdentity> = sqlx::query_as(
        "SELECT id, company, status, requisition_id, ats_name FROM applications ORDER BY id",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ApplicationIdentity {
            status: normalize_status(&row.status),
            ..row
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_normalization_matches_old_nexus() {
        for (input, expected) in [
            ("inbound", "inbound"),
            ("  Interview ", "interview"),
            ("waiting", "applied"),
            ("draft", "applied"),
            ("ghost", "rejected"),
            ("anything else", "applied"),
            ("", "applied"),
        ] {
            assert_eq!(normalize_status(input), expected, "{input}");
        }
    }

    #[test]
    fn applied_at_defaults_only_for_sent_statuses() {
        assert!(resolve_applied_at_for_create("interview", None).is_some());
        assert!(resolve_applied_at_for_create("inbound", None).is_none());
        let date = Timestamp::parse("2026-01-02T03:04:05Z").unwrap();
        assert_eq!(
            resolve_applied_at_for_create("inbound", Some(date.clone())),
            Some(date)
        );
    }

    #[test]
    fn notes_limit_counts_utf16_code_units() {
        assert!(check_notes_length(&"x".repeat(10_000)).is_ok());
        assert!(check_notes_length(&"x".repeat(10_001)).is_err());
        assert!(check_notes_length(&"\u{1F600}".repeat(5_001)).is_err());
    }

    #[test]
    fn json_list_reads_non_arrays_as_empty() {
        assert_eq!(JsonList::from(None), JsonList(vec![]));
        assert_eq!(JsonList::from(Some("{}".to_owned())), JsonList(vec![]));
        assert_eq!(
            JsonList::from(Some("[\"DE\"]".to_owned())),
            JsonList(vec!["DE".into()])
        );
    }
}
