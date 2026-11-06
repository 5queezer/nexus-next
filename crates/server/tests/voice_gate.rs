//! The gate of SPEC section 9b: a voice session reaches the eight allowed tools and nothing else, so
//! it can't mark an application `applied`. Runs the real router behind the real login on a real
//! socket, against a local stand-in for OpenAI. All data is invented.

mod support;

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use nexus_auth::AuthConfig;
use nexus_core::Db;
use nexus_core::auth::{SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_core::blob::sha256_hex;
use nexus_core::repo::applications::{self, ApplicationFields};
use nexus_core::repo::cv_facts;
use nexus_mcp::{McpState, Renderer, all_tools};
use nexus_server::config::{Access, ServeConfig};
use nexus_server::serve::{self, OWNER};
use nexus_web::{MAX_MODEL_OUTPUT_BYTES, MINTS_PER_HOUR, VOICE_TOOLS, VoiceConfig, VoiceKey};
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Row};
use support::{PASSPHRASE, nexus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Long and distinctive, so that any leak of it is found by a plain search.
const REAL_KEY: &str = "sk-real-synthetic-key-AAAAbbbbCCCCdddd0123456789";
const EPHEMERAL: &str = "ek_synthetic_ephemeral_0123456789";

#[derive(Default)]
struct Stub {
    hits: AtomicUsize,
    fail: AtomicBool,
    authorization: Mutex<Vec<String>>,
    bodies: Mutex<Vec<Value>>,
}

async fn stub_mint(State(stub): State<Arc<Stub>>, headers: HeaderMap, body: String) -> Response {
    stub.hits.fetch_add(1, Ordering::SeqCst);
    stub.authorization
        .lock()
        .unwrap()
        .push(headers["authorization"].to_str().unwrap().to_owned());
    stub.bodies
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap());
    if stub.fail.load(Ordering::SeqCst) {
        // As a real provider might: an error that quotes the key it refused.
        let text = format!(r#"{{"error":{{"message":"Incorrect API key provided: {REAL_KEY}"}}}}"#);
        return (StatusCode::UNAUTHORIZED, text).into_response();
    }
    axum::Json(json!({"value": EPHEMERAL, "expires_at": 4_102_444_800_i64})).into_response()
}

async fn start_stub() -> (Arc<Stub>, String) {
    let stub = Arc::new(Stub::default());
    let app = Router::new()
        .route("/v1/realtime/client_secrets", post(stub_mint))
        .with_state(stub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (stub, url)
}

struct Harness {
    addr: SocketAddr,
    db: Db,
    dir: tempfile::TempDir,
    stub: Arc<Stub>,
}

async fn start(lifetime: Duration, voice_on: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut set = nexus()
        .args(["set-passphrase", "--data-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(set.stdin.take().unwrap(), "{PASSPHRASE}").unwrap();
    assert!(set.wait().unwrap().success());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stub, stub_url) = start_stub().await;
    let db = Db::open(dir.path()).await.unwrap();
    let config = ServeConfig {
        bind: addr,
        data_dir: dir.path().to_path_buf(),
        access: Access::Owner(
            AuthConfig::from_env(&format!("http://localhost:{}", addr.port()), dir.path()).unwrap(),
        ),
        owner_id: Some("owner-1".to_owned()),
        owner_email: Some("owner@example.test".to_owned()),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: None,
        log_level: tracing::Level::INFO,
        voice: voice_on.then(|| VoiceConfig {
            key: VoiceKey::new(REAL_KEY.to_owned()),
            model: "gpt-realtime-2.1-mini".to_owned(),
            voice: "marin".to_owned(),
            base_url: stub_url,
            lifetime,
        }),
        renderer: Renderer::InProcess,
        stream_shutdown: watch::Sender::new(false),
    };
    let app = serve::build_app(&db, &config).unwrap();
    tokio::spawn(serve::serve(
        listener,
        app,
        std::future::pending(),
        Duration::from_secs(1),
    ));
    Harness {
        addr,
        db,
        dir,
        stub,
    }
}

/// One login, the way a browser holds it.
struct Login {
    http: reqwest::Client,
    base: String,
    cookies: String,
    csrf: String,
}

fn field(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\"");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("{name} in {page}"));
    let tag_start = page[..at].rfind('<').unwrap();
    let tag_end = at + page[at..].find('>').unwrap();
    let value = page[tag_start..tag_end].split("value=\"").nth(1).unwrap();
    value.split('"').next().unwrap().to_owned()
}

fn cookies_of(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

impl Harness {
    async fn login(&self) -> Login {
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .unwrap();
        let base = format!("http://localhost:{}", self.addr.port());
        let form = http.get(format!("{base}/login")).send().await.unwrap();
        let mut cookies = cookies_of(&form);
        let page = form.text().await.unwrap();
        let body = format!(
            "csrf={}&passphrase={}",
            field(&page, "csrf"),
            PASSPHRASE.replace(' ', "+")
        );
        let done = http
            .post(format!("{base}/login"))
            .header("cookie", cookies.join("; "))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(done.status(), 303);
        cookies.extend(cookies_of(&done));
        let mut login = Login {
            http,
            base,
            cookies: cookies.join("; "),
            csrf: String::new(),
        };
        let page = login
            .http
            .get(format!("{}/cv/variants/default", login.base))
            .header("cookie", &login.cookies)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        login.csrf = field(&page.text().await.unwrap(), "csrf");
        login
    }

    async fn seed(&self) -> String {
        let state = control_state(&self.db);
        let result = nexus_mcp::call_tool(
            state,
            &[SCOPE_TOOLS],
            "test",
            "upsert_cv_profile",
            json!({
                "name": "Mara Example",
                "contact": {"email": "mara@example.test", "location": "Hamburg"},
                "profile": "Backend engineer building freight software.",
                "skills": [{"category": "Languages", "items": ["Rust"]}],
                "experience": [{"id": "brightpath", "company": "Brightpath Logistics",
                    "title": "Software Engineer", "date": "03/2019 – 12/2022", "location": "Hamburg",
                    "tier": 1, "bullets": ["Built the routing service."]}],
                "education": [],
            }),
        )
        .await;
        assert_ne!(result.is_error, Some(true));
        let mut tx = self.db.begin_write().await.unwrap();
        cv_facts::lock(&mut tx, "test").await.unwrap();
        tx.commit().await.unwrap();
        let application = applications::create(
            &mut self.db.conn().await.unwrap(),
            "Gate Co",
            "Engineer",
            ApplicationFields {
                status: Some("inbound".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        application.id.to_string()
    }
}

fn control_state(db: &Db) -> Arc<McpState> {
    Arc::new(McpState::new(
        db.clone(),
        "owner-1".to_owned(),
        "owner@example.test".to_owned(),
        Renderer::InProcess,
    ))
}

impl Login {
    /// Every response any test sees passes through here, so the key check covers them all.
    async fn post(&self, path: &str, headers: &[(&str, &str)], body: String) -> (u16, Value) {
        let mut request = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let head = format!("{:?}", response.headers());
        let text = response.text().await.unwrap();
        assert!(
            !head.contains(REAL_KEY) && !text.contains(REAL_KEY),
            "{head} {text}"
        );
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn mint(&self) -> (u16, Value) {
        self.post(
            "/api/voice/session",
            &[("cookie", &self.cookies), ("x-nexus-csrf", &self.csrf)],
            json!({"variant": "default"}).to_string(),
        )
        .await
    }

    async fn voice_id(&self) -> String {
        let (status, body) = self.mint().await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["clientSecret"], EPHEMERAL);
        body["voiceSessionId"].as_str().unwrap().to_owned()
    }

    async fn tool(&self, id: &str, name: &str, arguments: Value) -> (u16, Value) {
        self.post(
            "/api/voice/tool",
            &[
                ("cookie", &self.cookies),
                ("x-nexus-csrf", &self.csrf),
                ("x-nexus-voice-session", id),
            ],
            json!({"name": name, "arguments": arguments}).to_string(),
        )
        .await
    }
}

/// Every row of every table not named `cv_*`, the sequence counters of those not counted, plus the
/// names of the stored files.
async fn outside_cv(db: &Db, dir: &Path) -> String {
    let tables: Vec<String> = sqlx::query_scalar(
        r"SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'cv\_%' ESCAPE '\'
          ORDER BY name",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(tables.len() >= 8, "{tables:?}");
    assert!(
        tables.iter().any(|table| table == "applications"),
        "{tables:?}"
    );
    let mut all = String::new();
    for table in &tables {
        let columns: Vec<String> =
            sqlx::query(AssertSqlSafe(format!("PRAGMA table_info(\"{table}\")")))
                .fetch_all(db.pool())
                .await
                .unwrap()
                .iter()
                .map(|row| row.get::<String, _>("name"))
                .collect();
        let row = columns
            .iter()
            .map(|column| format!("quote(\"{column}\")"))
            .collect::<Vec<_>>()
            .join(" || '|' || ");
        let filter = if table == "sqlite_sequence" {
            r"WHERE name NOT LIKE 'cv\_%' ESCAPE '\'"
        } else {
            ""
        };
        let rows: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT {row} FROM \"{table}\" {filter} ORDER BY 1"
        )))
        .fetch_all(db.pool())
        .await
        .unwrap();
        all.push_str(table);
        all.push_str(&rows.join("\n"));
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("blobs")) {
        let mut names: Vec<_> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        all.push_str(&names.join(","));
    }
    sha256_hex(all.as_bytes())
}

async fn facts_and_history(db: &Db) -> (String, i64) {
    let state = cv_facts::get(&mut db.conn().await.unwrap()).await.unwrap();
    let history = sqlx::query_scalar("SELECT count(*) FROM cv_facts_history")
        .fetch_one(db.pool())
        .await
        .unwrap();
    (serde_json::to_string(&state).unwrap(), history)
}

async fn revision_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM cv_revisions")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Everything a call could use to mark an application applied, whichever tool reads which part.
fn lifecycle_arguments(id: &str, expected: &str, name: &str) -> Value {
    let item = json!({"company": "Gate Co", "role": "Engineer", "status": "applied",
        "jobUrl": "https://jobs.example.test/gate", "appliedAt": "2026-11-06T10:00:00.000Z"});
    json!({
        "id": id,
        "ids": [id],
        "applicationId": id,
        "applicationIds": [id],
        "company": "Gate Co",
        "role": "Engineer",
        "status": "applied",
        "currentStage": "applied",
        "appliedAt": "2026-11-06T10:00:00.000Z",
        "jobUrl": "https://jobs.example.test/gate",
        "type": "stage_changed",
        "metadata": {"toStage": "applied", "toStatus": "applied"},
        "occurredAt": "2026-11-06T10:00:00.000Z",
        "submittedAt": "2026-11-06T10:00:00.000Z",
        "idempotencyKey": format!("gate-{name}"),
        "expectedUpdatedAt": expected,
        "note": "Marked as applied by the gate test",
        "items": [item],
        "name": "Mara Example",
        "experience": [],
        "education": [],
        "profile": "Rewritten by voice",
        "experienceIds": ["brightpath"],
        "skillCategories": ["Languages"],
        "filename": "cv.pdf",
        "mimeType": "application/pdf",
        "contentBase64": "JVBERi0=",
    })
}

const UNKNOWN_NAMES: [&str; 10] = [
    "",
    "mark_applied",
    "PATCH_CV",
    " patch_cv",
    "patch_cv ",
    "patch_cv\u{0}",
    "patch\u{0441}v",
    "tools/call",
    "__proto__",
    "../get_cv_source",
];

#[tokio::test(flavor = "multi_thread")]
async fn no_tool_but_the_allowed_ones_runs_and_no_allowed_one_writes_outside_the_cv_tables() {
    assert_eq!(all_tools().len(), 40);
    let h = start(Duration::from_secs(900), true).await;
    let id = h.seed().await;
    let login = h.login().await;
    let voice = login.voice_id().await;
    let expected = applications::get(&mut h.db.conn().await.unwrap(), id.parse().unwrap())
        .await
        .unwrap()
        .unwrap()
        .updated_at
        .as_str()
        .to_owned();
    let baseline = outside_cv(&h.db, h.dir.path()).await;
    let facts = facts_and_history(&h.db).await;

    let names: Vec<&str> = all_tools()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .chain(UNKNOWN_NAMES)
        .collect();
    let mut refused = 0;
    for name in &names {
        if VOICE_TOOLS.contains(name) {
            continue;
        }
        let revisions = revision_count(&h.db).await;
        let arguments = lifecycle_arguments(&id, &expected, name);
        let (status, body) = login.tool(&voice, name, arguments).await;
        assert_eq!(status, 403, "{name}: {body}");
        assert_eq!(
            body["error"]["code"], "voice_tool_not_allowed",
            "{name}: {body}"
        );
        assert!(body.get("output").is_none(), "{name}: {body}");
        assert_eq!(outside_cv(&h.db, h.dir.path()).await, baseline, "{name}");
        assert_eq!(revision_count(&h.db).await, revisions, "{name}");
        assert_eq!(facts_and_history(&h.db).await, facts, "{name}");
        refused += 1;
    }
    assert_eq!(refused, 32 + UNKNOWN_NAMES.len());

    // The allowed ones, each carrying the same attempt to change an application.
    let with_attempt = |name: &str, own: Value| {
        let mut merged = lifecycle_arguments(&id, &expected, name);
        for (key, value) in own.as_object().unwrap() {
            merged[key] = value.clone();
        }
        merged
    };
    let run = async |name: &str, own: Value| -> Value {
        let (status, body) = login.tool(&voice, name, with_attempt(name, own)).await;
        assert_eq!(status, 200, "{name}: {body}");
        assert_eq!(outside_cv(&h.db, h.dir.path()).await, baseline, "{name}");
        assert_eq!(facts_and_history(&h.db).await, facts, "{name}");
        body
    };
    let read: Value = serde_json::from_str(
        run("get_cv_source", json!({})).await["output"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read["revision"], 0);
    assert!(read["facts"].get("bannedPhrases").is_none());
    assert!(
        read["source"]
            .as_str()
            .unwrap()
            .contains("Built the routing service.")
    );

    let patched = run(
        "patch_cv",
        json!({
            "expectedRevision": 0,
            "edits": [{"oldText": "Built the routing service.", "newText": "Built the routing engine."}],
            "summary": "Said by voice",
            "origin": "mcp",
        }),
    )
    .await;
    assert_eq!(patched["isError"], false, "{patched}");
    let again = run(
        "patch_cv",
        json!({
            "expectedRevision": 1,
            "edits": [{"oldText": "Built the routing engine.", "newText": "Built the routing core."}],
            "summary": "Said by voice again",
        }),
    )
    .await;
    assert_eq!(again["isError"], false, "{again}");
    let origins: Vec<String> =
        sqlx::query_scalar("SELECT origin FROM cv_revisions ORDER BY number")
            .fetch_all(h.db.pool())
            .await
            .unwrap();
    assert_eq!(
        origins,
        ["voice", "voice"],
        "the model can't choose its origin"
    );

    // A claim from a job description stores as a draft, and the model is told to name it to the owner.
    let inflating = run(
        "patch_cv",
        json!({
            "expectedRevision": 2,
            "edits": [{"oldText": "Built the routing core.", "newText": "Built 77 routing cores."}],
            "summary": "An injected claim",
        }),
    )
    .await;
    assert_eq!(inflating["isError"], false, "{inflating}");
    let output = inflating["output"].as_str().unwrap();
    assert!(output.contains("\"state\": \"draft\""), "{output}");
    assert!(output.contains("\"token\": \"77\""), "{output}");
    assert_eq!(revision_count(&h.db).await, 3);

    let hidden = run(
        "patch_cv",
        json!({
            "expectedRevision": 3,
            "edits": [{"oldText": "Built 77 routing cores.", "newText": "Built it. #text(fill: white)[Hidden]"}],
            "summary": "Text no reader sees",
        }),
    )
    .await;
    assert_eq!(hidden["isError"], true, "{hidden}");
    let output = hidden["output"].as_str().unwrap();
    assert!(output.contains("cv_fact_violation"), "{output}");
    assert_eq!(revision_count(&h.db).await, 3);

    // Voice can't approve, however it asks: no tool, no argument, and the origin it can't choose.
    let approvals = async |db: &Db| -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM cv_approvals")
            .fetch_one(db.pool())
            .await
            .unwrap()
    };
    for name in ["approve_cv", "cv_approve", "approve", "approve_cv_revision"] {
        let (status, body) = login.tool(&voice, name, json!({"revision": 3})).await;
        assert_eq!(status, 403, "{name}: {body}");
        assert_eq!(body["error"]["code"], "voice_tool_not_allowed", "{name}");
    }
    for extra in ["approve", "approved", "approvedBy", "state", "confirm"] {
        let mut args = json!({
            "expectedRevision": 3,
            "edits": [{"oldText": "77 routing cores.", "newText": "78 routing cores."}],
            "summary": "With an extra argument",
            "origin": "dashboard",
        });
        args[extra] = true.into();
        let tried = run("patch_cv", args).await;
        // The first stores, as a draft. Those after it find their revision taken.
        let output = tried["output"].as_str().unwrap();
        assert!(
            output.contains("\"state\": \"draft\"") || output.contains("cv_revision_conflict"),
            "{extra}: {output}"
        );
    }
    assert_eq!(approvals(&h.db).await, 0);
    assert_eq!(revision_count(&h.db).await, 4);
    let origins: Vec<String> =
        sqlx::query_scalar("SELECT origin FROM cv_revisions ORDER BY number DESC LIMIT 1")
            .fetch_all(h.db.pool())
            .await
            .unwrap();
    assert_eq!(origins, ["voice"]);

    let rendered = run("render_cv_pdf", json!({})).await;
    assert_eq!(rendered["isError"], false, "{rendered}");
    let output = rendered["output"].as_str().unwrap();
    assert!(!output.contains("JVBERi0"), "no PDF for the model");
    assert!(output.contains("\"state\": \"draft\""), "{output}");
    assert!(
        !output.contains("pdfBytes"),
        "a voice patch makes no PDF: {output}"
    );
    assert!(
        output.contains("\"previews\": 0"),
        "no images for the model: {output}"
    );

    let (status, body) = login
        .tool(&voice, "render_cv_pdf", json!({"saveToApplicationId": id}))
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "voice_argument_not_allowed");
    let (status, _) = login
        .tool(
            &voice,
            "render_cv_pdf",
            json!({"saveToApplicationId": null}),
        )
        .await;
    assert_eq!(status, 403, "even empty");
    assert_eq!(outside_cv(&h.db, h.dir.path()).await, baseline);

    let listed = run("list_cv_revisions", json!({"includeDiff": true})).await;
    assert!(listed["output"].as_str().unwrap().contains("routing core"));
    let reverted = run("revert_cv", json!({"toRevision": 1, "expectedRevision": 4})).await;
    assert_eq!(reverted["isError"], false, "{reverted}");
    assert_eq!(revision_count(&h.db).await, 5);
    assert_eq!(approvals(&h.db).await, 0);

    let filtered = run("list_applications_filtered", json!({"status": ["inbound"]})).await;
    assert!(
        filtered["output"].as_str().unwrap().contains("Gate Co"),
        "{filtered}"
    );
    let application = run("get_application", json!({})).await;
    assert!(
        application["output"]
            .as_str()
            .unwrap()
            .contains("\"inbound\""),
        "{application}"
    );
    run("list_application_events", json!({})).await;

    // The refused arguments were live: run directly, they do what the gate kept out.
    let status_now = |db: Db, id: String| async move {
        applications::get(&mut db.conn().await.unwrap(), id.parse().unwrap())
            .await
            .unwrap()
            .unwrap()
            .status
    };
    assert_eq!(status_now(h.db.clone(), id.clone()).await, "inbound");
    let control = nexus_mcp::call_tool(
        control_state(&h.db),
        &[SCOPE_TOOLS],
        "mcp",
        "record_application_event",
        lifecycle_arguments(&id, &expected, "record_application_event"),
    )
    .await;
    assert_ne!(control.is_error, Some(true), "{:?}", control.content);
    assert_eq!(status_now(h.db.clone(), id.clone()).await, "applied");
    assert_ne!(
        outside_cv(&h.db, h.dir.path()).await,
        baseline,
        "the hash can see it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_live_session_of_this_login_with_its_token_gets_through() {
    let h = start(Duration::from_secs(900), true).await;
    let id = h.seed().await;
    let first = h.login().await;
    let second = h.login().await;
    assert_ne!(first.csrf, second.csrf);
    let voice = first.voice_id().await;
    let baseline = outside_cv(&h.db, h.dir.path()).await;

    let patch = json!({
        "name": "patch_cv",
        "arguments": {
            "expectedRevision": 0,
            "edits": [{"oldText": "Built the routing service.", "newText": "Built the routing core."}],
            "summary": "Must not land",
        },
    })
    .to_string();
    type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, u16, &'a str);
    let cases: Vec<Case> = vec![
        (
            "no cookie",
            vec![
                ("x-nexus-csrf", &first.csrf),
                ("x-nexus-voice-session", &voice),
            ],
            401,
            "",
        ),
        (
            "no CSRF",
            vec![
                ("cookie", &first.cookies),
                ("x-nexus-voice-session", &voice),
            ],
            403,
            "csrf_invalid",
        ),
        (
            "wrong CSRF",
            vec![
                ("cookie", &first.cookies),
                ("x-nexus-csrf", "nope"),
                ("x-nexus-voice-session", &voice),
            ],
            403,
            "csrf_invalid",
        ),
        (
            "another login's CSRF",
            vec![
                ("cookie", &first.cookies),
                ("x-nexus-csrf", &second.csrf),
                ("x-nexus-voice-session", &voice),
            ],
            403,
            "csrf_invalid",
        ),
        (
            "no voice id",
            vec![("cookie", &first.cookies), ("x-nexus-csrf", &first.csrf)],
            403,
            "voice_session_invalid",
        ),
        (
            "wrong voice id",
            vec![
                ("cookie", &first.cookies),
                ("x-nexus-csrf", &first.csrf),
                ("x-nexus-voice-session", "00ff"),
            ],
            403,
            "voice_session_invalid",
        ),
        (
            "another login's voice id",
            vec![
                ("cookie", &second.cookies),
                ("x-nexus-csrf", &second.csrf),
                ("x-nexus-voice-session", &voice),
            ],
            403,
            "voice_session_invalid",
        ),
    ];
    for (label, headers, status, code) in cases {
        let (got, body) = first.post("/api/voice/tool", &headers, patch.clone()).await;
        assert_eq!(got, status, "{label}: {body}");
        if !code.is_empty() {
            assert_eq!(body["error"]["code"], code, "{label}: {body}");
        }
        assert_eq!(revision_count(&h.db).await, 0, "{label}");
    }

    let hits = h.stub.hits.load(Ordering::SeqCst);
    let mint = json!({"variant": "default"}).to_string();
    let (status, _) = first
        .post(
            "/api/voice/session",
            &[("x-nexus-csrf", &first.csrf)],
            mint.clone(),
        )
        .await;
    assert_eq!(status, 401, "no cookie");
    let (status, body) = first
        .post(
            "/api/voice/session",
            &[("cookie", &first.cookies)],
            mint.clone(),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (403, &json!("csrf_invalid"))
    );
    let (status, body) = first
        .post(
            "/api/voice/session",
            &[("cookie", &first.cookies), ("x-nexus-csrf", &first.csrf)],
            json!({"variant": "no-such-variant"}).to_string(),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (404, &json!("cv_variant_not_found"))
    );
    assert_eq!(
        h.stub.hits.load(Ordering::SeqCst),
        hits,
        "nothing was minted"
    );

    // The same call does get through with everything in order, so the refusals above meant it.
    let (status, body) = first
        .post(
            "/api/voice/tool",
            &[
                ("cookie", &first.cookies),
                ("x-nexus-csrf", &first.csrf),
                ("x-nexus-voice-session", &voice),
            ],
            patch,
        )
        .await;
    assert_eq!((status, &body["isError"]), (200, &json!(false)), "{body}");
    assert_eq!(revision_count(&h.db).await, 1);
    assert_eq!(outside_cv(&h.db, h.dir.path()).await, baseline);
    let _ = id;
}

impl Login {
    async fn status(&self, id: &str) -> (u16, String, Value) {
        let mut request = self
            .http
            .get(format!("{}/api/voice/status", self.base))
            .header("cookie", &self.cookies);
        if !id.is_empty() {
            request = request.header("x-nexus-voice-session", id);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let store = response.headers()["cache-control"]
            .to_str()
            .unwrap()
            .to_owned();
        let text = response.text().await.unwrap();
        (
            status,
            store,
            serde_json::from_str(&text).unwrap_or(Value::Null),
        )
    }

    async fn log_out(&self) {
        let page = self
            .http
            .get(format!("{}/logout", self.base))
            .header("cookie", &self.cookies)
            .send()
            .await
            .unwrap();
        let body = format!("csrf={}", field(&page.text().await.unwrap(), "csrf"));
        let done = self
            .http
            .post(format!("{}/logout", self.base))
            .header("cookie", &self.cookies)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(done.status(), 303);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_route_tells_a_live_session_from_every_way_of_ending() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let login = h.login().await;
    let id = login.voice_id().await;

    let (status, store, body) = login.status(&id).await;
    assert_eq!((status, store.as_str()), (200, "no-store"), "{body}");
    assert_eq!(body, json!({"live": true}));

    for wrong in ["", "not-a-session"] {
        let (status, _, body) = login.status(wrong).await;
        assert_eq!(status, 403, "{wrong}");
        assert_eq!(body["error"]["code"], "voice_session_invalid", "{wrong}");
    }
    let other = h.login().await;
    let (status, _, body) = other.status(&id).await;
    assert_eq!(status, 403, "another login's session");
    assert_eq!(body["error"]["code"], "voice_session_invalid");

    login.voice_id().await;
    login.voice_id().await;
    let (status, _, body) = login.status(&id).await;
    assert_eq!(status, 403, "evicted");
    assert_eq!(body["error"]["code"], "voice_session_invalid");

    let live = login.voice_id().await;
    assert_eq!(login.status(&live).await.0, 200);
    login.log_out().await;
    let (status, _, body) = login.status(&live).await;
    assert_eq!(status, 401, "logged out: {body}");
    assert_eq!(other.status(&live).await.0, 403, "still not theirs");

    let expired = start(Duration::from_millis(200), true).await;
    expired.seed().await;
    let login = expired.login().await;
    let id = login.voice_id().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(login.status(&id).await.0, 403, "past its lifetime");
}

/// A busy dashboard isn't a lost server: the page ends a session after two failed checks.
#[tokio::test(flavor = "multi_thread")]
async fn the_status_route_answers_while_the_owner_pool_is_full() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let login = h.login().await;
    let id = login.voice_id().await;
    let mut held = Vec::new();
    for _ in 0..OWNER.in_flight {
        let mut socket = tokio::net::TcpStream::connect(h.addr).await.unwrap();
        let head = format!(
            "POST /api/voice/tool HTTP/1.1\r\nHost: localhost:{}\r\nCookie: {}\r\nContent-Length: 100\r\n\r\n",
            h.addr.port(),
            login.cookies
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        held.push(socket);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let page = {
        let (http, url, cookies) = (
            login.http.clone(),
            format!("{}/cv", login.base),
            login.cookies.clone(),
        );
        tokio::spawn(async move {
            let response = http
                .get(url)
                .header("cookie", cookies)
                .send()
                .await
                .unwrap();
            response.status().as_u16()
        })
    };
    let started = std::time::Instant::now();
    assert_eq!(login.status(&id).await.0, 200);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(page.await.unwrap(), 503, "the pool really was full");
    drop(held);
}

/// Reads a stream until the server ends it.
async fn read_to_end_within(socket: &mut tokio::net::TcpStream, limit: Duration) -> String {
    let mut seen = Vec::new();
    tokio::time::timeout(limit, socket.read_to_end(&mut seen))
        .await
        .expect("the stream ended in time")
        .unwrap();
    String::from_utf8_lossy(&seen).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn logging_out_ends_the_streams_at_once_and_only_the_logged_out_browser_stays_out() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let (leaving, staying) = (h.login().await, h.login().await);
    let open = |cookies: String| async move {
        let mut socket = tokio::net::TcpStream::connect(h.addr).await.unwrap();
        let head = format!(
            "GET /api/cv/default/events HTTP/1.1\r\nHost: localhost:{}\r\nCookie: {cookies}\r\nConnection: close\r\n\r\n",
            h.addr.port()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        let mut first = [0u8; 12];
        socket.read_exact(&mut first).await.unwrap();
        (socket, String::from_utf8_lossy(&first).into_owned())
    };
    let (mut gone, first) = open(leaving.cookies.clone()).await;
    assert_eq!(first, "HTTP/1.1 200");
    let (mut other, _) = open(staying.cookies.clone()).await;

    leaving.log_out().await;
    let limit = Duration::from_secs(2);
    assert!(
        read_to_end_within(&mut gone, limit)
            .await
            .ends_with("0\r\n\r\n")
    );
    assert!(
        read_to_end_within(&mut other, limit)
            .await
            .ends_with("0\r\n\r\n")
    );

    let (mut refused, first) = open(leaving.cookies.clone()).await;
    assert_eq!(first, "HTTP/1.1 401");
    drop(refused.shutdown().await);
    let (_back, first) = open(staying.cookies.clone()).await;
    assert_eq!(first, "HTTP/1.1 200", "the other login just reconnects");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_session_is_refused_even_though_it_was_good() {
    let h = start(Duration::from_secs(2), true).await;
    h.seed().await;
    let login = h.login().await;
    let voice = login.voice_id().await;
    let (status, _) = login.tool(&voice, "get_cv_source", json!({})).await;
    assert_eq!(status, 200);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let (status, body) = login.tool(&voice, "get_cv_source", json!({})).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (403, &json!("voice_session_invalid"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mint_sends_the_real_key_only_to_openai_and_only_the_list_of_tools() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let login = h.login().await;
    let (status, body) = login.mint().await;
    assert_eq!(status, 200);
    assert_eq!(body["clientSecret"], EPHEMERAL);
    assert_eq!(body["lifetimeSeconds"], 900);
    assert_eq!(body["idleSeconds"], 300);
    assert!(
        body["callsUrl"]
            .as_str()
            .unwrap()
            .ends_with("/v1/realtime/calls")
    );

    assert_eq!(
        h.stub.authorization.lock().unwrap()[0],
        format!("Bearer {REAL_KEY}")
    );
    let sent = h.stub.bodies.lock().unwrap()[0].clone();
    assert!(!sent.to_string().contains(REAL_KEY));
    assert_eq!(sent["expires_after"]["anchor"], "created_at");
    assert_eq!(sent["expires_after"]["seconds"], 60);
    let session = &sent["session"];
    assert_eq!(session["type"], "realtime");
    assert_eq!(session["model"], "gpt-realtime-2.1-mini");
    assert_eq!(session["tool_choice"], "auto");
    assert_eq!(session["audio"]["output"]["voice"], "marin");
    assert!(session["audio"]["input"]["turn_detection"].is_null());
    let tools = session["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, VOICE_TOOLS);
    let patch = tools
        .iter()
        .find(|tool| tool["name"] == "patch_cv")
        .unwrap();
    assert!(patch["parameters"]["properties"].get("origin").is_none());
    let render = tools
        .iter()
        .find(|tool| tool["name"] == "render_cv_pdf")
        .unwrap();
    assert!(
        render["parameters"]["properties"]
            .get("saveToApplicationId")
            .is_none()
    );
    assert!(
        session["instructions"]
            .as_str()
            .unwrap()
            .contains("named default")
    );

    h.stub.fail.store(true, Ordering::SeqCst);
    let (status, body) = login.mint().await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "voice_upstream_failed");
    assert!(
        !body.to_string().contains("Incorrect"),
        "upstream's words stay out: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn minting_is_limited_and_a_failed_mint_counts() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let login = h.login().await;
    h.stub.fail.store(true, Ordering::SeqCst);
    for attempt in 0..MINTS_PER_HOUR {
        let wanted = if attempt < 3 { 502 } else { 200 };
        if attempt == 3 {
            h.stub.fail.store(false, Ordering::SeqCst);
        }
        assert_eq!(login.mint().await.0, wanted, "{attempt}");
    }
    let (status, body) = login.mint().await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["error"]["code"], "voice_rate_limited");
    assert_eq!(h.stub.hits.load(Ordering::SeqCst), MINTS_PER_HOUR);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_long_job_description_can_not_burn_the_budget_and_a_huge_body_is_refused() {
    let h = start(Duration::from_secs(900), true).await;
    let id = h.seed().await;
    sqlx::query("UPDATE applications SET job_description = ? WHERE id = ?")
        .bind("Lorem ipsum ü ".repeat(20_000))
        .bind(id.parse::<i64>().unwrap())
        .execute(h.db.pool())
        .await
        .unwrap();
    let login = h.login().await;
    let voice = login.voice_id().await;
    let (status, body) = login
        .tool(&voice, "get_application", json!({"id": id}))
        .await;
    assert_eq!(status, 200);
    let output = body["output"].as_str().unwrap();
    assert!(output.len() <= MAX_MODEL_OUTPUT_BYTES, "{}", output.len());
    assert!(
        output.ends_with("more bytes not shown.]"),
        "{}",
        &output[output.len() - 80..]
    );

    let huge = json!({"name": "get_cv_source", "arguments": {"pad": "x".repeat(2 * 1024 * 1024)}});
    let (status, _) = login
        .post(
            "/api/voice/tool",
            &[
                ("cookie", &login.cookies),
                ("x-nexus-csrf", &login.csrf),
                ("x-nexus-voice-session", &voice),
            ],
            huge.to_string(),
        )
        .await;
    assert_eq!(status, 413);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answers_in_a_submission_never_reach_the_model() {
    let h = start(Duration::from_secs(900), true).await;
    let id = h.seed().await;
    let scopes = [SCOPE_TOOLS, SCOPE_SUBMISSIONS];
    let uploaded = nexus_mcp::call_tool(
        control_state(&h.db),
        &scopes,
        "mcp",
        "upload_document_content",
        json!({
            "filename": "cv.pdf",
            "mimeType": "application/pdf",
            "contentBase64": "JVBERi0xLjQK",
            "applicationIds": [id],
        }),
    )
    .await;
    let document = uploaded.structured_content.unwrap();
    let document_id = document["id"].as_str().map_or_else(
        || document["document"]["id"].to_string().replace('"', ""),
        str::to_owned,
    );
    let recorded = nexus_mcp::call_tool(
        control_state(&h.db),
        &scopes,
        "mcp",
        "record_application_submission",
        json!({
            "applicationId": id,
            "submittedAt": "2026-11-06T10:00:00.000Z",
            "idempotencyKey": "gate-submission",
            "answers": [{"question": "Why us?", "answer": "SENTINEL-ANSWER-TEXT"}],
            "documentIds": [document_id],
            "policy": {
                "humanReviewed": true,
                "identityConsistent": true,
                "factsVerified": true,
                "profileConsistencyStatus": "verified",
            },
        }),
    )
    .await;
    assert_ne!(recorded.is_error, Some(true), "{:?}", recorded.content);
    let login = h.login().await;
    let voice = login.voice_id().await;
    for (name, arguments) in [
        ("get_application", json!({"id": id})),
        ("list_application_events", json!({"applicationId": id})),
        ("list_applications_filtered", json!({})),
    ] {
        let (status, body) = login.tool(&voice, name, arguments).await;
        assert_eq!(status, 200, "{name}");
        assert_eq!(body["isError"], false, "{name}: {body}");
        assert!(
            !body.to_string().contains("SENTINEL-ANSWER-TEXT"),
            "{name}: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_with_no_variant_uses_the_one_the_session_was_opened_for() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let state = control_state(&h.db);
    let created = nexus_mcp::call_tool(
        state,
        &[SCOPE_TOOLS],
        "test",
        "patch_cv",
        json!({"variant": "platform", "createFrom": "default", "expectedRevision": 0,
            "edits": [], "summary": "New variant"}),
    )
    .await;
    assert_ne!(created.is_error, Some(true), "{:?}", created.content);
    let login = h.login().await;
    let (status, body) = login
        .post(
            "/api/voice/session",
            &[("cookie", &login.cookies), ("x-nexus-csrf", &login.csrf)],
            json!({"variant": "platform"}).to_string(),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let voice = body["voiceSessionId"].as_str().unwrap();
    let (_, body) = login.tool(voice, "get_cv_source", json!({})).await;
    assert!(
        body["output"]
            .as_str()
            .unwrap()
            .contains("\"variant\": \"platform\""),
        "{body}"
    );
    let (_, body) = login
        .tool(voice, "get_cv_source", json!({"variant": "default"}))
        .await;
    assert!(
        body["output"]
            .as_str()
            .unwrap()
            .contains("\"variant\": \"default\""),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_key_both_routes_say_voice_is_off() {
    let h = start(Duration::from_secs(900), false).await;
    h.seed().await;
    let login = h.login().await;
    let (status, body) = login.mint().await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (503, &json!("voice_disabled")),
        "{body}"
    );
    let (status, body) = login.tool("anything", "get_cv_source", json!({})).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (503, &json!("voice_disabled")),
        "{body}"
    );
    assert_eq!(h.stub.hits.load(Ordering::SeqCst), 0);
}

// The real binary, for what only a separate process can show.

struct Process {
    child: Child,
    base: String,
    log: std::path::PathBuf,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn spawn_binary(dir: &Path, stub_url: &str, key_file: Option<&Path>) -> Process {
    let port = free_port();
    let log = dir.join("server.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut command = nexus();
    command
        .env("NEXUS_OWNER_ID", "owner-1")
        .env("NEXUS_OWNER_EMAIL", "owner@example.test")
        .args(["serve", "--insecure-local-only", "--bind"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("--data-dir")
        .arg(dir.join("data"))
        .args(["--voice-test-base-url", stub_url])
        .stdout(Stdio::from(out.try_clone().unwrap()))
        .stderr(Stdio::from(out));
    if let Some(file) = key_file {
        command.env("NEXUS_OPENAI_API_KEY_FILE", file);
    }
    let process = Process {
        child: command.spawn().unwrap(),
        base: format!("http://localhost:{port}"),
        log,
    };
    let http = reqwest::Client::new();
    for _ in 0..200 {
        if let Ok(reply) = http.get(format!("{}/healthz", process.base)).send().await
            && reply.status().is_success()
        {
            return process;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "did not start: {}",
        std::fs::read_to_string(dir.join("server.log")).unwrap()
    );
}

/// The peers of the process's TCP sockets. Listening ones have no peer.
fn tcp_peers(pid: u32) -> Vec<(IpAddr, u16, String)> {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|link| {
            link.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect();
    let mut peers = Vec::new();
    for file in ["tcp", "tcp6"] {
        let table = std::fs::read_to_string(format!("/proc/{pid}/net/{file}")).unwrap_or_default();
        for line in table.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if !inodes.iter().any(|inode| inode == parts[9]) {
                continue;
            }
            let (address, port) = parts[2].split_once(':').unwrap();
            let port = u16::from_str_radix(port, 16).unwrap();
            let ip = if address.len() == 8 {
                let bytes = u32::from_str_radix(address, 16).unwrap().to_le_bytes();
                IpAddr::from(bytes)
            } else {
                let mut octets = [0u8; 16];
                for (index, chunk) in address.as_bytes().chunks(8).enumerate() {
                    let word =
                        u32::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
                    octets[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                IpAddr::from(octets)
            };
            peers.push((ip, port, parts[3].to_owned()));
        }
    }
    peers
}

async fn local_csrf(http: &reqwest::Client, base: &str) -> String {
    let page = http
        .get(format!("{base}/cv/variants/default"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    field(&page.text().await.unwrap(), "csrf")
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_key_file_the_process_opens_no_outbound_connection() {
    let root = tempfile::tempdir().unwrap();
    // A stand-in that counts everything reaching it, where a mint would go.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_url = format!("http://{}", listener.local_addr().unwrap());
    let stub_port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    let process = spawn_binary(root.path(), &stub_url, None).await;
    let http = reqwest::Client::new();
    let csrf = local_csrf(&http, &process.base).await;
    for (path, body) in [
        ("/api/voice/session", json!({"variant": "default"})),
        ("/api/voice/tool", json!({"name": "get_cv_source"})),
    ] {
        let reply = http
            .post(format!("{}{path}", process.base))
            .header("x-nexus-csrf", &csrf)
            .header("x-nexus-voice-session", "anything")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 503, "{path}");
        let body: Value = serde_json::from_str(&reply.text().await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "voice_disabled");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no OpenAI key")
        );
    }
    let page = http
        .get(format!("{}/cv/variants/default", process.base))
        .send()
        .await
        .unwrap();
    assert!(page.text().await.unwrap().contains("Voice is off"));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "nothing reached the stand-in"
    );
    for (ip, port, state) in tcp_peers(process.child.id()) {
        assert!(
            ip.is_unspecified() || ip.is_loopback() || ip.to_canonical().is_loopback(),
            "{ip}:{port} {state}"
        );
        assert_ne!(port, stub_port, "{state}");
        assert_ne!(state, "02", "a connection being opened: {ip}:{port}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_stays_out_of_the_environment_the_arguments_and_the_log() {
    let root = tempfile::tempdir().unwrap();
    let key_file = root.path().join("openai-key");
    std::fs::write(&key_file, format!("{REAL_KEY}\n")).unwrap();
    std::fs::set_permissions(
        &key_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o400),
    )
    .unwrap();
    let (stub, stub_url) = start_stub().await;
    let process = spawn_binary(root.path(), &stub_url, Some(&key_file)).await;
    let pid = process.child.id();
    let http = reqwest::Client::new();
    let csrf = local_csrf(&http, &process.base).await;

    let minted: Value = serde_json::from_str(
        &http
            .post(format!("{}/api/voice/session", process.base))
            .header("x-nexus-csrf", &csrf)
            .body(json!({"variant": "default"}).to_string())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(minted["clientSecret"], EPHEMERAL);
    assert_eq!(
        stub.authorization.lock().unwrap()[0],
        format!("Bearer {REAL_KEY}")
    );

    let secret_argument = "SENTINEL-ARGUMENT-TEXT";
    for name in [
        "get_cv_source",
        "delete_application",
        "made_up_SENTINEL_tool",
    ] {
        let reply = http
            .post(format!("{}/api/voice/tool", process.base))
            .header("x-nexus-csrf", &csrf)
            .header(
                "x-nexus-voice-session",
                minted["voiceSessionId"].as_str().unwrap(),
            )
            .body(json!({"name": name, "arguments": {"note": secret_argument}}).to_string())
            .send()
            .await
            .unwrap();
        let text = reply.text().await.unwrap();
        assert!(!text.contains(REAL_KEY), "{text}");
    }

    for file in ["environ", "cmdline"] {
        let bytes = std::fs::read(format!("/proc/{pid}/{file}")).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(REAL_KEY),
            "{file}"
        );
    }
    let log = std::fs::read_to_string(&process.log).unwrap();
    assert!(log.contains("voice=true"), "{log}");
    assert!(
        log.contains("tool=\"get_cv_source\"") || log.contains("tool=get_cv_source"),
        "{log}"
    );
    assert!(log.contains("delete_application"), "{log}");
    assert!(
        log.contains("tool=\"unknown\"") || log.contains("tool=unknown"),
        "{log}"
    );
    for secret in [REAL_KEY, EPHEMERAL, secret_argument, "SENTINEL_tool"] {
        assert!(!log.contains(secret), "{secret} in {log}");
    }
}

#[tokio::test]
async fn a_local_stand_in_can_not_be_named_on_a_real_server() {
    let root = tempfile::tempdir().unwrap();
    let output = nexus()
        .env("NEXUS_PUBLIC_BASE_URL", "http://localhost:3999")
        .args(["serve", "--data-dir"])
        .arg(root.path())
        .args(["--voice-test-base-url", "http://127.0.0.1:9"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("--insecure-local-only"), "{text}");
    let help = nexus().args(["serve", "--help"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("voice-test-base-url"));
}

/// The owner's correction holds against a voice session too: its revert is a draft, and naming the old
/// revision gets no approved state. Its undo back to the newest approved text needs no tap.
#[tokio::test(flavor = "multi_thread")]
async fn voice_cannot_bring_back_text_the_owner_replaced() {
    let h = start(Duration::from_secs(900), true).await;
    h.seed().await;
    let state = control_state(&h.db);
    let login = h.login().await;
    let voice = login.voice_id().await;
    let output =
        |body: &Value| -> Value { serde_json::from_str(body["output"].as_str().unwrap()).unwrap() };
    let source = {
        let (_, body) = login.tool(&voice, "get_cv_source", json!({})).await;
        output(&body)["source"].as_str().unwrap().to_owned()
    };
    let approve = async |revision: i64| {
        let review = nexus_mcp::owner::review(state.clone(), "default")
            .await
            .unwrap();
        assert_eq!(review.revision, revision);
        let done = nexus_mcp::owner::approve(
            state.clone(),
            "default",
            revision,
            &review.source_sha256,
            &review.facts_sha256,
            "dashboard",
        )
        .await;
        assert_ne!(done.is_error, Some(true), "{done:?}");
    };
    let patch = async |expected: i64, old: &str, new: &str| {
        let (status, body) = login
            .tool(
                &voice,
                "patch_cv",
                json!({"expectedRevision": expected, "summary": "By voice",
                    "edits": [{"oldText": old, "newText": new}]}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        output(&body)
    };
    assert!(source.contains("Built the routing service."), "{source}");
    patch(
        0,
        "Built the routing service.",
        "Led a team of 20 engineers.",
    )
    .await;
    approve(1).await;
    patch(1, "a team of 20", "a team of 12").await;
    approve(2).await;

    let revert = async |to: i64, expected: i64| {
        let (status, body) = login
            .tool(
                &voice,
                "revert_cv",
                json!({"toRevision": to, "expectedRevision": expected}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        output(&body)
    };
    let reverted = revert(1, 2).await;
    assert_eq!(reverted["state"], "draft", "{reverted}");
    let (_, body) = login
        .tool(&voice, "render_cv_pdf", json!({"revision": 1}))
        .await;
    assert_eq!(output(&body)["state"], "draft", "{body}");
    let (_, body) = login
        .tool(&voice, "get_cv_source", json!({"revision": 1}))
        .await;
    assert_eq!(output(&body)["state"], "draft", "{body}");

    let undone = revert(2, 3).await;
    assert_eq!(undone["state"], "approved", "{undone}");
}
