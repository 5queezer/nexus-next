//! The CV workspace, `/cv`: variants, source editor, preview, approval, revisions and the facts page.
//! A revert runs `revert_cv` through `nexus_mcp::call_tool`, and a save the same code as `patch_cv`,
//! so the integrity check and the revision rules are the tools' own, whoever is editing. What only
//! the owner may do goes through `nexus_mcp::owner`, from these handlers, behind the session and the
//! CSRF token: a save stored here is approved, an agent's never is. Nothing here needs JavaScript
//! except the live preview and voice.

use std::path::Path as FilePath;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router, middleware};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono_tz::Tz;
use leptos::prelude::*;
use nexus_core::auth::{FormToken, SCOPE_TOOLS};
use nexus_core::blob::sha256_hex;
use nexus_core::repo::cv_approvals;
use nexus_core::repo::cv_facts::{self, FactChange, FactsState, LockPreview, OwnerFacts};
use nexus_core::repo::cv_revisions;
use nexus_core::{Db, Error, Timestamp};
use nexus_mcp::{CallToolResult, McpState, Review, owner};
use nexus_tailor::{LineChange, Language, Metric, ReviewItem, Rule, review_diff};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::cv_events::head_number;
use crate::dashboard::{PageError, with_form_token};
use crate::format;
use crate::forms::{Flash, Typed, csrf_ok, notice_page};
use crate::render_cache::RenderCache;
use crate::views::{self, Nav, NavCounts};
use crate::voice::{self, MAX_SESSION_BODY_BYTES, MAX_TOOL_BODY_BYTES, Voice, VoiceConfig};

/// A source of 256 KiB percent-encoded, with the small fields around it.
const SAVE_BODY_LIMIT: usize = 1024 * 1024;
const REVISIONS_PER_PAGE: i64 = 20;
const PREVIEW_PAGES: u32 = 3;
const FACTS_ACTOR: &str = "dashboard";
/// How many review items a page lists. Past that the diff still marks them where they stand.
const MAX_LISTED_ITEMS: usize = 200;
/// Unchanged lines kept around a change in the diff, so that it reads in place.
const DIFF_CONTEXT_LINES: usize = 1;

#[derive(Clone)]
pub(crate) struct Cv {
    pub db: Db,
    pub mcp: Arc<McpState>,
    pub tz: Tz,
    pub voice: Voice,
    cache: RenderCache,
    /// A page asks for its image and its page count at once: one compile serves both.
    rendering: Arc<Mutex<()>>,
}

/// Mounts `/cv/*`, `/api/voice/*` and the slow-poll fallback. `voice` is `None` without a key file:
/// the routes then answer that voice is off, and nothing can reach out. Auth is the caller's.
pub fn cv_router(
    state: McpState,
    tz: Tz,
    voice: Option<VoiceConfig>,
    data_dir: &FilePath,
) -> Router {
    with_form_token(
        Router::new()
            .route("/cv", get(list_page))
            .route("/cv/facts", get(facts_page).post(facts_save))
            .route("/cv/facts/lock", post(facts_lock))
            .route("/cv/facts/unlock", post(facts_unlock))
            .route("/cv/variants/{variant}", get(variant_page))
            .route("/cv/variants/{variant}/save", post(save))
            .route("/cv/variants/{variant}/approve", post(approve))
            .route("/cv/variants/{variant}/revert", post(revert))
            .route("/cv/variants/{variant}/preview.png", get(preview_png))
            .route("/cv/variants/{variant}/preview.json", get(preview_json))
            .route("/cv/variants/{variant}/cv.pdf", get(pdf))
            .route("/api/cv/{variant}/head", get(head))
            .layer(DefaultBodyLimit::max(SAVE_BODY_LIMIT))
            .route(
                "/api/voice/session",
                post(voice::session).layer(DefaultBodyLimit::max(MAX_SESSION_BODY_BYTES)),
            )
            .route(
                "/api/voice/tool",
                post(voice::tool).layer(DefaultBodyLimit::max(MAX_TOOL_BODY_BYTES)),
            ),
    )
    .layer(middleware::map_response(crate::secure))
    .with_state(Cv {
        db: state.db.clone(),
        mcp: Arc::new(state),
        tz,
        voice: Voice::new(voice),
        cache: RenderCache::open(data_dir),
        rendering: Arc::default(),
    })
}

/// What a tool said: its JSON, and whether it refused.
struct Told {
    value: Value,
    is_error: bool,
    result: CallToolResult,
}

impl Told {
    fn code(&self) -> &str {
        self.value["error"]["code"].as_str().unwrap_or_default()
    }
}

fn told(result: CallToolResult) -> Told {
    let value = result.structured_content.clone().unwrap_or(Value::Null);
    Told {
        value,
        is_error: result.is_error == Some(true),
        result,
    }
}

async fn call(cv: &Cv, name: &str, args: Value) -> Told {
    told(nexus_mcp::call_tool(cv.mcp.clone(), &[SCOPE_TOOLS], "dashboard", name, args).await)
}

/// For the top nav. With unlocked facts nothing can be approved, so nothing is waiting.
pub(crate) async fn awaiting_count(db: &Db) -> nexus_core::Result<usize> {
    let mut conn = db.conn().await?;
    let state = cv_facts::get(&mut conn).await?;
    if !state.is_locked() {
        return Ok(0);
    }
    let hash = cv_approvals::facts_sha256(&state.facts);
    Ok(cv_approvals::awaiting(&mut conn, &hash).await?.len())
}

/// Why a tool refused, in words. Raw codes and JSON never reach the page.
fn explain(code: &str) -> &'static str {
    match code {
        "cv_facts_not_locked" => {
            "The facts aren't locked yet, so no CV edit can be stored. Check them and lock them on the Facts page."
        }
        "cv_no_change" => {
            "That is the same text as the current revision, so there is nothing to save."
        }
        "cv_compile_failed" => {
            "The source doesn't compile, so nothing was stored. See the problems below."
        }
        "cv_fact_violation" => {
            "The integrity check refused this, so nothing was stored. See what it found below."
        }
        "cv_approval_stale" => {
            "This changed since you opened the page, so nothing was approved. Read it again below."
        }
        "cv_facts_changed" => {
            "The facts changed while this was compiling. Nothing was stored: try again."
        }
        "cv_render_busy" => "Other CVs are rendering. Nothing was stored: try again shortly.",
        "cv_source_too_large" => "The source is over the 256 KiB limit. Nothing was stored.",
        "cv_too_many_pages" => "The CV is over the 10 page limit. Nothing was stored.",
        "cv_pdf_too_large" | "cv_render_result_too_large" => "The PDF is too large to render.",
        "cv_render_timeout" => "The CV took over 30 seconds to compile. Nothing was stored.",
        "cv_render_memory_exceeded" => {
            "The CV used too much memory while compiling. Nothing was stored."
        }
        "cv_render_failed" => "The renderer failed. Nothing was stored: try again.",
        "cv_variant_not_found" => "This CV variant no longer exists.",
        "cv_revision_not_found" => "That revision no longer exists.",
        _ => "This was not accepted. Check the values and try again.",
    }
}

/// How a refusal or a review item reads to the person whose CV it is.
fn rule_text(rule: &str) -> &'static str {
    match rule {
        "missingFact" => "Was on the last approved CV, and isn't on this one",
        "entityBinding" => {
            "A company or institution outside its own entry, or with a date, title or place that isn't its own"
        }
        "languageLevel" => "A language level that differs from your facts",
        "claimWords" => "A degree, title or language claim. Is it true here?",
        "numbers" => "A number or date. Is it the right one, in the right place?",
        "numberWords" => "A spelled-out count. Is it right?",
        "newWords" => "A new word. Is it true?",
        "bannedPhrases" => "A phrase you have banned",
        "images" => "An image, which the check can't read",
        "hiddenText" => "Text a reader can't see",
        "mixedScript" => "A word mixing Latin with lookalike letters",
        "bidiControl" => "A control character that draws text in another order",
        "transform" => "Text drawn rotated, stretched or squeezed, so it can read as something else",
        "contactDetails" => "An address, phone number or link that isn't in your contact details",
        "digitLookalike" => "Letters that read as digits",
        _ => "Something the check refuses",
    }
}

/// `context` with every `token` in it marked.
fn marked(context: &str, token: &str) -> impl IntoView + use<> {
    let mut parts = Vec::new();
    let mut rest = context;
    while !token.is_empty()
        && let Some(at) = rest.find(token)
    {
        parts.push(view! { <span>{rest[..at].to_owned()}</span><mark>{token.to_owned()}</mark> });
        rest = &rest[at + token.len()..];
    }
    let tail = rest.to_owned();
    view! { {parts}<span>{tail}</span> }
}

fn violations_view(violations: &[Value]) -> impl IntoView + use<> {
    let items: Vec<_> = violations
        .iter()
        .map(|violation| {
            let token = violation["token"].as_str().unwrap_or_default();
            let context = violation["context"].as_str().unwrap_or_default();
            view! {
                <li>
                    <b>{rule_text(violation["rule"].as_str().unwrap_or_default())}</b>
                    ": " <mark>{token.to_owned()}</mark>
                    {(!context.is_empty()).then(|| view! {
                        <small class="context">{marked(context, token)}</small>
                    })}
                </li>
            }
        })
        .collect();
    view! { <ul class="problems">{items}</ul> }
}

fn diagnostics_view(diagnostics: &[Value]) -> impl IntoView + use<> {
    let items: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| {
            let place = match (diagnostic["line"].as_u64(), diagnostic["column"].as_u64()) {
                (Some(line), Some(column)) => format!("Line {line}, column {column}"),
                (Some(line), None) => format!("Line {line}"),
                _ => "Somewhere in the document".to_owned(),
            };
            let in_template = diagnostic["file"]
                .as_str()
                .is_some_and(|file| !file.ends_with("main.typ"));
            let hints: Vec<_> = diagnostic["hints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|hint| view! { <small class="context">{hint.to_owned()}</small> })
                .collect();
            view! {
                <li>
                    <b>{place}</b>
                    {in_template.then_some(" (in the template library)")}
                    ": " {diagnostic["message"].as_str().unwrap_or_default().to_owned()}
                    {hints}
                </li>
            }
        })
        .collect();
    view! { <ul class="problems">{items}</ul> }
}

fn flash_view(flash: Option<&Flash>) -> impl IntoView + use<> {
    flash.map(|flash| {
        view! {
            <p class=if flash.ok { "banner" } else { "banner warn" } role="status">{flash.text.clone()}</p>
        }
    })
}

fn expired_form() -> Response {
    notice_page(
        StatusCode::FORBIDDEN,
        "This form has expired",
        "It did not come from your current session. Go back, reload the page and send it again.",
    )
}

fn variant_url(variant: &str) -> String {
    format!("/cv/variants/{variant}")
}

async fn list_page(State(cv): State<Cv>) -> Result<Html<String>, PageError> {
    let mut conn = cv.db.conn().await?;
    let variants = cv_revisions::list_variants(&mut conn).await?;
    let facts = cv_facts::get(&mut conn).await?;
    let awaiting = if facts.is_locked() {
        let hash = cv_approvals::facts_sha256(&facts.facts);
        cv_approvals::awaiting(&mut conn, &hash).await?
    } else {
        Vec::new()
    };
    let awaiting_rows: Vec<_> = awaiting
        .iter()
        .map(|variant| {
            let detail = format!(
                "Revision {}, by {}, {}: {}",
                variant.number,
                variant.origin,
                format::moment(&variant.created_at, cv.tz),
                variant.summary
            );
            view! {
                <li>
                    <span>
                        <a class="dl" href=format!("{}#cv-approval", variant_url(&variant.name))>{variant.name.clone()}</a>
                        <small>{detail}</small>
                    </span>
                    <span class="tag draft">"Draft"</span>
                </li>
            }
        })
        .collect();
    let counts = NavCounts {
        overdue: 0,
        awaiting: awaiting.len(),
    };
    let rows: Vec<_> = variants
        .iter()
        .map(|variant| {
            let detail = match (&variant.head_origin, &variant.head_created_at) {
                (Some(origin), Some(at)) => format!(
                    "Revision {}, by {origin}, {}",
                    variant.head_number,
                    format::moment(at, cv.tz)
                ),
                _ => "Nothing stored yet: starts from the profile".to_owned(),
            };
            let application = variant.application_id.map(|id| {
                view! { <span class="tag">{format!("Application #{id}")}</span> }
            });
            view! {
                <li>
                    <span>
                        <a class="dl" href=variant_url(&variant.name)>{variant.name.clone()}</a>
                        <small>{detail}</small>
                    </span>
                    {application}
                </li>
            }
        })
        .collect();
    let body = view! {
        <h1>"CV"</h1>
        <p class="lede">"Each variant has its own source and revisions. Facts are shared."</p>
        {(!facts.is_locked()).then(|| view! {
            <p class="banner warn" role="status">
                "The facts aren't locked, so no CV edit can be stored yet. "
                <a href="/cv/facts">"Check and lock them"</a>
            </p>
        })}
        {(!awaiting_rows.is_empty()).then(|| view! {
            <h2>"Waiting for your approval"</h2>
            <div class="card"><ul class="items">{awaiting_rows}</ul></div>
        })}
        <h2>"Variants"</h2>
        <div class="card"><ul class="items">{rows}</ul></div>
        <h2>"Facts"</h2>
        <div class="card">
            <ul class="items"><li>
                <span>
                    <a class="dl" href="/cv/facts">"Facts"</a>
                    <small>{facts_state_text(&facts)}</small>
                </span>
            </li></ul>
        </div>
    };
    Ok(Html(views::page("CV", Nav::Cv, counts, body)))
}

fn facts_state_text(state: &FactsState) -> String {
    match &state.locked_at {
        Some(_) => format!("Locked, version {}", state.version),
        None => format!("Not locked, version {}", state.version),
    }
}

#[derive(Deserialize, Default)]
struct VariantQuery {
    done: Option<String>,
    page: Option<i64>,
}

/// What a page shows besides stored data: the outcome of the form just sent.
#[derive(Default)]
struct Outcome {
    flash: Option<Flash>,
    violations: Vec<Value>,
    diagnostics: Vec<Value>,
    typed_source: Option<String>,
    typed_summary: Option<String>,
    /// Set after a compile or check failure, to keep the base those edits were written against. A
    /// conflict leaves it empty on purpose: the page then names the new head, and saving again
    /// after reading it is a choice.
    kept_expected: Option<i64>,
}

fn csp(config: Option<&VoiceConfig>) -> String {
    let (connect, media) = match config {
        Some(config) => (
            format!("connect-src 'self' {}", config.connect_origin()),
            "media-src 'self'; ",
        ),
        None => ("connect-src 'self'".to_owned(), ""),
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; manifest-src 'self'; worker-src 'self'; {connect}; {media}base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    )
}

async fn variant_page(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Query(query): Query<VariantQuery>,
    Extension(FormToken(token)): Extension<FormToken>,
) -> Result<Response, PageError> {
    let outcome = Outcome {
        flash: query.done.as_deref().and_then(done_notice),
        ..Outcome::default()
    };
    render_variant(&cv, &variant, query.page.unwrap_or(1), &token, outcome).await
}

fn done_notice(done: &str) -> Option<Flash> {
    let text = match done {
        "saved" => "Saved and approved: you wrote this, and you are looking at it.",
        "reverted" => "Reverted: the old text is now a new revision.",
        "approved" => "Approved. The PDF of this revision now has no stamp.",
        _ => return None,
    };
    Some(Flash {
        ok: true,
        text: text.to_owned(),
    })
}

async fn render_variant(
    cv: &Cv,
    variant: &str,
    page: i64,
    token: &str,
    outcome: Outcome,
) -> Result<Response, PageError> {
    let page = page.clamp(1, i64::from(i32::MAX));
    let source = call(cv, "get_cv_source", json!({"variant": variant})).await;
    if source.is_error {
        return Err(PageError(StatusCode::NOT_FOUND));
    }
    let review = owner::review(cv.mcp.clone(), variant).await.map_err(|error| told(error));
    let revisions = call(
        cv,
        "list_cv_revisions",
        json!({"variant": variant, "page": page, "pageSize": REVISIONS_PER_PAGE, "includeDiff": true}),
    )
    .await
    .value;
    let head = source.value["revision"].as_i64().unwrap_or(0);
    let locked = source.value["factsLocked"].as_bool().unwrap_or(false);
    let approved = source.value["state"].as_str() == Some("approved");
    let counts = NavCounts {
        overdue: 0,
        awaiting: awaiting_count(&cv.db).await?,
    };
    let text = outcome.typed_source.clone().unwrap_or_else(|| {
        source.value["source"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    });
    let expected = outcome.kept_expected.unwrap_or(head);
    let items = revisions["revisions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let voice_config = cv.voice.config();
    let body = view! {
        <p><a class="crumb" href="/cv">"\u{2190} CV"</a></p>
        <div class="hero">
            <h1>{variant.to_owned()}</h1>
            <span class="id" id="cv-revision">
                {if head == 0 { "nothing stored yet".to_owned() } else { format!("revision {head}") }}
            </span>
            {state_badge(approved)}
        </div>
        <div
            id="cv-workspace"
            data-variant=variant.to_owned()
            data-revision=head.to_string()
            data-state=if approved { "approved" } else { "draft" }
        >
            {(!locked).then(|| view! {
                <p class="banner warn" role="status">
                    "The facts aren't locked, so edits can't be stored yet. "
                    <a href="/cv/facts">"Check and lock them"</a>
                </p>
            })}
            {outcome.typed_source.is_none().then(|| flash_view(outcome.flash.as_ref()))}
            <p class="banner" id="cv-live-note" role="status" hidden></p>
            {approval_view(variant, token, &review)}
            <div class="cv-grid">
                <div class="cv-side">
                    {preview_view(variant, head, approved)}
                    {voice_view(variant, voice_config, token)}
                </div>
                {editor_view(variant, token, &text, expected, &outcome)}
            </div>
            {revisions_view(variant, token, head, page, &revisions, &items, cv.tz)}
        </div>
    };
    let scripts: &[&str] = if voice_config.is_some() {
        &["cv.js", "voice.js"]
    } else {
        &["cv.js"]
    };
    let html = views::page_with_scripts(variant, Nav::Cv, counts, body, scripts);
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&csp(voice_config)) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    if voice_config.is_some() {
        headers.insert(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("microphone=(self)"),
        );
    }
    Ok(response)
}

fn state_badge(approved: bool) -> impl IntoView + use<> {
    if approved {
        view! { <span class="tag approved" id="cv-state">"Approved"</span> }.into_any()
    } else {
        view! { <span class="tag draft" id="cv-state">"Draft, not approved"</span> }.into_any()
    }
}

/// `line`'s text with these tokens marked, wherever they stand exactly as listed, in any case.
fn highlighted(line: &str, tokens: &[&str]) -> impl IntoView + use<> {
    let lower = line.to_ascii_lowercase();
    let mut spots: Vec<(usize, usize)> = tokens
        .iter()
        .filter(|token| !token.is_empty())
        .flat_map(|token| {
            let token = token.to_ascii_lowercase();
            let found: Vec<(usize, usize)> = lower
                .match_indices(&token)
                .map(|(at, found)| (at, at + found.len()))
                .collect();
            found
        })
        .collect();
    spots.sort_unstable();
    let mut parts = Vec::new();
    let mut from = 0;
    for (start, end) in spots {
        if start < from || !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        parts.push(view! { {line[from..start].to_owned()}<mark>{line[start..end].to_owned()}</mark> });
        from = end;
    }
    let tail = line[from..].to_owned();
    view! { {parts}{tail} }
}

/// The rendered text against the last approved text: changed lines with their highlights, and an
/// unchanged line around them so that they read in place.
fn rendered_diff_view(review: &Review) -> impl IntoView + use<> {
    let lines = review_diff(review.baseline.as_deref(), &review.rendered_text);
    let changed: Vec<bool> = lines
        .iter()
        .map(|line| line.change != LineChange::Same)
        .collect();
    let near = |at: usize| {
        let from = at.saturating_sub(DIFF_CONTEXT_LINES);
        let to = (at + DIFF_CONTEXT_LINES + 1).min(changed.len());
        changed[from..to].iter().any(|changed| *changed)
    };
    let mut rows = Vec::new();
    let mut skipped = false;
    for (at, line) in lines.iter().enumerate() {
        if !near(at) {
            skipped = true;
            continue;
        }
        if std::mem::take(&mut skipped) {
            rows.push(view! { <span class="gap">"\u{2026}"</span> }.into_any());
        }
        let tokens: Vec<&str> = review
            .items
            .iter()
            .filter(|item| line.line.is_some() && item.line == line.line)
            .map(|item| item.token.as_str())
            .collect();
        let (class, sign) = match line.change {
            LineChange::Added => ("da", "+ "),
            LineChange::Removed => ("dd", "- "),
            LineChange::Same => ("dc", "  "),
        };
        rows.push(view! { <span class=class>{sign}{highlighted(&line.text, &tokens)}</span> }.into_any());
    }
    view! { <pre class="rendered" id="cv-rendered-diff">{rows}</pre> }
}

fn rule_of(item: &ReviewItem) -> String {
    serde_json::to_value(item.rule)
        .ok()
        .and_then(|rule| rule.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Most serious first, as the check orders them, a rule's items together.
fn review_items_view(items: &[ReviewItem]) -> impl IntoView + use<> {
    let listed: Vec<_> = items
        .iter()
        .take(MAX_LISTED_ITEMS)
        .map(|item| {
            let serious = matches!(item.rule, Rule::MissingFact | Rule::EntityBinding | Rule::LanguageLevel);
            let context = item.context.clone();
            view! {
                <li class=serious.then_some("serious")>
                    <b>{rule_text(&rule_of(item))}</b>
                    ": " <mark>{item.token.clone()}</mark>
                    {(!context.is_empty()).then(|| view! {
                        <small class="context">{marked(&context, &item.token)}</small>
                    })}
                </li>
            }
        })
        .collect();
    let more = items.len().saturating_sub(MAX_LISTED_ITEMS);
    view! {
        <ul class="problems" id="cv-review-items">{listed}</ul>
        {(more > 0).then(|| view! {
            <p class="hint">{format!("And {more} more. The text below marks them where they stand.")}</p>
        })}
    }
}

/// The owner's reading of what they are about to approve: above the editor, since it comes first.
fn approval_view(variant: &str, token: &str, review: &Result<Review, Told>) -> impl IntoView + use<> {
    let inner = match review {
        Err(refused) => {
            let violations = refused.value["error"]["violations"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let diagnostics = refused.value["error"]["diagnostics"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            view! {
                <p class="banner warn" role="status">{explain(refused.code())}" It can't be approved like this."</p>
                {(!violations.is_empty()).then(|| violations_view(&violations))}
                {(!diagnostics.is_empty()).then(|| diagnostics_view(&diagnostics))}
            }
            .into_any()
        }
        Ok(review) if review.approved => view! {
            <p>"You approved this text. The PDF of this revision has no stamp, and an agent can store it for an application."</p>
        }
        .into_any(),
        Ok(review) => {
            let against = review.baseline_revision.map_or_else(
                || "Nothing is approved yet, so this is the whole CV. Every number and date is marked.".to_owned(),
                |at| format!("Compared with revision {at}, the last you approved, however many drafts came between."),
            );
            let action = if review.revision == 0 {
                view! { <p class="hint">"This is the starting text, not stored yet. Saving it below stores and approves it."</p> }.into_any()
            } else if !review.locked {
                view! {
                    <p class="hint">"You can approve once the facts are locked. " <a href="/cv/facts">"Check and lock them"</a></p>
                }
                .into_any()
            } else {
                view! {
                    <form method="post" action=format!("{}/approve#cv-approval", variant_url(variant)) class="approve-form">
                        <input type="hidden" name="csrf" value=token.to_owned() />
                        <input type="hidden" name="revision" value=review.revision.to_string() />
                        <input type="hidden" name="sourceSha256" value=review.source_sha256.clone() />
                        <input type="hidden" name="factsSha256" value=review.facts_sha256.clone() />
                        <button type="submit">{format!("Approve revision {}", review.revision)}</button>
                    </form>
                }
                .into_any()
            };
            view! {
                <p>"An agent wrote or changed this. Nothing leaves here as a final CV until you approve exactly this text."</p>
                {(!review.items.is_empty()).then(|| view! {
                    <h3>"Check these first"</h3>
                    {review_items_view(&review.items)}
                })}
                <h3>"What a reader sees"</h3>
                <p class="hint">{against}</p>
                {rendered_diff_view(review)}
                {action}
                <details>
                    <summary>"Source changes"</summary>
                    {diff_view(&review.source_diff)}
                </details>
            }
            .into_any()
        }
    };
    view! {
        <section id="cv-approval">
            <h2>"Approval"</h2>
            <div class="card approval">{inner}</div>
        </section>
    }
}

fn preview_view(variant: &str, head: i64, approved: bool) -> impl IntoView + use<> {
    let base = variant_url(variant);
    let state = if approved { "approved" } else { "draft" };
    let label = if approved {
        "Download PDF"
    } else {
        "Download draft PDF, stamped"
    };
    view! {
        <section id="cv-preview">
            <h2>"Preview"</h2>
            <div class="preview-pages" id="cv-preview-pages">
                <img
                    src=format!("{base}/preview.png?revision={head}&page=1&state={state}")
                    alt=format!("Preview of revision {head}, page 1. It can't be shown if this revision no longer passes the check.")
                />
            </div>
            <p class="hint preview-caption" id="cv-preview-caption" hidden=approved>
                "Stamped until you approve it. The stamp is no part of the CV."
            </p>
            <p class="banner warn" id="cv-preview-note" role="status" hidden></p>
            <div class="buttons preview-actions" id="cv-preview-actions">
                <a class="button secondary" href=format!("{base}/cv.pdf?revision={head}") id="cv-pdf">{label}</a>
            </div>
        </section>
    }
}

fn voice_view(variant: &str, config: Option<&VoiceConfig>, token: &str) -> impl IntoView + use<> {
    let Some(config) = config else {
        return view! {
            <section id="voice">
                <h2>"Voice"</h2>
                <p class="hint" id="voice-off">
                    "Voice is off: no OpenAI key is configured on this server. Everything else here works without it."
                </p>
            </section>
        }
        .into_any();
    };
    let minutes = config.lifetime.as_secs().div_ceil(60);
    view! {
        <section id="voice" data-variant=variant.to_owned() data-csrf=token.to_owned()>
            <h2>"Voice"</h2>
            <div class="card voice">
                <p id="voice-status" role="status">
                    {format!("Not connected. A session lasts at most {minutes} minutes and is sent to OpenAI.")}
                </p>
                <noscript><p class="hint">"Voice needs JavaScript. Saving and reverting below don't."</p></noscript>
                <div class="buttons">
                    <button type="button" id="voice-connect">"Connect"</button>
                    <button type="button" id="voice-stop" class="quiet" hidden>"Stop"</button>
                    <span class="hint" id="voice-remaining" hidden></span>
                </div>
                <div id="voice-live" hidden>
                    <button type="button" id="voice-hold" class="hold" aria-pressed="false">"Hold to talk"</button>
                    <p class="hint">"Or hold the space bar."</p>
                    <label class="check">
                        <input type="checkbox" id="voice-handsfree" />
                        "Hands-free: the microphone stays on"
                    </label>
                    <h3>"Transcript"</h3>
                    <ol class="transcript" id="voice-transcript" aria-live="polite"></ol>
                    <h3>"Tool activity"</h3>
                    <ul class="tool-list" id="voice-tools"></ul>
                </div>
            </div>
        </section>
    }
    .into_any()
}

/// The form's fragment brings a refused save back to its problems, past the preview on a phone.
fn editor_view(
    variant: &str,
    token: &str,
    text: &str,
    expected: i64,
    outcome: &Outcome,
) -> impl IntoView + use<> {
    view! {
        <section id="cv-editor">
            <h2>"Source"</h2>
            {outcome.typed_source.is_some().then(|| flash_view(outcome.flash.as_ref()))}
            {(!outcome.violations.is_empty()).then(|| violations_view(&outcome.violations))}
            {(!outcome.diagnostics.is_empty()).then(|| diagnostics_view(&outcome.diagnostics))}
            <form method="post" action=format!("{}/save#cv-editor", variant_url(variant)) class="act" id="cv-editor-form">
                <input type="hidden" name="csrf" value=token.to_owned() />
                <input type="hidden" name="expected" id="cv-expected" value=expected.to_string() />
                <label>"Typst source"
                    <textarea
                        name="source"
                        class="code"
                        id="cv-source"
                        rows="24"
                        spellcheck="false"
                        autocapitalize="off"
                        data-kept=outcome.typed_source.is_some().then_some("1")
                    >{views::textarea_text(text)}</textarea>
                </label>
                <label>"What changed"
                    <input
                        type="text"
                        name="summary"
                        maxlength="500"
                        value=outcome.typed_summary.clone().unwrap_or_default()
                        placeholder="Edited by hand"
                    />
                </label>
                <button type="submit">"Save and approve as a new revision"</button>
            </form>
        </section>
    }
}

fn diff_view(diff: &str) -> impl IntoView + use<> {
    let lines: Vec<_> = diff
        .lines()
        .map(|line| {
            let class =
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    "dh"
                } else if line.starts_with('+') {
                    "da"
                } else if line.starts_with('-') {
                    "dd"
                } else {
                    "dc"
                };
            view! { <span class=class>{line.to_owned()}</span> }
        })
        .collect();
    view! { <pre class="diff">{lines}</pre> }
}

fn revisions_view(
    variant: &str,
    token: &str,
    head: i64,
    page: i64,
    listing: &Value,
    items: &[Value],
    tz: Tz,
) -> impl IntoView + use<> {
    let base = variant_url(variant);
    let rows: Vec<_> = items
        .iter()
        .map(|revision| {
            let number = revision["revision"].as_i64().unwrap_or_default();
            let when = revision["createdAt"]
                .as_str()
                .and_then(|at| Timestamp::parse(at).ok())
                .map(|at| format::moment(&at, tz))
                .unwrap_or_default();
            let is_approved = revision["state"].as_str() == Some("approved");
            let diff = revision["diff"].as_str().unwrap_or_default().to_owned();
            let revert = (number != head).then(|| {
                view! {
                    <form method="post" action=format!("{base}/revert") class="inline">
                        <input type="hidden" name="csrf" value=token.to_owned() />
                        <input type="hidden" name="expected" value=head.to_string() />
                        <input type="hidden" name="to" value=number.to_string() />
                        <button type="submit" class="quiet">{format!("Revert to {number}")}</button>
                    </form>
                }
            });
            view! {
                <li class="revision">
                    <div class="revision-head">
                        <span>
                            <b>{format!("Revision {number}")}</b>
                            " " <span class="tag">{revision["origin"].as_str().unwrap_or_default().to_owned()}</span>
                            " " <span class=if is_approved { "tag approved" } else { "tag draft" }>{if is_approved { "Approved" } else { "Draft" }}</span>
                            <small>{when}</small>
                            <small>{revision["summary"].as_str().unwrap_or_default().to_owned()}</small>
                        </span>
                        <span class="buttons">
                            <a class="button quiet" href=format!("{base}/cv.pdf?revision={number}")>{if is_approved { "PDF" } else { "Stamped PDF" }}</a>
                            {revert}
                        </span>
                    </div>
                    <details>
                        <summary>"Changes"</summary>
                        {diff_view(&diff)}
                    </details>
                </li>
            }
        })
        .collect();
    let total = listing["total"].as_i64().unwrap_or(0);
    let newer = (page > 1).then(|| {
        view! { <a class="button quiet" href=format!("{base}?page={}#cv-revisions", page - 1)>"Newer"</a> }
    });
    let older = (page * REVISIONS_PER_PAGE < total).then(|| {
        view! { <a class="button quiet" href=format!("{base}?page={}#cv-revisions", page + 1)>"Older"</a> }
    });
    view! {
        <section id="cv-revisions">
            <h2>"Revisions"</h2>
            {if rows.is_empty() {
                view! { <div class="card"><p class="empty">"Nothing stored yet. Saving makes the first revision."</p></div> }.into_any()
            } else {
                view! { <div class="card"><ol class="revisions">{rows}</ol></div> }.into_any()
            }}
            <div class="buttons pager">{newer}{older}</div>
        </section>
    }
}

/// What a human save is sent as: one edit replacing the whole source, the same arguments an agent sends.
fn whole_source_edit(
    variant: &str,
    expected: i64,
    base: &str,
    source: &str,
    summary: &str,
) -> Value {
    json!({
        "variant": variant,
        "expectedRevision": expected,
        "edits": [{"oldText": base, "newText": source}],
        "summary": summary,
        "origin": "dashboard",
    })
}

/// A failed tool call as a page's outcome, keeping what was typed.
fn refused(told: &Told, mut outcome: Outcome) -> (StatusCode, Outcome) {
    let code = told.code().to_owned();
    if code == "cv_revision_conflict" {
        let current = told.value["error"]["currentRevision"].as_i64().unwrap_or(0);
        outcome.kept_expected = None;
        outcome.flash = Some(Flash {
            ok: false,
            text: format!(
                "Revision {current} landed while you were working, so nothing was stored. Your text is still below. Check the new revision under Revisions: saving again makes your text the newest one, and history keeps both."
            ),
        });
        return (StatusCode::CONFLICT, outcome);
    }
    outcome.violations = told.value["error"]["violations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    outcome.diagnostics = told.value["error"]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    outcome.flash = Some(Flash {
        ok: false,
        text: explain(&code).to_owned(),
    });
    (StatusCode::UNPROCESSABLE_ENTITY, outcome)
}

async fn save(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    // Browsers send a textarea's line ends as CRLF, which would rewrite every line of the source.
    let source = form
        .get("source")
        .map(|text| text.replace("\r\n", "\n"))
        .unwrap_or_default();
    let typed_summary = form.get("summary").cloned().unwrap_or_default();
    let summary = match typed_summary.trim() {
        "" => "Edited by hand",
        summary => summary,
    }
    .to_owned();
    let Some(expected) = form
        .get("expected")
        .and_then(|text| text.parse::<i64>().ok())
    else {
        return Err(PageError(StatusCode::BAD_REQUEST));
    };
    let mut read = json!({"variant": variant});
    if expected > 0 {
        read["revision"] = expected.into();
    }
    let base = call(&cv, "get_cv_source", read).await;
    let kept = Outcome {
        typed_source: Some(source.clone()),
        typed_summary: Some(typed_summary),
        kept_expected: Some(expected),
        ..Outcome::default()
    };
    if base.is_error {
        if base.code() == "cv_variant_not_found" {
            return Err(PageError(StatusCode::NOT_FOUND));
        }
        let (status, outcome) = refused(&base, kept);
        return Ok(with_status(
            status,
            render_variant(&cv, &variant, 1, &token, outcome).await?,
        ));
    }
    let base_source = base.value["source"].as_str().unwrap_or_default();
    // The caller is what grants this: the owner's session, in this handler. Not `origin`, which is
    // text any caller can send.
    let told = told(
        owner::save_as_approved(
            cv.mcp.clone(),
            &whole_source_edit(&variant, expected, base_source, &source, &summary),
        )
        .await,
    );
    if !told.is_error {
        return Ok(Redirect::to(&format!(
            "{}?done=saved#cv-workspace",
            variant_url(&variant)
        ))
        .into_response());
    }
    let (status, outcome) = refused(&told, kept);
    Ok(with_status(
        status,
        render_variant(&cv, &variant, 1, &token, outcome).await?,
    ))
}

/// The owner's tap. Bound by the form to the exact revision and hashes on the page they read, so that
/// a revision or a fact landing in between can't be approved by accident.
async fn approve(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    let text = |name: &str| form.get(name).map_or("", String::as_str);
    let Ok(revision) = text("revision").parse::<i64>() else {
        return Err(PageError(StatusCode::BAD_REQUEST));
    };
    let told = told(
        owner::approve(
            cv.mcp.clone(),
            &variant,
            revision,
            text("sourceSha256"),
            text("factsSha256"),
            FACTS_ACTOR,
        )
        .await,
    );
    if !told.is_error {
        return Ok(Redirect::to(&format!(
            "{}?done=approved#cv-approval",
            variant_url(&variant)
        ))
        .into_response());
    }
    if told.code() == "cv_variant_not_found" {
        return Err(PageError(StatusCode::NOT_FOUND));
    }
    let (status, mut outcome) = refused(&told, Outcome::default());
    if told.code() == "cv_approval_stale" {
        outcome.flash = Some(Flash {
            ok: false,
            text: explain(told.code()).to_owned(),
        });
        return Ok(with_status(
            StatusCode::CONFLICT,
            render_variant(&cv, &variant, 1, &token, outcome).await?,
        ));
    }
    Ok(with_status(
        status,
        render_variant(&cv, &variant, 1, &token, outcome).await?,
    ))
}

fn with_status(status: StatusCode, mut response: Response) -> Response {
    *response.status_mut() = status;
    response
}

async fn revert(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    let number = |name: &str| form.get(name).and_then(|text| text.parse::<i64>().ok());
    let (Some(expected), Some(to)) = (number("expected"), number("to")) else {
        return Err(PageError(StatusCode::BAD_REQUEST));
    };
    let told = call(
        &cv,
        "revert_cv",
        json!({"variant": variant, "toRevision": to, "expectedRevision": expected}),
    )
    .await;
    if !told.is_error {
        return Ok(
            Redirect::to(&format!("{}?done=reverted", variant_url(&variant))).into_response(),
        );
    }
    if told.code() == "cv_variant_not_found" {
        return Err(PageError(StatusCode::NOT_FOUND));
    }
    let (status, outcome) = refused(&told, Outcome::default());
    Ok(with_status(
        status,
        render_variant(&cv, &variant, 1, &token, outcome).await?,
    ))
}

#[derive(Deserialize, Default)]
struct RevisionQuery {
    revision: Option<i64>,
    page: Option<usize>,
}

/// A rendered preview, or why there is none.
enum Preview {
    Ready {
        revision: i64,
        state: String,
        pages: Vec<Vec<u8>>,
    },
    Refused(Box<Told>),
}

/// Number 0 reads a variant's head, which is how the page names a variant with nothing stored.
fn revision_args(variant: &str, revision: Option<i64>) -> Value {
    let mut args = json!({"variant": variant});
    if let Some(number) = revision.filter(|number| *number > 0) {
        args["revision"] = number.into();
    }
    args
}

/// From the cache, or through `render_cv_pdf`, which checks the revision against today's facts and
/// stamps a draft. The key's state and facts version come from that same render, and a failure is
/// never cached, so a stamp can't fall off a draft or stay on what the owner approved.
async fn preview(cv: &Cv, variant: &str, revision: Option<i64>) -> Preview {
    let read = call(cv, "get_cv_source", revision_args(variant, revision)).await;
    if read.is_error {
        return Preview::Refused(Box::new(read));
    }
    let number = read.value["revision"].as_i64().unwrap_or(0);
    let hash = sha256_hex(read.value["source"].as_str().unwrap_or_default().as_bytes());
    let version = read.value["factsVersion"].as_i64().unwrap_or(0);
    let state = read.value["state"].as_str().unwrap_or("draft").to_owned();
    let cached = || {
        let (cache, key) = (cv.cache.clone(), format!("{hash}-{state}"));
        tokio::task::spawn_blocking(move || cache.get(&key, version))
    };
    if let Ok(Some(pages)) = cached().await {
        return Preview::Ready {
            revision: number,
            state,
            pages,
        };
    }
    let _rendering = cv.rendering.lock().await;
    if let Ok(Some(pages)) = cached().await {
        return Preview::Ready {
            revision: number,
            state,
            pages,
        };
    }
    let mut args = revision_args(variant, revision);
    args["previewPages"] = PREVIEW_PAGES.into();
    let told = call(cv, "render_cv_pdf", args).await;
    if told.is_error {
        return Preview::Refused(Box::new(told));
    }
    let pages = blocks_of(&told.result, |block| {
        (block["type"] == "image").then(|| block["data"].clone())
    });
    let state = told.value["state"].as_str().unwrap_or("draft").to_owned();
    // A variant with nothing stored can gain a head between the read and the render.
    if told.value["revision"].as_i64() == Some(number) {
        let version = told.value["factsVersion"].as_i64().unwrap_or(version);
        let (cache, stored, key) = (cv.cache.clone(), pages.clone(), format!("{hash}-{state}"));
        let _ = tokio::task::spawn_blocking(move || cache.put(&key, version, &stored)).await;
    }
    Preview::Ready {
        revision: number,
        state,
        pages,
    }
}

/// The base64 payloads `pick` finds among a tool result's content blocks, decoded.
fn blocks_of(result: &CallToolResult, pick: impl Fn(&Value) -> Option<Value>) -> Vec<Vec<u8>> {
    result
        .content
        .iter()
        .filter_map(|block| serde_json::to_value(block).ok())
        .filter_map(|block| pick(&block))
        .filter_map(|data| BASE64.decode(data.as_str()?).ok())
        .collect()
}

async fn preview_png(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Query(query): Query<RevisionQuery>,
) -> Response {
    let index = query.page.unwrap_or(1).saturating_sub(1);
    match preview(&cv, &variant, query.revision).await {
        Preview::Ready { mut pages, .. } if index < pages.len() => (
            [(header::CONTENT_TYPE, "image/png")],
            Body::from(pages.swap_remove(index)),
        )
            .into_response(),
        Preview::Ready { .. } => StatusCode::NOT_FOUND.into_response(),
        Preview::Refused(told) => {
            (StatusCode::UNPROCESSABLE_ENTITY, explain(told.code())).into_response()
        }
    }
}

/// For the page: how many preview pages there are, or the reason in words.
async fn preview_json(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Query(query): Query<RevisionQuery>,
) -> Response {
    match preview(&cv, &variant, query.revision).await {
        Preview::Ready {
            revision,
            state,
            pages,
        } => Json(json!({"revision": revision, "state": state, "pages": pages.len()}))
            .into_response(),
        Preview::Refused(told) => {
            let tokens: Vec<_> = told.value["error"]["violations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|violation| violation["token"].as_str())
                .collect();
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"message": explain(told.code()), "tokens": tokens})),
            )
                .into_response()
        }
    }
}

async fn pdf(
    State(cv): State<Cv>,
    Path(variant): Path<String>,
    Query(query): Query<RevisionQuery>,
) -> Result<Response, PageError> {
    let revision = query.revision.filter(|number| *number > 0);
    let told = told(owner::download_pdf(cv.mcp.clone(), &variant, revision).await);
    if told.is_error {
        return match told.code() {
            "cv_variant_not_found" | "cv_revision_not_found" => {
                Err(PageError(StatusCode::NOT_FOUND))
            }
            code => Ok(notice_page(
                StatusCode::UNPROCESSABLE_ENTITY,
                "No PDF for this revision",
                explain(code),
            )),
        };
    }
    let number = told.value["revision"].as_i64().unwrap_or(0);
    let mut documents = blocks_of(&told.result, |block| {
        (block["type"] == "resource").then(|| block["resource"]["blob"].clone())
    });
    let Some(document) = documents.pop() else {
        return Err(PageError(StatusCode::INTERNAL_SERVER_ERROR));
    };
    let safe: String = variant
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let label = if told.value["state"].as_str() == Some("approved") {
        ""
    } else {
        "-DRAFT-NOT-APPROVED"
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"cv-{safe}-{number}{label}.pdf\""),
            ),
        ],
        Body::from(document),
    )
        .into_response())
}

/// The fallback when a stream can't stay open: a page asks this every so often.
async fn head(State(cv): State<Cv>, Path(variant): Path<String>) -> Result<Response, PageError> {
    let Some(revision) = head_number(&cv.db, &variant).await? else {
        return Err(PageError(StatusCode::NOT_FOUND));
    };
    let read = call(&cv, "get_cv_source", json!({"variant": variant})).await;
    let state = read.value["state"].as_str().unwrap_or("draft").to_owned();
    Ok(Json(json!({"revision": revision, "state": state})).into_response())
}

fn lines_of<T>(items: &[T], text: impl Fn(&T) -> String) -> String {
    items.iter().map(text).collect::<Vec<_>>().join("\n")
}

/// Non-empty lines, trimmed.
fn list_field(form: &Typed, name: &str) -> Vec<String> {
    form.get(name)
        .map_or("", String::as_str)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `name: value` lines. The first colon splits, since a metric's text can hold others.
fn pair_field(form: &Typed, name: &str, label: &str) -> Result<Vec<(String, String)>, String> {
    list_field(form, name)
        .iter()
        .map(|line| {
            line.split_once(':')
                .map(|(left, right)| (left.trim().to_owned(), right.trim().to_owned()))
                .filter(|(left, right)| !left.is_empty() && !right.is_empty())
                .ok_or_else(|| format!("{label}: each line needs the form \u{201c}name: value\u{201d}. This one doesn't: {line}"))
        })
        .collect()
}

fn owner_facts_of(form: &Typed) -> Result<OwnerFacts, String> {
    Ok(OwnerFacts {
        languages: Some(
            pair_field(form, "languages", "Languages")?
                .into_iter()
                .map(|(name, level)| Language { name, level })
                .collect(),
        ),
        metrics: Some(
            pair_field(form, "metrics", "Metrics")?
                .into_iter()
                .map(|(key, text)| Metric { key, text })
                .collect(),
        ),
        allowed_tokens: Some(list_field(form, "allowedTokens")),
        banned_phrases: Some(list_field(form, "bannedPhrases")),
        vocabulary: Some(list_field(form, "vocabulary")),
    })
}

#[derive(Default)]
struct FactsOutcome {
    flash: Option<Flash>,
    added: Vec<String>,
    typed: Option<Typed>,
}

fn changes_view(title: &str, changes: &[FactChange]) -> impl IntoView + use<> {
    let items: Vec<_> = changes
        .iter()
        .map(|change| {
            view! {
                <li>
                    <b>{change.field.clone()}</b>": "
                    <del>{change.old.clone()}</del>" \u{2192} "<ins>{change.new.clone()}</ins>
                </li>
            }
        })
        .collect();
    (!items.is_empty()).then(|| {
        view! {
            <h3>{title.to_owned()}</h3>
            <ul class="lock-list">{items}</ul>
        }
    })
}

/// Every change a lock would confirm, and the form that names exactly this list. Written after the
/// page loaded, anything else makes the lock refuse and this list show it.
fn lock_form(token: &str, preview: &LockPreview) -> impl IntoView + use<> {
    let added: Vec<_> = preview
        .added
        .iter()
        .map(|url| view! { <li>{url.clone()}</li> })
        .collect();
    view! {
        {(!preview.needs_confirmation()).then(|| view! {
            <p class="hint">"Nothing changed since the last lock."</p>
        })}
        {preview.needs_confirmation().then(|| view! {
            <p>"Locking confirms all of this as true, and anything an agent wrote is in it. A CV generated from this wording needs no further approval."</p>
        })}
        {changes_view("Facts changed", &preview.changes)}
        {changes_view("Wording changed", &preview.wording)}
        {(!added.is_empty()).then(|| view! {
            <h3>"URLs from your wording, added to what a CV may show"</h3>
            <ul class="lock-list">{added}</ul>
        })}
        <form method="post" action="/cv/facts/lock" class="act">
            <input type="hidden" name="csrf" value=token.to_owned() />
            <input type="hidden" name="hash" value=preview.hash() />
            <button type="submit">"Lock the facts"</button>
        </form>
    }
}

async fn facts_page(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
) -> Result<Html<String>, PageError> {
    Ok(Html(
        render_facts(&cv, &token, FactsOutcome::default()).await?,
    ))
}

async fn render_facts(cv: &Cv, token: &str, outcome: FactsOutcome) -> Result<String, PageError> {
    let mut conn = cv.db.conn().await?;
    let state = cv_facts::get(&mut conn).await?;
    let preview = if state.is_locked() {
        None
    } else {
        // Nothing to lock yet is not a failure of this page: the lock's own answer says so.
        cv_facts::lock_preview(&mut conn).await.ok()
    };
    drop(conn);
    let counts = NavCounts {
        overdue: 0,
        awaiting: awaiting_count(&cv.db).await?,
    };
    let facts = &state.facts;
    let typed = |name: &str, saved: String| -> String {
        let value = outcome
            .typed
            .as_ref()
            .and_then(|typed| typed.get(name).cloned())
            .unwrap_or(saved);
        views::textarea_text(&value)
    };
    let lock_state = match &state.locked_at {
        Some(at) => format!(
            "Locked since {}. Agents and voice can't change the facts, and what they write to a CV stays a draft until you approve it.",
            format::moment(at, cv.tz)
        ),
        None => "Not locked. Agents can still change these through upsert_cv_profile, and no CV edit can be stored until you lock them.".to_owned(),
    };
    let added: Vec<_> = outcome
        .added
        .iter()
        .map(|token| view! { <mark>{token.clone()}</mark>" " })
        .collect();
    let employment: Vec<_> = facts
        .employment
        .iter()
        .map(|job| {
            view! {
                <li><span>
                    <b>{job.company.clone()}</b>
                    <small>{format!("{}, {}, {}", job.title, job.date, job.location)}</small>
                    <small class="id">{format!("#employment(\"{}\")", job.id)}</small>
                </span></li>
            }
        })
        .collect();
    let education: Vec<_> = facts
        .education
        .iter()
        .map(|entry| {
            view! {
                <li><span>
                    <b>{entry.institution.clone()}</b>
                    <small>{format!("{}, {}, {}", entry.qualification, entry.date, entry.location)}</small>
                    <small class="id">{format!("#education(\"{}\")", entry.id)}</small>
                </span></li>
            }
        })
        .collect();
    let contact = [
        &facts.contact.email,
        &facts.contact.phone,
        &facts.contact.location,
        &facts.contact.linkedin,
        &facts.contact.github,
    ]
    .iter()
    .filter(|value| !value.is_empty())
    .map(|value| value.as_str())
    .collect::<Vec<_>>()
    .join(", ");
    let is_locked = state.is_locked();
    let body = view! {
        <p><a class="crumb" href="/cv">"\u{2190} CV"</a></p>
        <div class="hero">
            <h1>"Facts"</h1>
            <span class="id">{format!("version {}", state.version)}</span>
        </div>
        <p class="lede">"The names, jobs and dates a CV renders by id. Only you can change them here, or with nexus cv-facts on the server. Voice and agents can't."</p>
        {flash_view(outcome.flash.as_ref())}
        {(!outcome.added.is_empty()).then(|| view! {
            <div class="banner review">
                "Locking confirmed these URLs from your profile's wording, and added them to the allowed URLs. Change them below if any is wrong: "
                <span class="tokens">{added}</span>
            </div>
        })}
        <h2>"Lock"</h2>
        <div class="card lock-card">
            <p>{lock_state}</p>
            {if is_locked {
                view! {
                    <form method="post" action="/cv/facts/unlock" class="act">
                        <input type="hidden" name="csrf" value=token.to_owned() />
                        <button type="submit" class="danger">"Unlock\u{2026}"</button>
                    </form>
                }.into_any()
            } else if let Some(preview) = &preview {
                lock_form(token, preview).into_any()
            } else {
                view! { <p class="hint">"There is nothing to lock yet: no profile has set the name."</p> }.into_any()
            }}
        </div>
        <h2>"From the profile"</h2>
        <div class="card">
            <dl class="facts">
                <dt>"Name"</dt><dd>{facts.name.clone()}</dd>
                <dt>"Contact"</dt><dd>{contact}</dd>
            </dl>
        </div>
        <div class="card">
            <ul class="items">{employment}</ul>
        </div>
        <div class="card">
            <ul class="items">{education}</ul>
        </div>
        <p class="hint">"These change through upsert_cv_profile, only while the facts are unlocked."</p>
        <h2>"Yours to edit"</h2>
        <form method="post" action="/cv/facts" class="act card">
            <input type="hidden" name="csrf" value=token.to_owned() />
            <input type="hidden" name="expected" value=state.version.to_string() />
            <label>"Languages, one per line: name: level"
                <textarea name="languages" rows="4">{typed("languages", lines_of(&facts.languages, |l| format!("{}: {}", l.name, l.level)))}</textarea>
            </label>
            <label>"Metrics, one per line: key: text"
                <textarea name="metrics" rows="4">{typed("metrics", lines_of(&facts.metrics, |m| format!("{}: {}", m.key, m.text)))}</textarea>
            </label>
            <label>"Allowed URLs and emails, one per line: ones a CV may show beyond your contact details"
                <textarea name="allowedTokens" rows="4">{typed("allowedTokens", facts.allowed_tokens.join("\n"))}</textarea>
            </label>
            <label>"Vocabulary, one per line: words review doesn't need to list"
                <textarea name="vocabulary" rows="4">{typed("vocabulary", facts.vocabulary.join("\n"))}</textarea>
            </label>
            <label>"Banned phrases, one per line: agents and voice never see this list"
                <textarea name="bannedPhrases" rows="4">{typed("bannedPhrases", facts.banned_phrases.join("\n"))}</textarea>
            </label>
            <button type="submit">"Save the facts"</button>
        </form>
    };
    Ok(views::page("Facts", Nav::Cv, counts, body))
}

fn facts_failure(error: Error) -> Result<String, PageError> {
    match error {
        Error::CvFacts(text) => {
            let mut text = text.to_owned();
            if let Some(first) = text.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            text.push('.');
            Ok(text)
        }
        other => Err(other.into()),
    }
}

async fn facts_save(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    let refuse =
        async |status: StatusCode, text: String, form: Typed| -> Result<Response, PageError> {
            let outcome = FactsOutcome {
                flash: Some(Flash { ok: false, text }),
                typed: Some(form),
                ..FactsOutcome::default()
            };
            Ok((status, Html(render_facts(&cv, &token, outcome).await?)).into_response())
        };
    let form = normalize_lines(form);
    let fields = match owner_facts_of(&form) {
        Ok(fields) => fields,
        Err(text) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, text, form).await,
    };
    let expected = form
        .get("expected")
        .and_then(|text| text.parse::<i64>().ok());
    let mut tx = cv.db.begin_write().await?;
    if Some(cv_facts::get(&mut tx).await?.version) != expected {
        drop(tx);
        return refuse(
            StatusCode::CONFLICT,
            "The facts changed since you opened this page, so nothing was saved. Your text is below, next to what is stored now under \u{201c}From the profile\u{201d}. Check it and save again.".to_owned(),
            form,
        )
        .await;
    }
    cv_facts::set_owner_facts(&mut tx, &fields, FACTS_ACTOR).await?;
    tx.commit().await.map_err(Error::from)?;
    let outcome = FactsOutcome {
        flash: Some(Flash {
            ok: true,
            text: "Facts saved.".to_owned(),
        }),
        ..FactsOutcome::default()
    };
    Ok(Html(render_facts(&cv, &token, outcome).await?).into_response())
}

fn normalize_lines(mut form: Typed) -> Typed {
    for value in form.values_mut() {
        if value.contains('\r') {
            *value = value.replace("\r\n", "\n");
        }
    }
    form
}

async fn facts_lock(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    let mut tx = cv.db.begin_write().await?;
    // What the owner read must be what is there now: an agent may have written since the page loaded.
    let shown = form.get("hash").map(String::as_str);
    let preview = cv_facts::lock_preview(&mut tx).await;
    if let Ok(preview) = &preview
        && shown != Some(preview.hash().as_str())
    {
        drop(tx);
        let outcome = FactsOutcome {
            flash: Some(Flash {
                ok: false,
                text: "Something changed since you opened this page, so nothing was locked. Read the new list below.".to_owned(),
            }),
            ..FactsOutcome::default()
        };
        return Ok((
            StatusCode::CONFLICT,
            Html(render_facts(&cv, &token, outcome).await?),
        )
            .into_response());
    }
    let outcome = match cv_facts::lock(&mut tx, FACTS_ACTOR).await {
        Ok((_, added)) => {
            tx.commit().await.map_err(Error::from)?;
            FactsOutcome {
                flash: Some(Flash {
                    ok: true,
                    text: "The facts are locked.".to_owned(),
                }),
                added,
                ..FactsOutcome::default()
            }
        }
        Err(error) => {
            drop(tx);
            FactsOutcome {
                flash: Some(Flash {
                    ok: false,
                    text: facts_failure(error)?,
                }),
                ..FactsOutcome::default()
            }
        }
    };
    Ok(Html(render_facts(&cv, &token, outcome).await?).into_response())
}

/// Asks first: unlocking lets an agent rewrite the facts, and the page says what that means.
async fn facts_unlock(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
    Form(form): Form<Typed>,
) -> Result<Response, PageError> {
    if !csrf_ok(form.get("csrf").map(String::as_str), &token) {
        return Ok(expired_form());
    }
    if form.get("confirm").map(String::as_str) != Some("1") {
        let body = view! {
            <h1>"Unlock the facts?"</h1>
            <p class="lede">"While they are unlocked, an agent can rewrite your name, jobs, dates and education through upsert_cv_profile, and no CV edit by you, an agent or voice can be stored until you lock them again."</p>
            <form method="post" action="/cv/facts/unlock" class="act">
                <input type="hidden" name="csrf" value=token.clone() />
                <input type="hidden" name="confirm" value="1" />
                <div class="buttons">
                    <button type="submit" class="danger">"Unlock"</button>
                    <a class="button secondary" href="/cv/facts">"Cancel"</a>
                </div>
            </form>
        };
        return Ok(Html(views::page("Unlock the facts?", Nav::Cv, 0, body)).into_response());
    }
    let mut tx = cv.db.begin_write().await?;
    let flash = match cv_facts::unlock(&mut tx, FACTS_ACTOR).await {
        Ok(_) => {
            tx.commit().await.map_err(Error::from)?;
            Flash {
                ok: true,
                text: "The facts are unlocked. Lock them again when they are right.".to_owned(),
            }
        }
        Err(error) => {
            drop(tx);
            Flash {
                ok: false,
                text: facts_failure(error)?,
            }
        }
    };
    let outcome = FactsOutcome {
        flash: Some(flash),
        ..FactsOutcome::default()
    };
    Ok(Html(render_facts(&cv, &token, outcome).await?).into_response())
}
