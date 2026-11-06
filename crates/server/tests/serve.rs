//! Server behaviour: health check, mounting, limits, timeouts and shutdown.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Extension;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::{any, get, post};
use futures_util::stream;
use http_body_util::BodyExt;
use nexus_auth::{AuthConfig, AuthState, issue_api_token, require_mcp_auth};
use nexus_core::Db;
use nexus_core::auth::AuthContext;
use nexus_server::config::{Access, ServeConfig};
use nexus_server::serve::{self, HANDLER_TIMEOUT, MAX_BODY_BYTES, MAX_IN_FLIGHT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const TOKEN: &str = "tok-0123456789abcdef0123456789abcdef";

const BASE: &str = "https://crm.example.test";

/// Parses as argon2id but matches no passphrase. Enough for everything short of logging in.
fn auth_config() -> AuthConfig {
    AuthConfig {
        public_base_url: BASE.to_owned(),
        owner_passphrase_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA"
            .to_owned(),
        trusted_proxy: None,
    }
}

fn config(dir: &std::path::Path, calendar: bool, insecure: bool) -> ServeConfig {
    ServeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.to_path_buf(),
        access: if insecure {
            Access::InsecureLocalOnly
        } else {
            Access::Owner(auth_config())
        },
        owner_id: "owner-1".to_owned(),
        owner_email: "owner@example.net".to_owned(),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: calendar.then(|| TOKEN.to_owned()),
        log_level: tracing::Level::INFO,
    }
}

async fn status(app: &Router, uri: &str) -> StatusCode {
    let request = Request::get(uri).body(Body::empty()).unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn healthz_reports_ok_then_unavailable_without_detail() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let app = serve::build_app(&db, &config(dir.path(), false, false)).unwrap();

    let response = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");

    db.close().await;
    let response = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"unavailable");
}

#[tokio::test]
async fn the_dashboard_and_api_need_a_session_unless_the_run_is_local() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();

    let owner = serve::build_app(&db, &config(dir.path(), true, false)).unwrap();
    for path in [
        "/",
        "/queue",
        "/applications/1",
        "/assets/app.css",
        "/api/applications",
        "/api/applications/1/events",
        "/api/documents/1/download",
    ] {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let response = owner.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        let location = response.headers()["location"].to_str().unwrap();
        assert!(location.starts_with("/login?next="), "{path}: {location}");
    }
    assert_eq!(status(&owner, "/login").await, StatusCode::OK);
    assert_eq!(
        status(&owner, "/.well-known/oauth-authorization-server").await,
        StatusCode::OK
    );
    assert_eq!(status(&owner, "/healthz").await, StatusCode::OK);

    // The calendar stays on its own secret: no session, no redirect.
    assert_eq!(
        status(&owner, &format!("/calendar.ics?token={TOKEN}")).await,
        StatusCode::OK
    );
    assert_eq!(status(&owner, "/calendar.ics").await, StatusCode::NOT_FOUND);

    let local = serve::build_app(&db, &config(dir.path(), false, true)).unwrap();
    for path in ["/", "/queue", "/api/applications"] {
        assert_eq!(status(&local, path).await, StatusCode::OK, "{path}");
    }
    assert_eq!(status(&local, "/calendar.ics").await, StatusCode::NOT_FOUND);
    for path in ["/login", "/.well-known/oauth-authorization-server"] {
        assert_eq!(status(&local, path).await, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn auth_routes_keep_their_own_body_limits_inside_the_stack() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let app = serve::build_app(&db, &config(dir.path(), false, false)).unwrap();
    // No peer address under oneshot; that shares one bucket, which is fine for a single request.
    for (path, size) in [
        ("/api/mcp/register", 17 * 1024),
        ("/api/mcp/token", 65 * 1024),
        ("/login", 65 * 1024),
    ] {
        let request = Request::post(path)
            .body(Body::from(vec![b'x'; size]))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "{path}"
        );
    }
}

/// One request over a real socket, so the peer address is real too.
async fn http(addr: std::net::SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("{head}\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

/// The MCP mount as `build_app` will make it, behind the real limits and a real socket.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_auth_headers_and_limits_pass_through_the_stack() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let token = issue_api_token(&db, "agent").await.unwrap();
    let auth = AuthState::new(db.clone(), auth_config()).unwrap();
    let mcp =
        Router::new()
            .route(
                "/api/mcp",
                any(|Extension(context): Extension<AuthContext>| async move {
                    context.scopes.join(" ")
                }),
            )
            .layer(from_fn_with_state(auth.clone(), require_mcp_auth));
    let app = serve::harden(mcp.merge(nexus_auth::router(auth)));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve::serve(
        listener,
        app,
        async move {
            let _ = stopped.await;
        },
        Duration::from_secs(1),
    ));

    let tokenless = http(addr, "POST /api/mcp HTTP/1.1\r\nContent-Length: 0").await;
    assert!(tokenless.starts_with("HTTP/1.1 401"), "{tokenless}");
    assert!(
        tokenless.to_lowercase().contains(&format!(
            "www-authenticate: bearer realm=\"{BASE}/api/mcp\", resource_metadata=\"{BASE}/.well-known/oauth-protected-resource\""
        )),
        "{tokenless}"
    );

    let bad = "POST /api/mcp HTTP/1.1\r\nContent-Length: 0\r\nAuthorization: Bearer jt_guess";
    for _ in 0..30 {
        assert!(http(addr, bad).await.starts_with("HTTP/1.1 401"));
    }
    let limited = http(addr, bad).await;
    assert!(limited.starts_with("HTTP/1.1 429"), "{limited}");
    assert!(
        limited.to_lowercase().contains("retry-after: "),
        "{limited}"
    );

    // Spoofing the address doesn't escape it: no proxy is trusted here.
    let spoofed = format!("{bad}\r\nX-Forwarded-For: 192.0.2.77\r\nCF-Connecting-IP: 192.0.2.77");
    assert!(http(addr, &spoofed).await.starts_with("HTTP/1.1 429"));

    let good =
        format!("POST /api/mcp HTTP/1.1\r\nContent-Length: 0\r\nAuthorization: Bearer {token}");
    let response = http(addr, &good).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.ends_with("mcp:tools mcp:submissions"),
        "{response}"
    );

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_concurrency_limit_counts_across_routes() {
    let (release, released) = tokio::sync::watch::channel(false);
    let hold = move || {
        let mut released = released.clone();
        async move {
            let _ = released.wait_for(|done| *done).await;
            "held"
        }
    };
    let app = serve::harden(
        Router::new()
            .route("/hold", get(hold))
            .route("/other", get(|| async { "other" })),
    );
    let mut held = Vec::new();
    for _ in 0..MAX_IN_FLIGHT {
        let app = app.clone();
        held.push(tokio::spawn(async move {
            app.oneshot(Request::get("/hold").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Every slot is taken, so a request to a different route waits too.
    let waiting = tokio::spawn({
        let app = app.clone();
        async move { status(&app, "/other").await }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!waiting.is_finished(), "the limit must be global");

    release.send(true).unwrap();
    assert_eq!(waiting.await.unwrap(), StatusCode::OK);
    for task in held {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}

fn echo_len() -> Router {
    serve::harden(Router::new().route(
        "/echo",
        post(|body: Bytes| async move { body.len().to_string() }),
    ))
}

#[tokio::test]
async fn body_limit_admits_the_largest_upload_and_refuses_more() {
    let app = echo_len();
    // Past axum's own 2 MiB extractor default, at the largest document tool call.
    let ok = Request::post("/echo")
        .body(Body::from(vec![b'x'; MAX_BODY_BYTES]))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(ok).await.unwrap().status(),
        StatusCode::OK
    );

    let too_big = Request::post("/echo")
        .body(Body::from(vec![b'x'; MAX_BODY_BYTES + 1]))
        .unwrap();
    assert_eq!(
        app.oneshot(too_big).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    const {
        assert!(
            MAX_BODY_BYTES > 13_981_020,
            "must hold MAX_DOCUMENT_BASE64_SIZE"
        )
    };
}

#[tokio::test(start_paused = true)]
async fn slow_handlers_time_out_but_streams_may_outlive_the_timeout() {
    let app = serve::harden(
        Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(HANDLER_TIMEOUT + Duration::from_secs(1)).await;
                    "late"
                }),
            )
            .route(
                "/stream",
                get(|| async {
                    let chunks = stream::unfold(0, |n| async move {
                        if n == 4 {
                            return None;
                        }
                        tokio::time::sleep(HANDLER_TIMEOUT / 2).await;
                        Some((Ok::<_, Infallible>(Bytes::from_static(b"tick")), n + 1))
                    });
                    Body::from_stream(chunks)
                }),
            ),
    );
    assert_eq!(status(&app, "/slow").await, StatusCode::REQUEST_TIMEOUT);

    let response = app
        .oneshot(Request::get("/stream").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], "tick".repeat(4).as_bytes());
}

#[tokio::test]
async fn shutdown_finishes_in_flight_requests_and_stops_accepting() {
    let app = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            "finished"
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve::serve(
        listener,
        app,
        async move {
            let _ = stopped.await;
        },
        Duration::from_secs(5),
    ));

    let in_flight = tokio::spawn(async move { http_get(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.send(()).unwrap();

    let response = in_flight.await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("finished"), "{response}");
    server.await.unwrap().unwrap();
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn shutdown_does_not_wait_forever_for_open_streams() {
    let app = Router::new().route(
        "/forever",
        get(|| async { Body::from_stream(stream::pending::<Result<Bytes, Infallible>>()) }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve::serve(
        listener,
        app,
        async move {
            let _ = stopped.await;
        },
        Duration::from_millis(300),
    ));
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /forever HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must stop within the drain timeout")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_closes_the_database_so_no_wal_is_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve::run(
        config(dir.path(), false, false),
        listener,
        async move {
            let _ = stopped.await;
        },
    ));
    let response = http_get(addr, "/healthz").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        dir.path().join("nexus.db-wal").exists(),
        "WAL is live while serving"
    );

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(!dir.path().join("nexus.db-wal").exists());
    assert!(!dir.path().join("nexus.db-shm").exists());
    Db::open(dir.path()).await.unwrap().close().await;
}
