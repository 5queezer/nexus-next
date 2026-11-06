//! Uploads racing deletes of the same content. Blobs are shared by content, so each test holds
//! SQLite's write lock to stop an upload between storing its file and inserting its row, then does
//! there what the other side would do.

mod common;

use std::time::Duration;

use common::*;
use nexus_core::repo::{applications, documents};
use serde_json::{Value, json};

/// Long enough for the upload to pass `put` and block on the lock. If it were ever slower, `put`
/// would run after the unlink and the test would pass without exercising the race.
const UNTIL_BLOCKED: Duration = Duration::from_millis(300);

fn id_of(document: &Value) -> i64 {
    document["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn an_upload_survives_a_delete_of_its_content_after_its_put() {
    let server = Server::start().await;
    let first = server.pdf(json!([])).await;
    let sha = server.db.blobs().list().unwrap().remove(0);

    let mut lock = server.db.begin_write().await.unwrap();
    let last_reference_goes = async {
        tokio::time::sleep(UNTIL_BLOCKED).await;
        documents::delete(&mut lock, id_of(&first)).await.unwrap();
        server.db.blobs().delete(&sha).unwrap();
        lock.commit().await.unwrap();
    };
    let (uploaded, ()) = tokio::join!(server.pdf(json!([])), last_reference_goes);

    assert_eq!(server.db.blobs().list().unwrap(), [sha]);
    assert_eq!(
        server
            .call("download_document_content", json!({"id": uploaded["id"]}))
            .await
            .ok()["size"],
        18
    );
}

/// Stops an upload at the lock, removes its application so inserting the row fails, and returns the
/// error it reports.
async fn fail_an_upload(server: &Server) -> String {
    let application = server.application("Vanishing Co").await;
    let mut lock = server.db.begin_write().await.unwrap();
    let application_goes = async {
        tokio::time::sleep(UNTIL_BLOCKED).await;
        applications::delete(&mut lock, application.parse().unwrap())
            .await
            .unwrap();
        lock.commit().await.unwrap();
    };
    let upload = server.call(
        "upload_document_content",
        json!({
            "filename": "cv.pdf",
            "mimeType": "application/pdf",
            "contentBase64": "JVBERi0xLjcgc3ludGhldGlj",
            "applicationIds": [application],
        }),
    );
    let (failed, ()) = tokio::join!(upload, application_goes);
    failed.err_text().to_owned()
}

#[tokio::test]
async fn a_failed_upload_removes_its_own_file() {
    let server = Server::start().await;
    assert_eq!(fail_an_upload(&server).await, "Failed to upload document");
    assert!(server.db.blobs().list().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_upload_keeps_a_file_another_document_uses() {
    let server = Server::start().await;
    let twin = server.pdf(json!([])).await;
    assert_eq!(fail_an_upload(&server).await, "Failed to upload document");
    assert_eq!(
        server
            .call("download_document_content", json!({"id": twin["id"]}))
            .await
            .ok()["size"],
        18
    );
}
