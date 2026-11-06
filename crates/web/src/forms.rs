//! The dashboard's writes. Every form posts here, passes a CSRF check, and runs its change through
//! `nexus_mcp::call_tool`, the entry point the MCP route shares, so the lifecycle rule,
//! `expectedUpdatedAt`, length limits and error codes are the tools' own. A success redirects (303)
//! to the page with a notice; a failure renders the page again with the notice and what was typed.

use std::collections::HashMap;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use leptos::prelude::*;
use nexus_core::Timestamp;
use nexus_core::auth::{FormToken, SCOPE_TOOLS};
use nexus_core::repo::applications::{self, Application};
use nexus_core::repo::contacts;
use rand::Rng;
use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;

use crate::dashboard::{Dashboard, PageError, detail_page};
use crate::views::{self, Nav};

pub(crate) type Typed = HashMap<String, String>;

/// A notice above the forms: what happened, in words.
pub struct Flash {
    pub ok: bool,
    pub text: String,
}

/// What the forms on the application page need besides the application itself.
pub struct Actions {
    pub token: String,
    pub flash: Option<Flash>,
    /// Which form the flash is about, so that form opens with its values kept.
    pub failed: Option<&'static str>,
    pub typed: Typed,
    /// Fixed when the page is built, so that sending a form twice is one change (idempotent retry).
    pub at: String,
    pub key: String,
}

impl Actions {
    pub fn new(token: String) -> Self {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        Self {
            token,
            flash: None,
            failed: None,
            typed: Typed::new(),
            at: Timestamp::now().as_str().to_owned(),
            key: hex::encode(bytes),
        }
    }
}

/// A form records a person's action as it happens. A page left open this long still gets through
/// without JavaScript, and its time is then out by that much.
const MAX_FORM_AGE_SECONDS: i64 = 24 * 3600;
/// Room for a device's clock to run ahead.
const MAX_FORM_AHEAD_SECONDS: i64 = 600;

const FORMS: [&str; 6] = ["note", "triage", "event", "contact", "archive", "unarchive"];

pub(crate) fn done_notice(done: &str) -> Option<Flash> {
    let text = match done {
        "note" => "Note added.",
        "triage" => "Rating and triage saved.",
        "event" => "Event recorded.",
        "contact" => "Contact added.",
        "archive" => "Application archived.",
        "unarchive" => "Application restored to the pipeline.",
        _ => return None,
    };
    Some(Flash {
        ok: true,
        text: text.to_owned(),
    })
}

fn failure_notice(text: &str) -> Flash {
    Flash {
        ok: false,
        text: text.to_owned(),
    }
}

/// The plain-language text for a tool error code. Raw codes and JSON never reach the page.
fn explain(code: &str) -> &'static str {
    match code {
        "conflict" => {
            "This application changed since you opened the page. It now shows the latest version: check it, then send again."
        }
        "not_found" => "This application no longer exists.",
        "application_deleting" => "This application is being deleted.",
        "idempotency_conflict" => {
            "This form was already sent with different content. Reload the page and try again."
        }
        "event_occurred_at_invalid" | "invalid_expected_updated_at" => {
            "A date or time was not accepted."
        }
        "event_metadata_invalid" | "validation" => {
            "A value was not accepted: it may be too long or in the wrong format."
        }
        _ => "The change was not saved. Check the values and try again.",
    }
}

/// The error code of a failed tool result, or `validation` for a rejected input.
fn error_code(result: &Value) -> String {
    if let Some(code) = result["structuredContent"]["error"]["code"].as_str() {
        return code.to_owned();
    }
    match result["content"][0]["text"].as_str() {
        Some(text) if text.starts_with("MCP error") => "validation".to_owned(),
        Some(text) if text.chars().all(|c| c.is_ascii_lowercase() || c == '_') => text.to_owned(),
        _ => String::new(),
    }
}

/// A local date and time typed into a `datetime-local` input, as the instant it means in `tz`.
fn instant(text: &str, tz: Tz) -> Option<String> {
    let naive = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M").ok()?;
    let local = tz.from_local_datetime(&naive).earliest()?;
    Some(Timestamp::from_datetime(local.to_utc()).as_str().to_owned())
}

fn field<'a>(form: &'a Typed, name: &str) -> &'a str {
    form.get(name).map_or("", |value| value.trim())
}

/// The form's `at`, which is only ever now: a crafted date, or a device's clock far out, isn't when
/// this happened. A missing one goes on to the tool, which refuses it in its own words. The refusal
/// renders the page again, with a fresh time for the next send.
fn acted_at(form: &Typed) -> Result<&str, String> {
    let at = field(form, "at");
    let near_now = |at: DateTime<_>| {
        let age = Utc::now().signed_duration_since(at).num_seconds();
        (-MAX_FORM_AHEAD_SECONDS..=MAX_FORM_AGE_SECONDS).contains(&age)
    };
    if at.is_empty() || DateTime::parse_from_rfc3339(at).is_ok_and(near_now) {
        return Ok(at);
    }
    Err(
        "The time on this form is too far from now. Check this device's clock, then send it again."
            .to_owned(),
    )
}

/// A blank number field clears the value; anything else must be a whole number from 1 to 5.
fn score(form: &Typed, name: &str, label: &str) -> Result<Value, String> {
    match field(form, name) {
        "" => Ok(Value::Null),
        text => text
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=5).contains(n))
            .map(Value::from)
            .ok_or_else(|| format!("{label} must be a whole number from 1 to 5.")),
    }
}

/// The tool call a form means, or the reason it can't be made yet.
fn build(
    kind: &str,
    application: &Application,
    form: &Typed,
    tz: Tz,
) -> Result<(&'static str, Value), String> {
    let id = application.id.to_string();
    let expected = field(form, "expected");
    let mut args = Map::new();
    let mut put = |key: &str, value: Value| args.insert(key.to_owned(), value);
    let name = match kind {
        "note" => {
            let note = field(form, "note");
            if note.is_empty() {
                return Err("Write a note first.".to_owned());
            }
            put("applicationId", id.into());
            put("note", note.into());
            put("occurredAt", acted_at(form)?.into());
            put("idempotencyKey", field(form, "key").into());
            put("expectedUpdatedAt", expected.into());
            "append_application_note"
        }
        "triage" => {
            put("id", id.into());
            put("rating", score(form, "rating", "Rating")?);
            put("triageQuality", score(form, "triageQuality", "Triage")?);
            let reason = field(form, "triageReason");
            put(
                "triageReason",
                (!reason.is_empty()).then_some(reason).into(),
            );
            put("expectedUpdatedAt", expected.into());
            "update_application"
        }
        "event" => {
            let detail = field(form, "detail");
            let when = field(form, "when");
            let at = (!when.is_empty())
                .then(|| instant(when, tz).ok_or("The date and time were not understood."))
                .transpose()?;
            let mut metadata = Map::new();
            let event = match field(form, "what") {
                "applied" => {
                    // A stage it already has stays, as in old Nexus's status control.
                    let stage = application.current_stage.as_deref().unwrap_or("applied");
                    metadata.insert("toStage".to_owned(), stage.into());
                    metadata.insert("toStatus".to_owned(), "applied".into());
                    "stage_changed"
                }
                "interview" => {
                    if detail.is_empty() || at.is_none() {
                        return Err("An interview needs a type and a date and time.".to_owned());
                    }
                    metadata.insert("interviewType".to_owned(), detail.into());
                    metadata.insert("scheduledAt".to_owned(), at.into());
                    "interview_scheduled"
                }
                "follow_up" => {
                    let Some(at) = at else {
                        return Err("A follow-up needs a date and time.".to_owned());
                    };
                    metadata.insert("followUpAt".to_owned(), at.into());
                    "follow_up_scheduled"
                }
                "offer" => "offer_received",
                "rejected" => {
                    if !detail.is_empty() {
                        metadata.insert("reason".to_owned(), detail.into());
                    }
                    "application_rejected"
                }
                _ => return Err("Choose what happened.".to_owned()),
            };
            put("applicationId", id.into());
            put("type", event.into());
            put("metadata", Value::Object(metadata));
            put("occurredAt", acted_at(form)?.into());
            put("idempotencyKey", field(form, "key").into());
            put("expectedUpdatedAt", expected.into());
            "record_application_event"
        }
        "contact" => {
            let contact_name = field(form, "name");
            if contact_name.is_empty() {
                return Err("A contact needs a name.".to_owned());
            }
            put("applicationId", id.into());
            put("name", contact_name.into());
            for key in ["email", "phone", "role", "linkedIn"] {
                if !field(form, key).is_empty() {
                    put(key, field(form, key).into());
                }
            }
            "create_contact"
        }
        _ => {
            put("id", id.into());
            put("expectedUpdatedAt", expected.into());
            if kind == "archive" {
                "archive_application"
            } else {
                "unarchive_application"
            }
        }
    };
    Ok((name, Value::Object(args)))
}

/// Whether this application already has a contact with exactly these details. A retry after a lost
/// response sends the same form again, and this form has no key to replay.
async fn contact_is_listed(dash: &Dashboard, id: i64, form: &Typed) -> Result<bool, PageError> {
    let same =
        |stored: &Option<String>, key| stored.as_deref().unwrap_or_default() == field(form, key);
    let listed = contacts::list_for_application(&mut *dash.db.conn().await?, id).await?;
    Ok(listed.iter().any(|contact| {
        contact.name == field(form, "name")
            && same(&contact.email, "email")
            && same(&contact.phone, "phone")
            && same(&contact.role, "role")
            && same(&contact.linked_in, "linkedIn")
    }))
}

/// Archiving and the three lifecycle events that close a chapter ask once more.
fn needs_confirmation(kind: &str, form: &Typed) -> bool {
    kind == "archive"
        || (kind == "event" && matches!(field(form, "what"), "applied" | "offer" | "rejected"))
}

fn confirm_question(kind: &str, form: &Typed, company: &str) -> String {
    match (kind, field(form, "what")) {
        ("archive", _) => {
            format!(
                "Archive {company}? It leaves the pipeline and the calendar until you restore it."
            )
        }
        (_, "applied") => format!("Mark {company} as applied? This changes its status."),
        (_, "offer") => format!("Record an offer from {company}? This changes its status."),
        _ => {
            format!("Mark {company} as rejected? This changes its status and clears its follow-up.")
        }
    }
}

fn confirm_page(application: &Application, kind: &str, form: &Typed, token: &str) -> Response {
    let hidden: Vec<_> = form
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "csrf" | "confirm"))
        .map(
            |(name, value)| view! { <input type="hidden" name=name.clone() value=value.clone() /> },
        )
        .collect();
    let back = format!("/applications/{}", application.id);
    let body = view! {
        <h1>"Please confirm"</h1>
        <p class="lede">{confirm_question(kind, form, &application.company)}</p>
        <form method="post" action=format!("/applications/{}/{kind}", application.id) class="act">
            <input type="hidden" name="csrf" value=token.to_owned() />
            <input type="hidden" name="confirm" value="1" />
            {hidden}
            <div class="buttons">
                <button type="submit">"Confirm"</button>
                <a class="button secondary" href=back>"Cancel"</a>
            </div>
        </form>
    };
    Html(views::page("Confirm", Nav::None, 0, body)).into_response()
}

/// In constant time, since a form token is a secret.
pub(crate) fn csrf_ok(sent: Option<&str>, token: &str) -> bool {
    sent.is_some_and(|sent| sent.as_bytes().ct_eq(token.as_bytes()).into())
}

pub(crate) fn notice_page(status: StatusCode, heading: &str, text: &str) -> Response {
    let body = views::page(heading, Nav::None, 0, views::message(heading, text));
    (status, Html(body)).into_response()
}

pub(crate) async fn submit(
    State(dash): State<Dashboard>,
    Path((id, kind)): Path<(String, String)>,
    axum::Extension(FormToken(token)): axum::Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    let kind = FORMS
        .iter()
        .find(|known| **known == kind)
        .copied()
        .ok_or(PageError(StatusCode::NOT_FOUND))?;
    let id: i64 = id.parse().map_err(|_| PageError(StatusCode::NOT_FOUND))?;
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(notice_page(
            StatusCode::FORBIDDEN,
            "This form has expired",
            "It did not come from your current session. Go back, reload the page and send it again.",
        ));
    }
    let application = applications::get(&mut *dash.db.conn().await?, id)
        .await?
        .ok_or(PageError(StatusCode::NOT_FOUND))?;

    // Held until the contact is stored, so that two sends at once can't both find none.
    let _adding = if kind == "contact" {
        Some(dash.adding_contact.lock().await)
    } else {
        None
    };
    let mut call = build(kind, &application, &form, dash.tz);
    if kind == "contact" && call.is_ok() && contact_is_listed(&dash, id, &form).await? {
        call = Err("This contact is already on this application.".to_owned());
    }
    let failure = match call {
        Err(text) => (StatusCode::UNPROCESSABLE_ENTITY, text),
        Ok((name, args)) => {
            if needs_confirmation(kind, &form) && field(&form, "confirm") != "1" {
                return Ok(confirm_page(&application, kind, &form, &token));
            }
            let result = if kind == "event" && field(&form, "what") == "applied" {
                nexus_mcp::owner::mark_applied(dash.mcp.clone(), &args).await
            } else {
                nexus_mcp::call_tool(dash.mcp.clone(), &[SCOPE_TOOLS], "dashboard", name, args)
                    .await
            };
            let result = serde_json::to_value(&result).unwrap_or_default();
            if result["isError"] != json!(true) {
                return Ok(
                    Redirect::to(&format!("/applications/{id}?done={kind}#actions"))
                        .into_response(),
                );
            }
            let code = error_code(&result);
            let status = if code == "conflict" {
                StatusCode::CONFLICT
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            (status, explain(&code).to_owned())
        }
    };
    let mut actions = Actions::new(token);
    actions.flash = Some(failure_notice(&failure.1));
    actions.failed = Some(kind);
    actions.typed = form;
    Ok((failure.0, Html(detail_page(&dash, id, actions).await?)).into_response())
}

fn value<'a>(actions: &'a Actions, name: &str, saved: &'a str) -> String {
    actions
        .typed
        .get(name)
        .map_or(saved, String::as_str)
        .to_owned()
}

fn hidden(actions: &Actions, application: &Application, with_nonce: bool) -> impl IntoView {
    view! {
        <input type="hidden" name="csrf" value=actions.token.clone() />
        <input type="hidden" name="expected" value=application.updated_at.as_str().to_owned() />
        {with_nonce.then(|| view! {
            <input type="hidden" name="at" value=actions.at.clone() />
            <input type="hidden" name="key" value=actions.key.clone() />
        })}
    }
}

/// The notice and the forms. Each is a `<details>` that opens when it is the one that failed.
pub fn actions_view(a: &Application, x: &Actions) -> impl IntoView {
    let open = |form: &str| x.failed == Some(form);
    let action = |form: &str| format!("/applications/{}/{form}", a.id);
    let flash = x.flash.as_ref().map(|flash| {
        view! {
            <p class=if flash.ok { "banner" } else { "banner warn" } role="status">{flash.text.clone()}</p>
        }
    });
    let what = value(x, "what", "");
    let option = |name: &'static str, label: &'static str| {
        view! { <option value=name selected=what == name>{label}</option> }
    };
    let number = |name: &'static str, saved: Option<i64>| {
        let saved = saved.map(|n| n.to_string()).unwrap_or_default();
        let current = value(x, name, &saved);
        view! {
            <select name=name>
                <option value="" selected=current.is_empty()>"None"</option>
                {(1..=5).map(|n| {
                    let n = n.to_string();
                    view! { <option value=n.clone() selected=current == n>{n.clone()}</option> }
                }).collect::<Vec<_>>()}
            </select>
        }
    };
    let archive = if a.archived_at.is_some() {
        view! {
            <details open=open("unarchive")>
                <summary>"Restore to the pipeline"</summary>
                <form method="post" action=action("unarchive") class="act">
                    {hidden(x, a, false)}
                    <button type="submit">"Restore"</button>
                </form>
            </details>
        }
        .into_any()
    } else {
        view! {
            <details open=open("archive")>
                <summary>"Archive"</summary>
                <form method="post" action=action("archive") class="act">
                    {hidden(x, a, false)}
                    <p class="hint">"Takes it out of the pipeline and the calendar. You can restore it."</p>
                    <button type="submit" class="danger">"Archive\u{2026}"</button>
                </form>
            </details>
        }
        .into_any()
    };
    view! {
        <h2 id="actions">"Update"</h2>
        {flash}
        <div class="card actions">
            <details open=open("note")>
                <summary>"Add a note"</summary>
                <form method="post" action=action("note") class="act">
                    {hidden(x, a, true)}
                    <label>"Note"
                        <textarea name="note" rows="4" maxlength="5000" required>{views::textarea_text(&value(x, "note", ""))}</textarea>
                    </label>
                    <button type="submit">"Add note"</button>
                </form>
            </details>
            <details open=open("event")>
                <summary>"Record what happened"</summary>
                <form method="post" action=action("event") class="act">
                    {hidden(x, a, true)}
                    <label>"What happened"
                        <select name="what" required>
                            <option value="" selected=what.is_empty()>"Choose\u{2026}"</option>
                            {option("applied", "Applied")}
                            {option("interview", "Interview scheduled")}
                            {option("follow_up", "Follow-up planned")}
                            {option("offer", "Offer received")}
                            {option("rejected", "Rejected")}
                        </select>
                    </label>
                    <label>"Interview type, or the reason for a rejection"
                        <input type="text" name="detail" maxlength="255" value=value(x, "detail", "") />
                    </label>
                    <label>"Date and time (interviews and follow-ups)"
                        <input type="datetime-local" name="when" value=value(x, "when", "") />
                    </label>
                    <button type="submit">"Record"</button>
                </form>
            </details>
            <details open=open("triage")>
                <summary>"Rating and triage"</summary>
                <form method="post" action=action("triage") class="act">
                    {hidden(x, a, false)}
                    <label>"Rating (1 to 5)" {number("rating", a.rating)}</label>
                    <label>"Triage priority (1 to 5)" {number("triageQuality", a.triage_quality)}</label>
                    <label>"Triage reason"
                        <input type="text" name="triageReason" maxlength="1000"
                            value=value(x, "triageReason", a.triage_reason.as_deref().unwrap_or_default()) />
                    </label>
                    <button type="submit">"Save"</button>
                </form>
            </details>
            <details open=open("contact")>
                <summary>"Add a contact"</summary>
                <form method="post" action=action("contact") class="act">
                    {hidden(x, a, false)}
                    <label>"Name" <input type="text" name="name" maxlength="255" required value=value(x, "name", "") /></label>
                    <label>"Role" <input type="text" name="role" maxlength="100" value=value(x, "role", "") /></label>
                    <label>"Email" <input type="email" name="email" maxlength="255" value=value(x, "email", "") /></label>
                    <label>"Phone" <input type="tel" name="phone" maxlength="50" value=value(x, "phone", "") /></label>
                    <label>"LinkedIn" <input type="text" name="linkedIn" maxlength="500" value=value(x, "linkedIn", "") /></label>
                    <button type="submit">"Add contact"</button>
                </form>
            </details>
            {archive}
        </div>
    }
}
