use std::sync::Arc;

use nexus_core::Error;
use nexus_core::blob::sha256_hex;
use nexus_core::repo::cv::{self as repo, CvPatchInput, CvProfile, CvProfileInput};
use nexus_core::repo::cv_approvals::{self as approvals, NewApproval, facts_sha256};
use nexus_core::repo::cv_facts;
use nexus_core::repo::cv_revisions::{self as revisions, NewRevision};
use nexus_core::repo::documents::{self as documents_repo, NewDocument};
use nexus_tailor::{check, review_text, source_from_profile};
use serde_json::{Value, json};

use super::cv_source::{
    application_variant, approval_required, capped_items, compile, content_of, error_with,
    facts_changed_error, facts_moved, is_approved, known_to_owner, publish, violation_error,
};
use super::documents::{put_blob, remove_blob_if_unreferenced};
use super::{
    Args, Ctx, ToolResult, bool_arg, find_application, json_result, req_str, str_arg, text_error,
    text_result,
};

fn profile_json(ctx: &Ctx, profile: &CvProfile) -> Value {
    let mut value = serde_json::to_value(profile).expect("profile serializes");
    ctx.with_owner(&mut value);
    value
}

/// The application's variant, with this source as its head: running a generate again changes nothing
/// in the history, and an agent's retries don't fill the owner's list. Whether it added one is last.
async fn store_revision(
    conn: &mut sqlx::SqliteConnection,
    name: &str,
    application_id: i64,
    new: &NewRevision<'_>,
) -> nexus_core::Result<(i64, revisions::Revision, bool)> {
    let variant = match revisions::get_variant(&mut *conn, name).await? {
        Some(variant) => variant,
        None => revisions::create_variant(&mut *conn, name, Some(application_id)).await?,
    };
    if let Some(head) = revisions::head(&mut *conn, variant.id).await?
        && head.source == new.source
    {
        return Ok((variant.id, head, false));
    }
    let revision = revisions::append(
        conn,
        variant.id,
        &NewRevision {
            parent_id: variant.head_revision_id,
            ..new.clone()
        },
    )
    .await?;
    Ok((variant.id, revision, true))
}

pub async fn get_profile(ctx: &Ctx) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    Ok(match repo::get_profile(&mut conn).await? {
        Some(profile) => json_result(profile_json(ctx, &profile), false),
        None => text_result("No CV profile found. Use upsert_cv_profile to create one."),
    })
}

pub async fn upsert_profile(ctx: &Ctx, args: &Args) -> ToolResult {
    let input = CvProfileInput {
        name: req_str(args, "name").to_owned(),
        contact: args["contact"].clone(),
        profile: req_str(args, "profile").to_owned(),
        skills: args["skills"].clone(),
        experience: args["experience"].clone(),
        projects: args.get("projects").cloned(),
        education: args.get("education").cloned(),
    };
    let upserted = async {
        let mut tx = ctx.state.db.begin_write().await?;
        let profile = repo::upsert_profile(&mut tx, &input).await?;
        tx.commit().await?;
        Ok::<_, Error>(profile)
    };
    Ok(match upserted.await {
        Ok(profile) => json_result(profile_json(ctx, &profile), false),
        Err(Error::CvFactsLocked { fields }) => {
            error_with("cv_facts_locked", json!({"fields": fields}))
        }
        Err(_) => text_error("Failed to upsert CV profile"),
    })
}

fn strings(args: &Args, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A lock confirms wording, not how a call arranges it. Picking from the locked jobs and categories, in
/// any order, is what the tool is for. A job or category shown twice, or a CV with no job, is something
/// the owner hasn't seen, so it needs reading like any other draft.
fn plain_selection(profile: &CvProfile, jobs: &[String], categories: &[String]) -> bool {
    let distinct = |names: &[String]| {
        let mut sorted: Vec<&String> = names.iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.len() == names.len()
    };
    let entries = profile
        .experience
        .0
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    distinct(jobs)
        && distinct(categories)
        && jobs
            .iter()
            .any(|id| entries.iter().any(|entry| entry["id"] == id.as_str()))
}

/// How a generated source stands with the owner, under locked facts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Generated {
    /// The owner approved this text.
    Approved,
    /// This text is a plain selection from just the wording the last lock confirmed, and the owner
    /// hasn't approved anything else in this variant since, so the lock approved it.
    ApprovedByLock,
    /// An agent's wording: a revision to approve, and no document.
    Draft,
    /// As old Nexus, for parity, with a PDF that says it is no final CV.
    Unlocked,
}

/// Port of `generateAndStoreCv`, with Typst for react-pdf and the integrity check before anything
/// is stored. Replaces the document the patch last tracked, unless that one was submitted. Unlike old
/// Nexus, a document also linked to another application stays for that one, and only loses this link.
pub async fn generate(ctx: &Ctx, args: &Args) -> ToolResult {
    let profile_override = str_arg(args, "profileOverride");
    let (application, profile, state, locked_wording) = {
        let mut conn = ctx.state.db.conn().await?;
        let Some(application) = find_application(&mut conn, req_str(args, "applicationId")).await?
        else {
            return Ok(text_error("Application not found or access denied"));
        };
        let Some(profile) = repo::get_profile(&mut conn).await? else {
            return Ok(text_error(
                "No CV profile found. Use upsert_cv_profile first.",
            ));
        };
        let state = cv_facts::get(&mut conn).await?;
        let locked_wording = cv_facts::locked_wording(&mut conn).await?;
        (application, profile, state, locked_wording)
    };
    let experience_ids = strings(args, "experienceIds");
    let skill_categories = strings(args, "skillCategories");
    let include_projects = bool_arg(args, "includeProjects").unwrap_or(false);
    let include_education = bool_arg(args, "includeEducation").unwrap_or(true);

    let unmatched = |ids: &[String], list: &Value, key: &str| -> Vec<String> {
        let known = list.as_array().map_or(&[][..], Vec::as_slice);
        ids.iter()
            .filter(|id| !known.iter().any(|entry| entry[key] == id.as_str()))
            .cloned()
            .collect()
    };
    let mut warnings = Vec::new();
    let missing = unmatched(&experience_ids, &profile.experience.0, "id");
    if !missing.is_empty() {
        warnings.push(format!(
            "Unmatched experience IDs ignored: {}",
            missing.join(", ")
        ));
    }
    let missing = unmatched(&skill_categories, &profile.skills.0, "category");
    if !missing.is_empty() {
        warnings.push(format!(
            "Unmatched skill categories ignored: {}",
            missing.join(", ")
        ));
    }

    let source = source_from_profile(&content_of(
        &profile,
        &state.facts,
        &experience_ids,
        &skill_categories,
        profile_override,
        include_projects,
        include_education,
    ));
    let name = application_variant(application.id);
    let source_sha256 = sha256_hex(source.as_bytes());
    let (standing, known) = {
        let mut conn = ctx.state.db.conn().await?;
        let variant = revisions::get_variant(&mut conn, &name).await?;
        let known = known_to_owner(&mut conn, variant.as_ref(), &state.facts).await?;
        let variant_id = variant.as_ref().map(|variant| variant.id);
        let owner_approved_here = match variant_id {
            Some(id) => approvals::newest_approved_by(&mut conn, id, &facts_sha256(&state.facts))
                .await?
                .is_some_and(|by| by != "lock"),
            None => false,
        };
        let standing = if !state.is_locked() {
            Generated::Unlocked
        } else if is_approved(&mut conn, &state, variant_id, &source_sha256).await? {
            Generated::Approved
        } else if profile_override.is_none()
            && repo::wording_of(&profile) == locked_wording
            && plain_selection(&profile, &experience_ids, &skill_categories)
            && !owner_approved_here
        {
            Generated::ApprovedByLock
        } else {
            Generated::Draft
        };
        (standing, known)
    };
    let rendered = match compile(
        ctx,
        &source,
        &state.facts,
        standing != Generated::Draft,
        0,
        standing == Generated::Unlocked,
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
    let review_items = capped_items(&report.review_items).0;
    let new_revision = NewRevision {
        parent_id: None,
        source: &source,
        facts_version: state.version,
        origin: "generate",
        summary: "Generated by generate_tailored_cv",
        warnings: &review_items,
    };
    let db = &ctx.state.db;
    if standing == Generated::Draft {
        let stored = async {
            let mut tx = db.begin_write().await?;
            if facts_moved(&mut tx, state.version).await? {
                return Ok::<_, Error>(None);
            }
            let (_, revision, added) =
                store_revision(&mut tx, &name, application.id, &new_revision).await?;
            tx.commit().await?;
            Ok(Some((revision, added)))
        };
        return Ok(match stored.await {
            Ok(Some((revision, added))) => {
                if added {
                    publish(ctx, &name, &revision, false, report.review_items.len());
                }
                approval_required(&name, revision.number)
            }
            Ok(None) => facts_changed_error(),
            Err(error) => {
                let message = super::Thrown::from(error).0;
                text_error(&format!("Failed to generate CV: {message}"))
            }
        });
    }
    let bytes = Arc::new(rendered.pdf.unwrap_or_default());
    let Ok(sha256) = put_blob(db, bytes.clone()).await else {
        return Ok(text_error("Failed to generate CV: unknown error"));
    };

    let original_name = format!("CV - {} - {}.pdf", application.company, application.role);
    let new = NewDocument {
        filename: format!("{}.pdf", uuid::Uuid::new_v4()),
        original_name,
        size: bytes.len() as i64,
        mime_type: "application/pdf".to_owned(),
        application_ids: vec![application.id],
        content_hash: Some(sha256_hex(&bytes)),
        blob_sha256: sha256.clone(),
        ..NewDocument::default()
    };
    let patch = CvPatchInput {
        profile_override: profile_override.map(str::to_owned),
        experience_ids,
        skill_categories,
        include_projects: Some(include_projects),
        include_education: Some(include_education),
    };
    let stored = async {
        let mut tx = db.begin_write().await?;
        if facts_moved(&mut tx, state.version).await? {
            return Ok::<_, Error>(None);
        }
        let (variant_id, revision, added) =
            store_revision(&mut tx, &name, application.id, &new_revision).await?;
        if standing == Generated::ApprovedByLock {
            approvals::record(
                &mut tx,
                &NewApproval {
                    source_sha256: &source_sha256,
                    facts_sha256: &facts_sha256(&state.facts),
                    rendered_text: &review_text(&rendered.text),
                    variant_id,
                    revision_number: revision.number,
                    approved_by: "lock",
                },
            )
            .await?;
        }
        let tracked = repo::upsert_patch(&mut tx, application.id, &patch).await?;
        let mut replaced_blob = None;
        if let Some(previous_id) = tracked.document_id
            && let Some(previous) = documents_repo::get(&mut tx, previous_id).await?
            && !previous.is_submission_document()
        {
            let others: Vec<i64> = previous
                .applications
                .iter()
                .map(|parent| parent.id)
                .filter(|id| *id != application.id)
                .collect();
            if others.is_empty() {
                replaced_blob = documents_repo::delete(&mut tx, previous_id)
                    .await?
                    .and_then(|deleted| deleted.blob_sha256);
            } else {
                documents_repo::set_links(&mut tx, previous_id, &others).await?;
            }
        }
        // A delete of the same bytes may have removed the file since the put above.
        if !db.blobs().exists(&sha256)? {
            put_blob(db, bytes.clone()).await?;
        }
        let document = documents_repo::create(&mut tx, &new).await?;
        repo::set_patch_document_id(&mut tx, tracked.id, Some(document.id)).await?;
        tx.commit().await?;
        Ok(Some((document, replaced_blob, revision, added)))
    };
    let (document, replaced_blob, revision, added) = match stored.await {
        Ok(Some(done)) => done,
        Ok(None) => {
            let _ = remove_blob_if_unreferenced(db, sha256).await;
            return Ok(facts_changed_error());
        }
        Err(error) => {
            let _ = remove_blob_if_unreferenced(db, sha256).await;
            let message = super::Thrown::from(error).0;
            return Ok(text_error(&format!("Failed to generate CV: {message}")));
        }
    };
    if added {
        let approved = standing != Generated::Unlocked;
        publish(ctx, &name, &revision, approved, report.review_items.len());
    }
    if let Some(replaced) = replaced_blob {
        let _ = remove_blob_if_unreferenced(db, replaced).await;
    }
    let mut result = json!({
        "message": "CV generated successfully",
        "documentId": document.id.to_string(),
        "originalName": document.original_name,
        "size": document.size,
        "applicationId": req_str(args, "applicationId"),
        "company": application.company,
        "role": application.role,
    });
    if !warnings.is_empty() {
        result["warnings"] = warnings.into();
    }
    Ok(json_result(result, false))
}
