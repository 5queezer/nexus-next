//! What keeps one caller from taking the server from another: request pools, deadlines, connection
//! limits, and what local mode will answer to.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use futures_util::{StreamExt, stream};
use http_body_util::BodyExt;
use nexus_auth::{AuthConfig, issue_api_token};
use nexus_core::Db;
use nexus_server::config::{Access, ServeConfig};
use nexus_server::serve::{
    self, BODY_IDLE_TIMEOUT, HANDLER_TIMEOUT, HEADER_READ_TIMEOUT, LARGE_BODY_BYTES,
    LARGE_BODY_DEADLINE, LARGE_BODY_WAIT, MAX_CONNECTIONS, MAX_LARGE_BODIES, MCP, OWNER, PUBLIC,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

fn owner(base: &str) -> Access {
    Access::Owner(AuthConfig {
        public_base_url: base.to_owned(),
        owner_passphrase_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA"
            .to_owned(),
        owner_passphrase_file: None,
        trusted_proxy: None,
        redirect_hosts: Vec::new(),
    })
}

fn config(dir: &std::path::Path, access: Access) -> ServeConfig {
    ServeConfig {
        bind: "127.0.0.1:8099".parse().unwrap(),
        data_dir: dir.to_path_buf(),
        access,
        owner_id: Some("owner-1".to_owned()),
        owner_email: Some("owner@example.net".to_owned()),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: None,
        log_level: tracing::Level::INFO,
        voice: None,
        renderer: nexus_mcp::Renderer::current_exe(),
        stream_shutdown: tokio::sync::watch::Sender::new(false),
    }
}

async fn listen(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve::serve(
        listener,
        app,
        std::future::pending(),
        Duration::from_secs(1),
    ));
    addr
}

async fn read_all(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    String::from_utf8_lossy(&response).into_owned()
}

async fn ask(addr: SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("{head}\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    read_all(&mut stream).await
}

/// A POST whose body never arrives. It needs no credentials and no valid form.
async fn slow_login(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"POST /login HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\ncsrf=")
        .await
        .unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_anonymous_bodies_starve_only_the_public_pool_and_only_until_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let token = issue_api_token(&db, "agent").await.unwrap();
    let app =
        serve::build_app(&db, &config(dir.path(), owner("https://crm.example.test"))).unwrap();
    let addr = listen(app).await;

    let mut held = Vec::new();
    for _ in 0..PUBLIC.in_flight + 16 {
        held.push(slow_login(addr).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let started = tokio::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /api/mcp HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ping}",
        ping.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mcp = read_all(&mut stream).await;
    assert!(mcp.starts_with("HTTP/1.1 200"), "{mcp}");
    let session_route = ask(addr, "GET /queue HTTP/1.1").await;
    assert!(session_route.starts_with("HTTP/1.1 303"), "{session_route}");
    assert!(started.elapsed() < Duration::from_secs(1), "not queued");

    let started = tokio::time::Instant::now();
    let shed = ask(addr, "GET /login HTTP/1.1").await;
    assert!(shed.starts_with("HTTP/1.1 503"), "{shed}");
    assert!(shed.to_lowercase().contains("retry-after: 5"), "{shed}");
    assert!(started.elapsed() < PUBLIC.queue_wait + Duration::from_secs(1));

    // The deadline frees the slots, even though the attacker never hangs up.
    let timed_out = read_all(&mut held[0]).await;
    assert!(timed_out.starts_with("HTTP/1.1 408"), "{timed_out}");
    tokio::time::sleep(PUBLIC.queue_wait + Duration::from_millis(500)).await;
    let free = ask(addr, "GET /login HTTP/1.1").await;
    assert!(free.starts_with("HTTP/1.1 200"), "{free}");
}

#[tokio::test(start_paused = true)]
async fn waiting_for_a_slot_is_bounded_and_pools_do_not_share_slots() {
    let hold = || async {
        std::future::pending::<()>().await;
        "never"
    };
    let free = || async { "free" };
    let full = serve::harden(
        Router::new()
            .route("/hold", get(hold))
            .route("/free", get(free)),
        OWNER,
    );
    let other = serve::harden(Router::new().route("/other", get(free)), PUBLIC);
    for _ in 0..OWNER.in_flight {
        let full = full.clone();
        tokio::spawn(async move {
            let request = Request::get("/hold").body(Body::empty()).unwrap();
            full.oneshot(request).await.unwrap();
        });
    }
    tokio::task::yield_now().await;

    let started = tokio::time::Instant::now();
    let request = Request::get("/free").body(Body::empty()).unwrap();
    let refused = full.oneshot(request).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()["retry-after"], "5");
    assert_eq!(started.elapsed(), OWNER.queue_wait);

    let request = Request::get("/other").body(Body::empty()).unwrap();
    assert_eq!(
        other.oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
}

/// Answers with how many bytes arrived, or 400 if the body failed, as the MCP route reads one.
fn upload_route() -> Router {
    serve::harden_mcp(Router::new().route(
        "/upload",
        post(|body: Body| async move {
            match to_bytes(body, usize::MAX).await {
                Ok(bytes) => (StatusCode::OK, bytes.len().to_string()),
                Err(_) => (StatusCode::BAD_REQUEST, String::new()),
            }
        }),
    ))
}

fn upload(body: Body) -> Request<Body> {
    Request::post("/upload").body(body).unwrap()
}

/// Of unknown length, so it counts as large: one chunk per `every`, `count` times.
fn trickle(every: Duration, count: usize) -> Body {
    Body::from_stream(stream::iter(0..count).then(move |_| async move {
        tokio::time::sleep(every).await;
        Ok::<_, Infallible>(Bytes::from_static(b"x"))
    }))
}

#[tokio::test(start_paused = true)]
async fn a_slow_upload_may_take_far_longer_than_a_handler() {
    let every = Duration::from_secs(20);
    let chunks = 12;
    assert!(every * chunks as u32 > HANDLER_TIMEOUT * 3);
    assert!(every < BODY_IDLE_TIMEOUT);
    let response = upload_route()
        .oneshot(upload(trickle(every, chunks)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], chunks.to_string().as_bytes());
}

#[tokio::test(start_paused = true)]
async fn an_upload_that_never_ends_stops_at_the_large_body_deadline() {
    let started = tokio::time::Instant::now();
    let response = upload_route()
        .oneshot(upload(trickle(Duration::from_secs(20), 10_000)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(started.elapsed(), LARGE_BODY_DEADLINE);
}

#[tokio::test(start_paused = true)]
async fn an_upload_that_stops_sending_gives_its_slots_back_soon() {
    let started = tokio::time::Instant::now();
    let stalled = Body::from_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"x")) })
            .chain(stream::pending()),
    );
    let response = upload_route().oneshot(upload(stalled)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(started.elapsed(), BODY_IDLE_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn a_small_body_keeps_the_normal_deadline() {
    let held = serve::harden_mcp(Router::new().route(
        "/upload",
        post(|| async {
            tokio::time::sleep(HANDLER_TIMEOUT + Duration::from_secs(1)).await;
            "late"
        }),
    ));
    let small = vec![0u8; LARGE_BODY_BYTES as usize];
    let response = held.oneshot(upload(Body::from(small))).await.unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn only_a_few_large_bodies_at_once_and_small_calls_still_get_through() {
    let app = serve::harden_mcp(
        Router::new()
            .route(
                "/upload",
                post(|body: Body| async move {
                    let _ = to_bytes(body, usize::MAX).await;
                    "done"
                }),
            )
            .route("/call", post(|| async { "called" })),
    );
    for _ in 0..MAX_LARGE_BODIES {
        let app = app.clone();
        tokio::spawn(async move {
            let _ = app
                .oneshot(upload(trickle(Duration::from_secs(20), 10_000)))
                .await;
        });
    }
    tokio::task::yield_now().await;
    const {
        assert!(
            MAX_LARGE_BODIES < MCP.in_flight,
            "slots stay for tool calls"
        )
    };

    let call = Request::post("/call").body(Body::from("{}")).unwrap();
    assert_eq!(
        app.clone().oneshot(call).await.unwrap().status(),
        StatusCode::OK
    );

    let started = tokio::time::Instant::now();
    let refused = app
        .oneshot(upload(trickle(Duration::from_secs(1), 1)))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(started.elapsed(), LARGE_BODY_WAIT);
}

#[tokio::test]
async fn half_sent_headers_and_idle_connections_are_closed() {
    let addr = listen(Router::new().route("/", get(|| async { "ok" }))).await;
    let mut half = TcpStream::connect(addr).await.unwrap();
    half.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nX-Pad: aaa")
        .await
        .unwrap();
    let mut idle = TcpStream::connect(addr).await.unwrap();
    let mut kept_alive = TcpStream::connect(addr).await.unwrap();
    kept_alive
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0u8; 15];
    kept_alive.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"HTTP/1.1 200 OK");

    let limit = HEADER_READ_TIMEOUT + Duration::from_secs(2);
    for stream in [&mut half, &mut idle, &mut kept_alive] {
        tokio::time::timeout(limit, read_all(stream))
            .await
            .expect("the server must close it");
    }
}

#[tokio::test]
async fn oversized_headers_are_refused_without_buffering_them() {
    let addr = listen(Router::new().route("/", get(|| async { "ok" }))).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let pad = "a".repeat(serve::MAX_READ_BUFFER_BYTES * 2);
    let request = format!("GET / HTTP/1.1\r\nHost: x\r\nX-Pad: {pad}\r\n\r\n");
    let _ = stream.write_all(request.as_bytes()).await;
    let response = read_all(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 431"), "{response}");

    // A long authorize URL with cookies still fits.
    let pad = "a".repeat(16 * 1024);
    let ok = ask(addr, &format!("GET /?{pad} HTTP/1.1")).await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}

#[tokio::test]
async fn past_the_connection_limit_new_connections_close_until_one_ends() {
    let addr = listen(Router::new().route("/", get(|| async { "ok" }))).await;
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut over = TcpStream::connect(addr).await.unwrap();
    let _ = over.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    let closed = tokio::time::timeout(Duration::from_secs(2), read_all(&mut over))
        .await
        .expect("closed at once, not queued");
    assert_eq!(closed, "");

    drop(held);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ok = ask(addr, "GET / HTTP/1.1").await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}

#[tokio::test]
async fn handlers_still_see_the_peer_address() {
    let addr = listen(Router::new().route(
        "/",
        get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
    ))
    .await;
    let response = ask(addr, "GET / HTTP/1.1").await;
    assert!(response.ends_with("127.0.0.1"), "{response}");
}

fn with_headers(path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::get(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn local_mode_answers_only_to_local_names_and_origins() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let app = serve::build_app(&db, &config(dir.path(), Access::InsecureLocalOnly)).unwrap();
    for path in ["/api/applications", "/"] {
        for headers in [
            vec![("host", "127.0.0.1:8099")],
            vec![("host", "localhost:8099")],
            vec![("host", "LOCALHOST")],
            vec![
                ("host", "localhost:8099"),
                ("origin", "http://localhost:8099"),
            ],
        ] {
            let response = app
                .clone()
                .oneshot(with_headers(path, &headers))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path} {headers:?}");
        }
        for headers in [
            vec![("host", "rebind.attacker.test")],
            vec![("host", "rebind.attacker.test:8099")],
            vec![("host", "localhost.attacker.test")],
            vec![("host", "127.0.0.1.attacker.test")],
            vec![
                ("host", "127.0.0.1:8099"),
                ("origin", "https://attacker.test"),
            ],
            vec![("host", "127.0.0.1:8099"), ("origin", "null")],
        ] {
            let response = app
                .clone()
                .oneshot(with_headers(path, &headers))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} {headers:?}"
            );
        }
    }
    // A cross-site page can send this without a preflight.
    let forged = Request::post("/api/mcp")
        .header("host", "127.0.0.1:8099")
        .header("origin", "https://attacker.test")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "text/plain; application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(forged).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .oneshot(with_headers("/healthz", &[("host", "anything.test")]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn owner_mode_does_not_care_about_host_behind_a_tunnel() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let app =
        serve::build_app(&db, &config(dir.path(), owner("https://crm.example.test"))).unwrap();
    let response = app
        .oneshot(with_headers("/login", &[("host", "127.0.0.1:8080")]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn html_gets_hsts_only_on_an_https_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let https =
        serve::build_app(&db, &config(dir.path(), owner("https://crm.example.test"))).unwrap();
    let page = https
        .clone()
        .oneshot(with_headers("/login", &[]))
        .await
        .unwrap();
    assert_eq!(
        page.headers()["strict-transport-security"],
        "max-age=31536000"
    );
    let json = https
        .oneshot(with_headers("/.well-known/oauth-authorization-server", &[]))
        .await
        .unwrap();
    assert!(!json.headers().contains_key("strict-transport-security"));

    let http = serve::build_app(&db, &config(dir.path(), owner("http://localhost:3000"))).unwrap();
    let page = http.oneshot(with_headers("/login", &[])).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(!page.headers().contains_key("strict-transport-security"));
}

#[tokio::test]
async fn mcp_trailing_slashes_redirect_and_responses_carry_nosniff() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let token = issue_api_token(&db, "agent").await.unwrap();
    let app =
        serve::build_app(&db, &config(dir.path(), owner("https://crm.example.test"))).unwrap();
    for (path, location) in [
        ("/api/mcp/", "/api/mcp"),
        ("/mcp/", "/mcp"),
        ("/api/mcp/?a=1&b=2", "/api/mcp?a=1&b=2"),
    ] {
        for method in ["GET", "POST", "DELETE"] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT, "{path}");
            assert_eq!(response.headers()["location"], location);
        }
    }
    let anonymous = app
        .clone()
        .oneshot(with_headers("/api/mcp", &[]))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers()["x-content-type-options"], "nosniff");
    let authorized = Request::post("/mcp")
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
        .unwrap();
    let response = app.oneshot(authorized).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}

fn download_call() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "download_document_content", "arguments": {"id": "1"}},
    })
}

fn call_route() -> Router {
    serve::harden_mcp(Router::new().route(
        "/call",
        post(|| async { Body::from_stream(stream::pending::<Result<Bytes, Infallible>>()) }),
    ))
}

fn call(body: serde_json::Value) -> Request<Body> {
    Request::post("/call")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_download_holds_a_large_slot_until_its_response_is_written() {
    let app = call_route();
    let mut open = Vec::new();
    for _ in 0..MAX_LARGE_BODIES {
        let response = app.clone().oneshot(call(download_call())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response);
    }
    // An escaped name is the same tool to the JSON parser, so it counts too.
    let escaped = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"download_document_conten\u0074"}}"#;
    let started = tokio::time::Instant::now();
    let refused = app
        .clone()
        .oneshot(Request::post("/call").body(Body::from(escaped)).unwrap())
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(started.elapsed(), LARGE_BODY_WAIT);

    // Other calls are not held up.
    let other = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let response = app.clone().oneshot(call(other)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    open.clear();
    let response = app.oneshot(call(download_call())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_cv_render_takes_the_same_large_slots_as_a_download() {
    let app = call_route();
    let render = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "render_cv_pdf", "arguments": {}},
    });
    let mut open = Vec::new();
    for _ in 0..MAX_LARGE_BODIES {
        let response = app.clone().oneshot(call(render.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response);
    }
    let refused = app.clone().oneshot(call(render.clone())).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let refused = app.oneshot(call(download_call())).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_batch_cannot_ask_for_more_downloads_than_there_are_slots() {
    let batch = vec![download_call(); MAX_LARGE_BODIES + 1];
    let response = call_route()
        .oneshot(call(serde_json::Value::Array(batch)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let batch = vec![download_call(); MAX_LARGE_BODIES];
    let response = call_route()
        .oneshot(call(serde_json::Value::Array(batch)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
