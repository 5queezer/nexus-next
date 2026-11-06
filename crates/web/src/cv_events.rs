//! `GET /api/cv/{variant}/events`: new revisions and approvals as Server-Sent Events, for a live preview. Mounted
//! outside the owner pool, so an open stream holds neither a request slot nor its deadline, and
//! behind the session check.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use futures_util::stream;
use nexus_core::Db;
use nexus_core::auth::SCOPE_TOOLS;
use nexus_core::repo::cv_revisions as repo;
use nexus_mcp::{CvRevisionEvent, McpState};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

/// Each stream is one connection, and a browser tab holds one per open variant page.
pub const MAX_STREAMS: usize = 4;
/// Under the 30 s an idle proxy or tunnel can be expected to allow.
pub const HEARTBEAT: Duration = Duration::from_secs(20);

/// How long a stream may run, and how much longer one whose client stopped reading may hold on.
#[derive(Clone, Copy)]
pub struct StreamLife {
    /// The session is checked only at connect, so this also bounds what a logged-out or expired
    /// browser can still receive. `EventSource` reconnects through the check.
    pub life: Duration,
    pub stall_grace: Duration,
}

pub const STREAM_LIFE: StreamLife = StreamLife {
    life: Duration::from_secs(600),
    stall_grace: Duration::from_secs(30),
};

/// Put on a request by whoever serves its connection. Notified, it drops the connection, and the
/// response body with it.
#[derive(Clone)]
pub struct CutConnection(pub Arc<Notify>);

/// Cuts the connection when its time is up, unless the stream has ended and dropped this first. A
/// timer in the stream isn't enough: hyper stops polling a body while the socket is full, so a
/// client that stopped reading would keep its slot for as long as its socket stays open.
struct Watchdog(JoinHandle<()>);

impl Watchdog {
    fn start(cut: CutConnection, after: Duration) -> Self {
        Self(tokio::spawn(async move {
            sleep(after).await;
            tracing::warn!(
                event = "stream_cut",
                "cut a live preview's connection: its client stopped reading"
            );
            cut.0.notify_one();
        }))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
struct Streams {
    db: Db,
    mcp: Arc<McpState>,
    events: broadcast::Sender<CvRevisionEvent>,
    slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    logouts: watch::Sender<u64>,
    life: StreamLife,
}

/// Open streams end when `shutdown` turns true: a stream left open would make the server's shutdown
/// wait out its whole drain timeout. They end too when `logouts` changes: any logout ends them all,
/// since a stream doesn't know its session, and a browser still logged in just reconnects.
pub fn cv_events_router(
    state: McpState,
    shutdown: watch::Receiver<bool>,
    logouts: watch::Sender<u64>,
    life: StreamLife,
) -> Router {
    Router::new()
        .route("/api/cv/{variant}/events", get(events))
        .layer(middleware::map_response(crate::secure))
        .with_state(Streams {
            db: state.db.clone(),
            events: state.cv_events.clone(),
            mcp: Arc::new(state),
            slots: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
            logouts,
            life,
        })
}

async fn head_number(db: &Db, variant: &str) -> nexus_core::Result<Option<i64>> {
    let mut conn = db.conn().await?;
    let Some(variant) = repo::get_variant(&mut conn, variant).await? else {
        return Ok(None);
    };
    Ok(Some(
        repo::head(&mut conn, variant.id)
            .await?
            .map_or(0, |revision| revision.number),
    ))
}

/// The number and state of a variant's head, from one read. `None` if there is no such variant.
pub(crate) async fn head_of(
    db: &Db,
    mcp: &Arc<McpState>,
    variant: &str,
) -> nexus_core::Result<Option<(i64, String)>> {
    let Some(number) = head_number(db, variant).await? else {
        return Ok(None);
    };
    let read = nexus_mcp::call_tool(
        mcp.clone(),
        &[SCOPE_TOOLS],
        "dashboard",
        "get_cv_source",
        json!({"variant": variant}),
    )
    .await;
    let read = read.structured_content.unwrap_or(Value::Null);
    let state = read["state"].as_str().unwrap_or("draft").to_owned();
    Ok(Some((read["revision"].as_i64().unwrap_or(number), state)))
}

async fn events(
    State(streams): State<Streams>,
    Path(variant): Path<String>,
    cut: Option<Extension<CutConnection>>,
) -> Response {
    let Ok(slot) = streams.slots.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "60")],
            "Too many live previews are open: at most 4. This page checks for new revisions slowly instead.",
        )
            .into_response();
    };
    // Subscribed before the head is read, so a revision in between is heard, not lost.
    let receiver = streams.events.subscribe();
    let head = match head_of(&streams.db, &streams.mcp, &variant).await {
        Ok(Some(head)) => head,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not read a CV head for a stream");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let StreamLife { life, stall_grace } = streams.life;
    let live = Live {
        receiver,
        shutdown: streams.shutdown,
        logouts: streams.logouts.subscribe(),
        ends_at: Instant::now() + life,
        db: streams.db,
        mcp: streams.mcp,
        variant,
        first: Some(head),
        _slot: slot,
        _watchdog: cut.map(|Extension(cut)| Watchdog::start(cut, life + stall_grace)),
    };
    let body = stream::unfold(live, |mut live| async move {
        let event = live.next().await?;
        Some((Ok::<_, std::convert::Infallible>(event), live))
    });
    (
        [
            (HeaderName::from_static("x-accel-buffering"), "no"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Sse::new(body).keep_alive(KeepAlive::new().interval(HEARTBEAT).text("heartbeat")),
    )
        .into_response()
}

struct Live {
    receiver: broadcast::Receiver<CvRevisionEvent>,
    shutdown: watch::Receiver<bool>,
    logouts: watch::Receiver<u64>,
    ends_at: Instant,
    db: Db,
    mcp: Arc<McpState>,
    variant: String,
    first: Option<(i64, String)>,
    _slot: OwnedSemaphorePermit,
    _watchdog: Option<Watchdog>,
}

/// With the state, because an approval made while this page had no stream is heard only here.
fn head_event((revision, state): (i64, String)) -> Event {
    Event::default()
        .event("head")
        .data(json!({ "revision": revision, "state": state }).to_string())
}

impl Live {
    /// The next event for this variant, or `None` to end the stream.
    async fn next(&mut self) -> Option<Event> {
        if let Some(head) = self.first.take() {
            return Some(head_event(head));
        }
        loop {
            let received = tokio::select! {
                biased;
                () = ended(&mut self.shutdown) => return None,
                _ = self.logouts.changed() => return None,
                () = sleep_until(self.ends_at) => return None,
                received = self.receiver.recv() => received,
            };
            match received {
                Ok(event) if event.variant == self.variant => {
                    let name = if event.approval {
                        "approved"
                    } else {
                        "revision"
                    };
                    return Event::default().event(name).json_data(&event).ok();
                }
                Ok(_) => {}
                // Too far behind to replay: the page re-reads from the head.
                Err(RecvError::Lagged(_)) => {
                    let head = head_of(&self.db, &self.mcp, &self.variant)
                        .await
                        .ok()
                        .flatten()?;
                    return Some(head_event(head));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

/// Never completes if the sender has gone, since then shutdown can't be signalled at all.
async fn ended(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}
