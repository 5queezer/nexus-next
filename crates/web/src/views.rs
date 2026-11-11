//! Server-rendered HTML. Leptos escapes every text node and attribute, and no view uses
//! `inner_html`, so stored text cannot become markup. `<textarea>` is the exception: Leptos writes
//! its children as they are, so every one goes through `textarea_text`.

use chrono_tz::Tz;
use leptos::prelude::*;
use leptos::tachys::view::RenderHtml;
use nexus_core::repo::applications::Application;
use nexus_core::repo::contacts::Contact;
use nexus_core::repo::documents::{BlobState, Document};
use nexus_core::repo::events::ApplicationEvent;
use nexus_core::repo::submissions::Submission;
use serde_json::Value;

use crate::assets::VERSION;
use crate::format::{self, VISIBLE_METADATA};
use crate::forms::{self, Actions};
use crate::queue::{self, Clock, Group};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Pipeline,
    FollowUps,
    Cv,
    Assistant,
    None,
}

/// The small numbers in the top nav.
#[derive(Clone, Copy, Default)]
pub struct NavCounts {
    pub overdue: usize,
    /// CV variants whose newest revision waits for the owner's approval.
    pub awaiting: usize,
}

impl From<usize> for NavCounts {
    fn from(overdue: usize) -> Self {
        Self {
            overdue,
            awaiting: 0,
        }
    }
}

pub fn page(title: &str, nav: Nav, counts: impl Into<NavCounts>, body: impl IntoView) -> String {
    page_with_scripts(title, nav, counts, body, &[])
}

/// `scripts` are further files under `/assets/`, for the page that needs them.
pub fn page_with_scripts(
    title: &str,
    nav: Nav,
    counts: impl Into<NavCounts>,
    body: impl IntoView,
    scripts: &[&str],
) -> String {
    let NavCounts { overdue, awaiting } = counts.into();
    let scripts: Vec<_> = scripts
        .iter()
        .map(|name| view! { <script src=format!("/assets/{name}?v={}", *VERSION) defer></script> })
        .collect();
    let current = |tab: Nav| (nav == tab).then_some("page");
    let label = nexus_core::env_label();
    let title = match label {
        Some(label) => format!("[{label}] {title} - Nexus"),
        None => format!("{title} - Nexus"),
    };
    let html = view! {
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="light dark" />
                <meta name="robots" content="noindex" />
                <title>{title}</title>
                <meta name="theme-color" content="#1f5f4a" />
                <link rel="manifest" href="/manifest.webmanifest" />
                <link rel="icon" type="image/png" href="/icons/icon-192.png" />
                <link rel="apple-touch-icon" href="/icons/apple-touch-icon.png" />
                <link rel="stylesheet" href=format!("/assets/app.css?v={}", *VERSION) />
                <script src=format!("/assets/app.js?v={}", *VERSION) defer></script>
                {scripts}
            </head>
            <body>
                <header class="top">
                    <div>
                        <a class="brand" href="/">"Nexus"</a>
                        {label.map(|label| view! { <span class="env">{label}</span> })}
                        <nav aria-label="Main">
                            <a href="/" aria-current=current(Nav::Pipeline)>"Pipeline"</a>
                            <a href="/queue" aria-current=current(Nav::FollowUps)>
                                "Follow-ups"
                                {(overdue > 0).then(|| view! { <span class="count">{overdue}</span> })}
                            </a>
                            <a href="/cv" id="nav-cv" aria-current=current(Nav::Cv)>
                                "CV"
                                {(awaiting > 0).then(|| view! {
                                    <span class="count" title="CV revisions waiting for your approval">{awaiting}</span>
                                })}
                            </a>
                            <a
                                href="/assistant"
                                id="nav-assistant"
                                target="nexus-assistant"
                                aria-current=current(Nav::Assistant)
                            >
                                "Assistant"
                            </a>
                        </nav>
                        <details class="menu account">
                            <summary>"Account"</summary>
                            <div>
                                <a href="/passphrase">"Change passphrase"</a>
                                <a href="/logout">"Log out"</a>
                            </div>
                        </details>
                    </div>
                </header>
                <main>
                    <p class="banner" id="assistant-update-note" role="status" hidden>
                        "The assistant updated Nexus. "
                        <button type="button" class="quiet" id="assistant-refresh">"Reload this page"</button>
                    </p>
                    {body}
                </main>
                <footer class="foot">
                    <a href="/passphrase">"Change passphrase"</a>
                    <a href="/logout">"Log out"</a>
                </footer>
            </body>
        </html>
    };
    format!("<!DOCTYPE html>{}", html.to_html())
}

/// A textarea's content, safe to write unescaped. The leading newline is there for the HTML parser
/// to eat, so that a value starting with its own newline keeps it.
pub fn textarea_text(value: &str) -> String {
    format!("\n{}", value.replace('&', "&amp;").replace('<', "&lt;"))
}

pub fn message(heading: &str, text: &str) -> impl IntoView {
    view! {
        <h1>{heading.to_owned()}</h1>
        <p class="lede">{text.to_owned()}</p>
        <p><a class="crumb" href="/">"Back to the pipeline"</a></p>
    }
}

fn status_cell(application: &Application) -> impl IntoView {
    let stage = application
        .current_stage
        .as_deref()
        .map(format::humanize)
        .filter(|stage| !stage.eq_ignore_ascii_case(&application.status));
    view! {
        <span class=format!("status {}", application.status)>
            {format::status_label(&application.status)}
            {stage.map(|stage| view! { <em>{stage}</em> })}
        </span>
    }
}

fn row(application: &Application, clock: Clock, show_action: bool) -> impl IntoView {
    let overdue = queue::group_of(application, clock) == Group::Overdue;
    let classes = format!(
        "row{}{}",
        if overdue { " overdue" } else { "" },
        if application.archived_at.is_some() {
            " archived"
        } else {
            ""
        }
    );
    let role = if application.archived_at.is_some() {
        format!("{} (archived)", application.role)
    } else {
        application.role.clone()
    };
    let action = show_action
        .then(|| application.next_action.clone())
        .flatten()
        .filter(|action| !action.is_empty());
    let follow_up = application
        .follow_up_at
        .as_ref()
        .map(|at| format::date(at, clock.tz));
    let last_contact = application
        .last_contact
        .as_ref()
        .map(|at| format::date(at, clock.tz));
    let when_class = match (overdue, follow_up.is_some()) {
        (true, _) => "when overdue",
        (false, true) => "when",
        (false, false) => "when none",
    };
    let contact_class = if last_contact.is_some() {
        "when dim"
    } else {
        "when dim none"
    };
    view! {
        <a class=classes href=format!("/applications/{}", application.id)>
            <span class="id">{format!("#{}", application.id)}</span>
            <span class="who">
                <b>{application.company.clone()}</b>
                <span>{role}</span>
                {action.map(|action| view! { <span>{action}</span> })}
            </span>
            {status_cell(application)}
            <span class=if application.rating.is_some() { "num" } else { "num none" }>
                <span class="k">"Rating"</span>
                {match application.rating {
                    Some(rating) => format!("\u{2605} {rating}"),
                    None => "\u{2013}".to_owned(),
                }}
            </span>
            <span class=when_class>
                <span class="k">{if overdue { "Overdue follow-up" } else { "Follow-up" }}</span>
                {follow_up.unwrap_or_else(|| "\u{2013}".to_owned())}
            </span>
            <span class=contact_class>
                <span class="k">"Last contact"</span>
                {last_contact.unwrap_or_else(|| "\u{2013}".to_owned())}
            </span>
        </a>
    }
}

const ROW_HEAD: [&str; 6] = [
    "ID",
    "Company",
    "Status",
    "Rating",
    "Follow-up",
    "Last contact",
];

fn list_head() -> impl IntoView {
    view! {
        <div class="head" aria-hidden="true">
            {ROW_HEAD.map(|label| view! { <span>{label}</span> }).into_iter().collect_view()}
        </div>
    }
}

pub struct PipelineView<'a> {
    pub sections: Vec<(&'a str, Vec<&'a Application>)>,
    pub counts: Vec<(&'a str, usize)>,
    pub total: usize,
    pub status: &'a str,
    pub archived: &'a str,
    pub search: &'a str,
    pub clock: Clock,
}

fn pipeline_href(status: &str, archived: &str, search: &str) -> String {
    let mut params = Vec::new();
    if status != "all" {
        params.push(format!("status={status}"));
    }
    if archived != "hide" {
        params.push(format!("archived={archived}"));
    }
    if !search.is_empty() {
        params.push(format!("q={}", percent_encode(search)));
    }
    if params.is_empty() {
        "/".to_owned()
    } else {
        format!("/?{}", params.join("&"))
    }
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn pipeline(v: PipelineView<'_>) -> impl IntoView {
    let chips = std::iter::once(("all", "All", v.total))
        .chain(
            v.counts
                .iter()
                .map(|(status, count)| (*status, format::status_label(status), *count)),
        )
        .map(|(status, label, count)| {
            view! {
                <a class="chip" href=pipeline_href(status, v.archived, v.search)
                    aria-current=(v.status == status).to_string()>
                    {label}<small>{count}</small>
                </a>
            }
        })
        .collect_view();
    let option = |value: &'static str, label: &'static str| {
        view! { <option value=value selected=v.archived == value>{label}</option> }
    };
    let sections = v
        .sections
        .iter()
        .map(|(title, items)| {
            let clock = v.clock;
            let rows = items.iter().map(|a| row(a, clock, false)).collect_view();
            view! {
                <h2 class="group-title">{format!("{title} ({})", items.len())}</h2>
                {rows}
            }
        })
        .collect_view();
    let shown = v
        .sections
        .iter()
        .map(|(_, items)| items.len())
        .sum::<usize>();
    view! {
        <h1>"Pipeline"</h1>
        <p class="lede">{format!("{shown} of {} opportunities", v.total)}</p>
        <nav class="chips" aria-label="Status">{chips}</nav>
        <form class="search" method="get" action="/">
            <input type="hidden" name="status" value=v.status.to_owned() />
            <input type="search" name="q" value=v.search.to_owned()
                placeholder="Search company, role, notes, description" aria-label="Search" />
            <select name="archived" aria-label="Archived">
                {option("hide", "Hide archived")}
                {option("include", "Include archived")}
                {option("only", "Archived only")}
            </select>
            <button type="submit">"Filter"</button>
        </form>
        {if shown == 0 {
            view! { <p class="empty">"Nothing matches these filters."</p> }.into_any()
        } else {
            view! { <div class="list">{list_head()}{sections}</div> }.into_any()
        }}
    }
}

pub fn follow_up_queue(groups: &[(Group, Vec<&Application>)], clock: Clock) -> impl IntoView {
    let body = if groups.is_empty() {
        view! { <p class="empty">"No open follow-ups."</p> }.into_any()
    } else {
        groups
            .iter()
            .map(|(group, items)| {
                let rows = items.iter().map(|a| row(a, clock, true)).collect_view();
                let title = format!("{} ({})", group.title(), items.len());
                let class = if *group == Group::Overdue {
                    "group-title overdue"
                } else {
                    "group-title"
                };
                view! {
                    <div class="list">
                        <h2 class=class>{title}</h2>
                        {rows}
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <h1>"Follow-ups"</h1>
        <p class="lede">"What needs a nudge, oldest first. Explicit hold instructions in notes move an opportunity to Waiting."</p>
        {body}
    }
}

enum Fact {
    Text(String),
    Link(String),
    Overdue(String),
}

fn facts(items: Vec<(&'static str, Option<Fact>)>) -> impl IntoView {
    let rows = items
        .into_iter()
        .filter_map(|(label, value)| value.map(|value| (label, value)))
        .map(|(label, value)| {
            let cell = match value {
                Fact::Text(text) => text.into_any(),
                Fact::Link(url) => match format::web_url(&url) {
                    Some(href) => {
                        let href = href.to_owned();
                        view! { <a href=href rel="noopener noreferrer nofollow">{url}</a> }
                            .into_any()
                    }
                    None => url.into_any(),
                },
                Fact::Overdue(text) => view! {
                    {text}" "<span class="tag warn">"overdue"</span>
                }
                .into_any(),
            };
            view! { <dt>{label}</dt><dd>{cell}</dd> }
        })
        .collect_view();
    view! { <dl class="facts">{rows}</dl> }
}

fn text(value: Option<&str>) -> Option<Fact> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| Fact::Text(v.to_owned()))
}

fn stamp(value: &Option<nexus_core::Timestamp>, tz: Tz) -> Option<Fact> {
    value.as_ref().map(|at| Fact::Text(format::moment(at, tz)))
}

fn list(items: &[Value]) -> Option<Fact> {
    let joined: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
    (!joined.is_empty()).then(|| Fact::Text(joined.join(", ")))
}

fn count_of(value: Option<i64>, suffix: &str) -> Option<Fact> {
    value.map(|v| Fact::Text(format!("{v}{suffix}")))
}

fn flag(value: Option<bool>) -> Option<Fact> {
    value.map(|v| Fact::Text(format::yes_no(v).to_owned()))
}

pub struct Detail<'a> {
    pub application: &'a Application,
    pub events: &'a [ApplicationEvent],
    pub contacts: &'a [Contact],
    pub documents: &'a [Document],
    pub submissions: &'a [Submission],
    pub clock: Clock,
    pub actions: &'a Actions,
    /// The timeline filter from the address: `notes`, `interviews`, or anything else for all.
    pub show: &'a str,
}

/// Stored text with its web addresses as links, and the first address. Only `http://` and
/// `https://` start a link, so a stored `javascript:` URL stays text. Leptos escapes the rest.
fn linked(text: &str) -> (Vec<AnyView>, Option<String>) {
    let mut parts = Vec::new();
    let mut plain = String::new();
    let mut first = None;
    for piece in text.split_inclusive(char::is_whitespace) {
        let lower = piece.to_ascii_lowercase();
        let start = [lower.find("http://"), lower.find("https://")]
            .into_iter()
            .flatten()
            .min();
        let url = start.map(|start| {
            let rest = &piece[start..];
            let end = rest
                .find(|c: char| c.is_whitespace() || "<>\"".contains(c))
                .unwrap_or(rest.len());
            rest[..end].trim_end_matches(|c: char| ".,;:!?)]}'".contains(c))
        });
        match (start, url) {
            (Some(start), Some(url)) if !url.ends_with("://") => {
                first.get_or_insert_with(|| url.to_owned());
                plain.push_str(&piece[..start]);
                parts.push(std::mem::take(&mut plain).into_any());
                parts.push(
                    view! { <a href=url.to_owned() rel="noopener noreferrer">{url.to_owned()}</a> }
                        .into_any(),
                );
                plain.push_str(&piece[start + url.len()..]);
            }
            _ => plain.push_str(piece),
        }
    }
    parts.push(plain.into_any());
    (parts, first)
}

/// Long text as its beginning, with all of it behind "Show all". Both are in the page, so it works
/// without JavaScript and the browser's search finds the rest.
fn fold(text: &str, class: &'static str, chars: usize) -> AnyView {
    const LINES: usize = 4;
    let end = [
        text.char_indices().nth(chars).map(|(at, _)| at),
        text.match_indices('\n').nth(LINES - 1).map(|(at, _)| at),
    ]
    .into_iter()
    .flatten()
    .min();
    let Some(end) = end else {
        return view! { <p class=class>{text.to_owned()}</p> }.into_any();
    };
    view! {
        <div class="fold">
            <details>
                <summary><span>"Show all"</span><span>"Show less"</span></summary>
                <p class=class>{text.to_owned()}</p>
            </details>
            <p class=class>{format!("{}\u{2026}", text[..end].trim_end().trim_end_matches('.'))}</p>
        </div>
    }
    .into_any()
}

/// The next step, large: its day, how far away that is, the next action with its links.
fn next_card(a: &Application, clock: Clock, overdue: bool) -> Option<impl IntoView + use<>> {
    let action = a.next_action.as_deref().filter(|action| !action.is_empty());
    if action.is_none() && a.follow_up_at.is_none() {
        return None;
    }
    let date = a.follow_up_at.as_ref().map(|at| {
        let (weekday, day, month) = format::day_parts(at, clock.tz);
        view! {
            <time datetime=at.to_string()><span>{weekday}</span><b>{day}</b><span>{month}</span></time>
        }
    });
    let away = queue::follow_up_day(a, clock.tz)
        .map(|day| format!(" \u{b7} {}", format::days_away(day, clock.today)));
    let (text, link) = linked(action.unwrap_or("Follow up"));
    Some(view! {
        <section class=if overdue { "next overdue" } else { "next" } aria-label="Next step">
            {date}
            <div>
                <p class="away">
                    "Next"{away}
                    {overdue.then(|| view! { " " <span class="tag warn">"overdue"</span> })}
                </p>
                <p class="what">{text}</p>
            </div>
            {link.map(|href| view! {
                <a class="button secondary" href=href rel="noopener noreferrer">"Open link"</a>
            })}
        </section>
    })
}

pub fn detail(d: Detail<'_>) -> impl IntoView {
    let a = d.application;
    let tz = d.clock.tz;
    let overdue = queue::group_of(a, d.clock) == Group::Overdue;
    let follow_up = a.follow_up_at.as_ref().map(|at| {
        let text = format::date(at, tz);
        if overdue {
            Fact::Overdue(text)
        } else {
            Fact::Text(text)
        }
    });
    let stage = a.current_stage.as_deref().map(format::humanize);
    let status = format::status_label(&a.status).to_owned();
    let overview = vec![
        ("Stage", stage.map(Fact::Text)),
        ("Follow-up", follow_up),
        ("Last contact", stamp(&a.last_contact, tz)),
        ("Applied", stamp(&a.applied_at, tz)),
        ("Source", text(a.source.as_deref())),
        ("Incoming source", text(a.incoming_source.as_deref())),
        (
            "Auto-rejected",
            a.auto_rejected.then(|| {
                Fact::Text(
                    match a.auto_reject_reason.as_deref().filter(|r| !r.is_empty()) {
                        Some(reason) => format!("Yes: {reason}"),
                        None => "Yes".to_owned(),
                    },
                )
            }),
        ),
    ];
    let overview = overview.iter().any(|(_, fact)| fact.is_some()).then(|| {
        view! {
            <h2>"Status"</h2>
            <div class="card">{facts(overview)}</div>
        }
    });
    let band = a
        .salary_band_mentioned
        .then(|| "Salary band mentioned in the posting".to_owned());
    let role = facts(vec![
        ("Job URL", a.job_url.clone().map(Fact::Link)),
        (
            "Salary",
            format::salary(
                a.salary_min,
                a.salary_max,
                a.salary_currency.as_deref(),
                a.salary_period.as_deref(),
            )
            .map(Fact::Text),
        ),
        ("Salary type", text(a.salary_type.as_deref())),
        ("Salary band", band.map(Fact::Text)),
        ("Work mode", text(a.work_mode.as_deref())),
        (
            "Remote",
            Some(Fact::Text(format::yes_no(a.remote).to_owned())),
        ),
        ("Eligible countries", list(&a.eligible_countries.0)),
        ("Primary locations", list(&a.primary_locations.0)),
        ("Office days (min)", count_of(a.office_days_min, "")),
        ("Travel", count_of(a.travel_percent, "%")),
        ("Visa sponsorship", flag(a.visa_sponsorship)),
        ("Right to work required", flag(a.right_to_work_required)),
        ("Timezone overlap", text(a.timezone_overlap.as_deref())),
        ("Company size", text(a.company_size.as_deref())),
        ("ATS", text(a.ats_name.as_deref())),
        ("Requisition ID", text(a.requisition_id.as_deref())),
        ("Posting status", text(a.job_liveness.as_deref())),
        ("Posted", stamp(&a.job_posted_at, tz)),
        ("Captured", stamp(&a.job_captured_at, tz)),
        ("Verified", stamp(&a.job_verified_at, tz)),
        ("Closed", stamp(&a.job_closed_at, tz)),
    ]);
    let record = facts(vec![
        (
            "Created",
            Some(Fact::Text(format::date_time(&a.created_at, tz))),
        ),
        (
            "Updated",
            Some(Fact::Text(format::date_time(&a.updated_at, tz))),
        ),
        (
            "Event version",
            Some(Fact::Text(a.event_version.to_string())),
        ),
        ("Resume ID", text(a.resume_id.as_deref())),
    ]);
    let archived = a.archived_at.as_ref().map(|at| {
        view! {
            <p class="banner">{format!("Archived on {}. It stays out of the default pipeline and the calendar.", format::date_time(at, tz))}</p>
        }
    });
    let summary = a.notes.as_deref().filter(|b| !b.is_empty()).map(|body| {
        view! {
            <h2>"Summary"</h2>
            <div class="card">{fold(body, "prose", 420)}</div>
        }
    });
    let job_summary = a
        .job_summary
        .as_deref()
        .filter(|b| !b.is_empty())
        .map(|body| view! { <p class="prose">{body.to_owned()}</p> });
    let description = a
        .job_description
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|body| {
            view! {
                <details>
                    <summary>
                        "Job description"
                        <span class="dim">{format!("{} characters", body.chars().count())}</span>
                    </summary>
                    <div class="scroll"><p class="prose">{body.to_owned()}</p></div>
                </details>
            }
        });
    let scores = [("Rating", a.rating), ("Triage", a.triage_quality)]
        .into_iter()
        .filter_map(|(label, score)| {
            score.map(
                |score| view! { <span class="tag">{format!("{label} {score} / 5")}</span> " " },
            )
        })
        .collect_view();
    let triage_reason = a
        .triage_reason
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|reason| view! { <p class="prose">{reason.to_owned()}</p> });
    let papers = if d.documents.is_empty() && d.submissions.is_empty() {
        view! { <p class="empty">"No documents or submissions yet."</p> }.into_any()
    } else {
        view! {
            <details>
                <summary>
                    "Documents and submissions"
                    <span class="dim">{format!("{} and {}", d.documents.len(), d.submissions.len())}</span>
                </summary>
                <h3>{format!("Documents ({})", d.documents.len())}</h3>
                {documents(d.documents, tz)}
                <h3>{format!("Submissions ({})", d.submissions.len())}</h3>
                {submissions(d.submissions, d.documents, tz)}
            </details>
        }
        .into_any()
    };
    let failed = |form: &str| d.actions.failed == Some(form);
    let restore = a.archived_at.is_some();
    let forms = forms::actions_view(a, d.actions);
    view! {
        <a class="crumb" href="/">"\u{2190} Pipeline"</a>
        <div class="hero">
            <span class="id">{format!("#{}", a.id)}</span>
            <h1>{a.company.clone()}</h1>
            <span class=format!("tag state {}", a.status)>{status}</span>
            {a.rating.map(|rating| view! { <span class="dim">{format!("Rating {rating} / 5")}</span> })}
            <div class="app-actions">
                <a class="button" href="#note">"Add a note"</a>
                <details class="menu">
                    <summary class="button secondary">"More"</summary>
                    <div>
                        <a href="#event">"Record what happened"</a>
                        <a href="#triage">"Rating and triage"</a>
                        <a href="#contact">"Add a contact"</a>
                        <a href="#archive">{if restore { "Restore to the pipeline" } else { "Archive" }}</a>
                    </div>
                </details>
            </div>
        </div>
        <p class="sub">{a.role.clone()}</p>
        {archived}
        {next_card(a, d.clock, overdue)}
        <div class="app-grid">
            <div>
                <h2 id="actions">"Update"</h2>
                {forms.flash}
                <div class="card actions">{forms.note}{forms.event}</div>
                {summary}
                {timeline(a.id, d.events, d.contacts, tz, d.show)}
            </div>
            <div class="app-side">
                <h2>{format!("People ({})", d.contacts.len())}</h2>
                <div class="card actions">{contacts(d.contacts)}{forms.contact}</div>
                {overview}
                <h2>"Details"</h2>
                <div class="card actions">
                    <details id="triage" open=failed("triage")>
                        <summary>
                            "Triage"
                            <span class="dim">{a.triage_quality.map(|q| format!("{q} / 5"))}</span>
                        </summary>
                        <p class="scores">{scores}</p>
                        {triage_reason}
                        {forms.triage}
                    </details>
                    <details>
                        <summary>"Role and posting"</summary>
                        {job_summary}
                        {role}
                    </details>
                    {description}
                    {papers}
                    <details id="archive" open=failed("archive") || failed("unarchive")>
                        <summary>
                            {if restore { "Record and restore" } else { "Record and archive" }}
                            <span class="dim">{format!("Version {}", a.event_version)}</span>
                        </summary>
                        {record}
                        {forms.archive}
                    </details>
                </div>
            </div>
        </div>
    }
}

fn none_yet(what: &str) -> impl IntoView {
    view! { <p class="empty">{format!("No {what}.")}</p> }
}

/// How many timeline entries show before "Show N older entries".
const NEWEST: usize = 6;

/// The filter an event falls under, which is also the class that colours its dot.
fn event_kind(event_type: &str) -> &'static str {
    match event_type {
        "note_added" => "note",
        "stage_changed" => "stage",
        kind if kind.starts_with("interview_") => "interview",
        _ => "other",
    }
}

/// `Stage: A → B` for a pair of metadata values, in one line, and whether the value moved.
fn change(label: &str, from: Option<String>, to: Option<String>) -> Option<(String, bool)> {
    let (from, to) = (
        from.as_deref().map(format::humanize),
        to.as_deref().map(format::humanize),
    );
    match (from, to) {
        (Some(from), Some(to)) if from != to => {
            Some((format!("{label}: {from} \u{2192} {to}"), true))
        }
        (_, Some(to)) => Some((format!("{label}: {to}"), false)),
        (Some(from), None) => Some((format!("{label}: was {from}"), false)),
        (None, None) => None,
    }
}

fn timeline_entry(event: &ApplicationEvent, contacts: &[Contact], tz: Tz) -> impl IntoView + use<> {
    let metadata = event.metadata.as_ref().and_then(|m| m.0.as_object());
    let value = |key: &str| match metadata?.get(key)? {
        Value::String(s) if !s.is_empty() => Some(format::metadata_text(s, tz)),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let mut stage = change("Stage", value("fromStage"), value("toStage"));
    let mut status = change("Status", value("fromStatus"), value("toStatus"));
    let staged = event.r#type == "stage_changed";
    let title = match event.r#type.as_str() {
        "note_added" => "Note".to_owned(),
        "stage_changed" => stage
            .take()
            .or_else(|| status.take())
            .map(|(line, _)| line)
            .unwrap_or_else(|| format::event_label(&event.r#type)),
        other => format::event_label(other),
    };
    // A stage change shows what moved. On any other event these are a side effect, kept in Details.
    let (lines, details): (Vec<_>, Vec<_>) = stage
        .into_iter()
        .chain(status)
        .partition(|(_, moved)| staged && *moved);
    let mut lines: Vec<String> = lines.into_iter().map(|(line, _)| line).collect();
    let mut details: Vec<String> = details.into_iter().map(|(line, _)| line).collect();
    let mut shown: Vec<String> = Vec::new();
    let mut note = None;
    for (key, label) in VISIBLE_METADATA {
        let Some(text) = value(key) else { continue };
        match key {
            "fromStage" | "toStage" | "fromStatus" | "toStatus" => {}
            "note" => note = Some(text),
            "interviewType" | "scheduledAt" | "followUpAt" | "outcome" | "reason" => {
                shown.push(format!("{label}: {text}"));
            }
            _ => details.push(format!("{label}: {text}")),
        }
    }
    if !shown.is_empty() {
        lines.push(shown.join(" \u{b7} "));
    }
    if let Some(contact) = event
        .contact_id
        .as_deref()
        .and_then(|id| contacts.iter().find(|c| c.id.to_string() == id))
    {
        details.push(format!("Contact: {}", contact.name));
    }
    let by = [event.actor.as_deref(), event.source.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" via ");
    if !by.is_empty() {
        details.push(format!("By: {by}"));
    }
    let details = (!details.is_empty()).then(|| {
        view! {
            <details class="more">
                <summary>"Details"</summary>
                <ul>{details.into_iter().map(|d| view! { <li>{d}</li> }).collect_view()}</ul>
            </details>
        }
    });
    view! {
        <li class=event_kind(&event.r#type)>
            <div class="t">
                <b>{title}</b>
                <time datetime=event.occurred_at.to_string()>{format::date_time(&event.occurred_at, tz)}</time>
            </div>
            {lines.into_iter().map(|line| view! { <p class="line">{line}</p> }).collect_view()}
            {note.map(|note| fold(&note, "note", 240))}
            {details}
        </li>
    }
}

fn timeline(
    id: i64,
    events: &[ApplicationEvent],
    contacts: &[Contact],
    tz: Tz,
    show: &str,
) -> impl IntoView {
    let count = |kind: &str| {
        events
            .iter()
            .filter(|event| event_kind(&event.r#type) == kind)
            .count()
    };
    let filters = [
        ("all", "All", events.len(), None),
        ("notes", "Notes", count("note"), Some("note")),
        (
            "interviews",
            "Interviews",
            count("interview"),
            Some("interview"),
        ),
    ];
    let current = filters
        .iter()
        .find(|(name, ..)| *name == show)
        .unwrap_or(&filters[0]);
    let chips = filters
        .iter()
        .map(|(name, label, count, _)| {
            let href = match *name {
                "all" => format!("/applications/{id}#timeline"),
                name => format!("/applications/{id}?show={name}#timeline"),
            };
            view! {
                <a class="chip" href=href aria-current=(*name == current.0).to_string()>
                    {*label}<small>{*count}</small>
                </a>
            }
        })
        .collect_view();
    let mut entries = events
        .iter()
        .filter(|event| {
            current
                .3
                .is_none_or(|kind| event_kind(&event.r#type) == kind)
        })
        .map(|event| timeline_entry(event, contacts, tz));
    let newest: Vec<_> = entries.by_ref().take(NEWEST).collect();
    let older: Vec<_> = entries.collect();
    let body = if newest.is_empty() {
        none_yet(match current.0 {
            "notes" => "notes",
            "interviews" => "interviews",
            _ => "events",
        })
        .into_any()
    } else {
        let older = (!older.is_empty()).then(|| {
            view! {
                <details class="older">
                    <summary>{format!("Show {} older entries", older.len())}</summary>
                    <ol class="timeline">{older}</ol>
                </details>
            }
        });
        view! { <ol class="timeline">{newest}</ol>{older} }.into_any()
    };
    view! {
        <div class="tl-head">
            <h2 id="timeline">"Timeline"</h2>
            <nav class="chips" aria-label="Timeline filter">{chips}</nav>
        </div>
        <div class="card">{body}</div>
    }
}

/// An email address as a `mailto:` link, when the stored text looks like one.
fn mailto(email: &str) -> AnyView {
    match email.split_once('@') {
        Some((name, host)) if !name.is_empty() && !host.is_empty() => {
            let href = format!("mailto:{}@{}", percent_encode(name), percent_encode(host));
            view! { <a href=href>{email.to_owned()}</a> }.into_any()
        }
        _ => email.to_owned().into_any(),
    }
}

fn contacts(contacts: &[Contact]) -> impl IntoView {
    if contacts.is_empty() {
        return none_yet("contacts").into_any();
    }
    let items = contacts
        .iter()
        .map(|c| {
            let linked_in = c.linked_in.as_deref().filter(|u| !u.is_empty()).map(|url| {
                match format::web_url(url) {
                    Some(href) => view! {
                        <a href=href.to_owned() rel="noopener noreferrer nofollow">"LinkedIn"</a>
                    }
                    .into_any(),
                    None => url.to_owned().into_any(),
                }
            });
            let detail = [c.role.as_deref(), c.phone.as_deref()]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            let email = c.email.as_deref().filter(|e| !e.is_empty()).map(mailto);
            view! {
                <li>
                    <span><b>{c.name.clone()}</b><small>{detail}</small></span>
                    <span class="links">{email}{linked_in}</span>
                </li>
            }
        })
        .collect_view();
    view! { <ul class="items">{items}</ul> }.into_any()
}

fn documents(documents: &[Document], tz: Tz) -> impl IntoView {
    if documents.is_empty() {
        return none_yet("documents").into_any();
    }
    let items = documents
        .iter()
        .map(|doc| {
            let access = match doc.blob_state {
                BlobState::Present => view! {
                    <a class="dl" href=format!("/api/documents/{}/download", doc.id)>"Download"</a>
                }
                .into_any(),
                BlobState::Missing => {
                    view! { <span class="tag warn">"file missing"</span> }.into_any()
                }
            };
            let meta = format!(
                "{} \u{b7} {} \u{b7} {} \u{b7} v{} \u{b7} uploaded {}",
                format::humanize(&doc.document_type),
                format::humanize(&doc.state),
                format::file_size(doc.size),
                doc.version,
                format::date(&doc.uploaded_at, tz),
            );
            view! {
                <li>
                    <span><b>{doc.original_name.clone()}</b><small>{meta}</small></span>
                    {access}
                </li>
            }
        })
        .collect_view();
    view! { <ul class="items">{items}</ul> }.into_any()
}

fn answers(answers: &Value) -> impl IntoView {
    let items = answers.as_array().map(Vec::as_slice).unwrap_or_default();
    let rows = items
        .iter()
        .map(|item| {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let sensitive = item.get("sensitive").and_then(Value::as_bool) == Some(true);
            view! {
                <div class="qa">
                    <p class="q">
                        {field("question")}
                        {sensitive.then(|| view! { " " <span class="tag">"sensitive"</span> })}
                    </p>
                    <p class="a">{field("answer")}</p>
                </div>
            }
        })
        .collect_view();
    view! { {rows} }
}

fn submissions(submissions: &[Submission], documents: &[Document], tz: Tz) -> impl IntoView {
    if submissions.is_empty() {
        return none_yet("submissions").into_any();
    }
    let cards = submissions
        .iter()
        .map(|s| {
            let salary = format::salary(
                s.candidate_salary_min,
                s.candidate_salary_max,
                s.candidate_salary_currency.as_deref(),
                s.candidate_salary_period.as_deref(),
            )
            .map(|salary| {
                let mut text = salary;
                if let Some(kind) = s.candidate_salary_type.as_deref().filter(|k| !k.is_empty()) {
                    text = format!("{text} ({kind})");
                }
                if s.candidate_salary_flexible {
                    text.push_str(", flexible");
                }
                Fact::Text(text)
            });
            let submitted: Vec<String> = documents
                .iter()
                .filter(|d| d.submission_id == Some(s.id))
                .map(|d| d.original_name.clone())
                .collect();
            let policy = s
                .policy
                .0
                .as_object()
                .map(|policy| {
                    policy
                        .iter()
                        .filter_map(|(key, value)| {
                            let value = match value {
                                Value::String(s) => s.clone(),
                                Value::Bool(b) => b.to_string(),
                                Value::Number(n) => n.to_string(),
                                _ => return None,
                            };
                            Some(format!("{key}: {value}"))
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|p| !p.is_empty());
            let overview = facts(vec![
                (
                    "Submitted",
                    Some(Fact::Text(format::date_time(&s.submitted_at, tz))),
                ),
                ("Application URL", s.application_url.clone().map(Fact::Link)),
                ("ATS", text(s.ats_name.as_deref())),
                ("Requisition ID", text(s.requisition_id.as_deref())),
                ("Language", text(s.language.as_deref())),
                ("Salary expectation", salary),
                (
                    "Documents",
                    (!submitted.is_empty()).then(|| Fact::Text(submitted.join(", "))),
                ),
                ("Policy", policy.map(Fact::Text)),
            ]);
            let count = s.answers.0.as_array().map_or(0, Vec::len);
            view! {
                <div class="submission">
                    {overview}
                    <details open>
                        <summary>{format!("Answers ({count})")}</summary>
                        {answers(&s.answers.0)}
                    </details>
                </div>
            }
        })
        .collect_view();
    view! { {cards} }.into_any()
}
