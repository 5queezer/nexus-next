//! What only the owner can do: approve a CV, save their own edit, download a stamped draft, and mark an
//! application applied with its dates. Plain functions for the web handlers and `nexus cv-facts`. None of this is a tool:
//! `tools/list` names nothing here and no argument of a tool call reaches it, which
//! `crates/server/tests/cv_gate.rs` tests. Callers must have established the owner first.

use std::sync::Arc;

use nexus_tailor::{LinePlace, MAX_PAGES, ReviewItem};
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
    pub facts_version: i64,
    /// What the check reads, and what an approval stores. A shape drawn over text can make it differ
    /// from the picture. Empty when already approved.
    pub rendered_text: String,
    /// Every page, stamped, as PNG, from the same compile as `rendered_text`. Empty when already
    /// approved.
    pub previews: Vec<Vec<u8>>,
    /// Where the words of each line of `rendered_text` stand in `previews`, from the same compile.
    /// Shorter than the text, which goes on with what the PDF says beyond its pages.
    pub places: Vec<LinePlace>,
    /// The approved text this draft is read against, or none if no approved text stands for it.
    pub baseline: Option<String>,
    pub baseline_revision: Option<i64>,
    /// Whether the baseline is this variant's own newest approval, or text approved in another.
    pub baseline_is_own: bool,
    /// Whether that approval is the one the owner's lock of the facts wrote, not a tap of their own.
    pub baseline_is_lock: bool,
    /// Whether the baseline is what this variant started from in another, which it never held itself.
    pub baseline_is_start: bool,
    /// Whether another variant has replaced the baseline since: see [`Review::REPLACED_ELSEWHERE`].
    pub baseline_is_replaced: bool,
    /// Whether this variant has an approval of its own, under any facts.
    pub approved_here: bool,
    /// The words with a small mark drawn on them: see [`Review::DRAWN_MARKS`]. Empty when approved.
    pub marked: Vec<String>,
    /// Against `baseline`, uncapped and most serious first.
    pub items: Vec<ReviewItem>,
    /// The source against the last approved revision's.
    pub source_diff: String,
}

impl Review {
    /// Said plainly above the button: this is the cost the owner accepted for reading a diff again.
    pub const REPLACED_ELSEWHERE: &str = "A text you approved has since been changed in another variant, and a line you corrected there won't show as changed here, so read the picture.";
    /// Said above the button when a draft draws small marks on its text.
    pub const DRAWN_MARKS: &str = "The draft draws marks on its text, which can make a number read as another. Read the numbers in the picture.";

    /// Why this reads as the whole CV, with nothing to compare it with. `None` if it has a baseline.
    #[must_use]
    pub fn whole_cv_because(&self) -> Option<&'static str> {
        self.baseline.is_none().then_some(if self.approved_here {
            "The facts have changed since you last approved text in this variant, so that text can't stand for this one. This is the whole CV: every number and date is marked, and the text doesn't show what a source draws, so read the picture."
        } else {
            "Nothing this variant holds or started from still stands as approved under today's facts. This is the whole CV: every number and date is marked, and the text doesn't show what a source draws, so read the picture."
        })
    }
}

fn ctx(state: Arc<McpState>) -> Ctx {
    Ctx {
        state,
        can_access_submissions: false,
        source: tools::OWNER_SOURCE,
        client_gone: CancellationToken::new(),
    }
}

fn outcome(result: tools::ToolResult) -> CallToolResult {
    result.unwrap_or_else(|Thrown(message)| text_error(&message))
}

/// What a page says when it ran out of time before its review could finish.
#[must_use]
pub fn render_busy_error() -> CallToolResult {
    *tools::render_busy()
}

/// Reads the variant's head for review. `Err` carries the tool error saying why it can't be read.
pub async fn review(state: Arc<McpState>, variant: &str) -> Result<Review, Box<CallToolResult>> {
    match tools::review(&ctx(state), variant).await {
        Ok(review) => review.map_err(Box::new),
        Err(Thrown(message)) => Err(Box::new(text_error(&message))),
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

/// The owner's edit on the dashboard. Stored and approved in one step if its base is still approved when
/// it is stored, since the whole difference is then theirs. On a draft it stores a draft, for the review
/// like any other.
/// `args` are `patch_cv`'s, and validated the same.
pub async fn save(state: Arc<McpState>, args: &Value) -> CallToolResult {
    save_as(state, args, false).await
}

/// The owner's edit from a form a conflict moved to a new base, or one that doesn't say its text came
/// from its base: that base then says nothing of the text, so this stores a draft whatever it is.
pub async fn save_draft(state: Arc<McpState>, args: &Value) -> CallToolResult {
    save_as(state, args, true).await
}

async fn save_as(state: Arc<McpState>, args: &Value, draft_only: bool) -> CallToolResult {
    let schema = all_tools()
        .iter()
        .find(|tool| tool["name"] == "patch_cv")
        .map_or(&Value::Null, |tool| &tool["inputSchema"]);
    match zod::parse("patch_cv", schema, Some(args)) {
        Ok(args) => {
            outcome(tools::patch_as(&ctx(state), &args, Some("dashboard"), draft_only).await)
        }
        Err(issues) => text_error(&issues),
    }
}

/// The owner marks an application applied on the dashboard: the lifecycle event, plus `applied_at`
/// when it is empty and `last_contact`. `args` are `record_application_event`'s, and validated the same.
pub async fn mark_applied(state: Arc<McpState>, args: &Value) -> CallToolResult {
    let schema = all_tools()
        .iter()
        .find(|tool| tool["name"] == "record_application_event")
        .map_or(&Value::Null, |tool| &tool["inputSchema"]);
    match zod::parse("record_application_event", schema, Some(args)) {
        Ok(args) => outcome(tools::mark_applied(&ctx(state), &args).await),
        Err(issues) => text_error(&format!("MCP error -32602: {issues}")),
    }
}

/// A revision's picture for the owner: every page, where an agent's call gets three. `render_cv_pdf`'s
/// answer, with the same check and the same stamp on a draft.
pub async fn preview(state: Arc<McpState>, variant: &str, revision: Option<i64>) -> CallToolResult {
    let mut args = json!({"variant": variant, "previewPages": MAX_PAGES});
    if let Some(revision) = revision {
        args["revision"] = revision.into();
    }
    let args = args.as_object().cloned().unwrap_or_default();
    outcome(tools::render_as(&ctx(state), &args, false).await)
}

/// The picture of text the owner has typed and not stored: stamped, checked, and stored nowhere.
pub async fn draw_unsaved(state: Arc<McpState>, variant: &str, source: &str) -> CallToolResult {
    outcome(tools::draw_unsaved(&ctx(state), variant, source).await)
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
