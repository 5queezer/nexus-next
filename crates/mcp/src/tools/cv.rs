use nexus_core::repo::cv::{self as repo, CvProfile, CvProfileInput};
use serde_json::Value;

use super::{
    Args, Ctx, ToolResult, find_application, json_result, req_str, text_error, text_result,
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
    let upserted = async { repo::upsert_profile(&mut *ctx.state.db.conn().await?, &input).await };
    Ok(match upserted.await {
        Ok(profile) => json_result(profile_json(ctx, &profile), false),
        Err(_) => text_error("Failed to upsert CV profile"),
    })
}

/// SPEC section 4 item 6: rendering arrives in M2, so this never writes.
pub async fn generate(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    if find_application(&mut conn, req_str(args, "applicationId"))
        .await?
        .is_none()
    {
        return Ok(text_error("Application not found or access denied"));
    }
    if repo::get_profile(&mut conn).await?.is_none() {
        return Ok(text_error(
            "No CV profile found. Use upsert_cv_profile first.",
        ));
    }
    Ok(text_error(
        "CV rendering is not available yet: it arrives in M2. Nothing was generated or stored.",
    ))
}
