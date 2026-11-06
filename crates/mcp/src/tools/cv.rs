use std::sync::Arc;

use nexus_core::Error;
use nexus_core::blob::sha256_hex;
use nexus_core::repo::cv::{self as repo, CvPatchInput, CvProfile, CvProfileInput};
use nexus_core::repo::cv_facts;
use nexus_core::repo::cv_revisions::{self as revisions, NewRevision};
use nexus_core::repo::documents::{self as documents_repo, NewDocument};
use nexus_tailor::{RenderedText, check, source_from_profile};
use serde_json::{Value, json};

use super::cv_source::{
    application_variant, compile, content_of, error_with, facts_changed_error, facts_moved,
    violation_error,
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
        Err(Error::CvFactsLocked { fields, tokens }) => error_with(
            "cv_facts_locked",
            json!({"fields": fields, "tokens": tokens}),
        ),
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

/// Everything the master profile says, for telling what a tailored CV adds to it.
fn profile_as_text(profile: &CvProfile) -> RenderedText {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            Value::Object(map) => map.values().for_each(|item| collect(item, out)),
            _ => {}
        }
    }
    let mut lines = vec![profile.profile.clone()];
    for value in [&profile.skills, &profile.experience, &profile.projects] {
        collect(&value.0, &mut lines);
    }
    RenderedText {
        lines,
        ..RenderedText::default()
    }
}

/// Port of `generateAndStoreCv`, with Typst for react-pdf and the integrity check before anything
/// is stored. Replaces the document the patch last tracked, unless that one was submitted.
pub async fn generate(ctx: &Ctx, args: &Args) -> ToolResult {
    let (application, profile, state) = {
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
        (application, profile, cv_facts::get(&mut conn).await?)
    };
    let experience_ids = strings(args, "experienceIds");
    let skill_categories = strings(args, "skillCategories");
    let include_projects = bool_arg(args, "includeProjects").unwrap_or(false);
    let include_education = bool_arg(args, "includeEducation").unwrap_or(true);
    let profile_override = str_arg(args, "profileOverride");

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
    let rendered = match compile(ctx, &source, &state.facts, true, 0).await {
        Ok(rendered) => rendered,
        Err(error) => return Ok(*error),
    };
    let report = check(
        &rendered.text,
        &state.facts,
        Some(&profile_as_text(&profile)),
    );
    if !report.violations.is_empty() {
        return Ok(violation_error(&report.violations));
    }
    let bytes = Arc::new(rendered.pdf.unwrap_or_default());
    let db = &ctx.state.db;
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
        let name = application_variant(application.id);
        let variant = match revisions::get_variant(&mut tx, &name).await? {
            Some(variant) => variant,
            None => revisions::create_variant(&mut tx, &name, Some(application.id)).await?,
        };
        let parent_id = variant.head_revision_id;
        revisions::append(
            &mut tx,
            variant.id,
            &NewRevision {
                parent_id,
                source: &source,
                facts_version: state.version,
                origin: "generate",
                summary: "Generated by generate_tailored_cv",
                warnings: &report.review_tokens,
            },
        )
        .await?;
        let tracked = repo::upsert_patch(&mut tx, application.id, &patch).await?;
        let mut replaced_blob = None;
        if let Some(previous_id) = tracked.document_id
            && let Some(previous) = documents_repo::get(&mut tx, previous_id).await?
            && !previous.is_submission_document()
        {
            replaced_blob = documents_repo::delete(&mut tx, previous_id)
                .await?
                .and_then(|deleted| deleted.blob_sha256);
        }
        // A delete of the same bytes may have removed the file since the put above.
        if !db.blobs().exists(&sha256)? {
            put_blob(db, bytes.clone()).await?;
        }
        let document = documents_repo::create(&mut tx, &new).await?;
        repo::set_patch_document_id(&mut tx, tracked.id, Some(document.id)).await?;
        tx.commit().await?;
        Ok(Some((document, replaced_blob)))
    };
    let (document, replaced_blob) = match stored.await {
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
