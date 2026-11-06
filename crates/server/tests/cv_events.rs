//! The live preview's stream over a real socket and in the real binary: who may open one, how many,
//! what ends one, and that shutdown doesn't wait for them.

mod support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nexus_core::Db;
use nexus_core::repo::cv_facts;
use nexus_mcp::{McpState, Renderer};
use nexus_server::config::{Access, ServeConfig};
use nexus_server::serve::{self, OWNER};
use nexus_web::{HEARTBEAT, MAX_STREAMS};
use serde_json::{Value, json};
use support::Node;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

const EVENTS: &str = "/api/cv/default/events";

struct Local {
    addr: SocketAddr,
    db: Db,
    _dir: tempfile::TempDir,
}

fn profile() -> Value {
    json!({
        "name": "Mara Example",
        "contact": {"email": "mara@example.test", "location": "Hamburg"},
        "profile": "Backend engineer building freight software.",
        "skills": [{"category": "Languages", "items": ["Rust"]}],
        "experience": [{"id": "brightpath", "company": "Brightpath Logistics",
            "title": "Software Engineer", "date": "03/2019 – 12/2022", "location": "Hamburg",
            "tier": 1, "bullets": ["Built the routing service."]}],
        "education": [],
    })
}

async fn local() -> Local {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServeConfig {
        bind: addr,
        data_dir: dir.path().to_path_buf(),
        access: Access::InsecureLocalOnly,
        owner_id: Some("owner-1".to_owned()),
        owner_email: Some("owner@example.test".to_owned()),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: None,
        log_level: tracing::Level::INFO,
        voice: None,
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
    let state = std::sync::Arc::new(McpState::new(
        db.clone(),
        "owner-1".to_owned(),
        "owner@example.test".to_owned(),
        Renderer::InProcess,
    ));
    let result = nexus_mcp::call_tool(
        state,
        &[nexus_core::auth::SCOPE_TOOLS],
        "test",
        "upsert_cv_profile",
        profile(),
    )
    .await;
    assert_ne!(result.is_error, Some(true));
    let mut tx = db.begin_write().await.unwrap();
    cv_facts::lock(&mut tx, "test").await.unwrap();
    tx.commit().await.unwrap();
    Local {
        addr,
        db,
        _dir: dir,
    }
}

/// A browser's view of a stream: raw bytes as they arrive.
struct Stream {
    socket: TcpStream,
    seen: String,
}

impl Stream {
    async fn open(addr: SocketAddr, path: &str, extra: &str) -> Self {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n");
        socket.write_all(request.as_bytes()).await.unwrap();
        Self {
            socket,
            seen: String::new(),
        }
    }

    /// Reads until `text` has arrived.
    async fn wait_for(&mut self, text: &str, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while !self.seen.contains(text) {
            let mut buffer = [0u8; 4096];
            let read = tokio::time::timeout_at(deadline, self.socket.read(&mut buffer))
                .await
                .unwrap_or_else(|_| panic!("no {text:?} in time, saw: {}", self.seen))
                .unwrap();
            assert_ne!(read, 0, "closed before {text:?}, saw: {}", self.seen);
            self.seen
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    }

    /// Reads until the server ends the response.
    async fn wait_for_end(&mut self, limit: Duration) {
        let mut rest = Vec::new();
        timeout(limit, self.socket.read_to_end(&mut rest))
            .await
            .expect("the stream ended in time")
            .unwrap();
        self.seen.push_str(&String::from_utf8_lossy(&rest));
    }
}

async fn mcp_call(addr: SocketAddr, name: &str, arguments: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}});
    let reply = reqwest::Client::new()
        .post(format!("http://{addr}/api/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let text = reply.text().await.unwrap();
    serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)["result"].clone()
}

async fn status_of(addr: SocketAddr, path: &str) -> String {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    // Bounded: a stream that should have been refused would otherwise never end.
    timeout(SOON, socket.read_to_string(&mut out))
        .await
        .expect("a complete response, not an open stream")
        .unwrap();
    out
}

const SOON: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn a_revision_stored_through_mcp_reaches_an_open_stream() {
    let l = local().await;
    let mut stream = Stream::open(l.addr, EVENTS, "").await;
    stream.wait_for("event: head", SOON).await;
    let head = stream.seen.to_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(head.contains("x-accel-buffering: no"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    stream.wait_for("\"revision\":0", SOON).await;

    let result = mcp_call(
        l.addr,
        "patch_cv",
        json!({
            "expectedRevision": 0,
            "edits": [{"oldText": "Built the routing service.", "newText": "Built the routing engine."}],
            "summary": "From an agent",
            "origin": "telegram",
        }),
    )
    .await;
    assert_ne!(result["isError"], true, "{result}");
    stream.wait_for("event: revision", SOON).await;
    stream.wait_for("\"revision\":1", SOON).await;
    assert!(
        stream.seen.contains("\"origin\":\"telegram\""),
        "{}",
        stream.seen
    );
    assert!(
        stream.seen.contains("\"summary\":\"From an agent\""),
        "{}",
        stream.seen
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM cv_revisions")
        .fetch_one(l.db.pool())
        .await
        .unwrap();
    assert_eq!(stored, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn generate_tailored_cv_is_heard_on_its_own_variant_only() {
    let l = local().await;
    let mut other = Stream::open(l.addr, EVENTS, "").await;
    other.wait_for("event: head", SOON).await;

    let created = mcp_call(
        l.addr,
        "create_application",
        json!({"company": "Stream Co", "role": "Engineer"}),
    )
    .await;
    let application: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    let id = application["id"].as_i64().map_or_else(
        || application["id"].as_str().unwrap().to_owned(),
        |id| id.to_string(),
    );
    let result = mcp_call(
        l.addr,
        "generate_tailored_cv",
        json!({"applicationId": id, "experienceIds": ["brightpath"], "skillCategories": ["Languages"]}),
    )
    .await;
    assert_ne!(result["isError"], true, "{result}");
    let path = format!("/api/cv/application-{id}/events");
    let mut own = Stream::open(l.addr, &path, "").await;
    own.wait_for("\"revision\":1", SOON).await;

    let patched = mcp_call(
        l.addr,
        "patch_cv",
        json!({
            "variant": format!("application-{id}"),
            "expectedRevision": 1,
            "edits": [{"oldText": "Built the routing service.", "newText": "Built the routing core."}],
            "summary": "Another",
        }),
    )
    .await;
    assert_ne!(patched["isError"], true, "{patched}");
    own.wait_for("\"revision\":2", SOON).await;
    assert!(!other.seen.contains("\"revision\":2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn at_most_four_streams_then_a_clear_refusal_and_one_going_away_frees_a_slot() {
    assert_eq!(MAX_STREAMS, 4);
    let l = local().await;
    let mut streams = Vec::new();
    for _ in 0..MAX_STREAMS {
        let mut stream = Stream::open(l.addr, EVENTS, "").await;
        stream.wait_for("event: head", SOON).await;
        streams.push(stream);
    }
    let refused = status_of(l.addr, EVENTS).await;
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    assert!(refused.contains("at most 4"), "{refused}");
    assert!(refused.to_lowercase().contains("retry-after"), "{refused}");

    // Streams hold no owner request slot: more ordinary requests than the pool has still run.
    let mut asks = Vec::new();
    for _ in 0..OWNER.in_flight + 2 {
        asks.push(tokio::spawn(status_of(l.addr, "/cv")));
    }
    for ask in asks {
        let page = ask.await.unwrap();
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    }

    drop(streams.pop());
    let started = Instant::now();
    loop {
        let mut next = Stream::open(l.addr, EVENTS, "").await;
        let mut buffer = [0u8; 16];
        timeout(SOON, next.socket.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        if buffer.starts_with(b"HTTP/1.1 200") {
            break;
        }
        assert!(
            started.elapsed() < HEARTBEAT + SOON,
            "the slot came back within one heartbeat at worst"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("slot freed after {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_stream_gets_heartbeats_and_outlives_the_owner_deadline() {
    assert!(HEARTBEAT < Duration::from_secs(30));
    let l = local().await;
    let mut stream = Stream::open(l.addr, EVENTS, "").await;
    stream.wait_for(": heartbeat", HEARTBEAT + SOON).await;
    tokio::time::sleep(OWNER.deadline + SOON).await;
    let mut buffer = [0u8; 4096];
    let read = timeout(SOON, stream.socket.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(read, 0, "still open past the deadline");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_variant_is_a_404_and_gives_its_slot_back() {
    let l = local().await;
    for _ in 0..MAX_STREAMS + 1 {
        let reply = status_of(l.addr, "/api/cv/no-such-variant/events").await;
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_open_streams_well_inside_the_drain_timeout() {
    let root = tempfile::tempdir().unwrap();
    let (mut node, _) = Node::create(root.path(), &[]).await;

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let base = format!("http://{}", node.addr);
    let form = http.get(format!("{base}/login")).send().await.unwrap();
    let mut cookies: Vec<String> = form
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
        .collect();
    let page = form.text().await.unwrap();
    let marker = "name=\"csrf\"";
    let at = page.find(marker).unwrap();
    let start = page[..at].rfind('<').unwrap();
    let csrf = page[start..]
        .split("value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let body = format!(
        "csrf={csrf}&passphrase={}",
        support::PASSPHRASE.replace(' ', "+")
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
    cookies.extend(done.headers().get_all("set-cookie").iter().map(|value| {
        value
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }));
    let cookie = format!("Cookie: {}\r\n", cookies.join("; "));

    let logged_out = status_of(node.addr, EVENTS).await;
    assert!(
        logged_out.starts_with("HTTP/1.1 401"),
        "not without a session: {logged_out}"
    );

    let mut streams = Vec::new();
    for _ in 0..MAX_STREAMS {
        let mut stream = Stream::open(node.addr, EVENTS, &cookie).await;
        stream.wait_for("event: head", SOON).await;
        streams.push(stream);
    }

    let started = Instant::now();
    node.terminate();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "shutdown took {took:?} with streams open"
    );
    for stream in &mut streams {
        stream.wait_for_end(SOON).await;
    }
    let logs = node.logs();
    assert!(!logs.contains("did not finish in time"), "{logs}");
    assert!(logs.contains("stopped"), "{logs}");
}
