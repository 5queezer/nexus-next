//! MCP tool server over Streamable HTTP: the 35 frozen tools of old Nexus, then the additive M2 ones.

mod http;
mod js;
mod tools;
mod zod;

use std::sync::{Arc, LazyLock};

use nexus_core::Db;
use nexus_core::auth::{AuthContext, AuthType, SCOPE_TOOLS};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ProtocolVersion, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

pub use http::router;
pub use nexus_tailor::Renderer;
pub use rmcp::model::CallToolResult;

/// Runs one tool for a caller inside this process: the same validation, handlers and error codes
/// as the HTTP route. `source` is who is calling (`mcp` on the route, `dashboard` for a person's click) and is stored on every event the call writes.
/// `scopes` are the grants the caller holds, so a caller without `mcp:tools`
/// is refused and one without `mcp:submissions` can't reach submission data.
pub async fn call_tool(
    state: Arc<McpState>,
    scopes: &[&str],
    source: &'static str,
    name: &str,
    args: Value,
) -> CallToolResult {
    let auth = AuthContext {
        auth_type: AuthType::OAuth,
        scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
    };
    if !auth.has_scope(SCOPE_TOOLS) {
        return tools::code_error("insufficient_scope");
    }
    tools::call(
        state,
        &auth,
        source,
        name,
        Some(args),
        CancellationToken::new(),
    )
    .await
}

const SNAPSHOT: &str = include_str!("../../../contract/mcp-tools.snapshot.json");

static FROZEN_TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    let mut snapshot: Value = serde_json::from_str(SNAPSHOT).expect("snapshot is valid JSON");
    match snapshot["tools"].take() {
        Value::Array(tools) => tools,
        _ => panic!("snapshot has no tools array"),
    }
});

/// The `tools` array of the frozen contract, with each tool's schema untouched.
pub fn frozen_tools() -> &'static [Value] {
    &FROZEN_TOOLS
}

const ADDITIVE: &str = include_str!("../../../contract/mcp-tools.additive.json");

static ALL_TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    let mut additive: Value = serde_json::from_str(ADDITIVE).expect("additive is valid JSON");
    let Value::Array(additive) = additive["tools"].take() else {
        panic!("additive has no tools array");
    };
    frozen_tools().iter().cloned().chain(additive).collect()
});

/// What `tools/list` serves: the frozen tools in order, then the additive ones, schemas untouched.
pub fn all_tools() -> &'static [Value] {
    &ALL_TOOLS
}

/// A revision that has just been committed. Live pages hear of it from whichever writer stored it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CvRevisionEvent {
    pub variant: String,
    pub revision: i64,
    pub origin: String,
    pub summary: String,
    pub review_tokens: Vec<String>,
}

/// A page that misses more than this reads the head again.
const CV_EVENT_BACKLOG: usize = 16;

/// What tool calls need. Responses still carry the owner's `userId`, and events record the owner's
/// email as `actor`, so both stay as they were when old Nexus wrote the imported rows.
#[derive(Clone, Debug)]
pub struct McpState {
    pub db: Db,
    pub owner_id: String,
    pub owner_email: String,
    pub renderer: Renderer,
    /// One compile at a time. Taken only around the compile itself, never across other work.
    render_slot: Arc<Semaphore>,
    /// The compile and the calls allowed to wait for it. Each holds a slot of the MCP pool meanwhile,
    /// so any more are refused at once.
    render_calls: Arc<Semaphore>,
    /// Sent after commit by every path that stores a revision. No receiver is fine.
    pub cv_events: broadcast::Sender<CvRevisionEvent>,
}

impl McpState {
    pub fn new(db: Db, owner_id: String, owner_email: String, renderer: Renderer) -> Self {
        Self {
            db,
            owner_id,
            owner_email,
            renderer,
            render_slot: Arc::new(Semaphore::new(1)),
            render_calls: Arc::new(Semaphore::new(tools::MAX_RENDER_CALLS)),
            cv_events: broadcast::channel(CV_EVENT_BACKLOG).0,
        }
    }
}

#[derive(Clone)]
struct Handler {
    state: Arc<McpState>,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .with_server_info(Implementation::new("nexus-crm", "1.0.0"))
        .with_instructions(
            "Nexus CRM MCP Server – manage job applications, contacts, and documents. All operations are scoped to the authenticated user.",
        )
    }

    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Owned(vec![
            serde_json::from_value(Value::from("2024-10-07")).expect("a version string"),
            ProtocolVersion::V_2024_11_05,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ])
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let auth = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<AuthContext>())
            .cloned()
            .ok_or_else(|| ErrorData::internal_error("missing auth context", None))?;
        let args = request.arguments.map(Value::Object);
        // rmcp runs this in a task of its own, so a lost connection doesn't drop it: it only cancels
        // this token.
        Ok(tools::call(
            self.state.clone(),
            &auth,
            "mcp",
            &request.name,
            args,
            context.ct,
        )
        .await
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_has_exactly_35_named_tools_with_schemas() {
        let tools = frozen_tools();
        assert_eq!(tools.len(), 35);
        let names: std::collections::BTreeSet<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        assert_eq!(names.len(), 35);
        assert!(tools.iter().all(|tool| tool["inputSchema"].is_object()));
    }

    #[test]
    fn additive_tools_follow_the_frozen_ones_in_order() {
        let tools = all_tools();
        assert_eq!(&tools[..35], frozen_tools());
        let added: Vec<_> = tools[35..].iter().map(|tool| &tool["name"]).collect();
        assert_eq!(
            added,
            [
                "get_cv_source",
                "patch_cv",
                "render_cv_pdf",
                "list_cv_revisions",
                "revert_cv"
            ]
        );
    }
}
