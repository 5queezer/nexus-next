//! Upload links: `create_document_upload` mints a one-time URL, and a PUT or POST of the raw file
//! to it stores the document. The file never passes through the MCP transport as base64, and is
//! streamed to disk rather than held in memory.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use http_body_util::BodyExt;
use nexus_core::Timestamp;
use nexus_core::repo::documents as repo;
use rand::Rng;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use super::documents::{
    APP_NOT_FOUND, Declared, MAX_STORED_BYTES, SIGNATURE_MISMATCH, STORAGE_FULL, UPLOAD_FAILED,
    linked_applications, matches_signature, run_blocking, store,
};
use super::{Args, Ctx, ToolResult, json_result, req_str, text_error};
use crate::McpState;

pub const MAX_LINK_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
const TICKET_LIFE: TimeDelta = TimeDelta::minutes(15);
/// Each open link costs memory until it is used or expires.
const MAX_TICKETS: usize = 64;

struct Ticket {
    declared: Declared,
    expires_at: DateTime<Utc>,
}

/// Open upload links by token. Kept in memory only, so a restart ends them all.
#[derive(Default)]
pub(crate) struct Tickets(Mutex<HashMap<String, Ticket>>);

impl fmt::Debug for Tickets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tickets")
    }
}

impl Tickets {
    /// A new token for `declared`, and when it expires. `None` while too many links are open.
    fn mint(&self, declared: Declared, now: DateTime<Utc>) -> Option<(String, DateTime<Utc>)> {
        let mut tickets = self.0.lock().expect("tickets lock");
        tickets.retain(|_, ticket| ticket.expires_at > now);
        if tickets.len() >= MAX_TICKETS {
            return None;
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let expires_at = now + TICKET_LIFE;
        tickets.insert(
            token.clone(),
            Ticket {
                declared,
                expires_at,
            },
        );
        Some((token, expires_at))
    }

    /// Removes the ticket, so that a token works once at most.
    fn take(&self, token: &str, now: DateTime<Utc>) -> Option<Declared> {
        let ticket = self.0.lock().expect("tickets lock").remove(token)?;
        (ticket.expires_at > now).then_some(ticket.declared)
    }
}

pub async fn create(ctx: &Ctx, args: &Args) -> ToolResult {
    let Some(application_ids) = linked_applications(ctx, args).await? else {
        return Ok(text_error(APP_NOT_FOUND));
    };
    let declared = Declared {
        filename: req_str(args, "filename").to_owned(),
        mime_type: req_str(args, "mimeType").to_owned(),
        application_ids,
    };
    let Some((token, expires_at)) = ctx.state.upload_tickets.mint(declared, Utc::now()) else {
        return Ok(text_error(
            "Too many open upload links. Use one, or wait until they expire.",
        ));
    };
    let url = format!("{}/api/uploads/{token}", ctx.state.public_base_url);
    Ok(json_result(
        json!({
            "uploadUrl": url,
            "method": "PUT",
            "expiresAt": Timestamp::from_datetime(expires_at).as_str(),
            "maxBytes": MAX_LINK_UPLOAD_BYTES,
            "example": format!("curl -fsS -T <file> '{url}'"),
        }),
        false,
    ))
}

/// Serves `PUT` and `POST /api/uploads/{token}`. The token is the credential, so this needs no
/// auth layer.
pub fn router(state: McpState) -> Router {
    Router::new()
        .route("/api/uploads/{token}", put(receive).post(receive))
        .with_state(Arc::new(state))
}

/// Removes the temp file however the request ends, a deadline that drops it included.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn refuse(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

fn too_large() -> Response {
    refuse(StatusCode::PAYLOAD_TOO_LARGE, "File too large (max 50 MB)")
}

async fn receive(
    State(state): State<Arc<McpState>>,
    Path(token): Path<String>,
    mut body: Body,
) -> Response {
    let Some(declared) = state.upload_tickets.take(&token, Utc::now()) else {
        return refuse(
            StatusCode::NOT_FOUND,
            "Upload link not found, used or expired",
        );
    };
    if body.size_hint().lower() > MAX_LINK_UPLOAD_BYTES {
        return too_large();
    }
    let blobs = state.db.blobs().clone();
    let Ok((path, file)) = run_blocking(move || blobs.create_temp()).await else {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, UPLOAD_FAILED);
    };
    let temp = TempFile(path);
    let mut file = tokio::fs::File::from_std(file);
    let (mut hasher, mut size, mut head) = (Sha256::new(), 0, Vec::new());
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return refuse(StatusCode::BAD_REQUEST, "The upload broke off");
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        size += data.len() as u64;
        if size > MAX_LINK_UPLOAD_BYTES {
            return too_large();
        }
        head.extend_from_slice(&data[..data.len().min(16 - head.len())]);
        hasher.update(&data);
        if file.write_all(&data).await.is_err() {
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, UPLOAD_FAILED);
        }
    }
    if size == 0 {
        return refuse(StatusCode::BAD_REQUEST, "The upload is empty");
    }
    if file.sync_all().await.is_err() {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, UPLOAD_FAILED);
    }
    drop(file);
    if !matches_signature(&head, &declared.mime_type) {
        return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, SIGNATURE_MISMATCH);
    }
    let size = size as i64;
    let stored = match state.db.conn().await {
        Ok(mut conn) => repo::total_size(&mut conn).await,
        Err(error) => Err(error),
    };
    match stored {
        Ok(stored) if stored + size > MAX_STORED_BYTES => {
            return refuse(StatusCode::INSUFFICIENT_STORAGE, STORAGE_FULL);
        }
        Ok(_) => {}
        Err(_) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, UPLOAD_FAILED),
    }
    let sha256 = format!("{:x}", hasher.finalize());
    let put = || {
        let (blobs, path, sha256) = (state.db.blobs().clone(), temp.0.clone(), sha256.clone());
        run_blocking(move || blobs.put_file(&path, &sha256).map(|()| sha256))
    };
    let Some(document) = store(&state.db, declared, size, sha256.clone(), put).await else {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, UPLOAD_FAILED);
    };
    let ctx = Ctx {
        state,
        can_access_submissions: true,
        source: "mcp",
        client_gone: CancellationToken::new(),
    };
    (StatusCode::CREATED, axum::Json(ctx.doc_json(&document))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Declared {
        Declared {
            filename: "cv.pdf".to_owned(),
            mime_type: "application/pdf".to_owned(),
            application_ids: Vec::new(),
        }
    }

    #[test]
    fn a_ticket_works_once_and_not_after_it_expires() {
        let tickets = Tickets::default();
        let now = Utc::now();
        let (token, expires_at) = tickets.mint(declared(), now).unwrap();
        assert_eq!(expires_at, now + TICKET_LIFE);
        assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 32);
        assert!(tickets.take(&token, now).is_some());
        assert!(tickets.take(&token, now).is_none());

        let (token, _) = tickets.mint(declared(), now).unwrap();
        assert!(tickets.take(&token, now + TICKET_LIFE).is_none());
    }

    #[test]
    fn minting_prunes_expired_tickets_and_caps_open_ones() {
        let tickets = Tickets::default();
        let now = Utc::now();
        for _ in 0..MAX_TICKETS {
            tickets.mint(declared(), now).unwrap();
        }
        assert!(tickets.mint(declared(), now).is_none());
        assert!(tickets.mint(declared(), now + TICKET_LIFE).is_some());
        assert_eq!(tickets.0.lock().unwrap().len(), 1);
    }
}
