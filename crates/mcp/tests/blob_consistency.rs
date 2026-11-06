//! Blob and row consistency: blobs are content-addressed and shared, and uploads write the file
//! before the row, so every order of interruption and race must leave rows pointing at real files.

mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::Server;
use nexus_core::blob::sha256_hex;
use serde_json::{Value, json};

fn pdf(label: &str) -> Vec<u8> {
    format!("%PDF-1.7 synthetic {label}").into_bytes()
}

async fn upload(server: &Server, name: &str, bytes: &[u8]) -> Value {
    server
        .call(
            "upload_document_content",
            json!({
                "filename": name,
                "mimeType": "application/pdf",
                "contentBase64": STANDARD.encode(bytes),
            }),
        )
        .await
        .ok()
}

async fn downloaded(server: &Server, id: &Value) -> Result<Vec<u8>, String> {
    let reply = server
        .call("download_document_content", json!({"id": id}))
        .await;
    if reply.is_error {
        return Err(reply.text);
    }
    Ok(STANDARD
        .decode(reply.json()["contentBase64"].as_str().unwrap())
        .unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upload_racing_the_delete_of_the_same_bytes_keeps_its_file() {
    let server = Server::start().await;
    for round in 0..300 {
        let bytes = pdf(&format!("race {round}"));
        let first = upload(&server, "first.pdf", &bytes).await;
        let (deleted, second) = tokio::join!(
            server.call("delete_document", json!({"id": first["id"]})),
            upload(&server, "second.pdf", &bytes),
        );
        assert!(!deleted.is_error, "{}", deleted.text);
        assert_eq!(
            downloaded(&server, &second["id"]).await,
            Ok(bytes),
            "round {round}: the acknowledged upload lost its file"
        );
    }
}

#[tokio::test]
async fn deleting_a_document_keeps_a_blob_another_one_shares() {
    let server = Server::start().await;
    let bytes = pdf("shared");
    let sha256 = sha256_hex(&bytes);
    let first = upload(&server, "first.pdf", &bytes).await;
    let second = upload(&server, "second.pdf", &bytes).await;
    assert_eq!(server.db.blobs().list().unwrap(), [sha256.clone()]);

    let reply = server
        .call("delete_document", json!({"id": first["id"]}))
        .await;
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(downloaded(&server, &second["id"]).await, Ok(bytes));

    let reply = server
        .call("delete_document", json!({"id": second["id"]}))
        .await;
    assert!(!reply.is_error, "{}", reply.text);
    assert!(server.db.blobs().list().unwrap().is_empty());
}

#[tokio::test]
async fn an_upload_interrupted_before_its_row_leaves_nothing_visible_and_a_retry_works() {
    let server = Server::start().await;
    let bytes = pdf("interrupted");
    // What a crash leaves behind: the blob without a row, and an unfinished temp file.
    let sha256 = server.db.blobs().put(&bytes).unwrap();
    let blobs_dir = server.dir.path().join("blobs");
    std::fs::write(blobs_dir.join(".tmp-1-1"), b"%PDF-half").unwrap();

    let listed = server.call("list_documents", json!({})).await.ok();
    assert_eq!(listed, json!([]));
    assert_eq!(server.db.blobs().list().unwrap(), [sha256.clone()]);

    let retried = upload(&server, "retried.pdf", &bytes).await;
    assert_eq!(retried["contentHash"], json!(sha256));
    assert_eq!(downloaded(&server, &retried["id"]).await, Ok(bytes));
}

#[tokio::test]
async fn a_damaged_blob_is_an_error_never_wrong_bytes_and_a_new_upload_repairs_it() {
    let server = Server::start().await;
    let bytes = pdf("damaged");
    let document = upload(&server, "damaged.pdf", &bytes).await;
    let path = server.dir.path().join("blobs").join(sha256_hex(&bytes));
    std::fs::write(&path, b"%PDF-half").unwrap();
    assert!(downloaded(&server, &document["id"]).await.is_err());

    upload(&server, "again.pdf", &bytes).await;
    assert_eq!(downloaded(&server, &document["id"]).await, Ok(bytes));
}
