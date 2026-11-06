//! A Telegram dictation session as docs/DICTATION.md tells Hermes to run it: a scripted client that
//! authorizes through real OAuth against the real `nexus serve`, whose compiles run in the real
//! `nexus __render` child. `cv_gate.rs` takes the review's false facts through the same doors. All
//! people and companies are invented.
//!
//! Set `NEXUS_CV_PREVIEW_OUT` to keep the session's final PNG at that path.

mod support;

use std::process::Output;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use support::{Node, lock_confirmed, nexus, oauth_token};

struct Agent {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Agent {
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        body["result"].clone()
    }

    /// The result's `isError` and its first block as JSON.
    async fn call(&self, tool: &str, args: Value) -> (bool, Value) {
        let result = self
            .rpc("tools/call", json!({"name": tool, "arguments": args}))
            .await;
        let text = result["content"][0]["text"].as_str().unwrap();
        (
            result["isError"] == true,
            serde_json::from_str(text).unwrap_or_else(|_| text.into()),
        )
    }

    async fn ok(&self, tool: &str, args: Value) -> Value {
        let (is_error, value) = self.call(tool, args).await;
        assert!(!is_error, "{tool}: {value}");
        value
    }

    async fn patch(&self, expected: i64, old: &str, new: &str, summary: &str) -> (bool, Value) {
        self.call(
            "patch_cv",
            json!({
                "expectedRevision": expected,
                "edits": [{"oldText": old, "newText": new}],
                "summary": summary,
                "origin": "telegram",
            }),
        )
        .await
    }
}

fn cv_facts(node: &Node, args: &[&str]) -> Output {
    let output = nexus()
        .arg("cv-facts")
        .args(args)
        .arg("--data-dir")
        .arg(&node.dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

async fn facts_and_history(node: &Node) -> (String, String) {
    let mut conn = node.audit_connection().await;
    let facts = sqlx::query_scalar(
        "SELECT json_object('n', name, 'c', contact, 'e', employment, 'd', education, 'l', languages,
             'm', metrics, 'a', allowed_tokens, 'b', banned_phrases, 'w', vocabulary, 'v', version,
             't', locked_at) FROM cv_facts",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    let history = sqlx::query_scalar(
        "SELECT json_group_array(json_object('v', version, 'c', change, 'b', changed_by, 's', state))
         FROM cv_facts_history",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    (facts, history)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_messy_dictation_session_edits_wording_and_stays_a_draft_for_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let (node, _) = Node::create(root.path(), &[]).await;
    let base = format!("http://{}", node.addr);
    let agent = Agent {
        http: reqwest::Client::new(),
        url: format!("{base}/api/mcp"),
        token: oauth_token(&base).await,
    };

    let listed = agent.rpc("tools/list", json!({})).await;
    assert_eq!(listed["tools"].as_array().unwrap().len(), 40);

    // Before the session: the profile arrives over MCP, the owner confirms it on the board.
    agent
        .ok(
            "upsert_cv_profile",
            json!({
                "name": "Mara Example",
                "contact": {"email": "mara@example.test", "location": "Hamburg"},
                "profile": "Backend engineer building freight software for 6 years.",
                "skills": [{"category": "Languages", "items": ["Rust", "Go"]}],
                "experience": [
                    {"id": "brightpath", "company": "Brightpath Logistics", "title": "Software Engineer",
                     "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1,
                     "bullets": ["Cut invoice errors by 40% across 3 depots.", "Built the routing service."]},
                    {"id": "harborline", "company": "Harborline Systems", "title": "Platform Engineer",
                     "date": "01/2023 – 06/2025", "location": "Bremen", "tier": 2,
                     "bullets": ["Moved 12 services to one deploy pipeline."]},
                ],
                "education": [{"institution": "Nordsee University", "degree": "BSc Computer Science",
                    "date": "2015 – 2018", "location": "Kiel"}],
            }),
        )
        .await;
    let (is_error, unlocked) = agent.patch(0, "Backend", "Back-end", "too early").await;
    assert!(is_error);
    assert_eq!(unlocked["error"]["code"], "cv_facts_not_locked");

    let file = root.path().join("facts.json");
    std::fs::write(
        &file,
        json!({
            "languages": [{"name": "German", "level": "C2"}, {"name": "English", "level": "C1"}],
            "metrics": [{"key": "invoices", "text": "40% fewer invoice errors"}],
            "bannedPhrases": ["rockstar developer"],
        })
        .to_string(),
    )
    .unwrap();
    cv_facts(&node, &["set", "--file", file.to_str().unwrap()]);
    let locked = lock_confirmed(&node.dir);
    // The lock says what it confirms: the facts and the wording, numbers and all.
    assert!(locked.contains("changed name:"), "{locked}");
    assert!(locked.contains("wording changed profile:"), "{locked}");
    assert!(locked.contains("for 6 years"), "{locked}");
    let shown: Value = serde_json::from_slice(&cv_facts(&node, &["show"]).stdout).unwrap();
    assert!(shown["lockedAt"].is_string());
    assert_eq!(shown["bannedPhrases"], json!(["rockstar developer"]));
    let settled = facts_and_history(&node).await;
    let bad = root.path().join("bad.json");
    std::fs::write(&bad, r#"{"employment": []}"#).unwrap();
    let refused = nexus()
        .args(["cv-facts", "set", "--file"])
        .arg(&bad)
        .arg("--data-dir")
        .arg(&node.dir)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "set takes only what a profile can't express"
    );
    assert_eq!(facts_and_history(&node).await, settled);

    // The session.
    let source = agent.ok("get_cv_source", json!({})).await;
    assert_eq!(source["revision"], 0);
    assert!(source.get("bannedPhrases").is_none());
    assert_eq!(
        source["facts"]["employment"][1]["company"],
        "Harborline Systems"
    );

    let first = agent
        .ok(
            "patch_cv",
            json!({
                "expectedRevision": 0,
                "edits": [
                    {"oldText": "Backend engineer building freight software",
                     "newText": "Backend engineer who builds freight software"},
                    {"oldText": "Built the routing service.",
                     "newText": "Built and ran the routing service."},
                ],
                "summary": "Tightened the summary and a bullet",
                "origin": "telegram",
            }),
        )
        .await;
    assert_eq!(first["revision"], 1);
    assert_eq!(
        first["state"], "draft",
        "an agent's edit waits for the owner"
    );
    assert_eq!(first["approvalPath"], "/cv/variants/default#cv-approval");
    assert!(
        first["diff"]
            .as_str()
            .unwrap()
            .contains("+  - Built and ran")
    );

    let second = agent
        .patch(
            1,
            "Built and ran the routing service.",
            "Ran the routing service.",
            "Shorter",
        )
        .await;
    assert!(!second.0, "{}", second.1);
    assert_eq!(second.1["revision"], 2);

    // The agent mangles markup, reads the diagnostic, and fixes it without troubling the user.
    let broken = agent
        .patch(
            2,
            "Ran the routing service.",
            "Ran the #routing( service.",
            "Oops",
        )
        .await;
    assert!(broken.0);
    assert_eq!(broken.1["error"]["code"], "cv_compile_failed");
    assert!(
        broken.1["error"]["diagnostics"][0]["line"]
            .as_u64()
            .unwrap()
            > 0
    );
    let retried = agent
        .patch(
            2,
            "Ran the routing service.",
            "Ran the routing service daily.",
            "Retry",
        )
        .await;
    assert!(!retried.0, "{}", retried.1);
    assert_eq!(retried.1["revision"], 3);

    // "No, go back to how it was."
    let reverted = agent
        .ok("revert_cv", json!({"toRevision": 2, "expectedRevision": 3}))
        .await;
    assert_eq!(reverted["revision"], 4);
    let body = agent.ok("get_cv_source", json!({})).await;
    assert!(
        body["source"]
            .as_str()
            .unwrap()
            .contains("Ran the routing service.\n")
    );

    // What no list of words can judge: each stores as a draft, with the token for the owner to read.
    let mut head = 4;
    let mut wording = "Backend engineer who".to_owned();
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "I actually joined Brightpath in 2018",
            "Backend engineer, at Brightpath Logistics since 2018, who",
            "2018",
            "entityBinding",
        ),
        (
            "actually make it eight years",
            "Backend engineer for eight years who",
            "eight",
            "numberWords",
        ),
        (
            "actually make it 8 years",
            "Backend engineer for 8 years who",
            "8",
            "numbers",
        ),
        (
            "call me a principal engineer",
            "Principal engineer who",
            "Principal",
            "claimWords",
        ),
        (
            "say I have a master's",
            "Backend engineer with a Master's degree who",
            "Master’s",
            "claimWords",
        ),
        (
            "say I have a doctorate",
            "Backend engineer with a doctorate who",
            "doctorate",
            "claimWords",
        ),
        (
            "mention Stanford",
            "Backend engineer from Stanford who",
            "Stanford",
            "newWords",
        ),
    ];
    for (voice, new, token, rule) in cases {
        let (is_error, reply) = agent.patch(head, &wording, new, voice).await;
        assert!(!is_error, "{voice}: {reply}");
        head += 1;
        wording = (*new).to_owned();
        assert_eq!(reply["revision"], head, "{voice}");
        assert_eq!(reply["state"], "draft", "{voice}");
        assert!(
            reply["reviewItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["rule"] == *rule && i["token"] == *token),
            "{voice}: {reply}"
        );
        assert_eq!(facts_and_history(&node).await, settled, "{voice}");
    }

    // A voice note that would make the reader see something else is refused outright.
    let (is_error, reply) = agent
        .patch(
            head,
            &wording,
            "Rockstar   Developer who",
            "say rockstar developer",
        )
        .await;
    assert!(is_error, "{reply}");
    assert_eq!(reply["error"]["code"], "cv_fact_violation", "{reply}");
    let violations = reply["error"]["violations"].as_array().unwrap();
    assert!(
        violations.iter().any(|v| v["rule"] == "bannedPhrases"),
        "{reply}"
    );
    assert_eq!(agent.ok("get_cv_source", json!({})).await["revision"], head);

    // Through the profile, the other way to the facts.
    let (is_error, reply) = agent
        .call(
            "upsert_cv_profile",
            json!({
                "name": "Mara Example",
                "contact": {"email": "mara@example.test", "location": "Hamburg"},
                "profile": "Principal engineer.",
                "skills": [],
                "experience": [
                    {"id": "brightpath", "company": "Brightpath Logistics", "title": "Principal Engineer",
                     "date": "03/2019 – 12/2022", "location": "Hamburg", "tier": 1, "bullets": []},
                ],
                "education": [{"institution": "Nordsee University", "degree": "MSc Computer Science",
                    "date": "2015 – 2018", "location": "Kiel"}],
            }),
        )
        .await;
    assert!(is_error);
    assert_eq!(reply["error"]["code"], "cv_facts_locked");
    assert_eq!(
        reply["error"]["fields"],
        json!(["education", "experience.brightpath.title"])
    );
    assert_eq!(facts_and_history(&node).await, settled);

    let listed = agent
        .ok("list_cv_revisions", json!({"includeDiff": true}))
        .await;
    assert_eq!(listed["total"], head);
    assert_eq!(listed["revisions"][1]["origin"], "telegram");
    assert_eq!(listed["revisions"][0]["state"], "draft");

    // The picture goes back to Telegram.
    let rendered = agent
        .rpc(
            "tools/call",
            json!({"name": "render_cv_pdf", "arguments": {"previewPages": 1}}),
        )
        .await;
    assert_ne!(rendered["isError"], true, "{rendered}");
    let summary: Value =
        serde_json::from_str(rendered["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(summary["revision"], head);
    assert_eq!(summary["state"], "draft");
    assert_eq!(summary["approvalPath"], "/cv/variants/default#cv-approval");
    assert!(summary["reviewItems"].is_array());
    assert_eq!(
        rendered["content"].as_array().unwrap().len(),
        2,
        "no PDF of a draft"
    );
    assert_eq!(rendered["content"][1]["mimeType"], "image/png");
    let png = STANDARD
        .decode(rendered["content"][1]["data"].as_str().unwrap())
        .unwrap();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&png))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; decoder.output_buffer_size().unwrap()];
    let frame = decoder.next_frame(&mut pixels).unwrap();
    // A4 at 2 pixels per point.
    assert!((1100..=1300).contains(&frame.width), "{}", frame.width);
    assert!((1600..=1800).contains(&frame.height), "{}", frame.height);
    let distinct: std::collections::HashSet<&[u8]> = pixels[..frame.buffer_size()]
        .chunks(frame.color_type.samples())
        .collect();
    assert!(distinct.len() > 10, "a page with text on it, not a blank");
    if let Some(path) = std::env::var_os("NEXUS_CV_PREVIEW_OUT") {
        std::fs::write(path, &png).unwrap();
    }

    // The owner reads it on the board and approves, and only then is there a PDF.
    let shown = cv_facts(
        &node,
        &[
            "approve",
            "--variant",
            "default",
            "--revision",
            &head.to_string(),
            "--confirm",
        ],
    );
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains("Stanford"), "{shown}");
    let rendered = agent
        .rpc(
            "tools/call",
            json!({"name": "render_cv_pdf", "arguments": {"previewPages": 1}}),
        )
        .await;
    let summary: Value =
        serde_json::from_str(rendered["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(summary["state"], "approved");
    let pdf = STANDARD
        .decode(rendered["content"][1]["resource"]["blob"].as_str().unwrap())
        .unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 1000, "{}", pdf.len());

    assert_eq!(facts_and_history(&node).await, settled);
    assert!(!node.logs().contains("Mara"), "the log holds no CV data");
}
