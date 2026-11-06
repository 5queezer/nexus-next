use std::collections::HashMap;

use serde::{Serialize, Serializer};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{QueryBuilder, SqliteConnection};

use super::{ApplicationRef, applications, id_string};
use crate::error::Result;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationEvent {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    #[serde(serialize_with = "id_string")]
    pub application_id: i64,
    pub r#type: String,
    pub idempotency_key: Option<String>,
    #[serde(skip)]
    pub request_hash: Option<String>,
    pub occurred_at: Timestamp,
    pub source: Option<String>,
    pub actor: Option<String>,
    pub contact_id: Option<String>,
    pub outcome: Option<String>,
    #[serde(serialize_with = "public_metadata")]
    pub metadata: Option<Json<Value>>,
    pub created_at: Timestamp,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<ApplicationRef>,
}

/// Port of `publicEventMetadata`: objects only, without the legacy `requestHash` key.
fn public_metadata<S: Serializer>(value: &Option<Json<Value>>, s: S) -> Result2<S> {
    match value.as_ref().map(|json| &json.0) {
        Some(Value::Object(metadata)) => {
            let mut metadata = metadata.clone();
            metadata.shift_remove("requestHash");
            metadata.serialize(s)
        }
        _ => s.serialize_none(),
    }
}

type Result2<S> = std::result::Result<<S as Serializer>::Ok, <S as Serializer>::Error>;

impl ApplicationEvent {
    /// The hash a replay is checked against. Older events keep it inside `metadata`.
    pub fn persisted_request_hash(&self) -> Option<&str> {
        self.request_hash.as_deref().or_else(|| {
            self.metadata
                .as_ref()?
                .0
                .as_object()?
                .get("requestHash")?
                .as_str()
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewEvent {
    pub application_id: i64,
    pub r#type: String,
    pub idempotency_key: Option<String>,
    pub request_hash: Option<String>,
    pub occurred_at: Option<Timestamp>,
    pub source: Option<String>,
    pub actor: Option<String>,
    pub contact_id: Option<String>,
    pub outcome: Option<String>,
    pub metadata: Option<Value>,
}

/// Inserts an event. `occurred_at` defaults to now. A taken idempotency key surfaces as an error
/// with `is_unique_violation()`.
pub async fn insert(conn: &mut SqliteConnection, event: &NewEvent) -> Result<ApplicationEvent> {
    Ok(sqlx::query_as(
        "INSERT INTO application_events (application_id, type, idempotency_key, request_hash,
             occurred_at, source, actor, contact_id, outcome, metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(event.application_id)
    .bind(&event.r#type)
    .bind(&event.idempotency_key)
    .bind(&event.request_hash)
    .bind(event.occurred_at.clone().unwrap_or_else(Timestamp::now))
    .bind(&event.source)
    .bind(&event.actor)
    .bind(&event.contact_id)
    .bind(&event.outcome)
    .bind(event.metadata.as_ref().map(Json))
    .fetch_one(conn)
    .await?)
}

pub async fn find_by_idempotency_key(
    conn: &mut SqliteConnection,
    key: &str,
) -> Result<Option<ApplicationEvent>> {
    Ok(
        sqlx::query_as("SELECT * FROM application_events WHERE idempotency_key = ?")
            .bind(key)
            .fetch_optional(conn)
            .await?,
    )
}

/// Newest first. `limit` is clamped to 1..=500, default 100, as in `listApplicationEvents`.
pub async fn list_for_application(
    conn: &mut SqliteConnection,
    application_id: i64,
    limit: Option<i64>,
) -> Result<Vec<ApplicationEvent>> {
    Ok(sqlx::query_as(
        "SELECT * FROM application_events WHERE application_id = ?
         ORDER BY occurred_at DESC, id DESC LIMIT ?",
    )
    .bind(application_id)
    .bind(limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(conn)
    .await?)
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub application_id: Option<i64>,
    /// Substring of the company name. Case-insensitive for ASCII only.
    pub company: Option<String>,
    pub types: Vec<String>,
    pub occurred_after: Option<Timestamp>,
    pub occurred_before: Option<Timestamp>,
    pub source: Option<String>,
    pub actor: Option<String>,
    pub contact_id: Option<String>,
    pub outcome: Option<String>,
    /// The last row of the previous page: `occurred_at` and `id`.
    pub cursor: Option<(Timestamp, i64)>,
    pub oldest_first: bool,
    pub limit: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    pub items: Vec<ApplicationEvent>,
    pub has_more: bool,
}

/// Port of `listApplicationEventsFiltered`, with each item's `application` filled in. Encoding the
/// cursor is the caller's.
pub async fn list_filtered(conn: &mut SqliteConnection, filter: &EventFilter) -> Result<EventPage> {
    let mut qb = QueryBuilder::new(
        "SELECT e.* FROM application_events e JOIN applications a ON a.id = e.application_id
         WHERE 1 = 1",
    );
    if let Some(id) = filter.application_id {
        qb.push(" AND e.application_id = ").push_bind(id);
    }
    if let Some(company) = &filter.company {
        qb.push(" AND instr(lower(a.company), lower(")
            .push_bind(company.clone())
            .push(")) > 0");
    }
    if !filter.types.is_empty() {
        qb.push(" AND e.type IN (");
        let mut types = qb.separated(", ");
        for event_type in &filter.types {
            types.push_bind(event_type.clone());
        }
        qb.push(")");
    }
    for (column, value) in [
        ("e.source", &filter.source),
        ("e.actor", &filter.actor),
        ("e.contact_id", &filter.contact_id),
        ("e.outcome", &filter.outcome),
    ] {
        if let Some(value) = value {
            qb.push(format!(" AND {column} = "))
                .push_bind(value.clone());
        }
    }
    if let Some(after) = &filter.occurred_after {
        qb.push(" AND e.occurred_at >= ").push_bind(after.clone());
    }
    if let Some(before) = &filter.occurred_before {
        qb.push(" AND e.occurred_at <= ").push_bind(before.clone());
    }
    let (compare, direction) = if filter.oldest_first {
        (">", "ASC")
    } else {
        ("<", "DESC")
    };
    if let Some((occurred_at, id)) = &filter.cursor {
        qb.push(format!(" AND (e.occurred_at {compare} "))
            .push_bind(occurred_at.clone())
            .push(" OR (e.occurred_at = ")
            .push_bind(occurred_at.clone())
            .push(format!(" AND e.id {compare} "))
            .push_bind(*id)
            .push("))");
    }
    qb.push(format!(
        " ORDER BY e.occurred_at {direction}, e.id {direction} LIMIT "
    ))
    .push_bind(filter.limit + 1);

    let mut items: Vec<ApplicationEvent> = qb.build_query_as().fetch_all(&mut *conn).await?;
    let has_more = items.len() as i64 > filter.limit;
    items.truncate(filter.limit.max(0) as usize);

    let ids: Vec<i64> = items.iter().map(|event| event.application_id).collect();
    let refs: HashMap<i64, ApplicationRef> = applications::refs(conn, &ids)
        .await?
        .into_iter()
        .map(|parent| (parent.id, parent))
        .collect();
    for item in &mut items {
        item.application = refs.get(&item.application_id).cloned();
    }
    Ok(EventPage { items, has_more })
}
