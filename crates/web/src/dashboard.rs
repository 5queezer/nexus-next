//! The read-only HTML dashboard.

use std::cmp::Reverse;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use chrono_tz::Tz;
use nexus_core::Db;
use nexus_core::repo::applications::{self, Application};
use nexus_core::repo::{contacts, documents, events, submissions};
use serde::Deserialize;

use crate::queue::{self, Clock, Group};
use crate::views::{self, Detail, Nav, PipelineView};

const STATUSES: [&str; 5] = ["interview", "offer", "applied", "inbound", "rejected"];
const CSS: &str = include_str!("style.css");

#[derive(Clone)]
struct Dashboard {
    db: Db,
    tz: Tz,
}

/// Mounts `/`, `/queue`, `/applications/{id}` and `/assets/app.css`, showing times in `tz`. Auth
/// is the caller's.
pub fn dashboard_router(db: Db, tz: Tz) -> Router {
    Router::new()
        .route("/", get(pipeline))
        .route("/queue", get(follow_ups))
        .route("/applications/{id}", get(application))
        .route("/assets/app.css", get(stylesheet))
        .layer(middleware::map_response(crate::secure))
        .with_state(Dashboard { db, tz })
}

struct PageError(StatusCode);

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

async fn stylesheet() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        CSS,
    )
}

#[derive(Deserialize, Default)]
struct PipelineQuery {
    status: Option<String>,
    archived: Option<String>,
    q: Option<String>,
}

fn matches_search(application: &Application, term: &str) -> bool {
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
    State(Dashboard { db, tz }): State<Dashboard>,
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
    let overdue = overdue_count(&all, clock);
    Ok(Html(views::page("Pipeline", Nav::Pipeline, overdue, view)))
}

async fn follow_ups(State(Dashboard { db, tz }): State<Dashboard>) -> Page {
    let all = applications::list(&mut *db.conn().await?).await?;
    let clock = Clock::now(tz);
    let open = all.iter().filter(|a| a.archived_at.is_none());
    let groups = queue::build(open, clock);
    let overdue = groups
        .iter()
        .find(|(group, _)| *group == Group::Overdue)
        .map_or(0, |(_, items)| items.len());
    let view = views::follow_up_queue(&groups, clock);
    Ok(Html(views::page(
        "Follow-ups",
        Nav::FollowUps,
        overdue,
        view,
    )))
}

async fn application(
    State(Dashboard { db, tz }): State<Dashboard>,
    Path(id): Path<String>,
) -> Page {
    let id: i64 = id.parse().map_err(|_| PageError(StatusCode::NOT_FOUND))?;
    let mut conn = db.conn().await?;
    let application = applications::get(&mut conn, id)
        .await?
        .ok_or(PageError(StatusCode::NOT_FOUND))?;
    let events = events::list_for_application(&mut conn, id, Some(500)).await?;
    let contacts = contacts::list_for_application(&mut conn, id).await?;
    let documents = documents::list_for_application(&mut conn, id).await?;
    let submissions = submissions::list_for_application(&mut conn, id, true).await?;
    let clock = Clock::now(tz);
    let overdue = overdue_count(&applications::list(&mut conn).await?, clock);
    let view = views::detail(Detail {
        application: &application,
        events: &events,
        contacts: &contacts,
        documents: &documents,
        submissions: &submissions,
        clock,
    });
    let title = format!("{} #{}", application.company, application.id);
    Ok(Html(views::page(&title, Nav::None, overdue, view)))
}
