use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use sqlx::{QueryBuilder, SqliteConnection};

use super::{ApplicationRef, id_string, opt_id_string, partial_fields};
use crate::error::{Error, Result};
use crate::time::Timestamp;

/// Whether a document's file exists. A `Missing` document's file was lost before migration, so
/// reads of its content must fail clearly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum BlobState {
    Present,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    #[serde(serialize_with = "id_string")]
    pub id: i64,
    pub filename: String,
    pub original_name: String,
    pub size: i64,
    pub mime_type: String,
    pub document_type: String,
    pub state: String,
    pub version: i64,
    /// Client-visible metadata that `update_document_metadata` can change. Locate the file with
    /// `blob_sha256`, never with this.
    pub content_hash: Option<String>,
    pub source: Option<String>,
    pub generated_at: Option<Timestamp>,
    pub submitted_at: Option<Timestamp>,
    #[serde(serialize_with = "opt_id_string")]
    pub submission_id: Option<i64>,
    pub uploaded_at: Timestamp,
    #[serde(skip)]
    pub blob_sha256: Option<String>,
    #[serde(skip)]
    pub blob_state: BlobState,
    /// `gcs` or `recovered-hermes` for imported files, NULL for files uploaded to this server.
    #[serde(skip)]
    pub blob_source: Option<String>,
    /// True when `content_hash` came from hashing the file at import, not from old Nexus.
    #[serde(skip)]
    pub content_hash_computed: bool,
    #[sqlx(skip)]
    pub applications: Vec<ApplicationRef>,
}

impl Document {
    /// Port of `isSubmissionDocument`: these documents are immutable.
    pub fn is_submission_document(&self) -> bool {
        self.submission_id.is_some() || self.state == "submitted" || self.state == "historical"
    }
}

async fn fill_applications(conn: &mut SqliteConnection, documents: &mut [Document]) -> Result<()> {
    if documents.is_empty() {
        return Ok(());
    }
    let mut qb = QueryBuilder::new(
        "SELECT l.document_id, a.id, a.company, a.role
         FROM application_documents l JOIN applications a ON a.id = l.application_id
         WHERE l.document_id IN (",
    );
    let mut list = qb.separated(", ");
    for document in documents.iter() {
        list.push_bind(document.id);
    }
    qb.push(") ORDER BY a.id");
    let rows: Vec<(i64, i64, String, String)> = qb.build_query_as().fetch_all(conn).await?;
    let mut parents: HashMap<i64, Vec<ApplicationRef>> = HashMap::new();
    for (document_id, id, company, role) in rows {
        parents
            .entry(document_id)
            .or_default()
            .push(ApplicationRef { id, company, role });
    }
    for document in documents {
        document.applications = parents.remove(&document.id).unwrap_or_default();
    }
    Ok(())
}

pub async fn get(conn: &mut SqliteConnection, id: i64) -> Result<Option<Document>> {
    let row: Option<Document> = sqlx::query_as("SELECT * FROM documents WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let mut rows = [row];
    fill_applications(conn, &mut rows).await?;
    let [row] = rows;
    Ok(Some(row))
}

async fn required(conn: &mut SqliteConnection, id: i64) -> Result<Document> {
    get(conn, id).await?.ok_or(Error::NotFound)
}

pub async fn list(conn: &mut SqliteConnection) -> Result<Vec<Document>> {
    fetch(conn, &DocumentFilter::default(), -1, 0).await
}

pub async fn list_for_application(
    conn: &mut SqliteConnection,
    application_id: i64,
) -> Result<Vec<Document>> {
    let filter = DocumentFilter {
        application_id: Some(application_id),
        ..Default::default()
    };
    fetch(conn, &filter, -1, 0).await
}

pub async fn list_for_submission(
    conn: &mut SqliteConnection,
    submission_id: i64,
) -> Result<Vec<Document>> {
    let filter = DocumentFilter {
        submission_id: Some(submission_id),
        ..Default::default()
    };
    fetch(conn, &filter, -1, 0).await
}

#[derive(Debug, Clone, Default)]
pub struct DocumentFilter {
    pub application_id: Option<i64>,
    pub document_type: Option<String>,
    pub state: Option<String>,
    pub submission_id: Option<i64>,
    pub orphaned: Option<bool>,
    /// Overrides `state` and `submission_id`, as in old Nexus.
    pub exclude_submission_artifacts: bool,
    pub limit: Option<i64>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// Port of `listDocumentsFiltered`: newest upload first. `page_size` (else `limit`, else 50) is
/// clamped to 1..=200.
pub async fn list_filtered(
    conn: &mut SqliteConnection,
    filter: &DocumentFilter,
) -> Result<Vec<Document>> {
    let page_size = filter
        .page_size
        .or(filter.limit)
        .unwrap_or(50)
        .clamp(1, 200);
    let offset = (filter.page.unwrap_or(1).max(1) - 1).saturating_mul(page_size);
    fetch(conn, filter, page_size, offset).await
}

async fn fetch(
    conn: &mut SqliteConnection,
    filter: &DocumentFilter,
    limit: i64,
    offset: i64,
) -> Result<Vec<Document>> {
    let mut qb = QueryBuilder::new("SELECT * FROM documents d WHERE 1 = 1");
    if let Some(document_type) = &filter.document_type {
        qb.push(" AND d.document_type = ")
            .push_bind(document_type.clone());
    }
    if filter.exclude_submission_artifacts {
        qb.push(" AND d.submission_id IS NULL AND d.state NOT IN ('submitted', 'historical')");
    } else {
        if let Some(state) = &filter.state {
            qb.push(" AND d.state = ").push_bind(state.clone());
        }
        if let Some(submission_id) = filter.submission_id {
            qb.push(" AND d.submission_id = ").push_bind(submission_id);
        }
    }
    if let Some(application_id) = filter.application_id {
        qb.push(
            " AND d.id IN (SELECT document_id FROM application_documents WHERE application_id = ",
        )
        .push_bind(application_id)
        .push(")");
    }
    match filter.orphaned {
        Some(true) => {
            qb.push(" AND d.id NOT IN (SELECT document_id FROM application_documents)");
        }
        Some(false) => {
            qb.push(" AND d.id IN (SELECT document_id FROM application_documents)");
        }
        None => {}
    }
    qb.push(" ORDER BY d.uploaded_at DESC, d.id DESC LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    let mut rows: Vec<Document> = qb.build_query_as().fetch_all(&mut *conn).await?;
    fill_applications(conn, &mut rows).await?;
    Ok(rows)
}

async fn all_applications_exist(conn: &mut SqliteConnection, ids: &BTreeSet<i64>) -> Result<bool> {
    if ids.is_empty() {
        return Ok(true);
    }
    let mut qb = QueryBuilder::new("SELECT count(*) FROM applications WHERE id IN (");
    let mut list = qb.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    qb.push(")");
    let found: i64 = qb.build_query_scalar().fetch_one(conn).await?;
    Ok(found as usize == ids.len())
}

async fn replace_links(
    conn: &mut SqliteConnection,
    document_id: i64,
    application_ids: &BTreeSet<i64>,
) -> Result<()> {
    sqlx::query("DELETE FROM application_documents WHERE document_id = ?")
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    for application_id in application_ids {
        sqlx::query(
            "INSERT INTO application_documents (application_id, document_id) VALUES (?, ?)",
        )
        .bind(application_id)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A document uploaded to this server; `blob_sha256` is where `BlobStore::put` stored the file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewDocument {
    pub filename: String,
    pub original_name: String,
    pub size: i64,
    pub mime_type: String,
    pub application_ids: Vec<i64>,
    pub document_type: Option<String>,
    pub state: Option<String>,
    pub version: Option<i64>,
    pub content_hash: Option<String>,
    pub source: Option<String>,
    pub generated_at: Option<Timestamp>,
    pub submitted_at: Option<Timestamp>,
    pub blob_sha256: String,
}

/// Port of `createDocument`. Run inside `Db::begin_write`.
pub async fn create(conn: &mut SqliteConnection, new: &NewDocument) -> Result<Document> {
    if new.state.as_deref() == Some("submitted") {
        return Err(Error::SubmittedStateReserved);
    }
    let application_ids: BTreeSet<i64> = new.application_ids.iter().copied().collect();
    if !all_applications_exist(&mut *conn, &application_ids).await? {
        return Err(Error::InvalidApplications);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (filename, original_name, size, mime_type, document_type, state,
             version, content_hash, source, generated_at, submitted_at, blob_sha256, blob_state)
         VALUES (?, ?, ?, ?, COALESCE(?, 'other'), COALESCE(?, 'current'), COALESCE(?, 1),
             ?, ?, ?, ?, ?, 'present')
         RETURNING id",
    )
    .bind(&new.filename)
    .bind(&new.original_name)
    .bind(new.size)
    .bind(&new.mime_type)
    .bind(&new.document_type)
    .bind(&new.state)
    .bind(new.version)
    .bind(&new.content_hash)
    .bind(&new.source)
    .bind(&new.generated_at)
    .bind(&new.submitted_at)
    .bind(&new.blob_sha256)
    .fetch_one(&mut *conn)
    .await?;
    replace_links(&mut *conn, id, &application_ids).await?;
    required(conn, id).await
}

partial_fields! {
    DocumentPatch {
        document_type: String,
        state: String,
        version: i64,
        content_hash: Option<String>,
        source: Option<String>,
        generated_at: Option<Timestamp>,
        submitted_at: Option<Timestamp>,
    }
}

/// Port of `updateDocumentMetadata`. A submission document only allows a no-op state write or
/// `submitted` to `historical`. A client-written `content_hash` clears `content_hash_computed`.
/// Run inside `Db::begin_write`.
pub async fn update_metadata(
    conn: &mut SqliteConnection,
    id: i64,
    patch: &DocumentPatch,
) -> Result<Document> {
    let existing = required(&mut *conn, id).await?;
    if existing.is_submission_document() {
        let state_only = *patch
            == DocumentPatch {
                state: patch.state.clone(),
                ..Default::default()
            };
        let allowed = patch.state.as_ref() == Some(&existing.state)
            || (existing.state == "submitted" && patch.state.as_deref() == Some("historical"));
        if !state_only || !allowed {
            return Err(Error::SubmittedDocumentImmutable);
        }
    } else if patch.state.as_deref() == Some("submitted") {
        return Err(Error::SubmittedStateReserved);
    }
    let mut qb = QueryBuilder::new("UPDATE documents SET id = id");
    patch.push_assignments(&mut qb);
    if patch.content_hash.is_some() {
        qb.push(", content_hash_computed = 0");
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.build().execute(&mut *conn).await?;
    required(conn, id).await
}

/// Port of `updateDocumentLinks`. Run inside `Db::begin_write`.
pub async fn set_links(
    conn: &mut SqliteConnection,
    id: i64,
    application_ids: &[i64],
) -> Result<Document> {
    if required(&mut *conn, id).await?.is_submission_document() {
        return Err(Error::SubmittedDocumentImmutable);
    }
    let application_ids: BTreeSet<i64> = application_ids.iter().copied().collect();
    if !all_applications_exist(&mut *conn, &application_ids).await? {
        return Err(Error::InvalidApplications);
    }
    replace_links(&mut *conn, id, &application_ids).await?;
    required(conn, id).await
}

/// Port of `renameDocument`: `None` when the document does not exist.
pub async fn rename(
    conn: &mut SqliteConnection,
    id: i64,
    new_name: &str,
) -> Result<Option<Document>> {
    let Some(existing) = get(&mut *conn, id).await? else {
        return Ok(None);
    };
    if existing.is_submission_document() {
        return Err(Error::SubmittedDocumentImmutable);
    }
    sqlx::query("UPDATE documents SET original_name = ? WHERE id = ?")
        .bind(new_name)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    get(conn, id).await
}

/// Port of `deleteDocument`: returns the deleted row, or `None` when it does not exist. The caller
/// removes the blob afterwards if [`blob_is_referenced`] says nothing else uses it, with the check
/// and the unlink inside one `Db::begin_write`, or an upload of the same content can lose its file.
/// Run inside `Db::begin_write`.
pub async fn delete(conn: &mut SqliteConnection, id: i64) -> Result<Option<Document>> {
    let Some(existing) = get(&mut *conn, id).await? else {
        return Ok(None);
    };
    if existing.is_submission_document() {
        return Err(Error::SubmittedDocumentImmutable);
    }
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(Some(existing))
}

/// Blobs are content-addressed, so two documents can share one.
pub async fn blob_is_referenced(conn: &mut SqliteConnection, blob_sha256: &str) -> Result<bool> {
    let row: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM documents WHERE blob_sha256 = ? LIMIT 1")
            .bind(blob_sha256)
            .fetch_optional(conn)
            .await?;
    Ok(row.is_some())
}

/// Port of the claim step in `recordApplicationSubmission`: atomically marks every listed document
/// as submitted, or fails if any is already claimed or missing.
pub async fn claim_for_submission(
    conn: &mut SqliteConnection,
    ids: &[i64],
    submission_id: i64,
    submitted_at: &Timestamp,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut qb = QueryBuilder::new("UPDATE documents SET state = 'submitted', submitted_at = ");
    qb.push_bind(submitted_at.clone());
    qb.push(", submission_id = ").push_bind(submission_id);
    qb.push(
        " WHERE submission_id IS NULL AND state NOT IN ('submitted', 'historical') AND id IN (",
    );
    let mut list = qb.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    qb.push(")");
    let done = qb.build().execute(conn).await?;
    if done.rows_affected() as usize == ids.len() {
        Ok(())
    } else {
        Err(Error::DocumentAlreadySubmitted)
    }
}
