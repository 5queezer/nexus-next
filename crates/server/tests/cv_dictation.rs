//! A Telegram dictation session as docs/DICTATION.md tells Hermes to run it: a scripted client that
//! authorizes through real OAuth against the real `nexus serve`, whose compiles run in the real
//! `nexus __render` child. All people and companies are invented.
//!
//! Set `NEXUS_CV_PREVIEW_OUT` to keep the session's final PNG at that path.

mod support;

use std::process::Output;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use support::{Node, PASSPHRASE, nexus};

const REDIRECT: &str = "http://127.0.0.1:9/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// A `<input name=... value=...>` from a served page.
fn field(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\"");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("{name} in {page}"));
    let tag_start = page[..at].rfind('<').unwrap();
    let tag_end = at + page[at..].find('>').unwrap();
    let tag = &page[tag_start..tag_end];
    let value = tag.split("value=\"").nth(1).unwrap();
    value.split('"').next().unwrap().to_owned()
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

struct Browser {
    http: reqwest::Client,
    base: String,
    cookies: Vec<(String, String)>,
}

impl Browser {
    fn cookie_header(&self) -> String {
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        pairs.join("; ")
    }

    async fn keep_cookies(&mut self, response: &reqwest::Response) {
        for cookie in response.headers().get_all(SET_COOKIE) {
            let first = cookie.to_str().unwrap().split(';').next().unwrap();
            let (key, value) = first.split_once('=').unwrap();
            self.cookies.retain(|(k, _)| k != key);
            if !value.is_empty() {
                self.cookies.push((key.to_owned(), value.to_owned()));
            }
        }
    }

    async fn get(&mut self, path: &str) -> (reqwest::StatusCode, String) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .header(COOKIE, self.cookie_header())
            .send()
            .await
            .unwrap();
        self.keep_cookies(&response).await;
        (response.status(), response.text().await.unwrap())
    }

    async fn post_form(&mut self, path: &str, body: String) -> reqwest::Response {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header(COOKIE, self.cookie_header())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        self.keep_cookies(&response).await;
        response
    }
}

/// Dynamic registration, owner login, consent and PKCE: what Hermes does at its one reconnect.
async fn oauth_token(base: &str) -> String {
    let http = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let registered = http
        .post(format!("{base}/api/mcp/register"))
        .header("content-type", "application/json")
        .body(
            json!({
                "client_name": "Scripted Hermes",
                "redirect_uris": [REDIRECT],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    let registered: Value = serde_json::from_str(&registered.text().await.unwrap()).unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_owned();

    let mut browser = Browser {
        http: http.clone(),
        base: base.to_owned(),
        cookies: Vec::new(),
    };
    let (_, login) = browser.get("/login").await;
    let body = form(&[("csrf", &field(&login, "csrf")), ("passphrase", PASSPHRASE)]);
    let logged_in = browser.post_form("/login", body).await;
    assert_eq!(logged_in.status(), 303);

    let query = form(&[
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", REDIRECT),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("state", "st-1"),
        ("scope", "mcp:tools"),
    ]);
    let (status, consent) = browser.get(&format!("/api/mcp/authorize?{query}")).await;
    assert_eq!(status, 200, "{consent}");
    let body = form(&[("consent", &field(&consent, "consent"))]);
    let granted = browser.post_form("/api/mcp/authorize", body).await;
    assert_eq!(granted.status(), 303);
    let location = granted.headers()[LOCATION].to_str().unwrap().to_owned();
    let code = url::Url::parse(&location)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();

    let token = http
        .post(format!("{base}/api/mcp/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ]))
        .send()
        .await
        .unwrap();
    let token: Value = serde_json::from_str(&token.text().await.unwrap()).unwrap();
    token["access_token"].as_str().unwrap().to_owned()
}

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
async fn a_messy_dictation_session_edits_wording_and_cannot_touch_a_fact() {
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
    let locked = String::from_utf8(cv_facts(&node, &["lock"]).stdout).unwrap();
    // Not 40%: the metric the owner just set already vouches for it.
    for token in ["6", "3", "12"] {
        assert!(
            locked
                .lines()
                .any(|l| l == format!("added to allowed tokens: {token}")),
            "{token} in {locked}"
        );
    }
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

    // The gate. Each of these is the voice note an agent must refuse, and each stays refused.
    let head = 4;
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "I actually joined Brightpath in 2018",
            "Backend engineer who",
            "Backend engineer, at Brightpath Logistics since 2018, who",
            "entityBinding",
        ),
        (
            "actually make it eight years",
            "for 6 years",
            "for eight years",
            "numberWords",
        ),
        (
            "actually make it eight years",
            "for 6 years",
            "for 8 years",
            "numbers",
        ),
        (
            "I left Harborline in 2026",
            "#employment(\"harborline\")",
            "Harborline Systems · Platform Engineer 01/2023 – 06/2026\n#employment(\"harborline\")",
            "numbers",
        ),
        (
            "call me a principal engineer",
            "Backend engineer who",
            "Principal engineer who",
            "claimWords",
        ),
        (
            "I'm a senior engineer at Brightpath",
            "Backend engineer who",
            "Senior engineer who",
            "claimWords",
        ),
        (
            "say I have a master's",
            "Backend engineer who",
            "Backend engineer with a Master's degree who",
            "claimWords",
        ),
        (
            "say I have a doctorate",
            "Backend engineer who",
            "Backend engineer with a doctorate who",
            "claimWords",
        ),
        (
            "say rockstar developer",
            "Backend engineer who",
            "Rockstar   Developer who",
            "bannedPhrases",
        ),
    ];
    for (voice, old, new, rule) in cases {
        let (is_error, reply) = agent.patch(head, old, new, voice).await;
        assert!(is_error, "{voice}: {reply}");
        assert_eq!(
            reply["error"]["code"], "cv_fact_violation",
            "{voice}: {reply}"
        );
        let violations = reply["error"]["violations"].as_array().unwrap();
        assert!(
            violations.iter().any(|v| v["rule"] == *rule),
            "{voice}: {reply}"
        );
        assert!(
            violations.iter().all(|v| v["token"].is_string()),
            "the agent can name the token: {reply}"
        );
        let current = agent.ok("get_cv_source", json!({})).await;
        assert_eq!(current["revision"], head, "{voice}");
        assert_eq!(facts_and_history(&node).await, settled, "{voice}");
    }
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
    assert_eq!(listed["total"], 4);
    assert_eq!(listed["revisions"][1]["origin"], "telegram");

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
    assert_eq!(summary["revision"], 4);
    let pdf = STANDARD
        .decode(rendered["content"][1]["resource"]["blob"].as_str().unwrap())
        .unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 1000, "{}", pdf.len());
    assert_eq!(rendered["content"][2]["mimeType"], "image/png");
    let png = STANDARD
        .decode(rendered["content"][2]["data"].as_str().unwrap())
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

    assert_eq!(facts_and_history(&node).await, settled);
    assert!(!node.logs().contains("Mara"), "the log holds no CV data");
}
