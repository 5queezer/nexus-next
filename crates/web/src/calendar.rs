//! `GET /calendar.ics`: scheduled interviews and follow-ups as an RFC 5545 feed.
//!
//! Old Nexus has no calendar export, so the rules here are derived from how it records events.
//! Events are immutable and have no cancellation type: a rescheduled interview is a newer
//! `interview_scheduled` event, and a closed or archived application drops out of the feed.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use chrono::{DateTime, Duration, Utc};
use nexus_core::repo::{applications, events};
use nexus_core::{Db, Timestamp};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::queue;

const DEFAULT_DURATION_MINUTES: i64 = 60;

#[derive(Clone)]
struct CalendarState {
    db: Db,
    token: String,
}

/// Mounts `/calendar.ics`, readable with `?token=<token>`. An empty token disables the feed.
pub fn calendar_router(db: Db, token: String) -> Router {
    Router::new()
        .route("/calendar.ics", get(feed))
        .layer(middleware::map_response(crate::secure))
        .with_state(CalendarState { db, token })
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn feed(State(state): State<CalendarState>, Query(query): Query<TokenQuery>) -> Response {
    let presented = query.token.unwrap_or_default();
    let valid =
        !state.token.is_empty() && presented.as_bytes().ct_eq(state.token.as_bytes()).into();
    if !valid {
        return StatusCode::NOT_FOUND.into_response();
    }
    match render(&state.db).await {
        Ok(body) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/calendar; charset=utf-8"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, max-age=900"),
                ),
            ],
            body,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "calendar feed failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn render(db: &Db) -> nexus_core::Result<String> {
    let mut conn = db.conn().await?;
    let open: Vec<_> = applications::list(&mut conn)
        .await?
        .into_iter()
        .filter(|a| a.archived_at.is_none() && !matches!(a.status.as_str(), "offer" | "rejected"))
        .collect();
    let filter = events::EventFilter {
        types: vec!["interview_scheduled".to_owned()],
        oldest_first: true,
        limit: 100_000,
        ..Default::default()
    };
    let mut scheduled: HashMap<i64, Vec<events::ApplicationEvent>> = HashMap::new();
    for event in events::list_filtered(&mut conn, &filter).await?.items {
        scheduled
            .entry(event.application_id)
            .or_default()
            .push(event);
    }
    let mut out = Calendar::new();
    for application in &open {
        let interviews = interviews(scheduled.get(&application.id).map_or(&[], Vec::as_slice));
        for interview in &interviews {
            out.interview(application, interview);
        }
        let held = queue::follow_up_hold_reason(application).is_some();
        if let Some(at) = application.follow_up_at.as_ref().filter(|_| !held)
            && !interviews.iter().any(|i| i.start == *at)
        {
            out.follow_up(application, at);
        }
    }
    Ok(out.finish())
}

#[derive(Debug, PartialEq)]
struct Interview {
    /// The first event of the interview: rescheduling keeps the UID so clients update the entry.
    uid_event: i64,
    revisions: i64,
    start: Timestamp,
    minutes: i64,
    kind: String,
    stamp: Timestamp,
}

/// Folds one application's `interview_scheduled` events (oldest first) into interviews. An event
/// reschedules an earlier interview of the same type that had not yet happened when it was
/// recorded; otherwise it is another round.
fn interviews(scheduled: &[events::ApplicationEvent]) -> Vec<Interview> {
    let mut found: Vec<Interview> = Vec::new();
    for event in scheduled {
        let metadata = event.metadata.as_ref().and_then(|m| m.0.as_object());
        let Some(start) = metadata
            .and_then(|m| m.get("scheduledAt"))
            .and_then(|v| v.as_str())
            .and_then(|v| Timestamp::parse(v).ok())
        else {
            continue;
        };
        let kind = metadata
            .and_then(|m| m.get("interviewType"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let minutes = metadata
            .and_then(|m| m.get("durationMinutes"))
            .and_then(|v| v.as_i64())
            .filter(|m| *m > 0)
            .unwrap_or(DEFAULT_DURATION_MINUTES);
        let replaced = found
            .iter_mut()
            .find(|i| i.kind.eq_ignore_ascii_case(&kind) && i.start > event.occurred_at);
        match replaced {
            Some(existing) => {
                existing.revisions += 1;
                existing.start = start;
                existing.minutes = minutes;
                existing.stamp = event.created_at.clone();
            }
            None => found.push(Interview {
                uid_event: event.id,
                revisions: 0,
                start,
                minutes,
                kind,
                stamp: event.created_at.clone(),
            }),
        }
    }
    found
}

struct Calendar(String);

impl Calendar {
    fn new() -> Self {
        let mut calendar = Self(String::new());
        calendar.line("BEGIN:VCALENDAR");
        calendar.line("VERSION:2.0");
        calendar.line("PRODID:-//Nexus//Nexus CRM//EN");
        calendar.line("CALSCALE:GREGORIAN");
        calendar.line("METHOD:PUBLISH");
        calendar.line("X-WR-CALNAME:Nexus");
        calendar
    }

    fn finish(mut self) -> String {
        self.line("END:VCALENDAR");
        self.0
    }

    /// Appends one content line, folded at 75 octets without splitting a character.
    fn line(&mut self, line: &str) {
        let mut width = 0;
        for c in line.chars() {
            let size = c.len_utf8();
            if width + size > 75 {
                self.0.push_str("\r\n ");
                width = 1;
            }
            self.0.push(c);
            width += size;
        }
        self.0.push_str("\r\n");
    }

    fn text(&mut self, name: &str, value: &str) {
        self.line(&format!("{name}:{}", escape_text(value)));
    }

    fn interview(&mut self, application: &applications::Application, interview: &Interview) {
        let start = interview.start.to_datetime();
        let end = start + Duration::minutes(interview.minutes);
        self.line("BEGIN:VEVENT");
        self.line(&format!(
            "UID:interview-{}-{}@nexus",
            application.id, interview.uid_event
        ));
        self.line(&format!("DTSTAMP:{}", utc(interview.stamp.to_datetime())));
        self.line(&format!("DTSTART:{}", utc(start)));
        self.line(&format!("DTEND:{}", utc(end)));
        self.line(&format!("SEQUENCE:{}", interview.revisions));
        let label = if interview.kind.is_empty() {
            "Interview".to_owned()
        } else {
            format!("Interview ({})", interview.kind)
        };
        self.text("SUMMARY", &format!("{label}: {}", application.company));
        self.text("DESCRIPTION", &description(application));
        self.line("END:VEVENT");
    }

    fn follow_up(&mut self, application: &applications::Application, at: &Timestamp) {
        self.line("BEGIN:VEVENT");
        self.line(&format!("UID:followup-{}@nexus", application.id));
        self.line(&format!(
            "DTSTAMP:{}",
            utc(application.updated_at.to_datetime())
        ));
        if queue::is_date_only(at) {
            self.line(&format!(
                "DTSTART;VALUE=DATE:{}",
                at.to_datetime().format("%Y%m%d")
            ));
        } else {
            self.line(&format!("DTSTART:{}", utc(at.to_datetime())));
        }
        self.line(&format!("SEQUENCE:{}", application.event_version));
        self.line("TRANSP:TRANSPARENT");
        self.text("SUMMARY", &format!("Follow up: {}", application.company));
        self.text("DESCRIPTION", &description(application));
        self.line("END:VEVENT");
    }
}

fn description(application: &applications::Application) -> String {
    let mut lines = vec![format!("{} (#{})", application.role, application.id)];
    if let Some(action) = application.next_action.as_deref().filter(|a| !a.is_empty()) {
        lines.push(format!("Next action: {action}"));
    }
    lines.join("\n")
}

fn utc(value: DateTime<Utc>) -> String {
    value.format("%Y%m%dT%H%M%SZ").to_string()
}

/// RFC 5545 TEXT escaping. Control characters other than newline are dropped.
fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}
