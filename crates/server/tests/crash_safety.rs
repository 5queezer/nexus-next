//! `kill -9` during sustained MCP writes, with a ledger of what was acknowledged. Slow, so ignored:
//!
//! ```sh
//! cargo test --release -p nexus --test crash_safety -- --ignored --nocapture
//! ```
//!
//! `QA_KILLS` sets the number of kills (default 25). Put `TMPDIR` on a real disk to catch kills
//! inside file writes: on tmpfs those windows are too short to hit.

mod support;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use support::{Agent, Node, jitter};

const WRITERS: usize = 6;

#[derive(Clone)]
struct Write {
    kind: &'static str,
    key: String,
    tool: &'static str,
    args: Value,
    result: Option<Value>,
}

fn next_write(n: u64, writer: usize, home: &str, seed: &mut u64, run: &str) -> Write {
    let key = format!("{run}-{writer}-{n}");
    let now = "2026-11-06T10:00:00.000Z";
    let url = format!("https://jobs.example.invalid/{key}");
    let upload = |bytes: Vec<u8>, name: &str| {
        json!({
            "filename": name,
            "mimeType": "application/pdf",
            "contentBase64": STANDARD.encode(bytes),
            "applicationIds": [home],
        })
    };
    let (kind, tool, args) = match n % 8 {
        0..=2 => (
            "event",
            "record_application_event",
            json!({"applicationId": home, "type": "note_added", "idempotencyKey": key,
                   "occurredAt": now, "metadata": {"note": key}}),
        ),
        3 => (
            "note",
            "append_application_note",
            json!({"applicationId": home, "note": key, "occurredAt": now, "idempotencyKey": key}),
        ),
        4 => (
            "upsert",
            "upsert_application_by_job_url",
            json!({"company": run, "role": key, "jobUrl": url}),
        ),
        5 => {
            let items: Vec<Value> = (0..10)
                .map(|i| json!({"company": run, "role": "batch", "jobUrl": format!("{url}/{i}")}))
                .collect();
            (
                "batch",
                "batch_upsert_applications",
                json!({"items": items}),
            )
        }
        6 => {
            let size = 1000 + jitter(seed, 2_000_000) as usize;
            let mut bytes = b"%PDF-1.7\n".to_vec();
            bytes.extend(key.as_bytes());
            bytes.resize(size, b'x');
            ("upload", "upload_document_content", upload(bytes, &key))
        }
        _ => {
            let shared = b"%PDF-1.7 the same bytes from many writers".to_vec();
            ("upload", "upload_document_content", upload(shared, &key))
        }
    };
    Write {
        kind,
        key,
        tool,
        args,
        result: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: see the module docs"]
async fn every_acknowledged_write_survives_kill_9() {
    let kills: u32 = std::env::var("QA_KILLS").map_or(25, |v| v.parse().unwrap());
    let root = tempfile::tempdir().unwrap();
    let (mut node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    let run = "crash";
    let mut homes = Vec::new();
    for writer in 0..WRITERS {
        let created = agent
            .ok(
                "create_application",
                json!({"company": format!("Home {writer}"), "role": "R"}),
            )
            .await;
        homes.push(created["id"].as_str().unwrap().to_owned());
    }

    let ledger: Arc<Mutex<Vec<Write>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let counter = Arc::new(AtomicU64::new(0));
    let mut writers = Vec::new();
    for (writer, home) in homes.iter().cloned().enumerate() {
        let (agent, ledger, stop, counter) =
            (agent.clone(), ledger.clone(), stop.clone(), counter.clone());
        writers.push(tokio::spawn(async move {
            let mut seed = 0x9E37_79B9_7F4A_7C15u64 + writer as u64;
            while !stop.load(Ordering::Relaxed) {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let mut write = next_write(n, writer, &home, &mut seed, run);
                match agent.call(write.tool, write.args.clone()).await {
                    Ok(reply) => {
                        assert!(!reply.is_error, "{}: {}", write.tool, reply.text);
                        write.result = Some(reply.json());
                    }
                    // The server is down. Slow down until it is back.
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
                ledger.lock().unwrap().push(write);
            }
        }));
    }

    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut slowest_restart = Duration::ZERO;
    for kill in 0..kills {
        tokio::time::sleep(Duration::from_millis(150 + jitter(&mut seed, 1000))).await;
        node.kill9();
        if kill % 5 == 0 {
            node.assert_database_sound().await;
        }
        let restart = Instant::now();
        node.start().await;
        slowest_restart = slowest_restart.max(restart.elapsed());
    }
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.await.unwrap();
    }

    let mut ledger = std::mem::take(&mut *ledger.lock().unwrap());
    let acknowledged = ledger.iter().filter(|w| w.result.is_some()).count();
    let mut conn = node.audit_connection().await;
    let keys: HashSet<String> = sqlx::query_scalar(
        "SELECT idempotency_key FROM application_events WHERE idempotency_key IS NOT NULL",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let urls: HashSet<String> = sqlx::query_scalar(
        "SELECT canonical_job_url FROM applications WHERE canonical_job_url IS NOT NULL",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let documents: HashSet<i64> = sqlx::query_scalar("SELECT id FROM documents")
        .fetch_all(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .collect();
    for write in ledger.iter().filter(|w| w.result.is_some()) {
        let result = write.result.as_ref().unwrap();
        let present = match write.kind {
            "event" | "note" => keys.contains(&write.key),
            "upsert" => urls.contains(write.args["jobUrl"].as_str().unwrap()),
            "batch" => {
                result["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item.get("error").is_none())
                    && write.args["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|item| urls.contains(item["jobUrl"].as_str().unwrap()))
            }
            _ => documents.contains(&result["id"].as_str().unwrap().parse().unwrap()),
        };
        assert!(present, "acknowledged {} {} is gone", write.kind, write.key);
    }

    // An agent that never heard back retries with the same key or URL, and gets one row.
    let mut retried = 0;
    for write in ledger.iter_mut().filter(|w| w.result.is_none()) {
        if matches!(write.kind, "event" | "note" | "upsert") {
            write.result = Some(agent.ok(write.tool, write.args.clone()).await);
            retried += 1;
        }
    }
    for (table, column) in [
        ("application_events", "idempotency_key"),
        ("applications", "canonical_job_url"),
    ] {
        let duplicates = node
            .count(&format!(
                "SELECT count(*) FROM (SELECT {column} FROM {table} WHERE {column} IS NOT NULL
                 GROUP BY {column} HAVING count(*) > 1)"
            ))
            .await;
        assert_eq!(duplicates, 0, "duplicates in {table}");
    }
    let expected_events = ledger
        .iter()
        .filter(|w| matches!(w.kind, "event" | "note") && w.result.is_some())
        .count() as i64;
    let events = node
        .count("SELECT count(*) FROM application_events WHERE idempotency_key LIKE 'crash-%'")
        .await;
    assert_eq!(events, expected_events);

    node.terminate();
    node.assert_database_sound().await;
    let (blobs, orphans, temp_files) = node.assert_blobs_sound().await;
    node.start().await;
    let (_, _, swept) = node.assert_blobs_sound().await;
    assert_eq!(swept, 0, "a restart removes temp files of killed writes");
    node.terminate();
    let logs = node.logs();
    assert!(!logs.to_lowercase().contains("panic"));
    println!(
        "{kills} kills, {} writes, {acknowledged} acknowledged, {retried} retried, slowest restart \
         {slowest_restart:?}, {blobs} blobs ({orphans} unreferenced), {temp_files} temp files before restart",
        ledger.len()
    );
}
