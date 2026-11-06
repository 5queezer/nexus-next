use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nexus_core::Timestamp;
use nexus_core::repo::applications::{self as apps_repo, ApplicationFields, Guard};
use nexus_core::repo::contacts as contacts_repo;
use nexus_core::repo::documents as documents_repo;
use nexus_core::repo::events::{self as repo, EventFilter, NewEvent};
use serde_json::{Map, Value, json};

use super::{Args, Ctx, ToolResult, find_application, json_result, req_str, str_arg};
use crate::js::{ilike_contains, nid, parse_date, request_hash, trim, utf16_len};

const ERROR_CODES: [&str; 17] = [
    "event_type_invalid",
    "event_metadata_invalid",
    "event_metadata_too_large",
    "event_occurred_at_invalid",
    "occurred_at_required_for_idempotency",
    "idempotency_key_invalid",
    "idempotency_conflict",
    "invalid_expected_updated_at",
    "not_found",
    "conflict",
    "application_deleting",
    "contact_not_found",
    "document_not_found",
    "submission_not_found",
    "verification_failed",
    "event_query_invalid",
    "submission_event_requires_submission_workflow",
];

const COMMON_KEYS: [&str; 3] = ["contactId", "outcome", "nextAction"];

fn event_keys(event_type: &str) -> Vec<&'static str> {
    let with_common = |rest: &[&'static str]| [&COMMON_KEYS[..], rest].concat();
    match event_type {
        "opportunity_discovered" => vec!["channel", "nextAction"],
        "recruiter_contacted" => with_common(&["channel", "direction", "followUpAt", "toStage"]),
        "outbound_contact_recorded" => with_common(&["channel"]),
        "reply_received" => with_common(&["channel", "responseKind", "replyDateKnown"]),
        "stage_changed" => vec!["fromStage", "toStage", "toStatus"],
        "interview_invited" => with_common(&[
            "interviewType",
            "scheduledAt",
            "durationMinutes",
            "followUpAt",
            "toStage",
        ]),
        "interview_scheduled" => {
            with_common(&["interviewType", "scheduledAt", "durationMinutes", "toStage"])
        }
        "interview_completed" => with_common(&["interviewType", "followUpAt", "toStage"]),
        "feedback_received" => with_common(&["followUpAt", "toStage"]),
        "follow_up_scheduled" => vec!["contactId", "followUpAt", "nextAction"],
        "offer_received" => with_common(&["followUpAt"]),
        "application_rejected" => vec!["contactId", "outcome", "reason", "fromStage"],
        "document_attached" => vec!["documentId", "documentType"],
        "note_added" => vec!["note"],
        _ => Vec::new(),
    }
}

fn required_keys(event_type: &str) -> &'static [&'static str] {
    match event_type {
        "stage_changed" => &["toStage"],
        "reply_received" => &["responseKind"],
        "interview_scheduled" => &["interviewType", "scheduledAt"],
        "follow_up_scheduled" => &["followUpAt"],
        "document_attached" => &["documentId"],
        "note_added" => &["note"],
        _ => &[],
    }
}

#[derive(Debug)]
pub(crate) enum EventError {
    Code(String),
    Metadata {
        path: String,
        expected: String,
        reason: String,
    },
}

impl EventError {
    fn code(code: &str) -> Self {
        Self::Code(code.to_owned())
    }

    fn metadata(
        path: impl Into<String>,
        expected: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::Metadata {
            path: path.into(),
            expected: expected.into(),
            reason: reason.into(),
        }
    }
}

impl From<nexus_core::Error> for EventError {
    fn from(error: nexus_core::Error) -> Self {
        Self::Code(super::Thrown::from(error).0)
    }
}

impl From<sqlx::Error> for EventError {
    fn from(error: sqlx::Error) -> Self {
        nexus_core::Error::Database(error).into()
    }
}

impl From<super::Thrown> for EventError {
    fn from(error: super::Thrown) -> Self {
        Self::Code(error.0)
    }
}

fn error_result(error: EventError, fallback: &str) -> rmcp::model::CallToolResult {
    match error {
        EventError::Code(code) => {
            let code = if ERROR_CODES.contains(&code.as_str()) {
                code.as_str()
            } else {
                fallback
            };
            json_result(json!({"error": {"code": code}}), true)
        }
        EventError::Metadata {
            path,
            expected,
            reason,
        } => json_result(
            json!({"error": {"code": "event_metadata_invalid", "path": path, "expected": expected, "reason": reason}}),
            true,
        ),
    }
}

fn optional_string(value: &Value, max: usize, path: &str) -> Result<Option<String>, EventError> {
    let expected = || format!("string with 1 to {max} characters");
    let name = path.rsplit('.').next().unwrap_or(path);
    let Some(text) = value.as_str() else {
        return Err(EventError::metadata(
            path,
            expected(),
            format!("{name} must be a string"),
        ));
    };
    let normalized = trim(text);
    if normalized.is_empty() || utf16_len(normalized) > max {
        return Err(EventError::metadata(
            path,
            expected(),
            format!("{name} must contain 1 to {max} characters after trimming"),
        ));
    }
    Ok(Some(normalized.to_owned()))
}

/// Port of `parseMetadata`. Keys that only `application_submitted` allows can't reach it, because
/// that type is rejected first.
fn parse_metadata(
    event_type: &str,
    raw: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, EventError> {
    let empty = Map::new();
    let raw = raw.unwrap_or(&empty);
    if raw.len() > 100 {
        return Err(EventError::metadata(
            "metadata",
            "object with at most 100 keys",
            "metadata must have at most 100 keys",
        ));
    }
    if let Some(key) = raw
        .keys()
        .find(|key| key.is_empty() || utf16_len(key) > 100)
    {
        return Err(EventError::metadata(
            if key.is_empty() {
                "metadata".to_owned()
            } else {
                format!("metadata.{key}")
            },
            "key with 1 to 100 characters",
            "metadata keys must contain 1 to 100 characters",
        ));
    }
    let allowed = event_keys(event_type);
    if let Some(key) = raw.keys().find(|key| !allowed.contains(&key.as_str())) {
        let expected = if allowed.is_empty() {
            "no metadata keys".to_owned()
        } else {
            allowed.join(", ")
        };
        return Err(EventError::metadata(
            format!("metadata.{key}"),
            format!("one of: {expected}"),
            format!("{key} is not accepted for {event_type}"),
        ));
    }

    let mut metadata = Map::new();
    for (key, item) in raw {
        if item.is_null() || item.as_str() == Some("") {
            continue;
        }
        let path = format!("metadata.{key}");
        match key.as_str() {
            "scheduledAt" | "followUpAt" => {
                let date = item.as_str().and_then(parse_date).ok_or_else(|| {
                    EventError::metadata(
                        path.as_str(),
                        "ISO 8601 date-time string",
                        format!("{key} must be a valid ISO 8601 date-time string"),
                    )
                })?;
                metadata.insert(key.clone(), date.as_str().into());
            }
            "durationMinutes" => match item.as_i64().filter(|n| (1..=1_440).contains(n)) {
                Some(minutes) => {
                    metadata.insert(key.clone(), minutes.into());
                }
                None => {
                    return Err(EventError::metadata(
                        path,
                        "integer from 1 to 1440",
                        "durationMinutes must be an integer from 1 to 1440",
                    ));
                }
            },
            "replyDateKnown" => {
                if !item.is_boolean() {
                    return Err(EventError::metadata(
                        path,
                        "boolean",
                        "replyDateKnown must be a boolean",
                    ));
                }
                metadata.insert(key.clone(), item.clone());
            }
            "responseKind" => {
                if !matches!(item.as_str(), Some("human" | "automatic")) {
                    return Err(EventError::metadata(
                        path,
                        "one of: human, automatic",
                        "responseKind must identify a human or automatic response",
                    ));
                }
                metadata.insert(key.clone(), item.clone());
            }
            "direction" => {
                if !matches!(item.as_str(), Some("inbound" | "outbound")) {
                    return Err(EventError::metadata(
                        path,
                        "one of: inbound, outbound",
                        "direction must identify an inbound or outbound contact",
                    ));
                }
                metadata.insert(key.clone(), item.clone());
            }
            "toStatus" => {
                let status = optional_string(item, 255, &path)?;
                if !matches!(
                    status.as_deref(),
                    Some("inbound" | "applied" | "interview" | "offer" | "rejected")
                ) {
                    return Err(EventError::metadata(
                        path,
                        "one of: inbound, applied, interview, offer, rejected",
                        "toStatus must be a valid application status",
                    ));
                }
                metadata.insert(key.clone(), status.into());
            }
            _ => {
                let max = match key.as_str() {
                    "note" => 5_000,
                    "reason" | "nextAction" => 2_000,
                    _ => 255,
                };
                if let Some(normalized) = optional_string(item, max, &path)? {
                    metadata.insert(key.clone(), normalized.into());
                }
            }
        }
    }

    if let Some(key) = required_keys(event_type)
        .iter()
        .find(|key| !metadata.contains_key(**key))
    {
        return Err(EventError::metadata(
            format!("metadata.{key}"),
            "required value",
            format!("{key} is required for {event_type}"),
        ));
    }
    if Value::Object(metadata.clone()).to_string().len() > 32_000 {
        return Err(EventError::code("event_metadata_too_large"));
    }
    Ok(metadata)
}

/// `ParsedApplicationEventCommand`.
pub(crate) struct EventCommand {
    event_type: String,
    occurred_at: Timestamp,
    idempotency_key: Option<String>,
    expected_updated_at: Option<Timestamp>,
    actor: String,
    metadata: Map<String, Value>,
    contact_id: Option<String>,
    outcome: Option<String>,
}

/// Port of `parseApplicationEventCommand`, with `source` always `mcp`.
fn parse_command(
    ctx: &Ctx,
    event_type: &str,
    args: &Args,
    metadata: Option<&Map<String, Value>>,
) -> Result<EventCommand, EventError> {
    let event_type = trim(event_type);
    if event_type == "application_submitted" {
        return Err(EventError::code(
            "submission_event_requires_submission_workflow",
        ));
    }
    let idempotency_key = str_arg(args, "idempotencyKey").map(trim).map(str::to_owned);
    if let Some(key) = &idempotency_key
        && !(8..=128).contains(&utf16_len(key))
    {
        return Err(EventError::code("idempotency_key_invalid"));
    }
    let occurred_at = str_arg(args, "occurredAt");
    if idempotency_key.is_some() && occurred_at.is_none() {
        return Err(EventError::code("occurred_at_required_for_idempotency"));
    }
    let occurred_at = match occurred_at {
        None => Timestamp::now(),
        Some(text) => {
            parse_date(text).ok_or_else(|| EventError::code("event_occurred_at_invalid"))?
        }
    };
    let expected_updated_at = str_arg(args, "expectedUpdatedAt")
        .map(|text| parse_date(text).ok_or_else(|| EventError::code("invalid_expected_updated_at")))
        .transpose()?;
    let metadata = parse_metadata(event_type, metadata)?;
    let string_of = |key: &str| metadata.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(EventCommand {
        event_type: event_type.to_owned(),
        occurred_at,
        idempotency_key,
        expected_updated_at,
        actor: trim(&ctx.state.owner_email).to_owned(),
        contact_id: string_of("contactId"),
        outcome: string_of("outcome"),
        metadata,
    })
}

fn stamp(metadata: &Map<String, Value>, key: &str) -> Option<Timestamp> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .and_then(parse_date)
}

/// Port of `deriveEventProjection`.
fn derive_projection(
    command: &EventCommand,
    status: &str,
    current_stage: Option<&str>,
) -> (ApplicationFields, Map<String, Value>) {
    let mut patch = ApplicationFields::default();
    let mut metadata = command.metadata.clone();
    let stage_value = current_stage.map_or(Value::Null, Value::from);
    if let Some(next_action) = metadata.get("nextAction").and_then(Value::as_str) {
        patch.next_action = Some(Some(next_action.to_owned()));
    }
    let occurred = Some(Some(command.occurred_at.clone()));
    let follow_up = stamp(&metadata, "followUpAt");
    let mut transition = |patch: &mut ApplicationFields, new_status: &str, stage: String| {
        patch.status = Some(new_status.to_owned());
        patch.current_stage = Some(Some(stage.clone()));
        patch.last_contact = occurred.clone();
        metadata.insert("fromStage".to_owned(), stage_value.clone());
        metadata.insert("toStage".to_owned(), stage.into());
        metadata.insert("fromStatus".to_owned(), status.into());
        metadata.insert("toStatus".to_owned(), new_status.into());
    };
    let to_stage = command
        .metadata
        .get("toStage")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match command.event_type.as_str() {
        "stage_changed" => {
            patch.current_stage = Some(to_stage);
            metadata.insert("fromStage".to_owned(), stage_value.clone());
            metadata.insert("fromStatus".to_owned(), status.into());
            if let Some(to_status) = command.metadata.get("toStatus").and_then(Value::as_str) {
                patch.status = Some(to_status.to_owned());
            }
        }
        kind @ ("interview_invited" | "interview_completed") => {
            transition(
                &mut patch,
                "interview",
                to_stage.unwrap_or_else(|| kind.to_owned()),
            );
            if follow_up.is_some() {
                patch.follow_up_at = Some(follow_up);
            }
        }
        "interview_scheduled" => {
            transition(
                &mut patch,
                "interview",
                to_stage.unwrap_or_else(|| "interview_scheduled".to_owned()),
            );
            patch.follow_up_at = Some(stamp(&command.metadata, "scheduledAt"));
        }
        "outbound_contact_recorded" => {
            if status == "inbound" {
                patch.status = Some("applied".to_owned());
            }
            patch.last_contact = occurred.clone();
        }
        "reply_received" => patch.last_contact = occurred.clone(),
        "recruiter_contacted" | "feedback_received" => {
            patch.last_contact = occurred.clone();
            if let Some(stage) = to_stage {
                patch.current_stage = Some(Some(stage));
                metadata.insert("fromStage".to_owned(), stage_value.clone());
            }
            if follow_up.is_some() {
                patch.follow_up_at = Some(follow_up);
            }
        }
        "follow_up_scheduled" => patch.follow_up_at = Some(follow_up),
        "offer_received" => {
            transition(&mut patch, "offer", "offer_received".to_owned());
            if follow_up.is_some() {
                patch.follow_up_at = Some(follow_up);
            }
        }
        "application_rejected" => {
            transition(&mut patch, "rejected", "rejected".to_owned());
            patch.follow_up_at = Some(None);
        }
        _ => {}
    }
    (patch, metadata)
}

/// Port of `recordApplicationEvent`, with replay and write under one lock in place of its
/// optimistic retry.
async fn record_event(
    ctx: &Ctx,
    application_id: i64,
    command: EventCommand,
) -> Result<Value, EventError> {
    let application_ref = application_id.to_string();
    let occurred_at = command.occurred_at.as_str();
    let metadata_value = Value::Object(command.metadata.clone());
    let hash = request_hash(&json!({
        "applicationId": application_ref,
        "type": command.event_type,
        "occurredAt": occurred_at,
        "source": ctx.source,
        "actor": command.actor,
        "metadata": metadata_value,
        "contactId": command.contact_id,
        "outcome": command.outcome,
        "expectedUpdatedAt": command.expected_updated_at.as_ref().map(Timestamp::as_str),
    }));
    let legacy_hash = request_hash(&json!({
        "applicationId": application_ref,
        "type": command.event_type,
        "occurredAt": occurred_at,
        "metadata": metadata_value,
    }));

    let mut tx = ctx.state.db.begin_write().await?;
    if let Some(key) = &command.idempotency_key
        && let Some(event) = repo::find_by_idempotency_key(&mut tx, key).await?
    {
        let persisted = event.persisted_request_hash();
        if persisted != Some(hash.as_str()) && persisted != Some(legacy_hash.as_str()) {
            return Err(EventError::code("idempotency_conflict"));
        }
        let application = apps_repo::get(&mut tx, application_id)
            .await?
            .ok_or_else(|| EventError::code("not_found"))?;
        let application = ctx.app_with_contacts(&mut tx, &application).await?;
        return Ok(
            json!({"event": ctx.event_json(&event), "application": application, "replayed": true}),
        );
    }

    let application = apps_repo::get(&mut tx, application_id)
        .await?
        .ok_or_else(|| EventError::code("not_found"))?;
    if command
        .expected_updated_at
        .as_ref()
        .is_some_and(|expected| *expected != application.updated_at)
    {
        return Err(EventError::code("conflict"));
    }
    if let Some(contact_id) = &command.contact_id {
        let id = nid(contact_id).map_err(|_| EventError::code("contact_not_found"))?;
        let contacts = contacts_repo::list_for_application(&mut tx, application_id).await?;
        if !contacts.iter().any(|contact| contact.id == id) {
            return Err(EventError::code("contact_not_found"));
        }
    }
    if let Some(document_id) = command.metadata.get("documentId").and_then(Value::as_str) {
        let id = nid(document_id).map_err(|_| EventError::code("document_not_found"))?;
        let linked = documents_repo::get(&mut tx, id)
            .await?
            .is_some_and(|document| {
                document
                    .applications
                    .iter()
                    .any(|parent| parent.id == application_id)
            });
        if !linked {
            return Err(EventError::code("document_not_found"));
        }
    }

    let (patch, metadata) = derive_projection(
        &command,
        &application.status,
        application.current_stage.as_deref(),
    );
    let updated = apps_repo::update(
        &mut tx,
        application_id,
        patch,
        Guard::EventVersion(application.event_version),
    )
    .await?;
    let event = repo::insert(
        &mut tx,
        &NewEvent {
            application_id,
            r#type: command.event_type,
            idempotency_key: command.idempotency_key,
            request_hash: Some(hash),
            occurred_at: Some(command.occurred_at),
            source: Some(ctx.source.to_owned()),
            actor: Some(command.actor),
            contact_id: command.contact_id,
            outcome: command.outcome,
            metadata: Some(Value::Object(metadata)),
        },
    )
    .await?;
    let application = ctx.app_with_contacts(&mut tx, &updated).await?;
    tx.commit().await?;
    Ok(json!({"event": ctx.event_json(&event), "application": application, "replayed": false}))
}

async fn require_application(ctx: &Ctx, id: &str) -> Result<i64, EventError> {
    let mut conn = ctx.state.db.conn().await?;
    match find_application(&mut conn, id).await? {
        Some(application) => Ok(application.id),
        None => Err(EventError::code("not_found")),
    }
}

pub async fn append_note(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let id = require_application(ctx, req_str(args, "applicationId")).await?;
        let mut metadata = Map::new();
        metadata.insert("note".to_owned(), req_str(args, "note").into());
        let command = parse_command(ctx, "note_added", args, Some(&metadata))?;
        record_event(ctx, id, command).await
    };
    Ok(match run.await {
        Ok(value) => json_result(value, false),
        Err(EventError::Metadata { .. }) => {
            error_result(EventError::code("event_metadata_invalid"), "append_failed")
        }
        Err(error) => error_result(error, "append_failed"),
    })
}

pub async fn record(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let id = require_application(ctx, req_str(args, "applicationId")).await?;
        let command = parse_command(
            ctx,
            req_str(args, "type"),
            args,
            args.get("metadata").and_then(Value::as_object),
        )?;
        record_event(ctx, id, command).await
    };
    Ok(match run.await {
        Ok(value) => json_result(value, false),
        Err(error) => error_result(error, "event_failed"),
    })
}

fn query_string(args: &Args, key: &str) -> Result<Option<String>, EventError> {
    match str_arg(args, key) {
        None | Some("") => Ok(None),
        Some(text) => {
            let normalized = trim(text);
            if normalized.is_empty() || utf16_len(normalized) > 255 {
                return Err(EventError::code("event_query_invalid"));
            }
            Ok(Some(normalized.to_owned()))
        }
    }
}

fn decode_cursor(value: &str) -> Result<(Timestamp, String), EventError> {
    let invalid = || EventError::code("event_query_invalid");
    let bytes = URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|_| invalid())?;
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if parsed["version"] != 1 {
        return Err(invalid());
    }
    let id = parsed["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    let occurred_at = parsed["occurredAt"]
        .as_str()
        .and_then(parse_date)
        .ok_or_else(invalid)?;
    Ok((occurred_at, id.to_owned()))
}

/// Port of `parseEventQuery`. Malformed IDs fail after parsing, in the adapter, so they surface as
/// the tool's fallback code rather than `event_query_invalid`.
fn parse_query(args: &Args) -> Result<EventFilter, EventError> {
    let invalid = || EventError::code("event_query_invalid");
    let mut types = Vec::new();
    if let Some(items) = args.get("types").and_then(Value::as_array) {
        if items.is_empty() {
            return Err(invalid());
        }
        for item in items.iter().filter_map(Value::as_str) {
            if !types.iter().any(|known: &String| known == item) {
                types.push(item.to_owned());
            }
        }
    }
    let date = |key| {
        str_arg(args, key)
            .map(|text| parse_date(text).ok_or_else(invalid))
            .transpose()
    };
    let occurred_after = date("occurredAfter")?;
    let occurred_before = date("occurredBefore")?;
    if let (Some(after), Some(before)) = (&occurred_after, &occurred_before)
        && after > before
    {
        return Err(invalid());
    }
    let application_id = query_string(args, "applicationId")?;
    let company = query_string(args, "company")?;
    let source = query_string(args, "source")?;
    let actor = query_string(args, "actor")?;
    let contact_id = query_string(args, "contactId")?;
    let outcome = query_string(args, "outcome")?;
    let cursor = str_arg(args, "cursor")
        .filter(|c| !c.is_empty())
        .map(decode_cursor)
        .transpose()?;
    let adapter_id = |id: &str| nid(id).map_err(EventError::Code);
    Ok(EventFilter {
        application_id: application_id.as_deref().map(adapter_id).transpose()?,
        application_ids: None,
        company,
        types,
        occurred_after,
        occurred_before,
        source,
        actor,
        contact_id,
        outcome,
        cursor: cursor
            .map(|(at, id)| adapter_id(&id).map(|id| (at, id)))
            .transpose()?,
        oldest_first: str_arg(args, "order") == Some("oldest"),
        limit: args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(50)
            .min(100),
    })
}

async fn event_page(ctx: &Ctx, mut filter: EventFilter) -> Result<Value, EventError> {
    let mut conn = ctx.state.db.conn().await?;
    // SQLite can't reproduce the `ILIKE` old Nexus matched companies with.
    if let Some(company) = filter.company.take() {
        let matching = apps_repo::list_identities(&mut conn).await?;
        filter.application_ids = Some(
            matching
                .iter()
                .filter(|application| ilike_contains(&application.company, &company))
                .map(|application| application.id)
                .collect(),
        );
    }
    let page = repo::list_filtered(&mut conn, &filter).await?;
    let next_cursor = match page.items.last() {
        Some(last) if page.has_more => {
            let cursor = json!({"version": 1, "occurredAt": last.occurred_at.as_str(), "id": last.id.to_string()});
            Value::from(URL_SAFE_NO_PAD.encode(cursor.to_string()))
        }
        _ => Value::Null,
    };
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|event| ctx.event_json(event))
        .collect();
    Ok(json!({"items": items, "nextCursor": next_cursor}))
}

pub async fn list(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        require_application(ctx, req_str(args, "applicationId")).await?;
        event_page(ctx, parse_query(args)?).await
    };
    Ok(match run.await {
        Ok(value) => json_result(value, false),
        Err(error) => error_result(error, "event_query_failed"),
    })
}

pub async fn activity(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async { event_page(ctx, parse_query(args)?).await };
    Ok(match run.await {
        Ok(value) => json_result(value, false),
        Err(error) => error_result(error, "event_query_failed"),
    })
}
