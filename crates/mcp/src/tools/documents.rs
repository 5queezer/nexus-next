use std::sync::Arc;

use base64::Engine;
use base64::alphabet::STANDARD;
use base64::engine::general_purpose::STANDARD as ENCODER;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use nexus_core::blob::sha256_hex;
use nexus_core::repo::documents::{
    self as repo, BlobState, Document, DocumentFilter, DocumentPatch, NewDocument,
};
use nexus_core::{Db, Timestamp};
use serde_json::{Map, Value, json};

use super::{
    Args, Ctx, Thrown, ToolResult, find_application, json_result, req_str, str_arg, text_error,
    text_result,
};
use crate::js::{nid, parse_date, slice16};

const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = MAX_UPLOAD_BYTES.div_ceil(3) * 4 + 4;
const NOT_FOUND: &str = "Document not found or access denied";
const APP_NOT_FOUND: &str = "Application not found or access denied";

fn requires_scope(document: &Document, requested_state: Option<&str>) -> bool {
    document.is_submission_document() || matches!(requested_state, Some("submitted" | "historical"))
}

pub async fn list(ctx: &Ctx, args: &Args) -> ToolResult {
    let id_filter = |key| {
        str_arg(args, key)
            .filter(|id| !id.is_empty())
            .map(nid)
            .transpose()
    };
    let filter = DocumentFilter {
        application_id: id_filter("applicationId")?,
        document_type: str_arg(args, "documentType")
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
        state: str_arg(args, "state").map(str::to_owned),
        submission_id: id_filter("submissionId")?,
        orphaned: args.get("orphaned").and_then(Value::as_bool),
        exclude_submission_artifacts: !ctx.can_access_submissions,
        limit: None,
        page: args.get("page").and_then(Value::as_i64),
        page_size: args.get("pageSize").and_then(Value::as_i64),
    };
    let mut conn = ctx.state.db.conn().await?;
    let documents = repo::list_filtered(&mut conn, &filter).await?;
    let fields: Vec<&str> = args
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let values: Vec<Value> = documents
        .iter()
        .map(|document| {
            let value = ctx.doc_json(document);
            if fields.is_empty() {
                return value;
            }
            let mut selected = Map::new();
            selected.insert("id".to_owned(), value["id"].clone());
            for field in &fields {
                if let Some(item) = value.get(*field) {
                    selected.insert((*field).to_owned(), item.clone());
                }
            }
            Value::Object(selected)
        })
        .collect();
    Ok(json_result(Value::Array(values), false))
}

async fn visible(ctx: &Ctx, id: &str) -> Result<Option<Document>, Thrown> {
    let id = nid(id)?;
    let mut conn = ctx.state.db.conn().await?;
    Ok(repo::get(&mut conn, id).await?)
}

pub async fn get(ctx: &Ctx, args: &Args) -> ToolResult {
    let Some(document) = visible(ctx, req_str(args, "id")).await? else {
        return Ok(text_error("Document not found"));
    };
    if document.is_submission_document()
        && let Some(error) = ctx.require_submissions()
    {
        return Ok(error);
    }
    Ok(json_result(ctx.doc_json(&document), false))
}

/// Port of `decodeBase64Content`.
fn decode(content: &str) -> Result<Vec<u8>, &'static str> {
    let normalized: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    if normalized.len() > MAX_BASE64_BYTES {
        return Err("File too large (max 10 MB)");
    }
    let body = normalized.trim_end_matches('=');
    let padding = normalized.len() - body.len();
    let alphabet_ok = body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    if normalized.is_empty() || !alphabet_ok || padding > 2 {
        return Err("Invalid base64 content");
    }
    let lenient = GeneralPurpose::new(
        &STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    let decoded = lenient.decode(body).map_err(|_| "Invalid base64 content")?;
    if ENCODER.encode(&decoded).trim_end_matches('=') != body {
        return Err("Invalid base64 content");
    }
    Ok(decoded)
}

/// Port of `matchesMimeSignature`.
fn matches_signature(bytes: &[u8], mime_type: &str) -> bool {
    match mime_type {
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

async fn run_blocking<T: Send + 'static>(
    task: impl FnOnce() -> nexus_core::Result<T> + Send + 'static,
) -> nexus_core::Result<T> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|e| nexus_core::Error::Io(std::io::Error::other(e)))?
}

pub(super) async fn put_blob(db: &Db, bytes: Arc<Vec<u8>>) -> nexus_core::Result<String> {
    let blobs = db.blobs().clone();
    run_blocking(move || blobs.put(&bytes)).await
}

/// Holds the write lock across the check and the removal, so an upload of the same bytes can't
/// commit its row in between. `upload` restores the file under the same lock.
pub(super) async fn remove_blob_if_unreferenced(db: &Db, sha256: String) -> nexus_core::Result<()> {
    let mut tx = db.begin_write().await?;
    if repo::blob_is_referenced(&mut tx, &sha256).await? {
        return Ok(());
    }
    let blobs = db.blobs().clone();
    run_blocking(move || blobs.delete(&sha256)).await
}

pub async fn upload(ctx: &Ctx, args: &Args) -> ToolResult {
    let ids: Vec<&str> = args
        .get("applicationIds")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut application_ids = Vec::with_capacity(ids.len());
    {
        let mut conn = ctx.state.db.conn().await?;
        for id in ids {
            match find_application(&mut conn, id).await {
                Ok(Some(application)) => application_ids.push(application.id),
                _ => return Ok(text_error(APP_NOT_FOUND)),
            }
        }
    }
    let bytes = match decode(req_str(args, "contentBase64")) {
        Ok(bytes) => bytes,
        Err(message) => return Ok(text_error(message)),
    };
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Ok(text_error("File too large (max 10 MB)"));
    }
    let mime_type = req_str(args, "mimeType");
    if !matches_signature(&bytes, mime_type) {
        return Ok(text_error("File bytes do not match the declared MIME type"));
    }
    let extension = match mime_type {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        _ => ".pdf",
    };
    let new = NewDocument {
        filename: format!("{}{extension}", uuid::Uuid::new_v4()),
        original_name: slice16(req_str(args, "filename"), 255),
        size: bytes.len() as i64,
        mime_type: mime_type.to_owned(),
        application_ids,
        document_type: Some("other".to_owned()),
        state: Some("current".to_owned()),
        version: Some(1),
        content_hash: Some(sha256_hex(&bytes)),
        source: Some("upload".to_owned()),
        generated_at: None,
        submitted_at: None,
        blob_sha256: String::new(),
    };
    let bytes = Arc::new(bytes);
    let Ok(sha256) = put_blob(&ctx.state.db, bytes.clone()).await else {
        return Ok(text_error("Failed to upload document"));
    };
    let new = NewDocument {
        blob_sha256: sha256.clone(),
        ..new
    };
    let created = async {
        let mut tx = ctx.state.db.begin_write().await?;
        // A delete of the same bytes may have removed the file since the put above.
        if !ctx.state.db.blobs().exists(&sha256)? {
            put_blob(&ctx.state.db, bytes.clone()).await?;
        }
        let document = repo::create(&mut tx, &new).await?;
        tx.commit().await?;
        nexus_core::Result::Ok(document)
    };
    Ok(match created.await {
        Ok(document) => json_result(ctx.doc_json(&document), false),
        Err(_) => {
            let _ = remove_blob_if_unreferenced(&ctx.state.db, sha256).await;
            text_error("Failed to upload document")
        }
    })
}

pub async fn download(ctx: &Ctx, args: &Args) -> ToolResult {
    let Some(document) = visible(ctx, req_str(args, "id")).await? else {
        return Ok(text_error("Document not found"));
    };
    if document.is_submission_document()
        && let Some(error) = ctx.require_submissions()
    {
        return Ok(error);
    }
    const MISSING: &str = "Document file is missing from storage";
    let (BlobState::Present, Some(sha256)) = (document.blob_state, document.blob_sha256.clone())
    else {
        return Ok(text_error(MISSING));
    };
    let blobs = ctx.state.db.blobs().clone();
    let bytes = match run_blocking(move || blobs.get(&sha256)).await {
        Ok(bytes) => bytes,
        Err(nexus_core::Error::Io(_)) => return Ok(text_error(MISSING)),
        Err(error) => return Err(error.into()),
    };
    Ok(json_result(
        json!({
            "id": document.id.to_string(),
            "filename": document.original_name,
            "mimeType": document.mime_type,
            "size": document.size,
            "contentBase64": ENCODER.encode(bytes),
        }),
        false,
    ))
}

pub async fn update_links(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let existing = visible(ctx, req_str(args, "id")).await.ok()??;
        let mut conn = ctx.state.db.conn().await.ok()?;
        let mut application_ids = Vec::new();
        for id in args["applicationIds"].as_array()? {
            let application = find_application(&mut conn, id.as_str()?).await.ok()??;
            application_ids.push(application.id);
        }
        drop(conn);
        if requires_scope(&existing, None)
            && let Some(error) = ctx.require_submissions()
        {
            return Some(error);
        }
        let mut tx = ctx.state.db.begin_write().await.ok()?;
        let document = repo::set_links(&mut tx, existing.id, &application_ids)
            .await
            .ok()?;
        tx.commit().await.ok()?;
        Some(json_result(ctx.doc_json(&document), false))
    };
    Ok(run.await.unwrap_or_else(|| text_error(NOT_FOUND)))
}

fn date_patch(args: &Args, key: &str) -> Option<Option<Timestamp>> {
    args.get(key)
        .map(|value| value.as_str().and_then(parse_date))
}

pub async fn update_metadata(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let existing = visible(ctx, req_str(args, "id")).await.ok()??;
        if requires_scope(&existing, str_arg(args, "state"))
            && let Some(error) = ctx.require_submissions()
        {
            return Some(error);
        }
        let nullable = |key| {
            args.get(key)
                .map(|value: &Value| value.as_str().map(str::to_owned))
        };
        let patch = DocumentPatch {
            document_type: str_arg(args, "documentType").map(str::to_owned),
            state: str_arg(args, "state").map(str::to_owned),
            version: args.get("version").and_then(Value::as_i64),
            content_hash: nullable("contentHash"),
            source: nullable("source"),
            generated_at: date_patch(args, "generatedAt"),
            submitted_at: date_patch(args, "submittedAt"),
        };
        // The version column is 32 bits, so a larger value failed the write.
        if args.get("version").is_some_and(|version| {
            version
                .as_i64()
                .is_none_or(|version| i32::try_from(version).is_err())
        }) {
            return None;
        }
        let mut tx = ctx.state.db.begin_write().await.ok()?;
        let document = repo::update_metadata(&mut tx, existing.id, &patch)
            .await
            .ok()?;
        tx.commit().await.ok()?;
        Some(json_result(ctx.doc_json(&document), false))
    };
    Ok(run.await.unwrap_or_else(|| text_error(NOT_FOUND)))
}

pub async fn delete(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let existing = visible(ctx, req_str(args, "id")).await.ok()??;
        let mut tx = ctx.state.db.begin_write().await.ok()?;
        let deleted = repo::delete(&mut tx, existing.id).await.ok()??;
        tx.commit().await.ok()?;
        if let Some(sha256) = deleted.blob_sha256 {
            remove_blob_if_unreferenced(&ctx.state.db, sha256)
                .await
                .ok()?;
        }
        Some(text_result("Document deleted"))
    };
    Ok(run.await.unwrap_or_else(|| text_error(NOT_FOUND)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_reference_being_committed_keeps_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let sha256 = db.blobs().put(b"%PDF-1.7 synthetic").unwrap();
        let mut tx = db.begin_write().await.unwrap();
        let new = NewDocument {
            blob_sha256: sha256.clone(),
            ..Default::default()
        };
        repo::create(&mut tx, &new).await.unwrap();
        let commit = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.commit().await.unwrap();
        };
        let (removed, ()) = tokio::join!(remove_blob_if_unreferenced(&db, sha256.clone()), commit);
        removed.unwrap();
        assert!(db.blobs().exists(&sha256).unwrap());
        db.close().await;
    }

    #[test]
    fn base64_follows_the_old_decoder() {
        assert_eq!(decode("aGk=").unwrap(), b"hi");
        assert_eq!(decode("aGk").unwrap(), b"hi");
        assert_eq!(decode(" aG\nk= ").unwrap(), b"hi");
        for bad in ["", "   ", "a", "a$b=", "aGk===", "aGl="] {
            assert_eq!(decode(bad), Err("Invalid base64 content"), "{bad}");
        }
    }

    #[test]
    fn signatures_are_checked() {
        assert!(matches_signature(b"%PDF-1.7", "application/pdf"));
        assert!(!matches_signature(b"%PD", "application/pdf"));
        assert!(matches_signature(b"RIFF\0\0\0\0WEBP", "image/webp"));
        assert!(!matches_signature(b"RIFF\0\0\0\0WAVE", "image/webp"));
    }
}
