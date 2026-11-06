//! What only the owner can do to a CV: approve it, save their own edit as approved, and download a
//! stamped draft. Plain functions for the web handlers and `nexus cv-facts`. None of this is a tool:
//! `tools/list` names nothing here and no argument of a tool call reaches it, which
//! `crates/server/tests/cv_gate.rs` tests. Callers must have established the owner first.

use std::sync::Arc;

use nexus_tailor::ReviewItem;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::{self, Ctx, Thrown, text_error};
use crate::{McpState, all_tools, zod};

/// What the owner looks at before approving a variant's head.
#[derive(Debug, Clone)]
pub struct Review {
    pub variant: String,
    /// 0 for a variant with nothing stored, which can't be approved until saved.
    pub revision: i64,
    pub approved: bool,
    pub locked: bool,
    pub source_sha256: String,
    pub facts_sha256: String,
    /// What a reader sees, and what an approval stores. Empty when already approved.
    pub rendered_text: String,
    /// The text last approved of this variant, or none for a first approval.
    pub baseline: Option<String>,
    pub baseline_revision: Option<i64>,
    /// Against `baseline`, uncapped and most serious first.
    pub items: Vec<ReviewItem>,
    /// The source against the last approved revision's.
    pub source_diff: String,
}

fn ctx(state: Arc<McpState>) -> Ctx {
    Ctx {
        state,
        can_access_submissions: false,
        source: "dashboard",
        client_gone: CancellationToken::new(),
    }
}

fn outcome(result: tools::ToolResult) -> CallToolResult {
    result.unwrap_or_else(|Thrown(message)| text_error(&message))
}

/// Reads the variant's head for review. `Err` carries the tool error saying why it can't be read.
pub async fn review(state: Arc<McpState>, variant: &str) -> Result<Review, CallToolResult> {
    match tools::review(&ctx(state), variant).await {
        Ok(review) => review,
        Err(Thrown(message)) => Err(text_error(&message)),
    }
}

/// The owner approves exactly this revision. `approved_by` is `cli` or `dashboard`.
pub async fn approve(
    state: Arc<McpState>,
    variant: &str,
    revision: i64,
    source_sha256: &str,
    facts_sha256: &str,
    approved_by: &str,
) -> CallToolResult {
    let ctx = ctx(state);
    let approved = tools::approve(
        &ctx,
        variant,
        revision,
        source_sha256,
        facts_sha256,
        approved_by,
    );
    outcome(approved.await)
}

/// The owner's edit on the dashboard, which they are looking at as they save it: stored and approved
/// in one step. `args` are `patch_cv`'s, and validated the same.
pub async fn save_as_approved(state: Arc<McpState>, args: &Value) -> CallToolResult {
    let schema = all_tools()
        .iter()
        .find(|tool| tool["name"] == "patch_cv")
        .map_or(&Value::Null, |tool| &tool["inputSchema"]);
    match zod::parse("patch_cv", schema, Some(args)) {
        Ok(args) => outcome(tools::patch_as(&ctx(state), &args, Some("dashboard")).await),
        Err(issues) => text_error(&issues),
    }
}

/// A revision's PDF for the owner to download: the real one when approved, the stamped one otherwise.
pub async fn download_pdf(
    state: Arc<McpState>,
    variant: &str,
    revision: Option<i64>,
) -> CallToolResult {
    let mut args = json!({"variant": variant, "previewPages": 0});
    if let Some(revision) = revision {
        args["revision"] = revision.into();
    }
    let args = args.as_object().cloned().unwrap_or_default();
    outcome(tools::render_as(&ctx(state), &args, true).await)
}
