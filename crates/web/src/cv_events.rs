//! `GET /api/cv/{variant}/events`: new revisions and approvals as Server-Sent Events, for a live preview. Mounted
//! outside the owner pool, so an open stream holds neither a request slot nor its deadline, and
//! behind the session check.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use nexus_core::Db;
use nexus_core::repo::cv_revisions as repo;
use nexus_mcp::{CvRevisionEvent, McpState};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};

/// Each stream is one connection, and a browser tab holds one per open variant page.
pub const MAX_STREAMS: usize = 4;
/// Under the 30 s an idle proxy or tunnel can be expected to allow.
pub const HEARTBEAT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Streams {
    db: Db,
    events: broadcast::Sender<CvRevisionEvent>,
    slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
}

/// Open streams end when `shutdown` turns true. A stream left open would make the server's
/// shutdown wait out its whole drain timeout.
pub fn cv_events_router(state: McpState, shutdown: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/api/cv/{variant}/events", get(events))
        .layer(middleware::map_response(crate::secure))
        .with_state(Streams {
            db: state.db.clone(),
            events: state.cv_events.clone(),
            slots: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
        })
}

pub(crate) async fn head_number(db: &Db, variant: &str) -> nexus_core::Result<Option<i64>> {
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

async fn events(State(streams): State<Streams>, Path(variant): Path<String>) -> Response {
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
    let head = match head_number(&streams.db, &variant).await {
        Ok(Some(head)) => head,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not read a CV head for a stream");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let live = Live {
        receiver,
        shutdown: streams.shutdown,
        db: streams.db,
        variant,
        first: Some(head),
        _slot: slot,
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
    db: Db,
    variant: String,
    first: Option<i64>,
    _slot: OwnedSemaphorePermit,
}

fn head_event(revision: i64) -> Event {
    Event::default()
        .event("head")
        .data(json!({ "revision": revision }).to_string())
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
                received = self.receiver.recv() => received,
            };
            match received {
                Ok(event) if event.variant == self.variant => {
                    let name = if event.approval { "approved" } else { "revision" };
                    return Event::default().event(name).json_data(&event).ok();
                }
                Ok(_) => {}
                // Too far behind to replay: the page re-reads from the head.
                Err(RecvError::Lagged(_)) => {
                    let head = head_number(&self.db, &self.variant).await.ok().flatten()?;
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
