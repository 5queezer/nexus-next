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
use nexus_core::repo::cv_facts::{self, FactsState};
use nexus_core::repo::cv_revisions::{self as repo, NewRevision, Revision, Variant};
use nexus_core::repo::documents::{self as documents_repo, NewDocument};
use nexus_tailor::{
    CvContent, CvEmployment, CvProject, CvSkills, Facts, RenderError, RenderRequest, Rendered,
    RenderedText, TEMPLATE_HELP, Violation, check, default_source, source_from_profile,
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use serde_json::{Value, json};
use similar::TextDiff;
use sqlx::SqliteConnection;

use super::documents::{put_blob, remove_blob_if_unreferenced};
use super::{Args, Ctx, ToolResult, code_error, find_application, json_result, req_str, str_arg};

/// With a compile's 30 s, this stays inside the MCP route's 60 s deadline.
const RENDER_QUEUE_WAIT: Duration = Duration::from_secs(20);
/// Past this a diff gets coarser, not wrong. A page holds up to 50 of them, on a runtime thread.
const DIFF_TIMEOUT: Duration = Duration::from_millis(200);
/// A PDF at its limit and a typical preview. One result costs several times this in the transport,
/// as a downloaded document does.
const MAX_RENDER_RESULT_BYTES: usize = 12 * 1024 * 1024;
const RESERVED_VARIANT_PREFIX: &str = "application-";

pub(super) fn error_with(code: &str, details: Value) -> CallToolResult {
    let mut error = json!({"code": code});
    if let (Some(error), Value::Object(details)) = (error.as_object_mut(), details) {
        error.extend(details);
    }
    json_result(json!({"error": error}), true)
}

pub(super) fn violation_error(violations: &[Violation]) -> CallToolResult {
    error_with("cv_fact_violation", json!({"violations": violations}))
}

fn render_error(error: RenderError) -> CallToolResult {
    match error {
        RenderError::Diagnostics(diagnostics) => {
            error_with("cv_compile_failed", json!({"diagnostics": diagnostics}))
        }
        RenderError::SourceTooLarge => code_error("cv_source_too_large"),
        RenderError::TooManyPages => code_error("cv_too_many_pages"),
        RenderError::PdfTooLarge => code_error("cv_pdf_too_large"),
        RenderError::Timeout => code_error("cv_render_timeout"),
        RenderError::MemoryExceeded => code_error("cv_render_memory_exceeded"),
        RenderError::Worker(detail) => {
            tracing::error!(%detail, "render worker failed");
            code_error("cv_render_failed")
        }
    }
}

/// One compile, waiting its turn for a bounded time. `/facts.json` goes to the compile without the
/// banned phrases: nothing a source could print needs them, and a source that printed them would
/// read them back from the violation.
pub(super) async fn compile(
    ctx: &Ctx,
    source: &str,
    facts: &Facts,
    pdf: bool,
    preview_pages: u32,
) -> Result<Rendered, Box<CallToolResult>> {
    let slot = ctx.state.render_slot.clone();
    let Ok(Ok(_slot)) = tokio::time::timeout(RENDER_QUEUE_WAIT, slot.acquire_owned()).await else {
        return Err(Box::new(error_with(
            "cv_render_busy",
            json!({"message": "Other CVs are rendering. Nothing was stored. Try again shortly."}),
        )));
    };
    let request = RenderRequest {
        source: source.to_owned(),
        facts: Facts {
            banned_phrases: Vec::new(),
            ..facts.clone()
        },
        pdf,
        preview_pages,
    };
    ctx.state
        .renderer
        .render(request)
        .await
        .map_err(|error| Box::new(render_error(error)))
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

/// Compiles and checks a source about to become a revision. `previous` is compiled only once the
/// new source has passed, for the review tokens: a refused edit costs one compile.
async fn compile_and_check(
    ctx: &Ctx,
    source: &str,
    previous: &str,
    facts: &Facts,
) -> Result<(Rendered, Vec<String>), Box<CallToolResult>> {
    let rendered = compile(ctx, source, facts, false, 0).await?;
    let report = check(&rendered.text, facts, None);
    if !report.violations.is_empty() {
        return Err(Box::new(violation_error(&report.violations)));
    }
    let previous: Option<RenderedText> = compile(ctx, previous, facts, false, 0)
        .await
        .ok()
        .map(|rendered| rendered.text);
    let review_tokens = check(&rendered.text, facts, previous.as_ref()).review_tokens;
    Ok((rendered, review_tokens))
}

fn valid_new_variant_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(RESERVED_VARIANT_PREFIX)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub async fn patch(ctx: &Ctx, args: &Args) -> ToolResult {
    let name = req_str(args, "variant");
    let expected = args["expectedRevision"].as_i64().unwrap_or_default();
    let create_from = str_arg(args, "createFrom");
    let summary = req_str(args, "summary");
    let origin = req_str(args, "origin");
    let edits = args["edits"].as_array().map_or(&[][..], Vec::as_slice);
    let state = match locked_facts(ctx).await? {
        Ok(state) => state,
        Err(error) => return Ok(error),
    };
    let base = {
        let mut conn = ctx.state.db.conn().await?;
        if create_from.is_some() {
            if !valid_new_variant_name(name) {
                return Ok(code_error("cv_variant_name_invalid"));
            }
            if repo::get_variant(&mut conn, name).await?.is_some() {
                return Ok(code_error("cv_variant_exists"));
            }
        }
        match resolve(&mut conn, &state.facts, create_from.unwrap_or(name), None).await? {
            Ok(base) => base,
            Err(missing) => return Ok(missing.error()),
        }
    };
    if base.number != expected {
        return Ok(conflict(expected, base.number));
    }
    let source = match apply_edits(&base.source, edits) {
        Ok(source) => source,
        Err(error) => return Ok(*error),
    };
    if source == base.source && create_from.is_none() {
        return Ok(code_error("cv_no_change"));
    }
    let (rendered, review_tokens) =
        match compile_and_check(ctx, &source, &base.source, &state.facts).await {
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
            warnings: &review_tokens,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(written(
        name,
        &revision,
        &base,
        &source,
        &rendered,
        &review_tokens,
    ))
}

fn written(
    name: &str,
    revision: &Revision,
    base: &Base,
    source: &str,
    rendered: &Rendered,
    review_tokens: &[String],
) -> CallToolResult {
    let mut out = json!({
        "variant": name,
        "revision": revision.number,
        "pageCount": rendered.page_count,
        "reviewTokens": review_tokens,
        "diff": unified_diff(
            &base.source,
            source,
            &format!("revision {}", base.number),
            &format!("revision {}", revision.number),
        ),
    });
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
    let (head, target) = {
        let mut conn = ctx.state.db.conn().await?;
        let head = match resolve(&mut conn, &state.facts, name, None).await? {
            Ok(head) => head,
            Err(missing) => return Ok(missing.error()),
        };
        if head.number != expected {
            return Ok(conflict(expected, head.number));
        }
        match resolve(&mut conn, &state.facts, name, Some(to)).await? {
            Ok(target) => (head, target),
            Err(missing) => return Ok(missing.error()),
        }
    };
    let (rendered, review_tokens) =
        match compile_and_check(ctx, &target.source, &head.source, &state.facts).await {
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
            warnings: &review_tokens,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(written(
        name,
        &revision,
        &head,
        &target.source,
        &rendered,
        &review_tokens,
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
    let total = repo::count(&mut conn, variant.id).await?;
    let revisions = repo::list(&mut conn, variant.id, page_size, (page - 1) * page_size).await?;
    let mut items = Vec::with_capacity(revisions.len());
    for revision in revisions {
        let mut item = json!({
            "revision": revision.number,
            "sourceSha256": revision.source_sha256,
            "factsVersion": revision.facts_version,
            "origin": revision.origin,
            "summary": revision.summary,
            "warnings": revision.warnings.0,
            "createdAt": revision.created_at,
        });
        if include_diff {
            // A parent in another variant can be gone with its application.
            let parent = match revision.parent_id {
                Some(id) => repo::get_by_id(&mut conn, id).await?,
                None => None,
            };
            item["diff"] = unified_diff(
                parent.as_ref().map_or("", |parent| &parent.source),
                &revision.source,
                &parent.as_ref().map_or_else(
                    || "empty".to_owned(),
                    |parent| format!("revision {}", parent.number),
                ),
                &format!("revision {}", revision.number),
            )
            .into();
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
    let name = req_str(args, "variant");
    let number = args.get("revision").and_then(Value::as_i64);
    let preview_pages = args["previewPages"].as_u64().unwrap_or(1) as u32;
    let (state, base, application) = {
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
        let base = match resolve(&mut conn, &state.facts, name, number).await? {
            Ok(base) => base,
            Err(missing) => return Ok(missing.error()),
        };
        (state, base, application)
    };
    let rendered = match compile(ctx, &base.source, &state.facts, true, preview_pages).await {
        Ok(rendered) => rendered,
        Err(error) => return Ok(*error),
    };
    let report = check(&rendered.text, &state.facts, None);
    if !report.violations.is_empty() {
        return Ok(violation_error(&report.violations));
    }
    let pdf = rendered.pdf.unwrap_or_default();
    let total: usize = pdf.len() + rendered.previews.iter().map(Vec::len).sum::<usize>();
    if total > MAX_RENDER_RESULT_BYTES {
        return Ok(code_error("cv_render_result_too_large"));
    }
    let mut summary = json!({
        "variant": base.variant.name,
        "revision": base.number,
        "pageCount": rendered.page_count,
        "pdfBytes": pdf.len(),
        "previews": rendered.previews.len(),
        "factsVersion": state.version,
    });
    if !rendered.warnings.is_empty() {
        summary["diagnostics"] = json!(rendered.warnings);
    }
    let blob = ENCODER.encode(&pdf);
    if let Some(application) = application {
        let original_name = format!("CV - {} - {}.pdf", application.company, application.role);
        let saved = save_pdf(ctx, pdf, application.id, original_name, state.version).await?;
        match saved {
            Ok(id) => summary["savedDocumentId"] = id.to_string().into(),
            Err(error) => return Ok(error),
        }
    }
    let uri = format!("nexus://cv/{}/{}.pdf", base.variant.name, base.number);
    let mut result = json_result(summary, false);
    result.content.push(ContentBlock::resource(
        ResourceContents::blob(blob, uri).with_mime_type("application/pdf"),
    ));
    for preview in &rendered.previews {
        result
            .content
            .push(ContentBlock::image(ENCODER.encode(preview), "image/png"));
    }
    Ok(result)
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
        };
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let error = compile(&ctx, "= CV", &Facts::default(), false, 0)
            .await
            .unwrap_err();
        assert!(started.elapsed() >= RENDER_QUEUE_WAIT);
        assert_eq!(error.is_error, Some(true));
        let text = serde_json::to_string(&error.structured_content).unwrap();
        assert!(text.contains("cv_render_busy"), "{text}");
        drop(held);
    }
}
