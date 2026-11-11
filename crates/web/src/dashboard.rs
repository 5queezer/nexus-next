//! The HTML dashboard: pages that only read, and the forms that change an application through the
//! tools.

use std::cmp::Reverse;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use chrono_tz::Tz;
use nexus_core::Db;
use nexus_core::auth::FormToken;
use nexus_core::repo::applications::{self, Application};
use nexus_core::repo::{contacts, documents, events, submissions};
use nexus_mcp::McpState;
use rand::Rng;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::forms::{self, Actions};
use crate::queue::{self, Clock, Group};
use crate::views::{self, Detail, Nav, PipelineView};

const STATUSES: [&str; 5] = ["interview", "offer", "applied", "inbound", "rejected"];
/// A form is a few KB; a note of 5000 characters percent-encoded stays well inside this.
const FORM_BODY_LIMIT: usize = 128 * 1024;

#[derive(Clone)]
pub(crate) struct Dashboard {
    pub db: Db,
    pub mcp: Arc<McpState>,
    pub tz: Tz,
    pub adding_contact: Arc<Mutex<()>>,
}

/// Mounts `/`, `/queue`, `/applications/{id}` and the form posts under it, showing times in `tz`.
/// Auth is the caller's: its layer puts the session's `FormToken` into the request. Without one
/// (no login in local mode) every form carries this router's own random token instead, so the same
/// check runs. Pages and the form routes are all that is mounted: the static files are
/// `static_router`.
pub fn dashboard_router(state: McpState, tz: Tz) -> Router {
    with_form_token(
        Router::new()
            .route("/", get(pipeline))
            .route("/queue", get(follow_ups))
            .route("/applications/{id}", get(application))
            .route(
                "/applications/{id}/{form}",
                post(forms::submit).layer(DefaultBodyLimit::max(FORM_BODY_LIMIT)),
            ),
    )
    .layer(middleware::map_response(crate::secure))
    .with_state(Dashboard {
        db: state.db.clone(),
        mcp: Arc::new(state),
        tz,
        adding_contact: Arc::default(),
    })
}

/// Without a session's token (no login in local mode), this router's own random one, so that the
/// same CSRF check runs.
pub(crate) fn with_form_token<S: Clone + Send + Sync + 'static>(routes: Router<S>) -> Router<S> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let local = FormToken(hex::encode(bytes));
    routes.layer(middleware::from_fn(
        move |mut request: Request, next: Next| {
            let local = local.clone();
            async move {
                if request.extensions().get::<FormToken>().is_none() {
                    request.extensions_mut().insert(local);
                }
                next.run(request).await
            }
        },
    ))
}

pub(crate) struct PageError(pub StatusCode);

impl From<nexus_core::Error> for PageError {
    fn from(e: nexus_core::Error) -> Self {
        match e {
            nexus_core::Error::NotFound => Self(StatusCode::NOT_FOUND),
            e => {
                tracing::error!(error = %e, "dashboard query failed");
                Self(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        let (heading, text) = if self.0 == StatusCode::NOT_FOUND {
            ("Not found", "There is nothing at this address.")
        } else {
            (
                "Something went wrong",
                "The page could not be built. Try again.",
            )
        };
        let body = views::page(heading, Nav::None, 0, views::message(heading, text));
        (self.0, Html(body)).into_response()
    }
}

type Page = Result<Html<String>, PageError>;

#[derive(Deserialize, Default)]
struct PipelineQuery {
    status: Option<String>,
    archived: Option<String>,
    q: Option<String>,
}

pub(crate) fn matches_search(application: &Application, term: &str) -> bool {
    let term = term.to_lowercase();
    [
        Some(application.company.as_str()),
        Some(application.role.as_str()),
        application.notes.as_deref(),
        application.job_description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(&term))
}

fn overdue_count(all: &[Application], clock: Clock) -> usize {
    all.iter()
        .filter(|a| a.archived_at.is_none() && queue::group_of(a, clock) == Group::Overdue)
        .count()
}

async fn pipeline(
    State(Dashboard { db, tz, .. }): State<Dashboard>,
    Query(query): Query<PipelineQuery>,
) -> Page {
    let all = applications::list(&mut *db.conn().await?).await?;
    let clock = Clock::now(tz);
    let archived = match query.archived.as_deref() {
        Some("include") => "include",
        Some("only") => "only",
        _ => "hide",
    };
    let search = query.q.as_deref().map(str::trim).unwrap_or_default();
    let status = query
        .status
        .as_deref()
        .and_then(|s| STATUSES.iter().find(|known| **known == s).copied())
        .unwrap_or("all");

    let scoped: Vec<&Application> = all
        .iter()
        .filter(|a| match archived {
            "hide" => a.archived_at.is_none(),
            "only" => a.archived_at.is_some(),
            _ => true,
        })
        .filter(|a| search.is_empty() || matches_search(a, search))
        .collect();
    let counts = STATUSES
        .map(|s| (s, scoped.iter().filter(|a| a.status == s).count()))
        .into();
    let sections = STATUSES
        .iter()
        .filter(|s| status == "all" || status == **s)
        .map(|s| {
            let mut items: Vec<&Application> =
                scoped.iter().copied().filter(|a| a.status == *s).collect();
            items.sort_by_key(|a| {
                (
                    queue::group_of(a, clock) != Group::Overdue,
                    a.follow_up_at.is_none(),
                    a.follow_up_at.clone(),
                    Reverse(a.updated_at.clone()),
                )
            });
            (crate::format::status_label(s), items)
        })
        .filter(|(_, items)| !items.is_empty())
        .collect();
    let view = views::pipeline(PipelineView {
        sections,
        counts,
        total: scoped.len(),
        status,
        archived,
        search,
        clock,
    });
    let counts = views::NavCounts {
        overdue: overdue_count(&all, clock),
        awaiting: crate::cv::awaiting_count(&db).await?,
    };
    Ok(Html(views::page("Pipeline", Nav::Pipeline, counts, view)))
}

async fn follow_ups(State(Dashboard { db, tz, .. }): State<Dashboard>) -> Page {
    let all = applications::list(&mut *db.conn().await?).await?;
    let clock = Clock::now(tz);
    let open = all.iter().filter(|a| a.archived_at.is_none());
    let groups = queue::build(open, clock);
    let overdue = groups
        .iter()
        .find(|(group, _)| *group == Group::Overdue)
        .map_or(0, |(_, items)| items.len());
    let view = views::follow_up_queue(&groups, clock);
    let counts = views::NavCounts {
        overdue,
        awaiting: crate::cv::awaiting_count(&db).await?,
    };
    Ok(Html(views::page(
        "Follow-ups",
        Nav::FollowUps,
        counts,
        view,
    )))
}

#[derive(Deserialize, Default)]
struct DetailQuery {
    done: Option<String>,
    /// The timeline filter: `notes`, `interviews`, or everything.
    show: Option<String>,
}

async fn application(
    State(dash): State<Dashboard>,
    Extension(FormToken(token)): Extension<FormToken>,
    Path(id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Page {
    let id: i64 = id.parse().map_err(|_| PageError(StatusCode::NOT_FOUND))?;
    let mut actions = Actions::new(token);
    actions.flash = query.done.as_deref().and_then(forms::done_notice);
    let show = query.show.as_deref().unwrap_or_default();
    Ok(Html(detail_page(&dash, id, actions, show).await?))
}

pub(crate) async fn detail_page(
    Dashboard { db, tz, .. }: &Dashboard,
    id: i64,
    actions: Actions,
    show: &str,
) -> Result<String, PageError> {
    let mut conn = db.conn().await?;
    let application = applications::get(&mut conn, id)
        .await?
        .ok_or(PageError(StatusCode::NOT_FOUND))?;
    let events = events::list_for_application(&mut conn, id, Some(500)).await?;
    let contacts = contacts::list_for_application(&mut conn, id).await?;
    let documents = documents::list_for_application(&mut conn, id).await?;
    let submissions = submissions::list_for_application(&mut conn, id, true).await?;
    let clock = Clock::now(*tz);
    let counts = views::NavCounts {
        overdue: overdue_count(&applications::list(&mut conn).await?, clock),
        awaiting: crate::cv::awaiting_count(db).await?,
    };
    let view = views::detail(Detail {
        application: &application,
        events: &events,
        contacts: &contacts,
        documents: &documents,
        submissions: &submissions,
        clock,
        actions: &actions,
        show,
    });
    let title = format!("{} #{}", application.company, application.id);
    Ok(views::page(&title, Nav::None, counts, view))
}
