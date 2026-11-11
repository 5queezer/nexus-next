//! The HTTP server: router assembly, limits, request logging, health check and graceful shutdown.

use std::net::IpAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::{Body, HttpBody, to_bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::header::{
    CONTENT_TYPE, HOST, ORIGIN, RETRY_AFTER, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next, from_fn, from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use nexus_auth::{AuthState, require_mcp_auth, require_owner_session};
use nexus_core::Db;
use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_mcp::{LARGE_RESULT_TOOLS, McpState};
use nexus_web::CutConnection;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, watch};
use tokio::time::{sleep, timeout};
use tower::ServiceExt;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{Access, ServeConfig};

/// A tool call carrying the largest document: 10 MiB as base64 (old Nexus's
/// `MAX_DOCUMENT_BASE64_SIZE`, 13,981,020 characters) plus the JSON around it.
pub const MAX_BODY_BYTES: usize = 15 * 1024 * 1024;
/// Time a handler gets to produce a response head, including reading the request body. Streaming
/// response bodies are not cut off.
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(60);

/// One pool of request slots. Separate pools keep one kind of caller from starving another, and
/// `queue_wait` bounds how long a request waits for a slot before it gets a 503.
#[derive(Clone, Copy)]
pub struct Limits {
    pub pool: &'static str,
    pub in_flight: usize,
    pub queue_wait: Duration,
    pub deadline: Duration,
}

/// Login, OAuth and the calendar: anyone can reach these, so they get few slots and a deadline that
/// also bounds their small bodies (64 KiB in 10 s is 52 kbit/s).
pub const PUBLIC: Limits = Limits {
    pool: "public",
    in_flight: 8,
    queue_wait: Duration::from_secs(2),
    deadline: Duration::from_secs(10),
};
/// The dashboard and JSON API. A session is checked before a slot is taken.
pub const OWNER: Limits = Limits {
    pool: "owner",
    in_flight: 8,
    queue_wait: Duration::from_secs(10),
    deadline: HANDLER_TIMEOUT,
};
/// The MCP route. A token is checked before a slot is taken and before the body is read.
pub const MCP: Limits = Limits {
    pool: "mcp",
    in_flight: 8,
    queue_wait: Duration::from_secs(10),
    deadline: HANDLER_TIMEOUT,
};
/// A body over this, or of unknown length, counts as large on the MCP route. Under it, the normal
/// 60 s deadline still allows 140 kbit/s.
pub const LARGE_BODY_BYTES: u64 = 1024 * 1024;
/// Calls moving a whole document at once, whichever way. One costs far more than its body, because
/// the transport holds several copies of it. They wait outside the MCP pool, so slow uploads can't
/// take every slot from ordinary tool calls.
pub const MAX_LARGE_BODIES: usize = nexus_mcp::MAX_LARGE_RESULTS;
pub const LARGE_BODY_WAIT: Duration = Duration::from_secs(30);
/// 14 MB in 300 s is 373 kbit/s, against 1.9 Mbit/s under the 60 s deadline.
pub const LARGE_BODY_DEADLINE: Duration = Duration::from_secs(300);
/// An upload that stops sending altogether gives its slots back this soon.
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub const MAX_CONNECTIONS: usize = 256;
/// Also closes a keep-alive connection idle this long, since hyper is then waiting for headers.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Holds a request's headers: the longest legitimate one is an 8 KiB authorize URL plus cookies.
pub const MAX_READ_BUFFER_BYTES: usize = 64 * 1024;
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
const CONNECTION_LIMIT_LOG_EVERY: Duration = Duration::from_secs(60);
/// How long shutdown waits for open requests and streams before closing the database anyway.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Compact logging without timestamps (journald adds its own). sqlx and rmcp are cut off at warnings
/// whatever the level, so SQL text and tool arguments and results can never reach the log.
pub fn init_logging(level: Level, writer: impl for<'a> MakeWriter<'a> + Send + Sync + 'static) {
    let filter = Targets::new()
        .with_default(level)
        .with_target("sqlx", LevelFilter::WARN)
        .with_target("rmcp", LevelFilter::WARN);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .without_time()
                .with_target(false)
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(filter),
        )
        .init();
}

/// Body limit, deadline and a pool of slots around `routes`. axum's own 2 MiB extractor limit is
/// lifted so `MAX_BODY_BYTES` is the limit for routes without their own: the auth routes set tighter
/// ones, which win because they sit further in. `Router::layer` applies a layer once per route, so
/// the slots live outside it and every route shares them.
pub fn harden(routes: Router, limits: Limits) -> Router {
    let slots = Arc::new(Semaphore::new(limits.in_flight));
    routes
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(from_fn(move |request: Request, next: Next| {
            admit(limits, slots.clone(), request, next)
        }))
}

/// `harden` for the MCP route, which alone reads bodies up to `MAX_BODY_BYTES` into memory, from
/// agents that can be on slow links. Put the auth layer outside this.
pub fn harden_mcp(routes: Router) -> Router {
    let large = Arc::new(Semaphore::new(MAX_LARGE_BODIES));
    harden(
        routes.layer(RequestBodyTimeoutLayer::new(BODY_IDLE_TIMEOUT)),
        MCP,
    )
    .layer(from_fn(move |request: Request, next: Next| {
        admit_large(large.clone(), request, next)
    }))
}

fn is_large(request: &Request) -> bool {
    !request
        .body()
        .size_hint()
        .upper()
        .is_some_and(|bytes| bytes <= LARGE_BODY_BYTES)
}

fn busy(pool: &str) -> Response {
    tracing::warn!(
        event = "overloaded",
        pool,
        "refused a request: no slot came free in time"
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, "5")],
        "Busy, try again shortly.",
    )
        .into_response()
}

async fn admit(limits: Limits, slots: Arc<Semaphore>, request: Request, next: Next) -> Response {
    let Ok(Ok(_slot)) = timeout(limits.queue_wait, slots.acquire_owned()).await else {
        return busy(limits.pool);
    };
    let deadline = if limits.pool == MCP.pool && is_large(&request) {
        LARGE_BODY_DEADLINE
    } else {
        limits.deadline
    };
    timeout(deadline, next.run(request))
        .await
        .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response())
}

/// How many documents a small request asks to download. A batch answers them all in one response.
fn downloads_in(body: &[u8]) -> usize {
    let is_download = |message: &Value| {
        message["method"] == "tools/call"
            && LARGE_RESULT_TOOLS
                .iter()
                .any(|tool| message["params"]["name"] == *tool)
    };
    match serde_json::from_slice(body) {
        Ok(Value::Array(messages)) => messages.iter().filter(|m| is_download(m)).count(),
        Ok(message) => usize::from(is_download(&message)),
        Err(_) => 0,
    }
}

/// Takes a slot for each whole document the request moves, and holds it until the response has been
/// written: a download's memory stays in use while a slow client reads it. A small body is read here
/// to tell, which is fine because the auth layer already ran.
async fn admit_large(large: Arc<Semaphore>, request: Request, next: Next) -> Response {
    let (slots, request) = if is_large(&request) {
        (1, request)
    } else {
        let (parts, body) = request.into_parts();
        let read = to_bytes(body, LARGE_BODY_BYTES as usize);
        let bytes = match timeout(HANDLER_TIMEOUT, read).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return StatusCode::BAD_REQUEST.into_response(),
            Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
        };
        let downloads = downloads_in(&bytes);
        if downloads > MAX_LARGE_BODIES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Too many document downloads in one batch.",
            )
                .into_response();
        }
        (downloads, Request::from_parts(parts, Body::from(bytes)))
    };
    if slots == 0 {
        return next.run(request).await;
    }
    let wanted = large.acquire_many_owned(slots as u32);
    let Ok(Ok(slot)) = timeout(LARGE_BODY_WAIT, wanted).await else {
        return busy("mcp-large");
    };
    let (parts, body) = next.run(request).await.into_parts();
    let body = body.map_frame(move |frame| {
        let _ = &slot;
        frame
    });
    Response::from_parts(parts, Body::new(body))
}

/// Next.js sent a trailing slash to the canonical path, and clients may have kept that habit.
async fn strip_trailing_slash(uri: Uri) -> Redirect {
    let path = uri.path().trim_end_matches('/');
    match uri.query() {
        Some(query) => Redirect::permanent(&format!("{path}?{query}")),
        None => Redirect::permanent(path),
    }
}

async fn nosniff(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// Only on HTML, which is what a browser navigates to. No `includeSubDomains`: other hosts under the
/// same domain are not this server's to decide for.
async fn hsts(mut response: Response) -> Response {
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/html"));
    if is_html {
        response.headers_mut().insert(
            STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

fn is_local_name(name: &str, bind: IpAddr) -> bool {
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == bind.to_string()
}

/// A browser sends `Host` with a rebound name, and `Origin` on the cross-site POST a `text/plain;
/// application/json` content type lets through without a preflight. The dashboard's own forms send
/// `Origin: null` because pages say `Referrer-Policy: no-referrer`; their CSRF token is the check,
/// so only that route family may carry it.
fn is_local_request(headers: &HeaderMap, path: &str, bind: IpAddr) -> bool {
    let value = |name| headers.get(name).map(|v| v.to_str().unwrap_or_default());
    let host_ok = value(HOST).is_none_or(|host| {
        host.parse::<Authority>()
            .is_ok_and(|authority| is_local_name(authority.host(), bind))
    });
    let origin_ok = value(ORIGIN).is_none_or(|origin| {
        (origin == "null" && (path.starts_with("/applications/") || path.starts_with("/cv/")))
            || origin
                .parse::<Uri>()
                .ok()
                .and_then(|uri| uri.host().map(|host| is_local_name(host, bind)))
                .unwrap_or(false)
    });
    host_ok && origin_ok
}

/// Without login, any web page could otherwise reach local data through DNS rebinding.
async fn local_only(State(bind): State<IpAddr>, request: Request, next: Next) -> Response {
    if is_local_request(request.headers(), request.uri().path(), bind) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "This local-only server answers only to localhost.",
        )
            .into_response()
    }
}

/// Every route, wrapped in limits and request logging. The calendar has its own secret URL token.
/// `/healthz` sits outside the limits so it still answers when the server is saturated. Auth runs
/// before a slot is taken, so a caller without credentials can only ever occupy the public pool.
pub fn build_app(db: &Db, config: &ServeConfig) -> anyhow::Result<Router> {
    let mut public = nexus_web::static_router();
    if let Some(token) = &config.calendar_token {
        public = public.merge(nexus_web::calendar_router(db.clone(), token.clone()));
    }
    let (owner_id, owner_email) = config
        .owner_id
        .clone()
        .zip(config.owner_email.clone())
        .context("the owner is not resolved")?;
    let state = McpState::new(db.clone(), owner_id, owner_email, config.renderer.clone());
    let mcp = nexus_mcp::router(state.clone());
    let mcp = harden_mcp(
        Router::new()
            .nest("/api/mcp", mcp.clone())
            .nest("/mcp", mcp),
    );
    let voice = nexus_web::Voice::new(config.voice.clone(), &config.data_dir);
    let auth = match &config.access {
        Access::Owner(auth) => Some(AuthState::new(db.clone(), auth.clone())?),
        Access::InsecureLocalOnly => None,
    };
    // Outside the owner pool: an open stream is neither a request in flight nor bound by its
    // deadline, and a voice session's status check mustn't read a busy dashboard as a lost server.
    // They still sit behind the session check.
    let unpooled = {
        let (state, voice) = (state.clone(), voice.clone());
        let shutdown = config.stream_shutdown.subscribe();
        move |logouts| {
            nexus_web::cv_events_router(state, shutdown, logouts, nexus_web::STREAM_LIFE)
                .merge(nexus_web::voice_status_router(voice))
        }
    };
    let humans = harden(
        nexus_web::dashboard_router(state.clone(), config.timezone)
            .merge(nexus_web::api_router(db.clone()))
            .merge(nexus_web::openai_key_router(
                db.clone(),
                config.timezone,
                voice.clone(),
                auth.clone(),
            ))
            .merge(nexus_web::cv_router(
                state,
                config.timezone,
                voice,
                &config.data_dir,
            )),
        OWNER,
    );
    let slashes = Router::new()
        .route("/api/mcp/", any(strip_trailing_slash))
        .route("/mcp/", any(strip_trailing_slash));
    let routes = match (&config.access, auth) {
        (Access::Owner(access), Some(auth)) => {
            let https = access.public_base_url.starts_with("https://");
            let routes = harden(public.merge(nexus_auth::router(auth.clone())), PUBLIC)
                .merge(humans.layer(from_fn_with_state(auth.clone(), require_owner_session)))
                .merge(
                    unpooled(auth.logouts())
                        .layer(from_fn_with_state(auth.clone(), require_owner_session)),
                )
                .merge(
                    mcp.layer(from_fn_with_state(auth, require_mcp_auth))
                        .merge(slashes)
                        .layer(map_response(nosniff)),
                );
            if https {
                routes.layer(map_response(hsts))
            } else {
                routes
            }
        }
        _ => harden(public, PUBLIC)
            .merge(humans)
            .merge(unpooled(watch::Sender::new(0)))
            .merge(
                mcp.layer(Extension(AuthContext {
                    auth_type: AuthType::ApiToken,
                    scopes: vec![SCOPE_TOOLS.to_owned(), SCOPE_SUBMISSIONS.to_owned()],
                }))
                .merge(slashes)
                .layer(map_response(nosniff)),
            )
            .layer(from_fn_with_state(config.bind.ip(), local_only)),
    };
    Ok(routes
        .route("/healthz", get(healthz).with_state(db.clone()))
        .layer(middleware::from_fn(log_request)))
}

/// Fills in the owner: the import's values win, because imported events' request hashes cover them,
/// and configuration covers a database no import wrote.
pub async fn resolve_owner(db: &Db, config: &mut ServeConfig) -> anyhow::Result<()> {
    let imported = nexus_core::import::imported_owner(&mut *db.conn().await?).await?;
    let (id, email) = imported.map_or((None, None), |(id, email)| (Some(id), email));
    config.owner_id = id.or(config.owner_id.take());
    config.owner_email = email.or(config.owner_email.take());
    if config.owner_id.is_none() {
        anyhow::bail!("the database has no imported owner: set NEXUS_OWNER_ID or --owner-id");
    }
    if config.owner_email.is_none() {
        anyhow::bail!(
            "the database has no imported owner email: set NEXUS_OWNER_EMAIL or --owner-email"
        );
    }
    Ok(())
}

/// Liveness plus a database round trip. The body never says what is wrong.
async fn healthz(axum::extract::State(db): axum::extract::State<Db>) -> (StatusCode, &'static str) {
    let check = sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(db.pool());
    match tokio::time::timeout(HEALTH_DB_TIMEOUT, check).await {
        Ok(Ok(1)) => (StatusCode::OK, "ok"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}

/// Logs method, route template and status. The URI, headers and bodies are never read, because
/// they carry the calendar token, OAuth parameters and personal data.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    let started = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        method = %method,
        route = %route,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// Opens the database, serves on `listener` until `shutdown` resolves, then lets in-flight
/// requests finish (up to `DRAIN_TIMEOUT`) and closes the database, which checkpoints the WAL.
pub async fn run(
    mut config: ServeConfig,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let db = Db::open(&config.data_dir)
        .await
        .with_context(|| format!("cannot open the database in {}", config.data_dir.display()))?;
    resolve_owner(&db, &mut config).await?;
    match db.blobs().sweep_temp_files() {
        Ok(0) => {}
        Ok(removed) => tracing::warn!(removed, "removed unfinished uploads a crash left behind"),
        Err(error) => tracing::warn!(%error, "could not remove unfinished uploads"),
    }
    let base_url = match &config.access {
        Access::Owner(auth) => auth.public_base_url.as_str(),
        Access::InsecureLocalOnly => {
            tracing::warn!("dashboard and API are served without login: for local runs only");
            "unset"
        }
    };
    let app = build_app(&db, &config)?;
    let streams = config.stream_shutdown.clone();
    let shutdown = async move {
        shutdown.await;
        streams.send_replace(true);
    };
    tracing::info!(
        addr = %listener.local_addr()?,
        base_url,
        timezone = %config.timezone,
        calendar = config.calendar_token.is_some(),
        voice = config.voice.key.is_some()
            || config.data_dir.join(nexus_web::OPENAI_KEY_FILE).exists(),
        "listening"
    );
    let result = serve(listener, app, shutdown, DRAIN_TIMEOUT).await;
    db.close().await;
    tracing::info!("stopped");
    result
}

/// Serves HTTP/1 until `shutdown` resolves, then waits up to `drain` for open requests. Each
/// connection gets a slot or is closed at once, so a pile of them can't sit in the accept queue
/// ahead of real clients.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain: Duration,
) -> anyhow::Result<()> {
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_buf_size(MAX_READ_BUFFER_BYTES);
    let graceful = GracefulShutdown::new();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut last_limit_log = None;
    let mut shutdown = pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(connection) => connection,
                // Most often out of file descriptors, which waiting can cure.
                Err(error) => {
                    tracing::warn!(event = "accept_failed", %error, "could not accept a connection");
                    sleep(ACCEPT_RETRY).await;
                    continue;
                }
            },
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            if last_limit_log.is_none_or(|at: Instant| at.elapsed() >= CONNECTION_LIMIT_LOG_EVERY) {
                last_limit_log = Some(Instant::now());
                tracing::warn!(
                    event = "connection_limit",
                    limit = MAX_CONNECTIONS,
                    "closing new connections until one ends"
                );
            }
            continue;
        };
        let app = app.clone();
        let cut = Arc::new(Notify::new());
        let cut_handle = CutConnection(cut.clone());
        let service = service_fn(move |mut request: hyper::Request<Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(cut_handle.clone());
            app.clone().oneshot(request.map(Body::new))
        });
        let connection = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            let _slot = slot;
            tokio::select! {
                result = connection => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "connection ended with an error");
                    }
                }
                () = cut.notified() => {}
            }
        });
    }
    drop(listener);
    tokio::select! {
        () = graceful.shutdown() => {}
        () = sleep(drain) => {
            tracing::warn!("open connections did not finish in time, closing them");
        }
    }
    Ok(())
}
