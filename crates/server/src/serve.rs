//! The HTTP server: router assembly, limits, request logging, health check and graceful shutdown.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::pin::pin;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use nexus_auth::{AuthState, require_mcp_auth, require_owner_session};
use nexus_core::Db;
use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_mcp::McpState;
use tokio::net::TcpListener;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
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
/// Time a handler gets to produce a response head. Streaming bodies are not cut off.
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests in flight at once. SQLite serialises writes and the board has 3.8 GiB, so a small
/// number is plenty for one owner and a few agents.
pub const MAX_IN_FLIGHT: usize = 32;
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

/// Body limit, handler timeout and concurrency limit around `routes`. axum's own 2 MiB extractor
/// limit is lifted so `MAX_BODY_BYTES` is the limit for routes without their own: the auth routes
/// set tighter ones, which win because they sit further in. The concurrency limit must be the global
/// kind: `Router::layer` applies a layer once per route, and the plain one would count per route.
pub fn harden(routes: Router) -> Router {
    routes
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            HANDLER_TIMEOUT,
        ))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
}

/// Every route, wrapped in limits and request logging. The calendar has its own secret URL token.
/// `/healthz` sits outside the limits so it still answers when the server is saturated.
pub fn build_app(db: &Db, config: &ServeConfig) -> anyhow::Result<Router> {
    let mut routes = Router::new();
    if let Some(token) = &config.calendar_token {
        routes = routes.merge(nexus_web::calendar_router(db.clone(), token.clone()));
    }
    let (owner_id, owner_email) = config
        .owner_id
        .clone()
        .zip(config.owner_email.clone())
        .context("the owner is not resolved")?;
    let mcp = nexus_mcp::router(McpState {
        db: db.clone(),
        owner_id,
        owner_email,
    });
    let mcp = Router::new()
        .nest("/api/mcp", mcp.clone())
        .nest("/mcp", mcp);
    let humans = nexus_web::dashboard_router(db.clone(), config.timezone)
        .merge(nexus_web::api_router(db.clone()));
    let routes = match &config.access {
        Access::Owner(auth) => {
            let auth = AuthState::new(db.clone(), auth.clone())?;
            routes
                .merge(humans.layer(from_fn_with_state(auth.clone(), require_owner_session)))
                .merge(mcp.layer(from_fn_with_state(auth.clone(), require_mcp_auth)))
                .merge(nexus_auth::router(auth))
        }
        Access::InsecureLocalOnly => {
            routes.merge(humans).merge(mcp.layer(Extension(AuthContext {
                auth_type: AuthType::ApiToken,
                scopes: vec![SCOPE_TOOLS.to_owned(), SCOPE_SUBMISSIONS.to_owned()],
            })))
        }
    };
    Ok(harden(routes)
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
    tracing::info!(
        addr = %listener.local_addr()?,
        base_url,
        timezone = %config.timezone,
        calendar = config.calendar_token.is_some(),
        "listening"
    );
    let result = serve(listener, app, shutdown, DRAIN_TIMEOUT).await;
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
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>()
        )
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
