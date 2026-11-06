//! Parallel agents against the real binary: races with asserted invariants, then a mixed load with
//! latency, memory, WAL and a backup taken under writes. Slow, so ignored:
//!
//! ```sh
//! cargo test --release -p nexus --test concurrency -- --ignored --nocapture
//! ```
//!
//! `QA_SECONDS` sets the length of the mixed load (default 45).

mod support;

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use age::secrecy::ExposeSecret;
use age::x25519;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use support::{Agent, Node, good_policy, jitter, nexus};

const AGENTS: usize = 10;
const NOW: &str = "2026-11-06T10:00:00.000Z";

fn pdf_args(label: &str, padding: usize) -> Value {
    json!({
        "filename": format!("{label}.pdf"),
        "mimeType": "application/pdf",
        "contentBase64": STANDARD.encode(format!("%PDF-1.7 {label} {}", "x".repeat(padding))),
    })
}

async fn from_all(
    agents: &[Agent],
    tool: &'static str,
    args: impl Fn(usize) -> Value,
) -> Vec<support::Reply> {
    let calls: Vec<_> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let (agent, args) = (agent.clone(), args(i));
            tokio::spawn(async move { agent.call(tool, args).await.expect("answered") })
        })
        .collect();
    let mut replies = Vec::with_capacity(calls.len());
    for call in calls {
        replies.push(call.await.unwrap());
    }
    replies
}

async fn races(node: &Node, agents: &[Agent]) {
    let first = &agents[0];
    let app = first
        .ok(
            "create_application",
            json!({"company": "Race", "role": "R"}),
        )
        .await;
    let id = app["id"].as_str().unwrap().to_owned();
    for round in 0..15 {
        // One key, one payload: one event, and every caller sees it.
        let key = format!("same-key-{round}");
        let replies = from_all(agents, "record_application_event", |_| {
            json!({"applicationId": id, "type": "note_added", "idempotencyKey": key,
                   "occurredAt": NOW, "metadata": {"note": "same"}})
        })
        .await;
        let ids: HashSet<String> = replies
            .iter()
            .map(|r| r.json()["event"]["id"].to_string())
            .collect();
        assert_eq!(ids.len(), 1, "{replies:?}");
        let first_writes = replies
            .iter()
            .filter(|r| r.json()["replayed"] == false)
            .count();
        assert_eq!(first_writes, 1);

        // One key, different payloads: one winner, the rest told so.
        let key = format!("other-key-{round}");
        let replies = from_all(agents, "record_application_event", |i| {
            json!({"applicationId": id, "type": "note_added", "idempotencyKey": key,
                   "occurredAt": NOW, "metadata": {"note": format!("payload {i}")}})
        })
        .await;
        assert_eq!(replies.iter().filter(|r| !r.is_error).count(), 1);
        assert!(
            replies
                .iter()
                .filter(|r| r.is_error)
                .all(|r| r.code() == "idempotency_conflict")
        );

        // One posting from many agents, with different tracking parameters: one row.
        let url = format!("https://jobs.example.invalid/race/{round}");
        let replies = from_all(agents, "upsert_application_by_job_url", |i| {
            json!({"company": "Race", "role": format!("R{i}"), "jobUrl": format!("{url}?utm_source=a{i}")})
        })
        .await;
        assert!(replies.iter().all(|r| !r.is_error), "{replies:?}");
        let ids: HashSet<String> = replies
            .iter()
            .map(|r| r.json()["application"]["id"].to_string())
            .collect();
        assert_eq!(ids.len(), 1, "{replies:?}");
        let created = replies
            .iter()
            .filter(|r| r.json()["operation"] == "created")
            .count();
        assert_eq!(created, 1, "{replies:?}");

        // Everyone holds the same version: one write wins and the rest get a conflict.
        let held = first.ok("get_application", json!({"id": id})).await["updatedAt"].clone();
        let replies = from_all(
            agents,
            "update_application",
            |i| json!({"id": id, "salaryMin": i, "expectedUpdatedAt": held}),
        )
        .await;
        assert_eq!(replies.iter().filter(|r| !r.is_error).count(), 1);
        assert!(
            replies
                .iter()
                .filter(|r| r.is_error)
                .all(|r| r.code() == "conflict"),
            "{replies:?}"
        );

        // A submission replayed by everyone, and one document claimed by many submissions.
        let doc = first
            .ok(
                "upload_document_content",
                pdf_args(&format!("replay {round}"), 0),
            )
            .await;
        let target = first
            .ok(
                "create_application",
                json!({"company": format!("Submit {round}"), "role": "R"}),
            )
            .await;
        let replies = from_all(agents, "record_application_submission", |_| {
            json!({"applicationId": target["id"], "submittedAt": NOW,
                   "idempotencyKey": format!("replay-{round}"), "documentIds": [doc["id"]],
                   "answers": [{"question": "Q", "answer": "A"}], "policy": good_policy()})
        })
        .await;
        assert!(replies.iter().all(|r| !r.is_error), "{replies:?}");
        let ids: HashSet<String> = replies
            .iter()
            .map(|r| r.json()["submission"]["id"].to_string())
            .collect();
        assert_eq!(ids.len(), 1);

        let doc = first
            .ok(
                "upload_document_content",
                pdf_args(&format!("claim {round}"), 0),
            )
            .await;
        let mut targets = Vec::new();
        for i in 0..agents.len() {
            let created = first
                .ok(
                    "create_application",
                    json!({"company": format!("Claim {round} {i}"), "role": "R"}),
                )
                .await;
            targets.push(created["id"].clone());
        }
        let replies = from_all(agents, "record_application_submission", |i| {
            json!({"applicationId": targets[i], "submittedAt": NOW,
                   "idempotencyKey": format!("claim-{round}-{i}"), "documentIds": [doc["id"]],
                   "answers": [{"question": "Q", "answer": "A"}], "policy": good_policy()})
        })
        .await;
        assert_eq!(
            replies.iter().filter(|r| !r.is_error).count(),
            1,
            "{replies:?}"
        );
    }
    assert_eq!(
        node.count(
            "SELECT count(*) FROM (SELECT idempotency_key FROM application_events
             WHERE idempotency_key IS NOT NULL GROUP BY 1 HAVING count(*) > 1)"
        )
        .await,
        0
    );
}

type Latencies = Arc<Mutex<BTreeMap<&'static str, Vec<f64>>>>;

async fn load(agent: Agent, seed: u64, apps: Vec<String>, stop: Arc<AtomicBool>, out: Latencies) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut seed = seed;
    while !stop.load(Ordering::Relaxed) {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = &apps[jitter(&mut seed, apps.len() as u64) as usize];
        let (tool, args): (&'static str, Value) = match jitter(&mut seed, 100) {
            0..=24 => ("get_application", json!({"id": id})),
            25..=36 => (
                "list_applications_filtered",
                json!({"fields": ["id", "company", "status"], "limit": 200}),
            ),
            37..=44 => ("list_applications", json!({"limit": 200})),
            45..=52 => (
                "list_application_events",
                json!({"applicationId": id, "limit": 100}),
            ),
            53..=56 => ("pipeline_healthcheck", json!({})),
            57..=74 => (
                "record_application_event",
                json!({"applicationId": id, "type": "note_added", "idempotencyKey": format!("load-key-{n}"),
                       "occurredAt": NOW, "metadata": {"note": "load"}}),
            ),
            75..=84 => (
                "upsert_application_by_job_url",
                json!({"company": "Load", "role": "R",
                       "jobUrl": format!("https://jobs.example.invalid/load/{}", n % 500)}),
            ),
            85..=92 => ("update_application", json!({"id": id, "rating": 1 + n % 5})),
            _ => (
                "upload_document_content",
                pdf_args(&format!("load {n}"), (n % 100_000) as usize),
            ),
        };
        let started = Instant::now();
        let reply = agent
            .call(tool, args)
            .await
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(!reply.is_error, "{tool}: {}", reply.text);
        out.lock()
            .unwrap()
            .entry(tool)
            .or_default()
            .push(started.elapsed().as_secs_f64() * 1000.0);
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

async fn backup_and_restore_under_load(node: &Node, root: &std::path::Path) {
    let identity = x25519::Identity::generate();
    let recipients = root.join("recipients.txt");
    let identity_file = root.join("identity.txt");
    std::fs::write(&recipients, format!("{}\n", identity.to_public())).unwrap();
    std::fs::write(
        &identity_file,
        format!("{}\n", identity.to_string().expose_secret()),
    )
    .unwrap();
    let out = root.join("under-load.age");
    let events_before = node
        .count("SELECT count(*) FROM application_events WHERE idempotency_key LIKE 'load-%'")
        .await;
    let backup = nexus()
        .args(["backup", "--data-dir"])
        .arg(&node.dir)
        .arg("--out")
        .arg(&out)
        .arg("--recipient-file")
        .arg(&recipients)
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "{}",
        String::from_utf8_lossy(&backup.stderr)
    );
    let target = root.join("restored");
    let restore = nexus()
        .args(["restore", "--from"])
        .arg(&out)
        .arg("--identity")
        .arg(&identity_file)
        .arg("--data-dir")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let mut restored = nexus()
        .env("NEXUS_OWNER_ID", "owner-synthetic")
        .env("NEXUS_OWNER_EMAIL", "owner@example.test")
        .args([
            "serve",
            "--insecure-local-only",
            "--bind",
            "127.0.0.1:0",
            "--data-dir",
        ])
        .arg(&target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Serving proves the migrations and pool accept it; contents are checked directly.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        restored.try_wait().unwrap().is_none(),
        "restored data serves"
    );
    restored.kill().unwrap();
    restored.wait().unwrap();
    let audit = Node::audit_of(&target).await;
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *audit.lock().await)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    let events_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM application_events WHERE idempotency_key LIKE 'load-%'",
    )
    .fetch_one(&mut *audit.lock().await)
    .await
    .unwrap();
    assert!(
        events_after >= events_before,
        "a backup holds what was acknowledged before it began"
    );
    let shas: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT blob_sha256 FROM documents WHERE blob_sha256 IS NOT NULL",
    )
    .fetch_all(&mut *audit.lock().await)
    .await
    .unwrap();
    for sha256 in shas {
        let bytes = std::fs::read(target.join("blobs").join(&sha256)).unwrap();
        assert_eq!(nexus_core::blob::sha256_hex(&bytes), sha256);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: see the module docs"]
async fn parallel_agents_keep_every_invariant() {
    let seconds: u64 = std::env::var("QA_SECONDS").map_or(45, |v| v.parse().unwrap());
    let root = tempfile::tempdir().unwrap();
    let (mut node, token) = Node::create(root.path(), &[]).await;
    let agents: Vec<Agent> = (0..AGENTS).map(|_| Agent::new(node.addr, &token)).collect();

    races(&node, &agents).await;

    let mut apps = Vec::new();
    for i in 0..30 {
        let created = agents[0]
            .ok(
                "create_application",
                json!({"company": format!("Pool {i}"), "role": "R", "jobDescription": "d".repeat(20_000)}),
            )
            .await;
        apps.push(created["id"].as_str().unwrap().to_owned());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let latencies: Latencies = Arc::default();
    let tasks: Vec<_> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            tokio::spawn(load(
                agent.clone(),
                0x9E37_79B9_7F4A_7C15 + i as u64,
                apps.clone(),
                stop.clone(),
                latencies.clone(),
            ))
        })
        .collect();

    let started = Instant::now();
    let mut peak_wal = 0;
    let mut backed_up = false;
    while started.elapsed() < Duration::from_secs(seconds) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        peak_wal = peak_wal.max(node.peak_rss_and_wal().1);
        if !backed_up && started.elapsed() > Duration::from_secs(seconds / 2) {
            backup_and_restore_under_load(&node, root.path()).await;
            backed_up = true;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for task in tasks {
        task.await.unwrap();
    }

    let (peak_rss, _) = node.peak_rss_and_wal();
    let table = latencies.lock().unwrap();
    let mut all: Vec<f64> = table.values().flatten().copied().collect();
    all.sort_by(f64::total_cmp);
    for (tool, values) in table.iter() {
        let mut values = values.clone();
        values.sort_by(f64::total_cmp);
        println!(
            "{tool:32} n={:6} p50={:7.1} p95={:7.1} p99={:7.1}",
            values.len(),
            percentile(&values, 0.5),
            percentile(&values, 0.95),
            percentile(&values, 0.99)
        );
    }
    println!(
        "overall n={} p50={:.1} p95={:.1} p99={:.1} ms, peak RSS {peak_rss} MiB, peak WAL {peak_wal} MiB",
        all.len(),
        percentile(&all, 0.5),
        percentile(&all, 0.95),
        percentile(&all, 0.99),
    );
    assert!(peak_rss < 1024, "memory stays bounded: {peak_rss} MiB");
    assert!(backed_up);

    node.terminate();
    node.assert_database_sound().await;
    node.assert_blobs_sound().await;
    let logs = node.logs().to_lowercase();
    assert!(!logs.contains("panic"));
    assert!(!logs.contains("database is locked"));
}
