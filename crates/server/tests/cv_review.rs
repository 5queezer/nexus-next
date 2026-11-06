//! The real `nexus serve` and `nexus cv-facts`: what an owner and a lost or crowding client see. All
//! people and companies are invented.

mod support;

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{Agent, Node, nexus};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const START: &str = "#import \"/nexus.typ\": *\n#show: cv\n";
/// Takes seconds in a debug build, in constant memory, and then passes the check.
const SLOW_BUT_FINE: &str = "#for a in range(30000000) { }\nWork.\n";
/// Runs until the worker's timeout.
const FOR_DAYS: &str =
    "#for a in range(9999) { for b in range(9999) { for c in range(9999) { } } }\n";

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "location": "Hamburg"},
        "profile": "Backend engineer.",
        "skills": [],
        "experience": [
            {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
             "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
             "bullets": ["Cut invoice errors by 40% across 3 depots."]},
        ],
        "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
            "date": "2015 – 2018", "location": "Kiel"}],
    })
}

fn cv_facts(dir: &Path, args: &[&str]) -> Output {
    nexus()
        .arg("cv-facts")
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .unwrap()
}

fn lock_confirmed(dir: &Path) -> String {
    let output = cv_facts(dir, &["lock", "--confirm"]);
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// The children of the server that run a compile.
fn compiles_of(node: &Node) -> Vec<String> {
    let mut found = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{}/task", node.pid()))
        .unwrap()
        .flatten()
    {
        let children = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in children.split_whitespace() {
            let command = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&command).contains("__render") {
                found.push(child.to_owned());
            }
        }
    }
    found
}

async fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "{what} after {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Finding 13, as the review ran it: an agent rewrites the facts in the unlocked window, and the owner
/// sees it all at the lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_shows_what_an_agent_wrote_and_needs_a_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;

    let refused = cv_facts(&node.dir, &["lock"]);
    assert!(
        !refused.status.success(),
        "a first lock confirms everything"
    );
    let printed = String::from_utf8(refused.stdout).unwrap();
    assert!(
        printed.contains("changed name: (none) -> \"Mara Example\""),
        "{printed}"
    );
    assert!(printed.contains("added to allowed tokens: 3"), "{printed}");
    let locked: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NOT NULL")
        .await;
    assert_eq!(locked, 0, "nothing written");

    lock_confirmed(&node.dir);
    assert!(cv_facts(&node.dir, &["unlock"]).status.success());
    let unchanged = cv_facts(&node.dir, &["lock"]);
    assert!(
        unchanged.status.success(),
        "nothing to confirm: {unchanged:?}"
    );
    assert!(cv_facts(&node.dir, &["unlock"]).status.success());

    let mut rewritten = profile();
    rewritten["experience"][0]["title"] = json!("Principal Engineer");
    rewritten["experience"][0]["date"] = json!("03/2015 – 12/2022");
    rewritten["education"][0]["degree"] = json!("PhD Computer Science");
    agent.ok("upsert_cv_profile", rewritten.clone()).await;
    rewritten["experience"][0]["title"] = json!("Distinguished Principal");
    agent.ok("upsert_cv_profile", rewritten.clone()).await;
    rewritten["experience"][0]["title"] = json!("Principal Engineer");
    agent.ok("upsert_cv_profile", rewritten).await;

    let refused = cv_facts(&node.dir, &["lock"]);
    assert!(!refused.status.success());
    let printed = String::from_utf8(refused.stdout).unwrap();
    for line in [
        "changed employment[0:brightpath].title: \"Software Engineer\" -> \"Principal Engineer\"",
        "changed employment[0:brightpath].date: \"03/2019 – 12/2022\" -> \"03/2015 – 12/2022\"",
        "changed education[0:edu-1].qualification: \"BSc Computer Science\" -> \"PhD Computer Science\"",
    ] {
        assert!(printed.lines().any(|l| l == line), "{line} in {printed}");
    }
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("--confirm")
    );
    let open: i64 = node
        .count("SELECT count(*) FROM cv_facts WHERE locked_at IS NULL")
        .await;
    assert_eq!(open, 1, "still unlocked");

    let confirmed = lock_confirmed(&node.dir);
    assert!(confirmed.contains("Principal Engineer"), "{confirmed}");

    let mut conn = node.audit_connection().await;
    let writes: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT changed_by, change, state ->> '$.employment[0].title' FROM cv_facts_history
         WHERE changed_by = 'mcp' ORDER BY id",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let titles: Vec<&str> = writes.iter().map(|w| w.2.as_str()).collect();
    assert_eq!(
        titles,
        [
            "",
            "Software Engineer",
            "Principal Engineer",
            "Distinguished Principal"
        ],
        "{writes:?}"
    );
}

/// Finding 20: rmcp runs a tool in a task of its own, so a lost connection reaches it only through
/// the token of the call. The compile ends with the client, and nothing is stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_patch_whose_client_has_gone_stops_and_stores_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let source = agent.ok("get_cv_source", json!({})).await["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = format!(
        "{START}#header()\n#employment(\"brightpath\")\n#education(\"edu-1\")\n{SLOW_BUT_FINE}"
    );
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "patch_cv", "arguments": {
            "expectedRevision": 0, "summary": "slow",
            "edits": [{"oldText": source, "newText": new}],
        }},
    })
    .to_string();
    let mut client = TcpStream::connect(node.addr).await.unwrap();
    let request = format!(
        "POST /api/mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\
         Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        node.addr,
        body.len()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    wait_for("no compile started", Duration::from_secs(5), || {
        !compiles_of(&node).is_empty()
    })
    .await;
    drop(client);
    wait_for("the compile still runs", Duration::from_secs(2), || {
        compiles_of(&node).is_empty()
    })
    .await;

    // Long enough for a compile that kept going to have stored its revision.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(agent.ok("get_cv_source", json!({})).await["revision"], 0);
    let count = node.count("SELECT count(*) FROM cv_revisions").await;
    assert_eq!(count, 0);
    let fine = agent
        .ok(
            "patch_cv",
            json!({"expectedRevision": 0, "summary": "next",
                "edits": [{"oldText": "Backend engineer", "newText": "Freight engineer"}]}),
        )
        .await;
    assert_eq!(fine["revision"], 1);
}

/// Finding 19: past a compile and two waiting, a call is refused at once and holds no slot of the
/// pool every other tool needs.
#[tokio::test(flavor = "multi_thread")]
async fn a_crowd_of_compiles_is_refused_at_once_and_other_tools_keep_working() {
    let root = tempfile::tempdir().unwrap();
    let (node, token) = Node::create(root.path(), &[]).await;
    let agent = Agent::new(node.addr, &token);
    agent.ok("upsert_cv_profile", profile()).await;
    lock_confirmed(&node.dir);
    let source = agent.ok("get_cv_source", json!({})).await["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let args = json!({"expectedRevision": 0, "summary": "slow",
        "edits": [{"oldText": source, "newText": format!("{START}{FOR_DAYS}")}]});
    let started = Instant::now();
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let (agent, args) = (agent.clone(), args.clone());
            tokio::spawn(async move {
                let reply = agent.call("patch_cv", args).await.unwrap();
                (reply.code(), started.elapsed())
            })
        })
        .collect();
    let mut refused = 0;
    let mut waiting = Vec::new();
    tokio::time::sleep(Duration::from_secs(3)).await;
    for call in calls {
        if call.is_finished() {
            let (code, took) = call.await.unwrap();
            assert_eq!(code, "cv_render_busy");
            assert!(took < Duration::from_secs(3), "{took:?}");
            refused += 1;
        } else {
            waiting.push(call);
        }
    }
    assert_eq!(refused, 5, "one compiling and two waiting stay");
    let before = Instant::now();
    agent.ok("list_applications", json!({})).await;
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "{:?}",
        before.elapsed()
    );
    for call in &waiting {
        call.abort();
    }
    wait_for(
        "a compile outlived its clients",
        Duration::from_secs(5),
        || compiles_of(&node).is_empty(),
    )
    .await;
}
