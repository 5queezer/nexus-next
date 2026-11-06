//! Read-only JSON under `/api`, for the dashboard and the later TUI. Field names and ID-as-string
//! encoding follow the repository's serde types, which keep old Nexus's shapes.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use nexus_core::Db;
use nexus_core::repo::applications::{self, ApplicationFilter};
use nexus_core::repo::documents::{self, BlobState, Document};
use nexus_core::repo::{contacts, events, submissions};
use serde::Deserialize;
use serde_json::{Value, json};

/// Mounts the read-only routes under `/api`. Auth is the caller's.
pub fn api_router(db: Db) -> Router {
    Router::new()
        .route("/api/applications", get(list_applications))
        .route("/api/applications/{id}", get(get_application))
        .route("/api/applications/{id}/events", get(list_events))
        .route("/api/applications/{id}/contacts", get(list_contacts))
        .route("/api/applications/{id}/documents", get(list_documents))
        .route("/api/applications/{id}/submissions", get(list_submissions))
        .route("/api/documents/{id}", get(get_document))
        .route("/api/documents/{id}/download", get(download_document))
        .layer(middleware::map_response(crate::secure))
        .with_state(db)
}

struct ApiError(StatusCode, &'static str);

impl From<nexus_core::Error> for ApiError {
    fn from(e: nexus_core::Error) -> Self {
        match e {
            nexus_core::Error::NotFound => Self(StatusCode::NOT_FOUND, "not_found"),
            e => {
                tracing::error!(error = %e, "api query failed");
                Self(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// IDs are decimal in the path; anything else is a miss, not a client error worth explaining.
fn parse_id(id: &str) -> Result<i64, ApiError> {
    id.parse()
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "not_found"))
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    search: Option<String>,
    #[serde(default)]
    archived: Archived,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Archived {
    #[default]
    Hide,
    Include,
    Only,
}

async fn list_applications(
    State(db): State<Db>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Value> {
    let filter = ApplicationFilter {
        status: query.status.into_iter().collect(),
        ..Default::default()
    };
    let mut items = applications::list_filtered(&mut *db.conn().await?, &filter).await?;
    // SQLite's lower() folds ASCII only; matching here keeps `Ü` and `ö` case-insensitive.
    if let Some(term) = query.search.as_deref().filter(|s| !s.is_empty()) {
        items.retain(|a| crate::dashboard::matches_search(a, term));
    }
    items.retain(|a| match query.archived {
        Archived::Hide => a.archived_at.is_none(),
        Archived::Include => true,
        Archived::Only => a.archived_at.is_some(),
    });
    Ok(Json(json!({ "items": items })))
}

async fn get_application(State(db): State<Db>, Path(id): Path<String>) -> ApiResult<Value> {
    let application = applications::get(&mut *db.conn().await?, parse_id(&id)?)
        .await?
        .ok_or(nexus_core::Error::NotFound)?;
    Ok(Json(json!(application)))
}

/// Parent lookups so a child list of a missing application is a 404, not an empty list.
async fn require_application(db: &Db, id: &str) -> Result<i64, ApiError> {
    let id = parse_id(id)?;
    if applications::exists(&mut *db.conn().await?, id).await? {
        Ok(id)
    } else {
        Err(nexus_core::Error::NotFound.into())
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<i64>,
}

async fn list_events(
    State(db): State<Db>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Value> {
    let id = require_application(&db, &id).await?;
    let items = events::list_for_application(&mut *db.conn().await?, id, query.limit).await?;
    Ok(Json(json!({ "items": items })))
}

async fn list_contacts(State(db): State<Db>, Path(id): Path<String>) -> ApiResult<Value> {
    let id = require_application(&db, &id).await?;
    let items = contacts::list_for_application(&mut *db.conn().await?, id).await?;
    Ok(Json(json!({ "items": items })))
}

async fn list_submissions(State(db): State<Db>, Path(id): Path<String>) -> ApiResult<Value> {
    let id = require_application(&db, &id).await?;
    let items = submissions::list_for_application(&mut *db.conn().await?, id, true).await?;
    Ok(Json(json!({ "items": items })))
}

fn document_json(document: &Document) -> Value {
    let mut value = json!(document);
    value["blobState"] = match document.blob_state {
        BlobState::Present => "present",
        BlobState::Missing => "missing",
    }
    .into();
    value
}

async fn list_documents(State(db): State<Db>, Path(id): Path<String>) -> ApiResult<Value> {
    let id = require_application(&db, &id).await?;
    let items = documents::list_for_application(&mut *db.conn().await?, id).await?;
    let items: Vec<Value> = items.iter().map(document_json).collect();
    Ok(Json(json!({ "items": items })))
}

async fn get_document(State(db): State<Db>, Path(id): Path<String>) -> ApiResult<Value> {
    let document = documents::get(&mut *db.conn().await?, parse_id(&id)?)
        .await?
        .ok_or(nexus_core::Error::NotFound)?;
    Ok(Json(document_json(&document)))
}

async fn download_document(
    State(db): State<Db>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let document = documents::get(&mut *db.conn().await?, parse_id(&id)?)
        .await?
        .ok_or(nexus_core::Error::NotFound)?;
    let file_missing = ApiError(StatusCode::GONE, "file_missing");
    let (BlobState::Present, Some(sha256)) = (document.blob_state, document.blob_sha256.clone())
    else {
        return Err(file_missing);
    };
    let blobs = db.blobs().clone();
    let bytes = tokio::task::spawn_blocking(move || blobs.get(&sha256))
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"))?;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(nexus_core::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(file_missing);
        }
        Err(e) => return Err(e.into()),
    };
    let content_type = HeaderValue::from_str(&document.mime_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let disposition = HeaderValue::from_str(&content_disposition(&document.original_name))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'; sandbox"),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// `attachment` with an ASCII fallback name and the RFC 5987 UTF-8 name.
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | ';' | '%') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        crate::views::percent_encode(name)
    )
}
