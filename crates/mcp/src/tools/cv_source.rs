//! The additive CV tools: Typst source, revisions and rendering. See SPEC section 9a.
//!
//! A write reads what it needs, lets go of the database, compiles, then checks the head again under
//! the write lock: a compile can take 30 s, and nothing may hold a connection that long.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as ENCODER;
use nexus_core::Timestamp;
use nexus_core::blob::sha256_hex;
use nexus_core::repo::cv as cv_repo;
use nexus_core::repo::cv_approvals::{self as approvals, NewApproval, facts_sha256};
use nexus_core::repo::cv_facts::{self, FactsState};
use nexus_core::repo::cv_revisions::{self as repo, NewRevision, Revision, Variant};
use nexus_core::repo::documents::{self as documents_repo, NewDocument};
use nexus_tailor::{
    CvContent, CvEmployment, CvProject, CvSkills, Facts, RenderError, RenderRequest, Rendered,
    Report, ReviewItem, TEMPLATE_HELP, Violation, check, default_source, review_text,
    source_from_profile,
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use serde_json::{Value, json};
use similar::TextDiff;
use sqlx::SqliteConnection;

use super::documents::{put_blob, remove_blob_if_unreferenced};
use super::{Args, Ctx, ToolResult, code_error, find_application, json_result, req_str, str_arg};
use crate::{CvRevisionEvent, Review};

/// With a compile's 30 s, this stays inside the MCP route's 60 s deadline.
const RENDER_QUEUE_WAIT: Duration = Duration::from_secs(20);
/// One compiling and two waiting. Of the MCP pool's 8 slots, that leaves 5 for every other tool.
pub(crate) const MAX_RENDER_CALLS: usize = 3;
/// What a refused call reports. The rest would only cost memory: the first ones tell what is wrong.
const MAX_REPORTED_VIOLATIONS: usize = 50;
const MAX_REPORTED_ITEMS: usize = 50;
const MAX_VIOLATION_TEXT_CHARS: usize = 200;
/// What one page of `list_cv_revisions` carries in diffs. A source can reach 256 KiB, so 50 full
/// diffs would be 50 MB in the reply, and several times that in the transport.
const MAX_DIFF_BYTES_PER_PAGE: usize = 1024 * 1024;
/// Past this a diff gets coarser, not wrong. A page holds up to 50 of them, on a runtime thread.
const DIFF_TIMEOUT: Duration = Duration::from_millis(200);
const RESERVED_VARIANT_PREFIX: &str = "application-";
/// Origins that say the owner acted, or that the revert tool did. A page shows them as such, so a tool
/// call can't send them: the owner's own paths and `revert_cv` write them.
const RESERVED_ORIGINS: [&str; 4] = ["dashboard", "revert", "cli", "lock"];

pub(super) fn error_with(code: &str, details: Value) -> CallToolResult {
    let mut error = json!({"code": code});
    if let (Some(error), Value::Object(details)) = (error.as_object_mut(), details) {
        error.extend(details);
    }
    json_result(json!({"error": error}), true)
}

pub(super) fn violation_error(violations: &[Violation]) -> CallToolResult {
    let short = |text: &str| text.chars().take(MAX_VIOLATION_TEXT_CHARS).collect();
    let reported: Vec<Violation> = violations
        .iter()
        .take(MAX_REPORTED_VIOLATIONS)
        .map(|violation| Violation {
            rule: violation.rule,
            token: short(&violation.token),
            context: short(&violation.context),
        })
        .collect();
    error_with(
        "cv_fact_violation",
        json!({
            "violations": reported,
            "truncated": violations.len() - reported.len(),
        }),
    )
}

/// The first review items, most serious first, and how many were left out.
pub(super) fn capped_items(items: &[ReviewItem]) -> (Vec<ReviewItem>, usize) {
    let short = |text: &str| text.chars().take(MAX_VIOLATION_TEXT_CHARS).collect();
    let reported: Vec<ReviewItem> = items
        .iter()
        .take(MAX_REPORTED_ITEMS)
        .map(|item| ReviewItem {
            rule: item.rule,
            token: short(&item.token),
            context: short(&item.context),
            line: item.line,
        })
        .collect();
    let truncated = items.len() - reported.len();
    (reported, truncated)
}

/// Where the owner looks at this variant and approves it. Relative: the client knows the origin.
pub(super) fn approval_path(variant: &str) -> String {
    format!("/cv/variants/{variant}#cv-approval")
}

pub(super) fn approval_required(variant: &str, revision: i64) -> CallToolResult {
    error_with(
        "cv_approval_required",
        json!({
            "variant": variant,
            "revision": revision,
            "approvalPath": approval_path(variant),
        }),
    )
}

pub(super) fn state_name(approved: bool) -> &'static str {
    if approved { "approved" } else { "draft" }
}

/// A source is approved in a variant when it is the text the owner approved there last, or for a
/// variant with no approval of its own, text still standing as approved elsewhere, under exactly these
/// facts, and the facts are locked. Computed each time: never stored on a revision, never passed by a
/// caller. `variant_id` is none for a variant that doesn't exist yet.
pub(super) async fn is_approved(
    conn: &mut SqliteConnection,
    state: &FactsState,
    variant_id: Option<i64>,
    source_sha256: &str,
) -> nexus_core::Result<bool> {
    Ok(state.is_locked()
        && approvals::is_approved(conn, variant_id, source_sha256, &facts_sha256(&state.facts))
            .await?)
}

/// `state` and what comes with it, for a reply about a revision.
fn add_standing(out: &mut Value, variant: &str, approved: bool, items: &[ReviewItem]) {
    out["state"] = state_name(approved).into();
    if approved {
        out["reviewItems"] = json!([]);
        out["truncated"] = 0.into();
        out["approvalPath"] = Value::Null;
    } else {
        let (reported, truncated) = capped_items(items);
        out["reviewItems"] = json!(reported);
        out["truncated"] = truncated.into();
        out["approvalPath"] = approval_path(variant).into();
    }
}

fn render_error(error: RenderError) -> CallToolResult {
    match error {
        RenderError::Diagnostics(diagnostics) => {
            error_with("cv_compile_failed", json!({"diagnostics": diagnostics}))
        }
        RenderError::SourceTooLarge => code_error("cv_source_too_large"),
        RenderError::TooManyPages => code_error("cv_too_many_pages"),
        RenderError::PdfTooLarge => code_error("cv_pdf_too_large"),
        RenderError::ResultTooLarge => code_error("cv_render_result_too_large"),
        RenderError::Timeout => code_error("cv_render_timeout"),
        RenderError::MemoryExceeded => code_error("cv_render_memory_exceeded"),
        RenderError::Worker(detail) => {
            tracing::error!(%detail, "render worker failed");
            code_error("cv_render_failed")
        }
    }
}

fn render_busy() -> Box<CallToolResult> {
    Box::new(error_with(
        "cv_render_busy",
        json!({"message": "Other CVs are rendering. Nothing was stored. Try again shortly."}),
    ))
}

/// One compile, waiting its turn for a bounded time, and only if few calls wait already. It ends with
/// the client's connection: the child dies with this future. `/facts.json` holds what a CV can
/// render and no more: see [`Facts::renderable`].
pub(super) async fn compile(
    ctx: &Ctx,
    source: &str,
    facts: &Facts,
    pdf: bool,
    preview_pages: u32,
    stamp: bool,
) -> Result<Rendered, Box<CallToolResult>> {
    let Ok(_admitted) = ctx.state.render_calls.clone().try_acquire_owned() else {
        return Err(render_busy());
    };
    let work = async {
        let slot = ctx.state.render_slot.clone();
        let Ok(Ok(_slot)) = tokio::time::timeout(RENDER_QUEUE_WAIT, slot.acquire_owned()).await
        else {
            return Err(render_busy());
        };
        let request = RenderRequest {
            source: source.to_owned(),
            facts: facts.renderable(),
            pdf,
            preview_pages,
            stamp,
        };
        ctx.state
            .renderer
            .render(request)
            .await
            .map_err(|error| Box::new(render_error(error)))
    };
    tokio::select! {
        () = ctx.client_gone.cancelled() => Err(client_gone_error()),
        result = work => result,
    }
}

/// Nobody reads this: it only makes the tool stop before it stores anything.
fn client_gone_error() -> Box<CallToolResult> {
    Box::new(code_error("cv_client_gone"))
}

/// `true` when the facts moved while a compile ran, so that its check says nothing about them.
pub(super) async fn facts_moved(
    conn: &mut SqliteConnection,
    version: i64,
) -> nexus_core::Result<bool> {
    Ok(cv_facts::get(conn).await?.version != version)
}

pub(super) fn facts_changed_error() -> CallToolResult {
    code_error("cv_facts_changed")
}

/// A master profile as a CV holding everything, or the bare facts when there is no profile.
fn starting_source(profile: Option<&cv_repo::CvProfile>, facts: &Facts) -> String {
    let Some(profile) = profile else {
        return default_source(facts);
    };
    source_from_profile(&content_of(
        profile,
        facts,
        &profile
            .experience
            .0
            .as_array()
            .map_or_else(Vec::new, |entries| {
                entries
                    .iter()
                    .map(|e| e["id"].as_str().unwrap_or_default().to_owned())
                    .collect()
            }),
        &profile.skills.0.as_array().map_or_else(Vec::new, |skills| {
            skills
                .iter()
                .map(|s| s["category"].as_str().unwrap_or_default().to_owned())
                .collect()
        }),
        None,
        true,
        true,
    ))
}

fn text_of(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

/// Port of `mergeCvData`: what a tailored CV holds, in the order the caller gave. IDs that match
/// nothing are skipped.
pub(super) fn content_of(
    profile: &cv_repo::CvProfile,
    facts: &Facts,
    experience_ids: &[String],
    skill_categories: &[String],
    profile_override: Option<&str>,
    include_projects: bool,
    include_education: bool,
) -> CvContent {
    let list = |value: &'_ Value| value.as_array().cloned().unwrap_or_default();
    let experience = list(&profile.experience.0);
    let skills = list(&profile.skills.0);
    CvContent {
        lang: "en".to_owned(),
        summary: profile_override.unwrap_or(&profile.profile).to_owned(),
        skills: skill_categories
            .iter()
            .filter_map(|category| skills.iter().find(|s| s["category"] == category.as_str()))
            .map(|s| CvSkills {
                category: text_of(s, "category"),
                items: list(&s["items"])
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect(),
            })
            .collect(),
        employment: experience_ids
            .iter()
            .filter_map(|id| experience.iter().find(|e| e["id"] == id.as_str()))
            .map(|e| CvEmployment {
                id: text_of(e, "id"),
                tier: e["tier"].as_f64().unwrap_or(1.0) as u8,
                bullets: list(&e["bullets"])
                    .iter()
                    .filter_map(|bullet| bullet.as_str().map(str::to_owned))
                    .collect(),
            })
            .collect(),
        projects: if include_projects {
            list(&profile.projects.0)
                .iter()
                .map(|p| CvProject {
                    name: text_of(p, "name"),
                    url: text_of(p, "url"),
                    stack: text_of(p, "stack"),
                    description: text_of(p, "description"),
                })
                .collect()
        } else {
            Vec::new()
        },
        education_ids: if include_education {
            facts.education.iter().map(|e| e.id.clone()).collect()
        } else {
            Vec::new()
        },
        languages: !facts.languages.is_empty(),
    }
}

/// A variant's revision as a caller sees it. Number 0 is a variant with nothing stored yet.
struct Base {
    variant: Variant,
    number: i64,
    id: Option<i64>,
    source: String,
}

enum Missing {
    Variant,
    Revision,
}

impl Missing {
    fn error(&self) -> CallToolResult {
        code_error(match self {
            Missing::Variant => "cv_variant_not_found",
            Missing::Revision => "cv_revision_not_found",
        })
    }
}

async fn resolve(
    conn: &mut SqliteConnection,
    facts: &Facts,
    name: &str,
    number: Option<i64>,
) -> Result<Result<Base, Missing>, super::Thrown> {
    let Some(variant) = repo::get_variant(&mut *conn, name).await? else {
        return Ok(Err(Missing::Variant));
    };
    let revision = match number {
        Some(number) => repo::get_by_number(&mut *conn, variant.id, number).await?,
        None => repo::head(&mut *conn, variant.id).await?,
    };
    Ok(match (revision, number) {
        (Some(revision), _) => Ok(Base {
            number: revision.number,
            id: Some(revision.id),
            source: revision.source,
            variant,
        }),
        (None, Some(_)) => Err(Missing::Revision),
        (None, None) => {
            let profile = cv_repo::get_profile(&mut *conn).await?;
            Ok(Base {
                number: 0,
                id: None,
                source: starting_source(profile.as_ref(), facts),
                variant,
            })
        }
    })
}

fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}

/// Call after commit: a page that hears of a revision reads it back at once.
pub(super) fn publish(ctx: &Ctx, variant: &str, revision: &Revision, approved: bool, items: usize) {
    let _ = ctx.state.cv_events.send(CvRevisionEvent {
        variant: variant.to_owned(),
        revision: revision.number,
        origin: revision.origin.clone(),
        summary: revision.summary.clone(),
        state: state_name(approved).to_owned(),
        review_items: items,
        approval: false,
    });
}

fn head_number(head: Option<&Revision>) -> i64 {
    head.map_or(0, |revision| revision.number)
}

fn conflict(expected: i64, current: i64) -> CallToolResult {
    error_with(
        "cv_revision_conflict",
        json!({"expectedRevision": expected, "currentRevision": current}),
    )
}

pub async fn get(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    let state = cv_facts::get(&mut conn).await?;
    let number = args.get("revision").and_then(Value::as_i64);
    let base = match resolve(&mut conn, &state.facts, req_str(args, "variant"), number).await? {
        Ok(base) => base,
        Err(missing) => return Ok(missing.error()),
    };
    let approved = is_approved(
        &mut conn,
        &state,
        Some(base.variant.id),
        &sha256_hex(base.source.as_bytes()),
    )
    .await?;
    // The owner's banned phrases stay in the database: an agent only hears of one it has used.
    let mut facts = serde_json::to_value(&state.facts).expect("facts serialize");
    if let Some(facts) = facts.as_object_mut() {
        facts.remove("bannedPhrases");
    }
    Ok(json_result(
        json!({
            "variant": base.variant.name,
            "revision": base.number,
            "source": base.source,
            "state": state_name(approved),
            "facts": facts,
            "factsVersion": state.version,
            "factsLocked": state.is_locked(),
            "templateHelp": TEMPLATE_HELP,
        }),
        false,
    ))
}

fn apply_edits(source: &str, edits: &[Value]) -> Result<String, Box<CallToolResult>> {
    let mut edited = source.to_owned();
    for (index, edit) in edits.iter().enumerate() {
        let old = edit["oldText"].as_str().unwrap_or_default();
        let new = edit["newText"].as_str().unwrap_or_default();
        let matches = edited.matches(old).count();
        if matches != 1 {
            return Err(Box::new(error_with(
                if matches == 0 {
                    "cv_edit_no_match"
                } else {
                    "cv_edit_ambiguous"
                },
                json!({"edit": index, "oldText": old, "matches": matches}),
            )));
        }
        edited = edited.replacen(old, new, 1);
    }
    Ok(edited)
}

async fn locked_facts(ctx: &Ctx) -> Result<Result<FactsState, CallToolResult>, super::Thrown> {
    let mut conn = ctx.state.db.conn().await?;
    let state = cv_facts::get(&mut conn).await?;
    Ok(if state.is_locked() {
        Ok(state)
    } else {
        Err(code_error("cv_facts_not_locked"))
    })
}

/// Compiles and checks a source about to become a revision.
async fn compile_and_check(
    ctx: &Ctx,
    source: &str,
    known: &Known,
    facts: &Facts,
) -> Result<(Rendered, Report), Box<CallToolResult>> {
    let rendered = compile(ctx, source, facts, false, 0, false).await?;
    let report = check(
        &rendered.text,
        facts,
        known.approved.as_deref(),
        &known.wording,
    );
    if !report.violations.is_empty() {
        return Err(Box::new(violation_error(&report.violations)));
    }
    Ok((rendered, report))
}

/// What a revision is read against: the text last approved of its variant, and the wording last
/// locked. Whoever is reading has seen those.
pub(super) struct Known {
    pub(super) approved: Option<String>,
    pub(super) wording: String,
}

pub(super) async fn known_to_owner(
    conn: &mut SqliteConnection,
    variant: Option<&Variant>,
    facts: &Facts,
) -> nexus_core::Result<Known> {
    let approved = match variant {
        Some(variant) => approvals::last_approved(&mut *conn, variant, &facts_sha256(facts))
            .await?
            .map(|(text, _)| text),
        None => None,
    };
    let wording = cv_facts::locked_wording(conn).await?;
    let wording: Vec<String> = wording.into_iter().map(|(_, text)| text).collect();
    Ok(Known {
        approved,
        wording: wording.join(" "),
    })
}

fn valid_new_variant_name(name: &str) -> bool {
    // A browser resolves `.` and `..` away, so a page of that name could never be opened.
    !name.chars().all(|c| c == '.')
        && !name.starts_with(RESERVED_VARIANT_PREFIX)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub async fn patch(ctx: &Ctx, args: &Args) -> ToolResult {
    patch_as(ctx, args, None).await
}

/// `approved_by` is for the owner's own edit. It approves only if the base was approved, because only
/// then is the whole difference theirs: a save on an agent's draft stays a draft. Only the web handler
/// passes it: no tool argument and no `origin` reaches it.
pub(crate) async fn patch_as(ctx: &Ctx, args: &Args, approved_by: Option<&str>) -> ToolResult {
    let name = req_str(args, "variant");
    let expected = args["expectedRevision"].as_i64().unwrap_or_default();
    let create_from = str_arg(args, "createFrom");
    let summary = req_str(args, "summary");
    let origin = req_str(args, "origin");
    let edits = args["edits"].as_array().map_or(&[][..], Vec::as_slice);
    if approved_by.is_none()
        && RESERVED_ORIGINS
            .iter()
            .any(|reserved| origin.trim().eq_ignore_ascii_case(reserved))
    {
        return Ok(code_error("cv_origin_reserved"));
    }
    let state = match locked_facts(ctx).await? {
        Ok(state) => state,
        Err(error) => return Ok(error),
    };
    let (base, known, base_approved) = {
        let mut conn = ctx.state.db.conn().await?;
        if create_from.is_some() {
            if !valid_new_variant_name(name) {
                return Ok(code_error("cv_variant_name_invalid"));
            }
            if repo::get_variant(&mut conn, name).await?.is_some() {
                return Ok(code_error("cv_variant_exists"));
            }
        }
        let base = match resolve(&mut conn, &state.facts, create_from.unwrap_or(name), None).await?
        {
            Ok(base) => base,
            Err(missing) => return Ok(missing.error()),
        };
        // A variant made with `createFrom` has no approved text of its own yet.
        let own = create_from.is_none().then_some(&base.variant);
        let known = known_to_owner(&mut conn, own, &state.facts).await?;
        let base_approved = is_approved(
            &mut conn,
            &state,
            Some(base.variant.id),
            &sha256_hex(base.source.as_bytes()),
        )
        .await?;
        (base, known, base_approved)
    };
    if base.number != expected {
        return Ok(conflict(expected, base.number));
    }
    let source = match apply_edits(&base.source, edits) {
        Ok(source) => source,
        Err(error) => return Ok(*error),
    };
    // The owner can store a variant's starting text as it stands: it is a change to the history.
    let stores_start = approved_by.is_some() && base.number == 0;
    if source == base.source && create_from.is_none() && !stores_start {
        return Ok(code_error("cv_no_change"));
    }
    let (rendered, report) = match compile_and_check(ctx, &source, &known, &state.facts).await {
        Ok(done) => done,
        Err(error) => return Ok(*error),
    };

    let mut tx = ctx.state.db.begin_write().await?;
    if facts_moved(&mut tx, state.version).await? {
        return Ok(facts_changed_error());
    }
    let current = repo::head(&mut tx, base.variant.id).await?;
    if head_number(current.as_ref()) != expected {
        return Ok(conflict(expected, head_number(current.as_ref())));
    }
    let variant_id = if create_from.is_some() {
        if repo::get_variant(&mut tx, name).await?.is_some() {
            return Ok(code_error("cv_variant_exists"));
        }
        repo::create_variant(&mut tx, name, None).await?.id
    } else {
        base.variant.id
    };
    let revision = repo::append(
        &mut tx,
        variant_id,
        &NewRevision {
            parent_id: base.id,
            source: &source,
            facts_version: state.version,
            origin,
            summary,
            warnings: &capped_items(&report.review_items).0,
        },
    )
    .await?;
    if let Some(approved_by) = approved_by.filter(|_| base_approved) {
        approvals::record(
            &mut tx,
            &NewApproval {
                source_sha256: &revision.source_sha256,
                facts_sha256: &facts_sha256(&state.facts),
                rendered_text: &review_text(&rendered.text),
                variant_id,
                revision_number: revision.number,
                approved_by,
            },
        )
        .await?;
    }
    let approved = is_approved(&mut tx, &state, Some(variant_id), &revision.source_sha256).await?;
    tx.commit().await?;
    publish(ctx, name, &revision, approved, report.review_items.len());
    Ok(written(
        name,
        &revision,
        &base,
        &source,
        &rendered,
        &report.review_items,
        approved,
    ))
}

fn written(
    name: &str,
    revision: &Revision,
    base: &Base,
    source: &str,
    rendered: &Rendered,
    items: &[ReviewItem],
    approved: bool,
) -> CallToolResult {
    let mut out = json!({
        "variant": name,
        "revision": revision.number,
        "pageCount": rendered.page_count,
        "diff": unified_diff(
            &base.source,
            source,
            &format!("revision {}", base.number),
            &format!("revision {}", revision.number),
        ),
    });
    add_standing(&mut out, name, approved, items);
    if !rendered.warnings.is_empty() {
        out["diagnostics"] = json!(rendered.warnings);
    }
    json_result(out, false)
}

pub async fn revert(ctx: &Ctx, args: &Args) -> ToolResult {
    let name = req_str(args, "variant");
    let to = args["toRevision"].as_i64().unwrap_or_default();
    let expected = args["expectedRevision"].as_i64().unwrap_or_default();
    let state = match locked_facts(ctx).await? {
        Ok(state) => state,
        Err(error) => return Ok(error),
    };
    let (head, target, known) = {
        let mut conn = ctx.state.db.conn().await?;
        let head = match resolve(&mut conn, &state.facts, name, None).await? {
            Ok(head) => head,
            Err(missing) => return Ok(missing.error()),
        };
        if head.number != expected {
            return Ok(conflict(expected, head.number));
        }
        let known = known_to_owner(&mut conn, Some(&head.variant), &state.facts).await?;
        match resolve(&mut conn, &state.facts, name, Some(to)).await? {
            Ok(target) => (head, target, known),
            Err(missing) => return Ok(missing.error()),
        }
    };
    let (rendered, report) =
        match compile_and_check(ctx, &target.source, &known, &state.facts).await {
            Ok(done) => done,
            Err(error) => return Ok(*error),
        };

    let mut tx = ctx.state.db.begin_write().await?;
    if facts_moved(&mut tx, state.version).await? {
        return Ok(facts_changed_error());
    }
    let current = repo::head(&mut tx, head.variant.id).await?;
    if head_number(current.as_ref()) != expected {
        return Ok(conflict(expected, head_number(current.as_ref())));
    }
    let revision = repo::append(
        &mut tx,
        head.variant.id,
        &NewRevision {
            parent_id: head.id,
            source: &target.source,
            facts_version: state.version,
            origin: "revert",
            summary: &format!("Revert to revision {to}"),
            warnings: &capped_items(&report.review_items).0,
        },
    )
    .await?;
    let approved = is_approved(
        &mut tx,
        &state,
        Some(head.variant.id),
        &revision.source_sha256,
    )
    .await?;
    tx.commit().await?;
    publish(ctx, name, &revision, approved, report.review_items.len());
    Ok(written(
        name,
        &revision,
        &head,
        &target.source,
        &rendered,
        &report.review_items,
        approved,
    ))
}

pub async fn list(ctx: &Ctx, args: &Args) -> ToolResult {
    let page = args["page"].as_i64().unwrap_or(1);
    let page_size = args["pageSize"].as_i64().unwrap_or(20);
    let include_diff = args["includeDiff"].as_bool().unwrap_or(false);
    let mut conn = ctx.state.db.conn().await?;
    let Some(variant) = repo::get_variant(&mut conn, req_str(args, "variant")).await? else {
        return Ok(code_error("cv_variant_not_found"));
    };
    let facts = cv_facts::get(&mut conn).await?;
    let total = repo::count(&mut conn, variant.id).await?;
    let revisions = repo::list(&mut conn, variant.id, page_size, (page - 1) * page_size).await?;
    let mut items = Vec::with_capacity(revisions.len());
    let mut diff_budget = MAX_DIFF_BYTES_PER_PAGE;
    for revision in revisions {
        let mut item = json!({
            "revision": revision.number,
            "sourceSha256": revision.source_sha256,
            "state": state_name(is_approved(&mut conn, &facts, Some(variant.id), &revision.source_sha256).await?),
            "factsVersion": revision.facts_version,
            "origin": revision.origin,
            "summary": revision.summary,
            "warnings": revision.warnings.0,
            "createdAt": revision.created_at,
        });
        if include_diff {
            // A parent in another variant can be gone with its application.
            let parent = match revision.parent_id {
                Some(id) if diff_budget > 0 => repo::get_by_id(&mut conn, id).await?,
                _ => None,
            };
            let diff = (diff_budget > 0).then(|| {
                unified_diff(
                    parent.as_ref().map_or("", |parent| &parent.source),
                    &revision.source,
                    &parent.as_ref().map_or_else(
                        || "empty".to_owned(),
                        |parent| format!("revision {}", parent.number),
                    ),
                    &format!("revision {}", revision.number),
                )
            });
            match diff.filter(|diff| diff.len() <= diff_budget) {
                Some(diff) => {
                    diff_budget -= diff.len();
                    item["diff"] = diff.into();
                }
                None => {
                    diff_budget = 0;
                    item["diffOmitted"] = true.into();
                }
            }
        }
        items.push(item);
    }
    Ok(json_result(
        json!({
            "variant": variant.name,
            "total": total,
            "page": page,
            "pageSize": page_size,
            "revisions": items,
        }),
        false,
    ))
}

/// Stores a rendered PDF as a normal document linked to an application.
async fn save_pdf(
    ctx: &Ctx,
    pdf: Vec<u8>,
    application_id: i64,
    original_name: String,
    facts_version: i64,
) -> Result<Result<i64, CallToolResult>, super::Thrown> {
    let bytes = Arc::new(pdf);
    let db = &ctx.state.db;
    let sha256 = put_blob(db, bytes.clone()).await?;
    let new = NewDocument {
        filename: format!("{}.pdf", uuid::Uuid::new_v4()),
        original_name,
        size: bytes.len() as i64,
        mime_type: "application/pdf".to_owned(),
        application_ids: vec![application_id],
        content_hash: Some(sha256_hex(&bytes)),
        source: Some("render_cv_pdf".to_owned()),
        generated_at: Some(Timestamp::now()),
        blob_sha256: sha256.clone(),
        ..NewDocument::default()
    };
    let stored = async {
        let mut tx = db.begin_write().await?;
        if facts_moved(&mut tx, facts_version).await? {
            return nexus_core::Result::Ok(None);
        }
        // A delete of the same bytes may have removed the file since the put above.
        if !db.blobs().exists(&sha256)? {
            put_blob(db, bytes.clone()).await?;
        }
        let document = documents_repo::create(&mut tx, &new).await?;
        tx.commit().await?;
        Ok(Some(document.id))
    };
    match stored.await {
        Ok(Some(id)) => Ok(Ok(id)),
        Ok(None) => {
            remove_blob_if_unreferenced(db, sha256).await?;
            Ok(Err(facts_changed_error()))
        }
        Err(error) => {
            remove_blob_if_unreferenced(db, sha256).await?;
            Err(error.into())
        }
    }
}

pub async fn render(ctx: &Ctx, args: &Args) -> ToolResult {
    render_as(ctx, args, false).await
}

/// `stamped_pdf` gives a draft's PDF, with the stamp, to the owner downloading it from the dashboard.
/// No tool passes it: an agent gets no PDF of a draft at all.
pub(crate) async fn render_as(ctx: &Ctx, args: &Args, stamped_pdf: bool) -> ToolResult {
    let name = req_str(args, "variant");
    let number = args.get("revision").and_then(Value::as_i64);
    let preview_pages = args["previewPages"].as_u64().unwrap_or(1) as u32;
    let (state, base, application, approved, known) = {
        let mut conn = ctx.state.db.conn().await?;
        let application = match str_arg(args, "saveToApplicationId") {
            Some(id) => match find_application(&mut conn, id).await? {
                Some(application) => Some(application),
                None => {
                    return Ok(super::text_error("Application not found or access denied"));
                }
            },
            None => None,
        };
        let state = cv_facts::get(&mut conn).await?;
        // A stored CV is built on facts the owner confirmed, from text the owner approved.
        if application.is_some() && !state.is_locked() {
            return Ok(code_error("cv_facts_not_locked"));
        }
        let base = match resolve(&mut conn, &state.facts, name, number).await? {
            Ok(base) => base,
            Err(missing) => return Ok(missing.error()),
        };
        let approved = is_approved(
            &mut conn,
            &state,
            Some(base.variant.id),
            &sha256_hex(base.source.as_bytes()),
        )
        .await?;
        if application.is_some() && !approved {
            return Ok(approval_required(&base.variant.name, base.number));
        }
        let known = known_to_owner(&mut conn, Some(&base.variant), &state.facts).await?;
        (state, base, application, approved, known)
    };
    let wants_pdf = approved || stamped_pdf;
    let rendered = match compile(
        ctx,
        &base.source,
        &state.facts,
        wants_pdf,
        preview_pages,
        !approved,
    )
    .await
    {
        Ok(rendered) => rendered,
        Err(error) => return Ok(*error),
    };
    let report = check(
        &rendered.text,
        &state.facts,
        known.approved.as_deref(),
        &known.wording,
    );
    if !report.violations.is_empty() {
        return Ok(violation_error(&report.violations));
    }
    // An approval holds for the facts it was given under, and a real PDF must not outlive them.
    if approved {
        let mut conn = ctx.state.db.conn().await?;
        if facts_moved(&mut conn, state.version).await? {
            return Ok(facts_changed_error());
        }
    }
    let mut summary = json!({
        "variant": base.variant.name,
        "revision": base.number,
        "pageCount": rendered.page_count,
        "previews": rendered.previews.len(),
        "factsVersion": state.version,
    });
    add_standing(
        &mut summary,
        &base.variant.name,
        approved,
        &report.review_items,
    );
    if !rendered.warnings.is_empty() {
        summary["diagnostics"] = json!(rendered.warnings);
    }
    let pdf = rendered.pdf;
    let blob = pdf.as_ref().map(|pdf| ENCODER.encode(pdf));
    if let Some(pdf) = &pdf {
        summary["pdfBytes"] = pdf.len().into();
    }
    if let (Some(application), Some(pdf)) = (application, pdf) {
        let original_name = format!("CV - {} - {}.pdf", application.company, application.role);
        let saved = save_pdf(ctx, pdf, application.id, original_name, state.version).await?;
        match saved {
            Ok(id) => summary["savedDocumentId"] = id.to_string().into(),
            Err(error) => return Ok(error),
        }
    }
    let uri = format!("nexus://cv/{}/{}.pdf", base.variant.name, base.number);
    let mut result = json_result(summary, false);
    if let Some(blob) = blob {
        result.content.push(ContentBlock::resource(
            ResourceContents::blob(blob, uri).with_mime_type("application/pdf"),
        ));
    }
    for preview in &rendered.previews {
        result
            .content
            .push(ContentBlock::image(ENCODER.encode(preview), "image/png"));
    }
    Ok(result)
}

/// What the owner looks at before approving a variant's head. `Err` is why it can't be looked at.
pub(crate) async fn review(
    ctx: &Ctx,
    name: &str,
) -> Result<Result<Review, CallToolResult>, super::Thrown> {
    let mut conn = ctx.state.db.conn().await?;
    let state = cv_facts::get(&mut conn).await?;
    let base = match resolve(&mut conn, &state.facts, name, None).await? {
        Ok(base) => base,
        Err(missing) => return Ok(Err(missing.error())),
    };
    let source_sha256 = sha256_hex(base.source.as_bytes());
    let approved = is_approved(&mut conn, &state, Some(base.variant.id), &source_sha256).await?;
    let last =
        approvals::last_approved(&mut conn, &base.variant, &facts_sha256(&state.facts)).await?;
    let (baseline, baseline_revision) =
        last.map_or((None, None), |(text, at)| (Some(text), Some(at)));
    let baseline_source = match baseline_revision {
        Some(at) => repo::get_by_number(&mut conn, base.variant.id, at)
            .await?
            .map(|revision| revision.source),
        None => None,
    };
    let known = known_to_owner(&mut conn, None, &state.facts).await?;
    drop(conn);
    let mut review = Review {
        variant: base.variant.name.clone(),
        revision: base.number,
        approved,
        locked: state.is_locked(),
        source_sha256,
        facts_sha256: facts_sha256(&state.facts),
        rendered_text: String::new(),
        baseline,
        baseline_revision,
        items: Vec::new(),
        source_diff: unified_diff(
            baseline_source.as_deref().unwrap_or(""),
            &base.source,
            &baseline_revision.map_or_else(
                || "nothing approved yet".to_owned(),
                |at| format!("revision {at}"),
            ),
            &format!("revision {}", base.number),
        ),
    };
    // Nothing to read over: approved text is approved. It still shows if it no longer compiles.
    if approved {
        return Ok(Ok(review));
    }
    let rendered = match compile(ctx, &base.source, &state.facts, false, 0, false).await {
        Ok(rendered) => rendered,
        Err(error) => return Ok(Err(*error)),
    };
    let report = check(
        &rendered.text,
        &state.facts,
        review.baseline.as_deref(),
        &known.wording,
    );
    if !report.violations.is_empty() {
        return Ok(Err(violation_error(&report.violations)));
    }
    review.rendered_text = review_text(&rendered.text);
    review.items = report.review_items;
    Ok(Ok(review))
}

fn stale(current: i64) -> CallToolResult {
    error_with("cv_approval_stale", json!({"currentRevision": current}))
}

/// The owner approves exactly this revision, with exactly these hashes, as `approved_by`. Needs locked
/// facts, passes the hard rules again, and refuses if the head or the facts moved since the owner
/// looked. Only `crate::owner` reaches this: it is no tool, and nothing in a tool call names it.
pub(crate) async fn approve(
    ctx: &Ctx,
    name: &str,
    number: i64,
    source_sha256: &str,
    facts_hash: &str,
    approved_by: &str,
) -> ToolResult {
    let state = match locked_facts(ctx).await? {
        Ok(state) => state,
        Err(error) => return Ok(error),
    };
    let (variant, revision, head) = {
        let mut conn = ctx.state.db.conn().await?;
        let Some(variant) = repo::get_variant(&mut conn, name).await? else {
            return Ok(code_error("cv_variant_not_found"));
        };
        let Some(revision) = repo::get_by_number(&mut conn, variant.id, number).await? else {
            return Ok(code_error("cv_revision_not_found"));
        };
        let head = head_number(repo::head(&mut conn, variant.id).await?.as_ref());
        (variant, revision, head)
    };
    if head != number
        || revision.source_sha256 != source_sha256
        || facts_sha256(&state.facts) != facts_hash
    {
        return Ok(stale(head));
    }
    let rendered = match compile(ctx, &revision.source, &state.facts, false, 0, false).await {
        Ok(rendered) => rendered,
        Err(error) => return Ok(*error),
    };
    let report = check(&rendered.text, &state.facts, None, "");
    if !report.violations.is_empty() {
        return Ok(violation_error(&report.violations));
    }
    let mut tx = ctx.state.db.begin_write().await?;
    if facts_moved(&mut tx, state.version).await? {
        return Ok(stale(head));
    }
    let current = head_number(repo::head(&mut tx, variant.id).await?.as_ref());
    if current != number {
        return Ok(stale(current));
    }
    approvals::record(
        &mut tx,
        &NewApproval {
            source_sha256,
            facts_sha256: facts_hash,
            rendered_text: &review_text(&rendered.text),
            variant_id: variant.id,
            revision_number: number,
            approved_by,
        },
    )
    .await?;
    tx.commit().await?;
    let _ = ctx.state.cv_events.send(CvRevisionEvent {
        variant: name.to_owned(),
        revision: number,
        origin: approved_by.to_owned(),
        summary: "Approved".to_owned(),
        state: state_name(true).to_owned(),
        review_items: 0,
        approval: true,
    });
    Ok(json_result(
        json!({"variant": name, "revision": number, "state": state_name(true)}),
        false,
    ))
}

/// The name of the variant `generate_tailored_cv` keeps an application's revisions in.
pub(super) fn application_variant(application_id: i64) -> String {
    format!("{RESERVED_VARIANT_PREFIX}{application_id}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexus_core::Db;
    use nexus_tailor::Renderer;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::McpState;

    #[tokio::test]
    async fn waiting_for_a_render_ends_in_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let state = Arc::new(McpState::new(
            db,
            "owner".to_owned(),
            "owner@example.test".to_owned(),
            Renderer::InProcess,
        ));
        let held = state.render_slot.clone().acquire_owned().await.unwrap();
        let ctx = Ctx {
            state,
            can_access_submissions: true,
            source: "test",
            client_gone: CancellationToken::new(),
        };
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let error = compile(&ctx, "= CV", &Facts::default(), false, 0, false)
            .await
            .unwrap_err();
        assert!(started.elapsed() >= RENDER_QUEUE_WAIT);
        assert_eq!(error.is_error, Some(true));
        let text = serde_json::to_string(&error.structured_content).unwrap();
        assert!(text.contains("cv_render_busy"), "{text}");
        drop(held);
    }

    #[tokio::test]
    async fn a_full_render_queue_refuses_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let state = Arc::new(McpState::new(
            db,
            "owner".to_owned(),
            "owner@example.test".to_owned(),
            Renderer::InProcess,
        ));
        let _held = state
            .render_calls
            .clone()
            .acquire_many_owned(MAX_RENDER_CALLS as u32)
            .await
            .unwrap();
        let ctx = Ctx {
            state,
            can_access_submissions: true,
            source: "test",
            client_gone: CancellationToken::new(),
        };
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let error = compile(&ctx, "= CV", &Facts::default(), false, 0, false)
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), Duration::ZERO);
        let text = serde_json::to_string(&error.structured_content).unwrap();
        assert!(text.contains("cv_render_busy"), "{text}");
    }
}
