//! The tools, ported from old Nexus's `app/api/mcp/route.ts`.

mod applications;
mod contacts;
mod cv;
mod documents;
mod events;
mod health;
mod submissions;

use std::sync::Arc;

use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS};
use nexus_core::repo::applications::Application;
use nexus_core::repo::contacts as contacts_repo;
use nexus_core::repo::documents::{Document, list_for_submission};
use nexus_core::repo::events::ApplicationEvent;
use nexus_core::repo::submissions::Submission;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};
use sqlx::SqliteConnection;

use crate::js::{nid, normalize_numbers};
use crate::{McpState, frozen_tools, zod};

/// An exception no handler caught. The SDK turned its message into a tool error.
#[derive(Debug)]
pub(crate) struct Thrown(pub String);

/// Replaces the text of a storage failure, which can hold SQL details, in what a client sees.
pub(crate) const INTERNAL_ERROR: &str = "Internal error";

impl From<nexus_core::Error> for Thrown {
    fn from(error: nexus_core::Error) -> Self {
        match error {
            nexus_core::Error::Database(_) | nexus_core::Error::Migrate(_) => {
                tracing::error!(%error, "tool failed on the database");
                Self(INTERNAL_ERROR.to_owned())
            }
            domain => Self(domain.to_string()),
        }
    }
}

impl From<String> for Thrown {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Thrown {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

pub(crate) type ToolResult = Result<CallToolResult, Thrown>;
pub(crate) type Args = Map<String, Value>;

fn result(text: String, structured: Option<Value>, is_error: bool) -> CallToolResult {
    let mut out = if is_error {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    };
    out.structured_content = structured;
    out.is_error = is_error.then_some(true);
    out
}

/// Port of `jsonToolResult`.
pub(crate) fn json_result(value: Value, is_error: bool) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).expect("JSON serializes");
    let structured = value.is_object().then_some(value);
    result(text, structured, is_error)
}

pub(crate) fn text_result(text: &str) -> CallToolResult {
    result(text.to_owned(), None, false)
}

pub(crate) fn text_error(text: &str) -> CallToolResult {
    result(text.to_owned(), None, true)
}

pub(crate) fn code_error(code: &str) -> CallToolResult {
    json_result(json!({"error": {"code": code}}), true)
}

fn submission_scope_error() -> CallToolResult {
    json_result(
        json!({"error": {"code": "insufficient_scope", "required": SCOPE_SUBMISSIONS}}),
        true,
    )
}

pub(crate) fn str_arg<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// A required string argument, which validation has already checked.
pub(crate) fn req_str<'a>(args: &'a Args, key: &str) -> &'a str {
    str_arg(args, key).unwrap_or_default()
}

pub(crate) fn bool_arg(args: &Args, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// One tool call: the state plus what the caller's token allows.
pub(crate) struct Ctx {
    pub state: Arc<McpState>,
    pub can_access_submissions: bool,
}

impl Ctx {
    fn require_submissions(&self) -> Option<CallToolResult> {
        (!self.can_access_submissions).then(submission_scope_error)
    }

    /// Fields old Nexus's records carry that a single-owner database no longer stores.
    fn with_owner(&self, value: &mut Value) {
        value["userId"] = self.state.owner_id.clone().into();
    }

    pub fn app_json(&self, application: &Application, contacts: Vec<Value>) -> Value {
        let mut value = serde_json::to_value(application).expect("application serializes");
        self.with_owner(&mut value);
        value["isDemo"] = false.into();
        value["demoWorkspaceId"] = Value::Null;
        value["demoKey"] = Value::Null;
        value["contacts"] = contacts.into();
        value
    }

    /// The application as `mapApp` returns it with `include: { contacts: true }`.
    pub async fn app_with_contacts(
        &self,
        conn: &mut SqliteConnection,
        application: &Application,
    ) -> Result<Value, Thrown> {
        let contacts = contacts_repo::list_for_application(conn, application.id).await?;
        let contacts = contacts
            .iter()
            .map(|contact| serde_json::to_value(contact).expect("contact serializes"))
            .collect();
        Ok(self.app_json(application, contacts))
    }

    pub fn event_json(&self, event: &ApplicationEvent) -> Value {
        let mut value = serde_json::to_value(event).expect("event serializes");
        self.with_owner(&mut value);
        value["isDemo"] = false.into();
        value["demoWorkspaceId"] = Value::Null;
        value["demoKey"] = Value::Null;
        value
    }

    /// A document after `sanitizeDocumentAssociations`. A `contentHash` computed at import stays
    /// hidden, because old Nexus returned null for it.
    pub fn doc_json(&self, document: &Document) -> Value {
        let mut value = serde_json::to_value(document).expect("document serializes");
        self.with_owner(&mut value);
        if document.content_hash_computed {
            value["contentHash"] = Value::Null;
        }
        value
    }

    /// A document straight from `mapDoc`, as the submission result's top-level `documents` are.
    pub fn raw_doc_json(&self, document: &Document) -> Value {
        let mut value = self.doc_json(document);
        value["demoProvenance"] = false.into();
        value["applicationIds"] = document
            .applications
            .iter()
            .map(|parent| Value::from(parent.id.to_string()))
            .collect::<Vec<_>>()
            .into();
        value
    }

    /// A submission with its claimed documents, in ID order as Prisma's relation query returns them.
    pub async fn submission_docs(
        &self,
        conn: &mut SqliteConnection,
        submission_id: i64,
    ) -> Result<Vec<Document>, Thrown> {
        let mut documents = list_for_submission(conn, submission_id).await?;
        documents.sort_by_key(|document| document.id);
        Ok(documents)
    }

    pub fn submission_json(&self, submission: &Submission, documents: &[Document]) -> Value {
        let mut value = serde_json::to_value(submission).expect("submission serializes");
        self.with_owner(&mut value);
        value["documents"] = documents
            .iter()
            .map(|d| self.doc_json(d))
            .collect::<Vec<_>>()
            .into();
        value
    }
}

/// `getRealApplication`: a malformed ID reads as not found.
pub(crate) async fn find_application(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<Application>, Thrown> {
    match nid(id) {
        Ok(id) => Ok(nexus_core::repo::applications::get(conn, id).await?),
        Err(_) => Ok(None),
    }
}

async fn dispatch(ctx: &Ctx, name: &str, args: Args) -> ToolResult {
    match name {
        "list_applications" => applications::list(ctx, &args).await,
        "get_application" => applications::get(ctx, &args).await,
        "create_application" => applications::create(ctx, &args).await,
        "update_application" => applications::update(ctx, &args).await,
        "archive_application" => applications::set_archived(ctx, &args, true).await,
        "unarchive_application" => applications::set_archived(ctx, &args, false).await,
        "delete_application" => applications::delete(ctx, &args).await,
        "find_application_by_job_url" => applications::find_by_job_url(ctx, &args).await,
        "upsert_application_by_job_url" => applications::upsert_by_job_url(ctx, &args).await,
        "batch_upsert_applications" => applications::batch_upsert(ctx, &args).await,
        "batch_delete_applications" => applications::batch_delete(ctx, &args).await,
        "list_applications_filtered" => applications::list_filtered(ctx, &args).await,
        "record_application_submission" => submissions::record(ctx, &args).await,
        "list_application_submissions" => submissions::list(ctx, &args).await,
        "get_application_submission" => submissions::get(ctx, &args).await,
        "append_application_note" => events::append_note(ctx, &args).await,
        "record_application_event" => events::record(ctx, &args).await,
        "list_application_events" => events::list(ctx, &args).await,
        "list_application_activity" => events::activity(ctx, &args).await,
        "get_interview_recall_package" => submissions::recall_package(ctx, &args).await,
        "pipeline_healthcheck" => health::check(ctx).await,
        "create_contact" => contacts::create(ctx, &args).await,
        "batch_create_contacts" => contacts::batch_create(ctx, &args).await,
        "update_contact" => contacts::update(ctx, &args).await,
        "delete_contact" => contacts::delete(ctx, &args).await,
        "list_documents" => documents::list(ctx, &args).await,
        "get_document" => documents::get(ctx, &args).await,
        "upload_document_content" => documents::upload(ctx, &args).await,
        "download_document_content" => documents::download(ctx, &args).await,
        "update_document_links" => documents::update_links(ctx, &args).await,
        "update_document_metadata" => documents::update_metadata(ctx, &args).await,
        "delete_document" => documents::delete(ctx, &args).await,
        "get_cv_profile" => cv::get_profile(ctx).await,
        "upsert_cv_profile" => cv::upsert_profile(ctx, &args).await,
        "generate_tailored_cv" => cv::generate(ctx, &args).await,
        _ => Ok(text_error(&format!(
            "MCP error -32602: Tool {name} not found"
        ))),
    }
}

/// The SDK's `tools/call` handler: validate, run, and turn every failure into a tool error.
pub(crate) async fn call(
    state: Arc<McpState>,
    auth: &AuthContext,
    name: &str,
    mut args: Option<Value>,
) -> CallToolResult {
    let Some(tool) = frozen_tools().iter().find(|tool| tool["name"] == name) else {
        return text_error(&format!("MCP error -32602: Tool {name} not found"));
    };
    if let Some(args) = &mut args {
        normalize_numbers(args);
    }
    let args = match zod::parse(name, &tool["inputSchema"], args.as_ref()) {
        Ok(args) => args,
        Err(issues) => {
            return text_error(&format!(
                "MCP error -32602: Input validation error: Invalid arguments for tool {name}: {issues}"
            ));
        }
    };
    let ctx = Ctx {
        state,
        can_access_submissions: auth.auth_type != AuthType::OAuth
            || auth.has_scope(SCOPE_SUBMISSIONS),
    };
    match dispatch(&ctx, name, args).await {
        Ok(result) => result,
        Err(Thrown(message)) => text_error(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_storage_failure_never_shows_its_text_but_a_domain_code_does() {
        let raw = nexus_core::Error::Database(sqlx::Error::PoolTimedOut);
        assert_eq!(Thrown::from(raw).0, INTERNAL_ERROR);
        assert_eq!(Thrown::from(nexus_core::Error::NotFound).0, "not_found");
    }
}
