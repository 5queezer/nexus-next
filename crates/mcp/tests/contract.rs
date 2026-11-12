//! The frozen contract, over a real MCP client.

mod common;

use common::*;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

#[test]
fn additive_tools_keep_the_snapshot_style() {
    let additive: Value =
        serde_json::from_str(include_str!("../../../contract/mcp-tools.additive.json")).unwrap();
    let tools = additive["tools"].as_array().unwrap();
    assert_eq!(additive["count"], tools.len());
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(tool["execution"]["taskSupport"], "forbidden");
    }
}

#[tokio::test]
async fn tools_list_is_byte_equal_to_the_contract_files() {
    let server = Server::start().await;
    let snapshot: Value =
        serde_json::from_str(include_str!("../../../contract/mcp-tools.snapshot.json")).unwrap();
    let additive: Value =
        serde_json::from_str(include_str!("../../../contract/mcp-tools.additive.json")).unwrap();
    let frozen = snapshot["tools"].as_array().unwrap();
    let added = additive["tools"].as_array().unwrap();
    for token in [FULL, TOOLS_ONLY] {
        let (status, body) = server
            .post(
                token,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await;
        assert_eq!(status, 200);
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), frozen.len() + added.len());
        let (served_frozen, served_added) = tools.split_at(frozen.len());
        assert_eq!(
            serde_json::to_string(served_frozen).unwrap(),
            serde_json::to_string(frozen).unwrap(),
            "the 35 frozen tools, in order, byte for byte"
        );
        assert_eq!(
            serde_json::to_string(served_added).unwrap(),
            serde_json::to_string(added).unwrap(),
            "then the additive ones"
        );
        assert_eq!(
            body["result"].as_object().unwrap().len(),
            1,
            "no extra result fields"
        );
    }
}

#[tokio::test]
async fn a_real_mcp_client_connects_lists_and_calls() {
    let server = Server::start().await;
    let config = StreamableHttpClientTransportConfig::with_uri(server.url()).auth_header(FULL);
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = ().serve(transport).await.unwrap();

    let info = client.peer_info().unwrap();
    let implementation = info.server_info.as_ref().unwrap();
    assert_eq!(implementation.name, "nexus-crm");
    assert_eq!(implementation.version, "1.0.0");
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .starts_with("Nexus CRM MCP Server")
    );

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 41);
    assert_eq!(tools[0].name, "list_applications");
    assert_eq!(tools[35].name, "get_cv_source");

    let mut args = serde_json::Map::new();
    args.insert("company".to_owned(), "Acme Robotics".into());
    args.insert("role".to_owned(), "Engineer".into());
    let created = client
        .call_tool(CallToolRequestParams::new("create_application").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(created.is_error, None);
    let structured = created.structured_content.unwrap();
    assert_eq!(structured["company"], "Acme Robotics");
    assert_eq!(structured["userId"], OWNER_ID);

    let missing = client
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_router_serves_both_mount_paths() {
    let server = Server::start().await;
    let http = reqwest::Client::new();
    for path in ["/api/mcp", "/mcp"] {
        let response = http
            .post(format!("http://{}{path}", server.addr))
            .bearer_auth(FULL)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
}
