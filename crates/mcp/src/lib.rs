//! MCP tool server: the 35 frozen tools of old Nexus over Streamable HTTP.

mod http;
mod js;
mod tools;
mod zod;

use std::sync::{Arc, LazyLock};

use nexus_core::Db;
use nexus_core::auth::AuthContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ProtocolVersion, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

pub use http::router;

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

/// What tool calls need. Responses still carry the owner's `userId`, and events record the owner's
/// email as `actor`, so both stay as they were when old Nexus wrote the imported rows.
#[derive(Clone, Debug)]
pub struct McpState {
    pub db: Db,
    pub owner_id: String,
    pub owner_email: String,
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
        std::borrow::Cow::Borrowed(&[
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
        Ok(tools::call(self.state.clone(), &auth, &request.name, args).await.into())
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
}
