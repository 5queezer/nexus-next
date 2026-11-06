//! MCP tool server. For now, only the frozen contract it must serve verbatim.

use serde_json::Value;

const SNAPSHOT: &str = include_str!("../../../contract/mcp-tools.snapshot.json");

/// The `tools` array of the frozen contract, with each tool's schema untouched.
pub fn frozen_tools() -> Vec<Value> {
    let mut snapshot: Value = serde_json::from_str(SNAPSHOT).expect("snapshot is valid JSON");
    match snapshot["tools"].take() {
        Value::Array(tools) => tools,
        _ => panic!("snapshot has no tools array"),
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
