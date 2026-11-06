//! Server-rendered HTML. Leptos escapes every text node and attribute, and no view uses
//! `inner_html`, so stored text cannot become markup.

use chrono::NaiveDate;
use leptos::prelude::*;
use leptos::tachys::view::RenderHtml;
use nexus_core::repo::applications::Application;
use nexus_core::repo::contacts::Contact;
use nexus_core::repo::documents::{BlobState, Document};
use nexus_core::repo::events::ApplicationEvent;
use nexus_core::repo::submissions::Submission;
use serde_json::Value;

use crate::format::{self, VISIBLE_METADATA};
use crate::queue::{self, Group};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Pipeline,
    FollowUps,
    None,
}

pub fn page(title: &str, nav: Nav, overdue: usize, body: impl IntoView) -> String {
    let current = |tab: Nav| (nav == tab).then_some("page");
    let html = view! {
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="light dark" />
                <meta name="robots" content="noindex" />
                <title>{format!("{title} - Nexus")}</title>
                <link rel="stylesheet" href="/assets/app.css" />
            </head>
            <body>
                <header class="top">
                    <div>
                        <a class="brand" href="/">"Nexus"</a>
                        <nav aria-label="Main">
                            <a href="/" aria-current=current(Nav::Pipeline)>"Pipeline"</a>
                            <a href="/queue" aria-current=current(Nav::FollowUps)>
                                "Follow-ups"
                                {(overdue > 0).then(|| view! { <span class="count">{overdue}</span> })}
                            </a>
                        </nav>
                    </div>
                </header>
                <main>{body}</main>
            </body>
        </html>
    };
    format!("<!DOCTYPE html>{}", html.to_html())
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

fn row(application: &Application, today: NaiveDate, show_action: bool) -> impl IntoView {
    let overdue = queue::group_of(application, today) == Group::Overdue;
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
    let follow_up = application.follow_up_at.as_ref().map(format::date);
    let last_contact = application.last_contact.as_ref().map(format::date);
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
                <span class="k">"Follow-up"</span>
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
    pub today: NaiveDate,
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
            let today = v.today;
            let rows = items.iter().map(|a| row(a, today, false)).collect_view();
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
            view! { <div class="list" style="margin-top:16px">{list_head()}{sections}</div> }.into_any()
        }}
    }
}

pub fn follow_up_queue(groups: &[(Group, Vec<&Application>)], today: NaiveDate) -> impl IntoView {
    let body = if groups.is_empty() {
        view! { <p class="empty">"No open follow-ups."</p> }.into_any()
    } else {
        groups
            .iter()
            .map(|(group, items)| {
                let rows = items.iter().map(|a| row(a, today, true)).collect_view();
                let title = format!("{} ({})", group.title(), items.len());
                let class = if *group == Group::Overdue {
                    "group-title overdue"
                } else {
                    "group-title"
                };
                view! {
                    <div class="list" style="margin-bottom:16px">
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

fn stamp(value: &Option<nexus_core::Timestamp>) -> Option<Fact> {
    value.as_ref().map(|at| Fact::Text(format::date_time(at)))
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
    pub today: NaiveDate,
}

pub fn detail(d: Detail<'_>) -> impl IntoView {
    let a = d.application;
    let overdue = queue::group_of(a, d.today) == Group::Overdue;
    let follow_up = a.follow_up_at.as_ref().map(|at| {
        let text = format::date(at);
        if overdue {
            Fact::Overdue(text)
        } else {
            Fact::Text(text)
        }
    });
    let stage = a.current_stage.as_deref().map(format::humanize);
    let status = format::status_label(&a.status).to_owned();
    let overview = facts(vec![
        ("Status", Some(Fact::Text(status))),
        ("Stage", stage.map(Fact::Text)),
        ("Rating", count_of(a.rating, " / 5")),
        (
            "Triage",
            a.triage_quality.map(|q| {
                let reason = a.triage_reason.as_deref().filter(|r| !r.is_empty());
                Fact::Text(match reason {
                    Some(reason) => format!("{q} / 5: {reason}"),
                    None => format!("{q} / 5"),
                })
            }),
        ),
        ("Next action", text(a.next_action.as_deref())),
        ("Follow-up", follow_up),
        ("Last contact", stamp(&a.last_contact)),
        ("Applied", stamp(&a.applied_at)),
        ("Source", text(a.source.as_deref())),
        ("Incoming source", text(a.incoming_source.as_deref())),
        ("Resume ID", text(a.resume_id.as_deref())),
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
    ]);
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
        ("Posted", stamp(&a.job_posted_at)),
        ("Captured", stamp(&a.job_captured_at)),
        ("Verified", stamp(&a.job_verified_at)),
        ("Closed", stamp(&a.job_closed_at)),
    ]);
    let record = facts(vec![
        (
            "Created",
            Some(Fact::Text(format::date_time(&a.created_at))),
        ),
        (
            "Updated",
            Some(Fact::Text(format::date_time(&a.updated_at))),
        ),
        (
            "Event version",
            Some(Fact::Text(a.event_version.to_string())),
        ),
    ]);
    let archived = a.archived_at.as_ref().map(|at| {
        view! {
            <p class="banner">{format!("Archived on {}. It stays out of the default pipeline and the calendar.", format::date_time(at))}</p>
        }
    });
    let prose = |heading: &'static str, body: Option<&str>| {
        body.filter(|b| !b.is_empty()).map(|body| {
            view! {
                <h2>{heading}</h2>
                <div class="card"><p class="prose">{body.to_owned()}</p></div>
            }
        })
    };
    let description = a
        .job_description
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(|body| {
            view! {
                <h2>"Job description"</h2>
                <div class="card">
                    <details>
                        <summary>{format!("{} characters", body.chars().count())}</summary>
                        <div class="scroll"><p class="prose">{body.to_owned()}</p></div>
                    </details>
                </div>
            }
        });
    view! {
        <a class="crumb" href="/">"\u{2190} Pipeline"</a>
        <div class="hero">
            <span class="id">{format!("#{}", a.id)}</span>
            <h1>{a.company.clone()}</h1>
        </div>
        <p class="sub">{a.role.clone()}</p>
        {archived}
        <h2>"Application"</h2>
        <div class="card">{overview}</div>
        {prose("Notes", a.notes.as_deref())}
        {prose("Job summary", a.job_summary.as_deref())}
        <h2>"Role and posting"</h2>
        <div class="card">{role}</div>
        {description}
        <h2>{format!("Timeline ({})", d.events.len())}</h2>
        {timeline(d.events, d.contacts)}
        <h2>{format!("Contacts ({})", d.contacts.len())}</h2>
        {contacts(d.contacts)}
        <h2>{format!("Documents ({})", d.documents.len())}</h2>
        {documents(d.documents)}
        <h2>{format!("Submissions ({})", d.submissions.len())}</h2>
        {submissions(d.submissions, d.documents)}
        <h2>"Record"</h2>
        <div class="card">{record}</div>
    }
}

fn none_yet(what: &str) -> impl IntoView {
    view! { <div class="card"><p class="empty" style="padding:12px 0">{format!("No {what}.")}</p></div> }
}

fn timeline(events: &[ApplicationEvent], contacts: &[Contact]) -> impl IntoView {
    if events.is_empty() {
        return none_yet("events").into_any();
    }
    let items = events
        .iter()
        .map(|event| {
            let metadata = event.metadata.as_ref().and_then(|m| m.0.as_object());
            let mut details: Vec<String> = Vec::new();
            let mut note = None;
            if let Some(metadata) = metadata {
                for (key, label) in VISIBLE_METADATA {
                    let text = match metadata.get(key) {
                        Some(Value::String(s)) if !s.is_empty() => format::metadata_text(s),
                        Some(Value::Number(n)) => n.to_string(),
                        Some(Value::Bool(b)) => b.to_string(),
                        _ => continue,
                    };
                    if key == "note" {
                        note = Some(text);
                    } else {
                        details.push(format!("{label}: {text}"));
                    }
                }
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
            let details = (!details.is_empty()).then(|| {
                view! { <ul>{details.into_iter().map(|d| view! { <li>{d}</li> }).collect_view()}</ul> }
            });
            view! {
                <li>
                    <div class="t">
                        <b>{format::event_label(&event.r#type)}</b>
                        <time datetime=event.occurred_at.to_string()>{format::date_time(&event.occurred_at)}</time>
                    </div>
                    {(!by.is_empty()).then(|| view! { <span class="dim">{by}</span> })}
                    {note.map(|note| view! { <p class="note">{note}</p> })}
                    {details}
                </li>
            }
        })
        .collect_view();
    view! { <div class="card"><ol class="timeline">{items}</ol></div> }.into_any()
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
            let detail = [c.role.as_deref(), c.email.as_deref(), c.phone.as_deref()]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            view! {
                <li>
                    <span><b>{c.name.clone()}</b><small>{detail}</small></span>
                    <span>{linked_in}</span>
                </li>
            }
        })
        .collect_view();
    view! { <div class="card"><ul class="items">{items}</ul></div> }.into_any()
}

fn documents(documents: &[Document]) -> impl IntoView {
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
                format::date(&doc.uploaded_at),
            );
            view! {
                <li>
                    <span><b>{doc.original_name.clone()}</b><small>{meta}</small></span>
                    {access}
                </li>
            }
        })
        .collect_view();
    view! { <div class="card"><ul class="items">{items}</ul></div> }.into_any()
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

fn submissions(submissions: &[Submission], documents: &[Document]) -> impl IntoView {
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
                    Some(Fact::Text(format::date_time(&s.submitted_at))),
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
                <div class="card" style="margin-bottom:12px">
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
