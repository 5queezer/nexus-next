use nexus_core::repo::contacts::{self as repo, ContactPatch, NewContact};
use serde_json::json;

use super::{Args, Ctx, ToolResult, find_application, json_result, req_str, str_arg, text_error, text_result};
use crate::js::{nid, slice16};

const NOT_FOUND: &str = "Contact not found or access denied";
const APP_NOT_FOUND: &str = "Application not found or access denied";

fn new_contact(value: &Args) -> NewContact {
    let limited = |key, max| str_arg(value, key).map(|text| slice16(text, max));
    NewContact {
        name: slice16(req_str(value, "name"), 255),
        email: limited("email", 255),
        phone: limited("phone", 50),
        role: limited("role", 100),
        linked_in: limited("linkedIn", 500),
    }
}

pub async fn create(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    let Some(application) = find_application(&mut conn, req_str(args, "applicationId")).await? else {
        return Ok(text_error(APP_NOT_FOUND));
    };
    let contact = repo::create(&mut conn, application.id, &new_contact(args)).await?;
    Ok(json_result(serde_json::to_value(contact).expect("contact serializes"), false))
}

pub async fn batch_create(ctx: &Ctx, args: &Args) -> ToolResult {
    let mut conn = ctx.state.db.conn().await?;
    let Some(application) = find_application(&mut conn, req_str(args, "applicationId")).await? else {
        return Ok(text_error(APP_NOT_FOUND));
    };
    let items = args["contacts"].as_array().cloned().unwrap_or_default();
    let mut results = Vec::with_capacity(items.len());
    let mut succeeded = 0;
    for (index, item) in items.iter().enumerate() {
        let contact = new_contact(item.as_object().expect("validated object"));
        match repo::create(&mut conn, application.id, &contact).await {
            Ok(created) => {
                succeeded += 1;
                results.push(json!({"index": index, "id": created.id.to_string(), "operation": "created"}));
            }
            Err(_) => results.push(json!({
                "index": index, "id": "", "operation": "created", "error": "contact_create_failed",
            })),
        }
    }
    Ok(json_result(
        json!({"total": items.len(), "succeeded": succeeded, "failed": items.len() - succeeded, "results": results}),
        false,
    ))
}

/// `undefined` leaves a column alone; `null` clears it.
fn nullable(args: &Args, key: &str, max: usize) -> Option<Option<String>> {
    args.get(key).map(|value| value.as_str().map(|text| slice16(text, max)))
}

pub async fn update(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let mut conn = ctx.state.db.conn().await.ok()?;
        let application = find_application(&mut conn, req_str(args, "applicationId")).await.ok()??;
        let patch = ContactPatch {
            name: str_arg(args, "name").map(|name| slice16(name, 255)),
            email: nullable(args, "email", 255),
            phone: nullable(args, "phone", 50),
            role: nullable(args, "role", 100),
            linked_in: nullable(args, "linkedIn", 500),
        };
        let id = nid(req_str(args, "contactId")).ok()?;
        repo::update(&mut conn, id, application.id, &patch).await.ok()
    };
    Ok(match run.await {
        Some(contact) => json_result(serde_json::to_value(contact).expect("contact serializes"), false),
        None => text_error(NOT_FOUND),
    })
}

pub async fn delete(ctx: &Ctx, args: &Args) -> ToolResult {
    let run = async {
        let mut conn = ctx.state.db.conn().await.ok()?;
        let application = find_application(&mut conn, req_str(args, "applicationId")).await.ok()??;
        let id = nid(req_str(args, "contactId")).ok()?;
        repo::delete(&mut conn, id, application.id).await.ok()
    };
    Ok(match run.await {
        Some(()) => text_result("Contact deleted"),
        None => text_error(NOT_FOUND),
    })
}

