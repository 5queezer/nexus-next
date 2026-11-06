//! The HTTP server: router assembly, limits, request logging, health check and graceful shutdown.

use std::future::IntoFuture;
use std::pin::pin;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use nexus_core::Db;
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::ServeConfig;

/// A tool call carrying the largest document: 10 MiB as base64 (old Nexus's
/// `MAX_DOCUMENT_BASE64_SIZE`, 13,981,020 characters) plus the JSON around it.
pub const MAX_BODY_BYTES: usize = 15 * 1024 * 1024;
/// Time a handler gets to produce a response head. Streaming bodies are not cut off.
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests in flight at once. SQLite serialises writes and the board has 3.8 GiB, so a small
/// number is plenty for one owner and a few agents.
pub const MAX_IN_FLIGHT: usize = 32;
/// How long shutdown waits for open requests and streams before closing the database anyway.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Compact logging without timestamps (journald adds its own). sqlx statement logging is cut off
/// at warnings whatever the level, so SQL text can never reach the log.
pub fn init_logging(level: Level, writer: impl for<'a> MakeWriter<'a> + Send + Sync + 'static) {
    let filter = Targets::new()
        .with_default(level)
        .with_target("sqlx", LevelFilter::WARN);
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

/// Body limit, handler timeout and concurrency limit around `routes`. axum's own 2 MiB extractor
/// limit is lifted so `MAX_BODY_BYTES` is the single limit.
pub fn harden(routes: Router) -> Router {
    routes
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            HANDLER_TIMEOUT,
        ))
        .layer(ConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
}

/// The routes this build can serve safely, wrapped in limits and request logging.
/// `/healthz` sits outside the limits so it still answers when the server is saturated.
pub fn build_app(db: &Db, config: &ServeConfig) -> Router {
    let mut routes = Router::new();
    if let Some(token) = &config.calendar_token {
        routes = routes.merge(nexus_web::calendar_router(db.clone(), token.clone()));
    }
    if config.insecure_local_only {
        routes = routes
            .merge(nexus_web::dashboard_router(db.clone(), config.timezone))
            .merge(nexus_web::api_router(db.clone()));
    }
    harden(routes)
        .route("/healthz", get(healthz).with_state(db.clone()))
        .layer(middleware::from_fn(log_request))
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
    config: ServeConfig,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let db = Db::open(&config.data_dir)
        .await
        .with_context(|| format!("cannot open the database in {}", config.data_dir.display()))?;
    if !config.insecure_local_only {
        tracing::warn!("dashboard and API are not served: no authentication layer in this build");
    }
    tracing::info!(
        addr = %listener.local_addr()?,
        base_url = config.base_url.as_deref().unwrap_or("unset"),
        timezone = %config.timezone,
        calendar = config.calendar_token.is_some(),
        "listening"
    );
    let result = serve(listener, build_app(&db, &config), shutdown, DRAIN_TIMEOUT).await;
    db.close().await;
    tracing::info!("stopped");
    result
}

pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain: Duration,
) -> anyhow::Result<()> {
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel();
    let mut server = pin!(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                let _ = signalled_tx.send(());
            })
            .into_future()
    );
    tokio::select! {
        result = &mut server => result.context("server failed"),
        _ = async {
            if signalled_rx.await.is_ok() {
                tokio::time::sleep(drain).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            tracing::warn!("open connections did not finish in time, closing them");
            Ok(())
        }
    }
}
