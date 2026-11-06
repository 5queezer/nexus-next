use std::collections::HashMap;

use nexus_core::Timestamp;
use nexus_core::repo::applications::{
    self as repo, Application, ApplicationFields, ApplicationFilter, Guard, SearchField,
    normalize_status,
};
use nexus_core::repo::contacts as contacts_repo;
use serde_json::{Map, Value, json};
use sqlx::types::Json;

use super::{
    Args, Ctx, Thrown, ToolResult, bool_arg, code_error, find_application, json_result, req_str,
    str_arg, text_error, text_result,
};
use crate::js::{canonicalize_job_url, ilike_contains, nid, parse_date, slice16, trim, utf16_len};

const UPDATE_ERROR_CODES: [&str; 4] = [
    "not_found",
    "conflict",
    "canonical_job_url_conflict",
    "application_deleting",
];

fn update_error(message: &str) -> rmcp::model::CallToolResult {
    if UPDATE_ERROR_CODES.contains(&message) {
        code_error(message)
    } else {
        code_error("application_update_failed")
    }
}

/// `optionalString`: `undefined` stays absent, and empty after trimming is null.
fn optional_string(input: &Args, key: &str, max: usize, out: &mut Args) {
    let Some(value) = input.get(key) else { return };
    let text = value
        .as_str()
        .map(|text| slice16(trim(text), max))
        .filter(|text| !text.is_empty());
    out.insert(key.to_owned(), text.map_or(Value::Null, Value::String));
}

fn string_array(input: &Args, key: &str, max: usize) -> Option<Value> {
    let mut seen = Vec::new();
    for item in input.get(key)?.as_array()? {
        let text = slice16(trim(item.as_str().unwrap_or_default()), max);
        if !text.is_empty() && !seen.contains(&text) {
            seen.push(text);
        }
    }
    Some(seen.into())
}

/// Port of `parseStructuredApplicationMetadata`, for values that passed tool validation. The keys
/// it returns are the fields to write.
fn parse_metadata(input: &Args) -> Result<Args, String> {
    let mut out = Map::new();
    if let Some(items) = input.get("eligibleCountries").and_then(Value::as_array) {
        let valid = items.iter().all(|item| {
            let text = trim(item.as_str().unwrap_or_default());
            text.len() == 2 && text.bytes().all(|b| b.is_ascii_alphabetic())
        });
        if !valid {
            return Err("eligibleCountries_invalid".to_owned());
        }
        let countries = string_array(input, "eligibleCountries", 2).unwrap_or_default();
        let upper = countries
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|country| Value::from(country.to_uppercase()))
            .collect::<Vec<_>>();
        out.insert("eligibleCountries".to_owned(), upper.into());
    }
    if let Some(locations) = string_array(input, "primaryLocations", 200) {
        out.insert("primaryLocations".to_owned(), locations);
    }
    optional_string(input, "workMode", 100, &mut out);
    for key in [
        "officeDaysMin",
        "travelPercent",
        "visaSponsorship",
        "rightToWorkRequired",
    ] {
        if let Some(value) = input.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    optional_string(input, "timezoneOverlap", 255, &mut out);
    optional_string(input, "salaryCurrency", 3, &mut out);
    if let Some(currency) = out.get("salaryCurrency").and_then(Value::as_str) {
        let upper = currency.to_uppercase();
        if !(upper.len() == 3 && upper.bytes().all(|b| b.is_ascii_uppercase())) {
            return Err("salaryCurrency_invalid".to_owned());
        }
        out.insert("salaryCurrency".to_owned(), upper.into());
    }
    optional_string(input, "salaryPeriod", 100, &mut out);
    optional_string(input, "salaryType", 100, &mut out);
    optional_string(input, "atsName", 100, &mut out);
    optional_string(input, "requisitionId", 255, &mut out);
    for key in [
        "jobCapturedAt",
        "jobVerifiedAt",
        "jobPostedAt",
        "jobClosedAt",
    ] {
        let Some(value) = input.get(key) else {
            continue;
        };
        let date = match value.as_str().filter(|text| !text.is_empty()) {
            None => Value::Null,
            Some(text) => parse_date(text)
                .ok_or_else(|| format!("{key}_invalid"))?
                .as_str()
                .into(),
        };
        out.insert(key.to_owned(), date);
    }
    optional_string(input, "jobContentHash", 64, &mut out);
    if let Some(hash) = out.get("jobContentHash").and_then(Value::as_str) {
        let lower = hash.to_lowercase();
        let valid = lower.len() == 64
            && lower
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err("jobContentHash_invalid".to_owned());
        }
        out.insert("jobContentHash".to_owned(), lower.into());
    }
    optional_string(input, "jobLiveness", 100, &mut out);
    optional_string(input, "jobSummary", 10_000, &mut out);
    optional_string(input, "currentStage", 255, &mut out);
    if let Some(value) = input.get("nextAction") {
        let text = value.as_str().map(trim).filter(|text| !text.is_empty());
        if text.is_some_and(|text| utf16_len(text) > 2_000) {
            return Err("nextAction_too_long".to_owned());
        }
        out.insert(
            "nextAction".to_owned(),
            text.map_or(Value::Null, Value::from),
        );
    }
    if let Some(job_url) = input.get("jobUrl") {
        let canonical = match job_url.as_str().filter(|text| !text.is_empty()) {
            None => Value::Null,
            Some(text) => {
                let canonical = canonicalize_job_url(text)?.ok_or("jobUrl_invalid")?;
                canonical.into()
            }
        };
        out.insert("canonicalJobUrl".to_owned(), canonical);
    }
    Ok(out)
}

const TRIAGE_FIELDS: [&str; 5] = [
    "companySize",
    "salaryBandMentioned",
    "triageQuality",
    "triageReason",
    "incomingSource",
];

fn add_triage(input: &Args, out: &mut Args) {
    for key in TRIAGE_FIELDS {
        if let Some(value) = input.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
}

/// A number bound to an integer column as Prisma treats it: fractions truncate.
fn truncated(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|n| n.trunc() as i64))
}

/// An integer column: out of 32-bit range, the write fails.
fn int_column(value: &Value, key: &str) -> Result<Option<i64>, String> {
    if value.is_null() {
        return Ok(None);
    }
    truncated(value)
        .filter(|n| i32::try_from(*n).is_ok())
        .map(Some)
        .ok_or_else(|| format!("Invalid value for {key}: expected a 32-bit integer"))
}

/// Converts a camelCase update, whose preview `dryRun` shows, into columns to write.
fn to_fields(map: &Args) -> Result<ApplicationFields, String> {
    let text = |key| map.get(key).map(|v: &Value| v.as_str().map(str::to_owned));
    let required = |key| map.get(key).and_then(Value::as_str).map(str::to_owned);
    let date = |key| -> Result<Option<Option<Timestamp>>, String> {
        map.get(key)
            .map(|value: &Value| match value.as_str() {
                None => Ok(None),
                Some(text) => parse_date(text)
                    .map(Some)
                    .ok_or_else(|| format!("Invalid value for {key}")),
            })
            .transpose()
    };
    let int = |key| map.get(key).map(|value| int_column(value, key)).transpose();
    let list = |key| {
        map.get(key).map(|value: &Value| {
            Json(
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
        })
    };
    let flag = |key| map.get(key).and_then(Value::as_bool);
    let nullable_flag = |key| map.get(key).map(|v: &Value| v.as_bool());
    Ok(ApplicationFields {
        company: required("company"),
        role: required("role"),
        status: required("status"),
        applied_at: date("appliedAt")?,
        last_contact: date("lastContact")?,
        notes: text("notes"),
        follow_up_at: date("followUpAt")?,
        job_description: text("jobDescription"),
        source: text("source"),
        remote: flag("remote"),
        salary_min: int("salaryMin")?,
        salary_max: int("salaryMax")?,
        rating: int("rating")?,
        job_url: text("jobUrl"),
        canonical_job_url: text("canonicalJobUrl"),
        resume_id: text("resumeId"),
        company_size: text("companySize"),
        salary_band_mentioned: flag("salaryBandMentioned"),
        triage_quality: int("triageQuality")?,
        triage_reason: text("triageReason"),
        incoming_source: text("incomingSource"),
        auto_rejected: flag("autoRejected"),
        auto_reject_reason: text("autoRejectReason"),
        archived_at: date("archivedAt")?,
        work_mode: text("workMode"),
        eligible_countries: list("eligibleCountries"),
        primary_locations: list("primaryLocations"),
        office_days_min: int("officeDaysMin")?,
        travel_percent: int("travelPercent")?,
        visa_sponsorship: nullable_flag("visaSponsorship"),
        right_to_work_required: nullable_flag("rightToWorkRequired"),
        timezone_overlap: text("timezoneOverlap"),
        salary_currency: text("salaryCurrency"),
        salary_period: text("salaryPeriod"),
        salary_type: text("salaryType"),
        ats_name: text("atsName"),
        requisition_id: text("requisitionId"),
        job_captured_at: date("jobCapturedAt")?,
        job_verified_at: date("jobVerifiedAt")?,
        job_posted_at: date("jobPostedAt")?,
        job_closed_at: date("jobClosedAt")?,
        job_content_hash: text("jobContentHash"),
        job_liveness: text("jobLiveness"),
        job_summary: text("jobSummary"),
        current_stage: text("currentStage"),
        next_action: text("nextAction"),
    })
}

fn sliced(args: &Args, key: &str, max: usize) -> Option<Value> {
    args.get(key).map(|value| match value.as_str() {
        Some(text) => slice16(text, max).into(),
        None => Value::Null,
    })
}

fn insert_some(map: &mut Args, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

const SLICED_FIELDS: [(&str, usize); 3] = [
    ("jobDescription", 50_000),
    ("source", 100),
    ("jobUrl", 2_000),
];

/// Port of `resolveAppliedAtForCreate` for a string or absent value.
fn applied_at_for_create(status: &str, value: Option<&str>) -> Result<Value, String> {
    match value.filter(|text| !text.is_empty()) {
        Some(text) => {
            let parsed = parse_date(text).ok_or("appliedAt_invalid")?;
            Ok(parsed.as_str().into())
        }
        None if matches!(status, "applied" | "interview" | "offer" | "rejected") => {
            Ok(Timestamp::now().as_str().into())
        }
        None => Ok(Value::Null),
    }
}

async fn insert_application(ctx: &Ctx, map: &Args) -> Result<Application, Thrown> {
    let company = map
        .get("company")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let role = map.get("role").and_then(Value::as_str).unwrap_or_default();
    let fields = to_fields(map)?;
    let mut conn = ctx.state.db.conn().await?;
    Ok(repo::create(&mut conn, company, role, fields).await?)
}

pub async fn create(ctx: &Ctx, args: &Args) -> ToolResult {
    let metadata = parse_metadata(args)?;
    let status = str_arg(args, "status").unwrap_or("inbound");
    let mut map = Map::new();
    map.insert(
        "company".to_owned(),
        slice16(req_str(args, "company"), 255).into(),
    );
    map.insert(
        "role".to_owned(),
        slice16(req_str(args, "role"), 255).into(),
    );
    map.insert("status".to_owned(), normalize_status(status).into());
    map.insert(
        "appliedAt".to_owned(),
        applied_at_for_create(status, str_arg(args, "appliedAt"))?,
    );
    for key in ["lastContact", "followUpAt"] {
        let date = str_arg(args, key).and_then(parse_date);
        map.insert(
            key.to_owned(),
            date.map_or(Value::Null, |d| d.as_str().into()),
        );
    }
    insert_some(&mut map, "notes", args.get("notes").cloned());
    for (key, max) in SLICED_FIELDS {
        insert_some(&mut map, key, sliced(args, key, max));
    }
    map.insert(
        "remote".to_owned(),
        bool_arg(args, "remote").unwrap_or(false).into(),
    );
    for key in ["salaryMin", "salaryMax", "rating", "resumeId"] {
        insert_some(&mut map, key, args.get(key).cloned());
    }
    map.extend(metadata);
    add_triage(args, &mut map);
    let application = insert_application(ctx, &map).await?;
    Ok(json_result(ctx.app_json(&application, Vec::new()), false))
}

pub async fn get(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    Ok(
        match find_application(&mut conn, req_str(args, "id")).await? {
            Some(application) => {
                json_result(ctx.app_with_contacts(&mut conn, &application).await?, false)
            }
            None => text_error("Application not found"),
        },
    )
}

async fn with_requested_contacts(
    ctx: &Ctx,
    conn: &mut sqlx::SqliteConnection,
    applications: &[Application],
    include_contacts: bool,
) -> Result<Vec<Value>, Thrown> {
    let mut contacts: HashMap<i64, Vec<Value>> = HashMap::new();
    if include_contacts {
        let ids: Vec<i64> = applications
            .iter()
            .map(|application| application.id)
            .collect();
        for contact in contacts_repo::list_for_applications(conn, &ids).await? {
            contacts
                .entry(contact.application_id)
                .or_default()
                .push(serde_json::to_value(&contact).expect("contact serializes"));
        }
    }
    Ok(applications
        .iter()
        .map(|application| {
            ctx.app_json(
                application,
                contacts.remove(&application.id).unwrap_or_default(),
            )
        })
        .collect())
}

/// `listApplicationsFiltered`, with search done here because SQLite can't reproduce `ILIKE`. The
/// row count is small enough to filter in memory.
async fn list_rows(
    conn: &mut sqlx::SqliteConnection,
    mut filter: ApplicationFilter,
) -> nexus_core::Result<Vec<Application>> {
    let Some(term) = filter.search.take() else {
        return repo::list_filtered(conn, &filter).await;
    };
    let (limit, cursor) = (filter.limit.take(), filter.cursor.take());
    let fields = filter.search_fields.take().unwrap_or_else(|| {
        vec![
            SearchField::Company,
            SearchField::Role,
            SearchField::Notes,
            SearchField::JobDescription,
        ]
    });
    let mut rows = repo::list_filtered(conn, &filter).await?;
    rows.retain(|row| {
        fields.iter().any(|field| {
            let text = match field {
                SearchField::Company => Some(row.company.as_str()),
                SearchField::Role => Some(row.role.as_str()),
                SearchField::Notes => row.notes.as_deref(),
                SearchField::JobDescription => row.job_description.as_deref(),
            };
            text.is_some_and(|text| ilike_contains(text, &term))
        })
    });
    if let Some(cursor) = cursor {
        let at = rows
            .iter()
            .position(|row| row.id == cursor)
            .ok_or(nexus_core::Error::ApplicationCursorInvalid)?;
        rows.drain(..=at);
    }
    if let Some(limit) = limit {
        rows.truncate(limit.max(0) as usize);
    }
    Ok(rows)
}

/// Port of `pickFields`: `id` always, and only the fields the record has.
fn pick_fields(records: Vec<Value>, fields: &[&str]) -> Vec<Value> {
    if fields.is_empty() {
        return records;
    }
    records
        .into_iter()
        .map(|record| {
            let mut picked = Map::new();
            for field in fields {
                if let Some(value) = record.get(*field) {
                    picked.insert((*field).to_owned(), value.clone());
                }
            }
            picked.insert("id".to_owned(), record["id"].clone());
            Value::Object(picked)
        })
        .collect()
}

fn string_list<'a>(args: &'a Args, key: &str) -> Vec<&'a str> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

pub async fn list(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut fields = string_list(args, "fields");
    if fields.is_empty() {
        fields = vec![
            "id",
            "company",
            "role",
            "status",
            "currentStage",
            "nextAction",
            "rating",
            "updatedAt",
        ];
    }
    let cursor = match str_arg(args, "cursor").filter(|cursor| !cursor.is_empty()) {
        None => None,
        Some(cursor) => match nid(cursor) {
            Ok(cursor) => Some(cursor),
            Err(_) => return Ok(code_error("application_cursor_invalid")),
        },
    };
    let filter = ApplicationFilter {
        search: str_arg(args, "q")
            .filter(|q| !q.is_empty())
            .map(str::to_owned),
        search_fields: Some(vec![SearchField::Company, SearchField::Role]),
        limit: args.get("limit").and_then(Value::as_i64),
        cursor,
        ..Default::default()
    };
    let mut conn = ctx.state.db.conn().await?;
    let applications = match list_rows(&mut conn, filter).await {
        Ok(applications) => applications,
        Err(nexus_core::Error::ApplicationCursorInvalid) => {
            return Ok(code_error("application_cursor_invalid"));
        }
        Err(error) => return Err(error.into()),
    };
    let records = with_requested_contacts(ctx, &mut conn, &applications, false).await?;
    Ok(json_result(
        Value::Array(pick_fields(records, &fields)),
        false,
    ))
}

pub async fn list_filtered(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let filter = ApplicationFilter {
            status: string_list(args, "status")
                .into_iter()
                .map(str::to_owned)
                .collect(),
            rating_gte: args.get("rating_gte").and_then(truncated),
            search: str_arg(args, "search")
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            remote: bool_arg(args, "remote"),
            sort: str_arg(args, "sort")
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            limit: args.get("limit").and_then(truncated),
            ..Default::default()
        };
        let mut conn = ctx.state.db.conn().await.ok()?;
        let applications = list_rows(&mut conn, filter).await.ok()?;
        let include_contacts = bool_arg(args, "include_contacts").unwrap_or(false);
        let records = with_requested_contacts(ctx, &mut conn, &applications, include_contacts)
            .await
            .ok()?;
        Some(pick_fields(records, &string_list(args, "fields")))
    };
    Ok(match run.await {
        Some(records) => json_result(Value::Array(records), false),
        None => text_error("Failed to list applications"),
    })
}

fn expected_guard(expected: Option<&Timestamp>) -> Guard {
    expected.map_or(Guard::None, |expected| Guard::UpdatedAt(expected.clone()))
}

pub async fn update(ctx: &Ctx, args: &Args) -> ToolResult {
    let lifecycle: Vec<&str> = [
        "status",
        "appliedAt",
        "lastContact",
        "followUpAt",
        "currentStage",
    ]
    .into_iter()
    .filter(|field| args.contains_key(*field))
    .collect();
    if !lifecycle.is_empty() {
        return Ok(json_result(
            json!({"error": {"code": "lifecycle_event_required", "fields": lifecycle}}),
            true,
        ));
    }
    let run = async {
        let mut conn = ctx.state.db.conn().await?;
        let current = find_application(&mut conn, req_str(args, "id"))
            .await?
            .ok_or_else(|| Thrown("not_found".to_owned()))?;
        let mut map = Map::new();
        insert_some(&mut map, "company", sliced(args, "company", 255));
        insert_some(&mut map, "role", sliced(args, "role", 255));
        insert_some(&mut map, "notes", args.get("notes").cloned());
        for (key, max) in SLICED_FIELDS {
            insert_some(&mut map, key, sliced(args, key, max));
        }
        for key in ["remote", "salaryMin", "salaryMax", "rating", "resumeId"] {
            insert_some(&mut map, key, args.get(key).cloned());
        }
        map.extend(parse_metadata(args)?);
        add_triage(args, &mut map);
        let expected = str_arg(args, "expectedUpdatedAt").and_then(parse_date);
        if bool_arg(args, "dryRun") == Some(true) {
            if expected
                .as_ref()
                .is_some_and(|expected| *expected != current.updated_at)
            {
                return Err(Thrown("conflict".to_owned()));
            }
            let mut preview = ctx.app_with_contacts(&mut conn, &current).await?;
            for (key, value) in map {
                preview[key] = value;
            }
            return Ok(json_result(
                json!({"dryRun": true, "application": preview}),
                false,
            ));
        }
        drop(conn);
        let fields = to_fields(&map)?;
        let mut tx = ctx.state.db.begin_write().await?;
        let updated = repo::update(
            &mut tx,
            current.id,
            fields,
            expected_guard(expected.as_ref()),
        )
        .await?;
        let value = ctx.app_with_contacts(&mut tx, &updated).await?;
        tx.commit().await.map_err(nexus_core::Error::from)?;
        Ok(json_result(value, false))
    };
    Ok(run
        .await
        .unwrap_or_else(|Thrown(message)| update_error(&message)))
}

pub async fn set_archived(ctx: &Ctx, args: &Args, archive: bool) -> ToolResult {
    let dry_run = bool_arg(args, "dryRun") == Some(true);
    let run = async {
        let mut conn = ctx.state.db.conn().await?;
        let current = find_application(&mut conn, req_str(args, "id"))
            .await?
            .ok_or_else(|| Thrown("not_found".to_owned()))?;
        let requested = if archive {
            str_arg(args, "archivedAt").and_then(parse_date)
        } else {
            None
        };
        let expected = str_arg(args, "expectedUpdatedAt").and_then(parse_date);
        let in_state = |application: &Application| application.archived_at.is_some() == archive;
        let exact_retry = |application: &Application| {
            archive && requested.is_some() && application.archived_at == requested
        };
        let expected_matches = expected
            .as_ref()
            .is_none_or(|expected| *expected == current.updated_at);

        // Repeated calls are successful no-ops. A stale token still conflicts, unless the caller
        // supplied the exact timestamp its earlier archive request stored.
        if in_state(&current) {
            if !expected_matches && !exact_retry(&current) {
                return Err(Thrown("conflict".to_owned()));
            }
            let application = ctx.app_with_contacts(&mut conn, &current).await?;
            let mut out = json!({"changed": false, "application": application});
            if dry_run {
                out["dryRun"] = true.into();
            }
            return Ok(json_result(out, false));
        }
        if !expected_matches {
            return Err(Thrown("conflict".to_owned()));
        }
        let next = archive.then(|| requested.clone().unwrap_or_else(Timestamp::now));
        if dry_run {
            let mut application = ctx.app_with_contacts(&mut conn, &current).await?;
            application["archivedAt"] = next.as_ref().map_or(Value::Null, |d| d.as_str().into());
            return Ok(json_result(
                json!({"dryRun": true, "changed": true, "application": application}),
                false,
            ));
        }
        drop(conn);
        let fields = ApplicationFields {
            archived_at: Some(next),
            ..Default::default()
        };
        let guard = Guard::UpdatedAt(
            expected
                .clone()
                .unwrap_or_else(|| current.updated_at.clone()),
        );
        let mut tx = ctx.state.db.begin_write().await?;
        match repo::update(&mut tx, current.id, fields, guard).await {
            Ok(updated) => {
                let application = ctx.app_with_contacts(&mut tx, &updated).await?;
                tx.commit().await.map_err(nexus_core::Error::from)?;
                Ok(json_result(
                    json!({"changed": true, "application": application}),
                    false,
                ))
            }
            Err(nexus_core::Error::Conflict) => {
                let latest = repo::get(&mut tx, current.id)
                    .await?
                    .ok_or_else(|| Thrown("not_found".to_owned()))?;
                if in_state(&latest) && (!archive || requested.is_none() || exact_retry(&latest)) {
                    let application = ctx.app_with_contacts(&mut tx, &latest).await?;
                    return Ok(json_result(
                        json!({"changed": false, "application": application}),
                        false,
                    ));
                }
                Err(Thrown("conflict".to_owned()))
            }
            Err(error) => Err(error.into()),
        }
    };
    Ok(run
        .await
        .unwrap_or_else(|Thrown(message)| update_error(&message)))
}

pub async fn delete(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let run = async {
        let id = nid(req_str(args, "id")).ok()?;
        let mut tx = ctx.state.db.begin_write().await.ok()?;
        repo::delete(&mut tx, id).await.ok()?;
        tx.commit().await.ok()
    };
    Ok(match run.await {
        Some(()) => text_result("Application deleted"),
        None => text_error("Application not found or access denied"),
    })
}

pub async fn find_by_job_url(ctx: &Ctx, args: &Args) -> ToolResult {
    let Some(canonical) = canonicalize_job_url(req_str(args, "jobUrl"))? else {
        return Ok(text_error("Invalid URL"));
    };
    let mut conn = ctx.state.db.conn().await?;
    let application = match repo::find_by_canonical_job_url(&mut conn, &canonical).await? {
        Some(application) => ctx.app_with_contacts(&mut conn, &application).await?,
        None => Value::Null,
    };
    Ok(json_result(
        json!({"canonicalJobUrl": canonical, "application": application}),
        false,
    ))
}

pub async fn upsert_by_job_url(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let job_url = req_str(args, "jobUrl");
        let canonical = canonicalize_job_url(job_url)?.ok_or("job_url_invalid")?;
        let salary = |key| args.get(key).and_then(Value::as_f64);
        if let (Some(min), Some(max)) = (salary("salaryMin"), salary("salaryMax"))
            && min > max
        {
            return Err(Thrown("salary_range_invalid".to_owned()));
        }
        let mut tx = ctx.state.db.begin_write().await?;
        let existing = repo::find_by_canonical_job_url(&mut tx, &canonical).await?;
        let metadata = parse_metadata(args)?;
        if let Some(existing) = &existing {
            let status_changes =
                str_arg(args, "status").is_some_and(|status| status != existing.status);
            let stage_changes = metadata
                .get("currentStage")
                .is_some_and(|stage| stage.as_str() != existing.current_stage.as_deref());
            if status_changes || stage_changes {
                return Err(Thrown("lifecycle_event_required".to_owned()));
            }
        }
        if bool_arg(args, "dryRun") == Some(true) {
            return Ok(json_result(
                json!({
                    "dryRun": true,
                    "operation": if existing.is_some() { "update" } else { "create" },
                    "canonicalJobUrl": canonical,
                    "existingId": existing.map(|existing| existing.id.to_string()),
                }),
                false,
            ));
        }
        let mut map = Map::new();
        for key in ["company", "role", "jobUrl"] {
            insert_some(&mut map, key, args.get(key).cloned());
        }
        for key in [
            "notes",
            "jobDescription",
            "source",
            "remote",
            "salaryMin",
            "salaryMax",
            "rating",
            "resumeId",
        ] {
            insert_some(&mut map, key, args.get(key).cloned());
        }
        map.extend(metadata);
        add_triage(args, &mut map);
        map.insert("canonicalJobUrl".to_owned(), canonical.into());
        let (operation, application) = if let Some(existing) = existing {
            map.remove("currentStage");
            let fields = to_fields(&map)?;
            (
                "updated",
                repo::update(&mut tx, existing.id, fields, Guard::None).await?,
            )
        } else {
            let status = str_arg(args, "status").unwrap_or("inbound");
            map.insert("status".to_owned(), status.into());
            map.insert("appliedAt".to_owned(), applied_at_for_create(status, None)?);
            let fields = to_fields(&map)?;
            let created = repo::create(
                &mut tx,
                req_str(args, "company"),
                req_str(args, "role"),
                fields,
            )
            .await?;
            ("created", created)
        };
        let application = ctx.app_with_contacts(&mut tx, &application).await?;
        tx.commit().await.map_err(nexus_core::Error::from)?;
        Ok(json_result(
            json!({"operation": operation, "application": application}),
            false,
        ))
    };
    Ok(run
        .await
        .unwrap_or_else(|Thrown(message)| code_error(&message)))
}

impl From<&str> for Thrown {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

/// `validateApplicationSummary`, which reads an empty string as null.
fn summary(value: &Value) -> Value {
    match value.as_str() {
        Some(text) if !text.is_empty() => text.into(),
        _ => Value::Null,
    }
}

/// `sanitizeTriageFields` for the validated values a batch item can carry. Empty reasons are null.
fn sanitized_triage(item: &Args, out: &mut Args) {
    add_triage(item, out);
    if let Some(reason) = out.get("triageReason") {
        out.insert("triageReason".to_owned(), summary(reason));
    }
}

/// A date column in a batch item, where a value `new Date` can't read fails the item's write.
fn batch_date(item: &Args, key: &str) -> Result<Option<Value>, String> {
    let Some(value) = item.get(key) else {
        return Ok(None);
    };
    match value.as_str().filter(|text| !text.is_empty()) {
        None => Ok(Some(Value::Null)),
        Some(text) => parse_date(text)
            .map(|date| Some(date.as_str().into()))
            .ok_or_else(|| format!("Invalid value for {key}: Invalid Date")),
    }
}

fn batch_common(item: &Args, map: &mut Args, metadata: &Args) {
    insert_some(map, "company", sliced(item, "company", 255));
    insert_some(map, "role", sliced(item, "role", 255));
    insert_some(map, "notes", item.get("notes").map(summary));
    for (key, max) in SLICED_FIELDS {
        insert_some(map, key, sliced(item, key, max));
    }
    for key in ["remote", "salaryMin", "salaryMax", "rating", "resumeId"] {
        insert_some(map, key, item.get(key).cloned());
    }
    map.extend(metadata.clone());
    sanitized_triage(item, map);
}

async fn batch_update_item(ctx: &Ctx, id: i64, item: &Args, metadata: &Args) -> Result<(), Thrown> {
    let mut map = Map::new();
    batch_common(item, &mut map, metadata);
    let fields = to_fields(&map)?;
    let mut tx = ctx.state.db.begin_write().await?;
    repo::update(&mut tx, id, fields, Guard::None).await?;
    tx.commit().await.map_err(nexus_core::Error::from)?;
    Ok(())
}

async fn batch_create_item(ctx: &Ctx, item: &Args, metadata: &Args) -> Result<Application, Thrown> {
    let status = str_arg(item, "status")
        .filter(|s| !s.is_empty())
        .unwrap_or("inbound");
    let mut map = Map::new();
    batch_common(item, &mut map, metadata);
    map.insert("status".to_owned(), normalize_status(status).into());
    let applied_at = match item.get("appliedAt").and_then(Value::as_str) {
        Some(text) if !text.is_empty() && parse_date(text).is_none() => {
            return Err(Thrown("appliedAt_invalid".to_owned()));
        }
        value => applied_at_for_create(status, value)?,
    };
    map.insert("appliedAt".to_owned(), applied_at);
    for key in ["lastContact", "followUpAt"] {
        insert_some(&mut map, key, batch_date(item, key)?);
    }
    insert_application(ctx, &map).await
}

pub async fn batch_upsert(ctx: &Ctx, args: &Args) -> ToolResult {
    const FAILED: &str = "Batch upsert failed";
    let items = args["items"].as_array().cloned().unwrap_or_default();
    let mut prepared = Vec::with_capacity(items.len());
    {
        let mut conn = ctx.state.db.conn().await?;
        for item in &items {
            let item = item.as_object().expect("validated object");
            if let Some(id) = str_arg(item, "id").filter(|id| !id.is_empty())
                && !matches!(find_application(&mut conn, id).await, Ok(Some(_)))
            {
                return Ok(text_error(FAILED));
            }
            match parse_metadata(item) {
                Ok(metadata) => prepared.push((item, metadata)),
                Err(_) => return Ok(text_error(FAILED)),
            }
        }
    }
    let mut results = Vec::with_capacity(items.len());
    let mut succeeded = 0;
    for (index, (item, metadata)) in prepared.iter().enumerate() {
        let id = str_arg(item, "id").filter(|id| !id.is_empty());
        let outcome = if let Some(id) = id {
            let lifecycle = ["status", "appliedAt", "lastContact", "followUpAt"]
                .iter()
                .any(|field| item.contains_key(*field))
                || metadata.contains_key("currentStage");
            if lifecycle {
                results.push(json!({"index": index, "id": id, "operation": "updated", "error": "lifecycle_event_required"}));
                continue;
            }
            match nid(id) {
                Ok(numeric) => batch_update_item(ctx, numeric, item, metadata)
                    .await
                    .map(|()| id.to_owned()),
                Err(message) => Err(Thrown(message)),
            }
        } else {
            let filled = |key| str_arg(item, key).is_some_and(|text| !text.is_empty());
            if !filled("company") || !filled("role") {
                results.push(json!({
                    "index": index,
                    "id": "",
                    "operation": "created",
                    "error": "company and role are required for new applications",
                }));
                continue;
            }
            batch_create_item(ctx, item, metadata)
                .await
                .map(|created| created.id.to_string())
        };
        let operation = if id.is_some() { "updated" } else { "created" };
        match outcome {
            Ok(id) => {
                succeeded += 1;
                results.push(json!({"index": index, "id": id, "operation": operation}));
            }
            Err(Thrown(message)) => results.push(json!({
                "index": index, "id": id.unwrap_or(""), "operation": operation, "error": message,
            })),
        }
    }
    Ok(json_result(
        json!({"total": items.len(), "succeeded": succeeded, "failed": items.len() - succeeded, "results": results}),
        false,
    ))
}

pub async fn batch_delete(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let ids = string_list(args, "ids");
    {
        let mut conn = ctx.state.db.conn().await?;
        for id in &ids {
            if !matches!(find_application(&mut conn, id).await, Ok(Some(_))) {
                return Ok(text_error("Batch delete failed"));
            }
        }
    }
    let mut results = Vec::with_capacity(ids.len());
    let mut succeeded = 0;
    for id in &ids {
        let deleted = async {
            let mut tx = ctx.state.db.begin_write().await?;
            repo::delete(&mut tx, nid(id)?).await?;
            tx.commit().await.map_err(nexus_core::Error::from)?;
            Ok::<_, Thrown>(())
        };
        match deleted.await {
            Ok(()) => {
                succeeded += 1;
                results.push(json!({"id": id, "deleted": true}));
            }
            Err(Thrown(message)) => {
                results.push(json!({"id": id, "deleted": false, "error": message}))
            }
        }
    }
    Ok(json_result(
        json!({"total": ids.len(), "succeeded": succeeded, "failed": ids.len() - succeeded, "results": results}),
        false,
    ))
}
