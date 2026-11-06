use std::collections::BTreeSet;

use nexus_core::Timestamp;
use nexus_core::repo::applications::{self as apps_repo, ApplicationFields, ApplicationIdentity, Guard};
use nexus_core::repo::documents::{self as documents_repo, Document};
use nexus_core::repo::events::{self as events_repo, NewEvent};
use nexus_core::repo::submissions::{self as repo, NewSubmission};
use serde_json::{Map, Value, json};

use super::{Args, Ctx, Thrown, ToolResult, bool_arg, code_error, find_application, json_result, req_str, str_arg, text_error};
use crate::js::{nid, parse_date, request_hash, trim, utf16_len};

const ERROR_CODES: [&str; 21] = [
    "salary_range_invalid",
    "not_found",
    "conflict",
    "application_deleting",
    "idempotency_conflict",
    "invalid_documents",
    "document_already_submitted",
    "human_review_required",
    "identity_consistency_required",
    "fact_verification_required",
    "profile_consistency_review_required",
    "submission_materials_required",
    "submission_answers_required",
    "submission_answers_conflict",
    "submission_documents_invalid",
    "submission_policy_reason_invalid",
    "submission_policy_reason_too_long",
    "application_already_submitted",
    "duplicate_requisition",
    "same_company_active_application",
    "verification_failed",
];

fn fail<T>(code: &str) -> Result<T, Thrown> {
    Err(Thrown(code.to_owned()))
}

/// Port of `validateSubmissionAnswers`, for values that passed tool validation.
fn validate_answers(answers: &[Value]) -> Result<Vec<Value>, Thrown> {
    let mut validated = Vec::with_capacity(answers.len());
    for (index, raw) in answers.iter().enumerate() {
        let question = trim(raw["question"].as_str().unwrap_or_default());
        if question.is_empty() {
            return Err(Thrown(format!("Answer {} requires a question", index + 1)));
        }
        let key = raw.get("key").and_then(Value::as_str).map(trim).filter(|key| !key.is_empty());
        if key.is_some_and(|key| utf16_len(key) > 100) {
            return fail("Answer keys must be at most 100 characters");
        }
        let mut answer = Map::new();
        if let Some(key) = key {
            answer.insert("key".to_owned(), key.into());
        }
        answer.insert("question".to_owned(), question.into());
        answer.insert("answer".to_owned(), raw["answer"].clone());
        if let Some(kind) = raw.get("kind") {
            answer.insert("kind".to_owned(), kind.clone());
        }
        if let Some(sensitive) = raw.get("sensitive") {
            answer.insert("sensitive".to_owned(), sensitive.clone());
        }
        validated.push(Value::Object(answer));
    }
    if Value::Array(validated.clone()).to_string().len() > 750_000 {
        return fail("Submission answers exceed the 750000-byte storage limit");
    }
    Ok(validated)
}

fn normalize_reason(value: Option<&Value>) -> Result<Option<String>, Thrown> {
    let Some(value) = value else { return Ok(None) };
    let Some(text) = value.as_str() else {
        return fail("submission_policy_reason_invalid");
    };
    let normalized = trim(text);
    if normalized.is_empty() {
        return Ok(None);
    }
    if utf16_len(normalized) > 1000 {
        return fail("submission_policy_reason_too_long");
    }
    Ok(Some(normalized.to_owned()))
}

/// Port of `validateSubmissionPolicy`. A policy that is not an object has none of its fields.
fn validate_policy(policy: Option<&Value>, answers: &[Value], document_ids: &[Value]) -> Result<Map<String, Value>, Thrown> {
    let field = |key: &str| policy.and_then(|policy| policy.as_object()).and_then(|policy| policy.get(key));
    if field("humanReviewed") != Some(&Value::Bool(true)) {
        return fail("human_review_required");
    }
    if field("identityConsistent") != Some(&Value::Bool(true)) {
        return fail("identity_consistency_required");
    }
    if field("factsVerified") != Some(&Value::Bool(true)) {
        return fail("fact_verification_required");
    }
    let status = field("profileConsistencyStatus").and_then(Value::as_str);
    if !matches!(status, Some("verified" | "unavailable_reviewed")) {
        return fail("profile_consistency_review_required");
    }
    if document_ids.is_empty() {
        return fail("submission_materials_required");
    }
    let confirmed_no_answers = field("confirmedNoAnswers") == Some(&Value::Bool(true));
    if confirmed_no_answers && !answers.is_empty() {
        return fail("submission_answers_conflict");
    }
    if answers.is_empty() && !confirmed_no_answers {
        return fail("submission_answers_required");
    }
    let same_company = normalize_reason(field("sameCompanyOverrideReason"))?;
    let resubmission = normalize_reason(field("resubmissionReason"))?;
    let mut validated = Map::new();
    validated.insert("humanReviewed".to_owned(), true.into());
    validated.insert("identityConsistent".to_owned(), true.into());
    validated.insert("factsVerified".to_owned(), true.into());
    validated.insert("profileConsistencyStatus".to_owned(), status.into());
    validated.insert("confirmedNoAnswers".to_owned(), confirmed_no_answers.into());
    if let Some(reason) = same_company {
        validated.insert("sameCompanyOverrideReason".to_owned(), reason.into());
    }
    if let Some(reason) = resubmission {
        validated.insert("resubmissionReason".to_owned(), reason.into());
    }
    Ok(validated)
}

/// Port of `validateSubmissionDocumentIds`.
fn validate_document_ids(ids: &[Value]) -> Result<Vec<String>, Thrown> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.as_str().unwrap_or_default();
        if ids.len() > 20 || trim(id).is_empty() || !seen.insert(id) {
            return fail("submission_documents_invalid");
        }
        out.push(id.to_owned());
    }
    Ok(out)
}

fn normalize_company(value: &str) -> String {
    trim(value).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn normalize_identifier(value: Option<&str>) -> Option<String> {
    value.map(|value| trim(value).to_lowercase()).filter(|value| !value.is_empty())
}

/// Port of `validateSubmissionConflicts`.
fn validate_conflicts(
    application_id: i64,
    company: &str,
    requisition_id: Option<&str>,
    ats_name: Option<&str>,
    existing_submissions: i64,
    policy: &Map<String, Value>,
    applications: &[ApplicationIdentity],
) -> Result<(), Thrown> {
    let resubmission = policy.contains_key("resubmissionReason");
    if existing_submissions > 0 && !resubmission {
        return fail("application_already_submitted");
    }
    let company = normalize_company(company);
    let requisition_id = normalize_identifier(requisition_id);
    let ats_name = normalize_identifier(ats_name);
    let same_company: Vec<_> = applications
        .iter()
        .filter(|other| other.id != application_id && normalize_company(&other.company) == company)
        .collect();
    if requisition_id.is_some() && !resubmission {
        let duplicate = same_company.iter().any(|other| {
            if normalize_identifier(other.requisition_id.as_deref()) != requisition_id {
                return false;
            }
            let other_ats = normalize_identifier(other.ats_name.as_deref());
            ats_name.is_none() || other_ats.is_none() || ats_name == other_ats
        });
        if duplicate {
            return fail("duplicate_requisition");
        }
    }
    if !policy.contains_key("sameCompanyOverrideReason")
        && same_company.iter().any(|other| matches!(other.status.as_str(), "applied" | "interview" | "offer"))
    {
        return fail("same_company_active_application");
    }
    Ok(())
}

fn hash_without_policy_change(input: &Map<String, Value>, policy: Option<Value>) -> String {
    let mut hashable = input.clone();
    match policy {
        Some(policy) => {
            hashable.insert("policy".to_owned(), policy);
        }
        None => {
            hashable.remove("policy");
        }
    }
    request_hash(&Value::Object(hashable))
}

fn nullable_str(args: &Args, key: &str) -> Option<Option<String>> {
    args.get(key).map(|value| value.as_str().map(str::to_owned))
}

#[derive(Clone, Copy)]
struct Outcome {
    replayed: bool,
    verified: bool,
}

fn submission_result(
    ctx: &Ctx,
    submission: &repo::Submission,
    documents: &[Document],
    application: Value,
    event: Value,
    outcome: Outcome,
) -> Value {
    json!({
        "replayed": outcome.replayed,
        "dryRun": false,
        "verified": outcome.verified,
        "application": application,
        "submission": ctx.submission_json(submission, documents),
        "event": event,
        "documents": documents.iter().map(|d| ctx.raw_doc_json(d)).collect::<Vec<_>>(),
    })
}

pub async fn record(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    Ok(match run_record(ctx, args).await {
        Ok(value) => json_result(value, false),
        Err(Thrown(message)) if ERROR_CODES.contains(&message.as_str()) => code_error(&message),
        Err(_) => code_error("submission_failed"),
    })
}

/// Port of `recordApplicationSubmission`, in one write transaction. Its `SELECT ... FOR UPDATE` of
/// every application becomes the write lock, so the concurrent-replay fallback is not needed.
async fn run_record(ctx: &Ctx, args: &Args) -> Result<Value, Thrown> {
    let application_id_arg = req_str(args, "applicationId");
    let application_id = {
        let mut conn = ctx.state.db.conn().await?;
        match find_application(&mut conn, application_id_arg).await? {
            Some(application) => application.id,
            None => return fail("not_found"),
        }
    };
    let salary = |key| args.get(key).and_then(Value::as_i64);
    if let (Some(min), Some(max)) = (salary("candidateSalaryMin"), salary("candidateSalaryMax"))
        && min > max
    {
        return fail("salary_range_invalid");
    }
    let answers = validate_answers(args["answers"].as_array().map_or(&[], Vec::as_slice))?;
    let raw_document_ids = args["documentIds"].as_array().cloned().unwrap_or_default();
    let submitted_at = parse_date(req_str(args, "submittedAt")).ok_or_else(|| Thrown("Invalid Date".to_owned()))?;
    let follow_up_at = args.get("followUpAt").map(|value| value.as_str().and_then(parse_date));
    let expected = str_arg(args, "expectedUpdatedAt").and_then(parse_date);
    let idempotency_key = req_str(args, "idempotencyKey");
    let currency = nullable_str(args, "candidateSalaryCurrency").map(|value| value.map(|c| c.to_uppercase()));
    let policy_arg = args.get("policy");

    // The fields the request hash covers, with `undefined` absent as in `stableSerialize`.
    let mut input = Map::new();
    input.insert("applicationId".to_owned(), application_id_arg.into());
    input.insert("submittedAt".to_owned(), submitted_at.as_str().into());
    if let Some(follow_up_at) = &follow_up_at {
        input.insert("followUpAt".to_owned(), follow_up_at.as_ref().map(Timestamp::as_str).into());
    }
    for key in ["applicationUrl", "atsName", "requisitionId", "language"] {
        if let Some(value) = args.get(key) {
            input.insert(key.to_owned(), value.clone());
        }
    }
    input.insert("answers".to_owned(), Value::Array(answers.clone()));
    if let Some(policy) = policy_arg {
        input.insert("policy".to_owned(), policy.clone());
    }
    for key in ["candidateSalaryMin", "candidateSalaryMax", "candidateSalaryPeriod", "candidateSalaryType", "candidateSalaryFlexible"] {
        if let Some(value) = args.get(key) {
            input.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(currency) = &currency {
        input.insert("candidateSalaryCurrency".to_owned(), currency.clone().into());
    }
    input.insert("documentIds".to_owned(), Value::Array(raw_document_ids.clone()));

    let policy_result = validate_policy(policy_arg, &answers, &raw_document_ids);
    let raw_hash = request_hash(&Value::Object(input.clone()));
    let request_hash_value = match &policy_result {
        Ok(policy) => hash_without_policy_change(&input, Some(Value::Object(policy.clone()))),
        Err(_) => raw_hash.clone(),
    };
    let mut accepted = vec![raw_hash, request_hash_value.clone()];
    if policy_arg.is_none_or(Value::is_null) {
        accepted.push(hash_without_policy_change(&input, None));
    }

    let mut tx = ctx.state.db.begin_write().await?;
    let event_key = format!("submission:{idempotency_key}");
    if let Some(replay) = repo::find_by_idempotency_key(&mut tx, idempotency_key).await? {
        if !accepted.contains(&replay.request_hash) {
            return fail("idempotency_conflict");
        }
        let application = apps_repo::get(&mut tx, replay.application_id)
            .await?
            .ok_or_else(|| Thrown("not_found".to_owned()))?;
        let event = events_repo::find_by_idempotency_key(&mut tx, &event_key).await?;
        let documents = ctx.submission_docs(&mut tx, replay.id).await?;
        let verified = application.status == "applied"
            && replay.document_ids.0.as_array().is_some_and(|ids| ids.len() == documents.len());
        let application = ctx.app_with_contacts(&mut tx, &application).await?;
        let event = event.as_ref().map_or(Value::Null, |event| ctx.event_json(event));
        let outcome = Outcome { replayed: true, verified };
        return Ok(submission_result(ctx, &replay, &documents, application, event, outcome));
    }
    let policy = policy_result?;
    let document_ids = validate_document_ids(&raw_document_ids)?;

    let Some(application) = apps_repo::get(&mut tx, application_id).await? else {
        return fail("not_found");
    };
    if expected.as_ref().is_some_and(|expected| *expected != application.updated_at) {
        return fail("conflict");
    }
    let ats_name = nullable_str(args, "atsName");
    let requisition_id = nullable_str(args, "requisitionId");
    let effective_ats = ats_name.clone().unwrap_or_else(|| application.ats_name.clone());
    let effective_requisition = requisition_id.clone().unwrap_or_else(|| application.requisition_id.clone());
    let existing_submissions = repo::count_for_application(&mut tx, application_id).await?;
    let identities = apps_repo::list_identities(&mut tx).await?;
    validate_conflicts(
        application_id,
        &application.company,
        effective_requisition.as_deref(),
        effective_ats.as_deref(),
        existing_submissions,
        &policy,
        &identities,
    )?;

    let numeric_ids = document_ids.iter().map(|id| nid(id)).collect::<Result<Vec<_>, _>>()?;
    let mut documents = Vec::with_capacity(numeric_ids.len());
    for id in &numeric_ids {
        match documents_repo::get(&mut tx, *id).await? {
            Some(document) => documents.push(document),
            None => return fail("invalid_documents"),
        }
    }
    documents.sort_by_key(|document| document.id);
    if documents.iter().any(Document::is_submission_document) {
        return fail("document_already_submitted");
    }

    let application_url = nullable_str(args, "applicationUrl").flatten();
    let language = nullable_str(args, "language").flatten();
    let candidate_flexible = bool_arg(args, "candidateSalaryFlexible").unwrap_or(false);
    let answer_count = answers.len();
    let policy_value = Value::Object(policy);

    if bool_arg(args, "dryRun") == Some(true) {
        let mut predicted = application.clone();
        predicted.status = "applied".to_owned();
        predicted.applied_at = Some(submitted_at.clone());
        if let Some(follow_up_at) = &follow_up_at {
            predicted.follow_up_at = follow_up_at.clone();
        }
        predicted.ats_name = effective_ats.clone();
        predicted.requisition_id = effective_requisition.clone();
        let predicted = ctx.app_with_contacts(&mut tx, &predicted).await?;
        for document in &mut documents {
            document.state = "submitted".to_owned();
            document.submitted_at = Some(submitted_at.clone());
        }
        let raw_documents: Vec<Value> = documents.iter().map(|d| ctx.raw_doc_json(d)).collect();
        let clean_documents: Vec<Value> = documents.iter().map(|d| ctx.doc_json(d)).collect();
        return Ok(json!({
            "replayed": false,
            "dryRun": true,
            "verified": true,
            "application": predicted,
            "submission": {
                "id": "dry-run",
                "userId": ctx.state.owner_id,
                "applicationId": application_id_arg,
                "idempotencyKey": idempotency_key,
                "requestHash": request_hash_value,
                "submittedAt": submitted_at.as_str(),
                "applicationUrl": application_url,
                "atsName": effective_ats,
                "requisitionId": effective_requisition,
                "language": language,
                "answers": answers,
                "policy": policy_value,
                "candidateSalaryMin": args.get("candidateSalaryMin"),
                "candidateSalaryMax": args.get("candidateSalaryMax"),
                "candidateSalaryCurrency": currency.clone().flatten(),
                "candidateSalaryPeriod": args.get("candidateSalaryPeriod"),
                "candidateSalaryType": args.get("candidateSalaryType"),
                "candidateSalaryFlexible": candidate_flexible,
                "documentIds": document_ids,
                "createdAt": submitted_at.as_str(),
                "documents": clean_documents,
            },
            "event": null,
            "documents": raw_documents,
        }));
    }

    let created = repo::insert(
        &mut tx,
        &NewSubmission {
            application_id,
            idempotency_key: idempotency_key.to_owned(),
            request_hash: request_hash_value,
            submitted_at: submitted_at.clone(),
            application_url,
            ats_name: effective_ats,
            requisition_id: effective_requisition,
            language,
            answers: Value::Array(answers),
            policy: policy_value.clone(),
            candidate_salary_min: salary("candidateSalaryMin"),
            candidate_salary_max: salary("candidateSalaryMax"),
            candidate_salary_currency: currency.flatten(),
            candidate_salary_period: str_arg(args, "candidateSalaryPeriod").map(str::to_owned),
            candidate_salary_type: str_arg(args, "candidateSalaryType").map(str::to_owned),
            candidate_salary_flexible: candidate_flexible,
            document_ids: document_ids.clone(),
        },
    )
    .await?;
    documents_repo::claim_for_submission(&mut tx, &numeric_ids, created.id, &submitted_at).await?;

    let guard = expected.map_or(Guard::None, Guard::UpdatedAt);
    let fields = ApplicationFields {
        status: Some("applied".to_owned()),
        applied_at: Some(Some(submitted_at.clone())),
        follow_up_at,
        ats_name,
        requisition_id,
        ..Default::default()
    };
    let updated = apps_repo::update(&mut tx, application_id, fields, guard).await?;
    let event = events_repo::insert(
        &mut tx,
        &NewEvent {
            application_id,
            r#type: "application_submitted".to_owned(),
            idempotency_key: Some(event_key),
            request_hash: None,
            occurred_at: Some(submitted_at.clone()),
            source: Some("mcp".to_owned()),
            actor: Some(ctx.state.owner_email.clone()),
            contact_id: None,
            outcome: None,
            metadata: Some(json!({
                "submissionId": created.id.to_string(),
                "documentIds": document_ids,
                "answerCount": answer_count,
                "policy": policy_value,
            })),
        },
    )
    .await?;
    let stored = ctx.submission_docs(&mut tx, created.id).await?;
    let verified = updated.status == "applied"
        && updated.applied_at.as_ref() == Some(&submitted_at)
        && stored.len() == document_ids.len();
    let application = ctx.app_with_contacts(&mut tx, &updated).await?;
    let event = ctx.event_json(&event);
    let outcome = Outcome { replayed: false, verified };
    let result = submission_result(ctx, &created, &stored, application, event, outcome);
    tx.commit().await.map_err(nexus_core::Error::from)?;
    Ok(result)
}

pub async fn list(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let run = async {
        let mut conn = ctx.state.db.conn().await.ok()?;
        let application = find_application(&mut conn, req_str(args, "applicationId")).await.ok()??;
        let rows = repo::list_for_application(&mut conn, application.id, false).await.ok()?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let documents = ctx.submission_docs(&mut conn, row.id).await.ok()?;
            out.push(ctx.submission_json(row, &documents));
        }
        Some(out)
    };
    Ok(match run.await {
        Some(values) => json_result(Value::Array(values), false),
        None => text_error("Application not found or access denied"),
    })
}

pub async fn get(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let id = nid(req_str(args, "id"))?;
    let mut conn = ctx.state.db.conn().await?;
    let Some(submission) = repo::get(&mut conn, id).await? else {
        return Ok(text_error("Submission not found"));
    };
    let documents = ctx.submission_docs(&mut conn, submission.id).await?;
    Ok(json_result(ctx.submission_json(&submission, &documents), false))
}

pub async fn recall_package(ctx: &Ctx, args: &Args) -> ToolResult {
    if let Some(error) = ctx.require_submissions() {
        return Ok(error);
    }
    let mut conn = ctx.state.db.conn().await?;
    let Some(application) = find_application(&mut conn, req_str(args, "applicationId")).await? else {
        return Ok(text_error("Application not found"));
    };
    let rows = repo::list_for_application(&mut conn, application.id, true).await?;
    let mut submissions = Vec::with_capacity(rows.len());
    for row in &rows {
        let documents = ctx.submission_docs(&mut conn, row.id).await?;
        submissions.push(ctx.submission_json(row, &documents));
    }
    let events = events_repo::list_for_application(&mut conn, application.id, Some(500)).await?;
    let documents = documents_repo::list_for_application(&mut conn, application.id).await?;
    Ok(json_result(
        json!({
            "application": ctx.app_with_contacts(&mut conn, &application).await?,
            "submissions": submissions,
            "events": events.iter().map(|event| ctx.event_json(event)).collect::<Vec<_>>(),
            "documents": documents.iter().map(|document| ctx.doc_json(document)).collect::<Vec<_>>(),
        }),
        false,
    ))
}
