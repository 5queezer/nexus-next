//! HTTP behaviour pinned from a differential run against the real TS SDK transport, wired as old
//! Nexus's route did: stateless, with JSON responses.

mod common;

use common::*;
use serde_json::{Value, json};

struct Reply {
    status: u16,
    content_type: Option<String>,
    session: Option<String>,
    body: Value,
    raw: String,
}

async fn send(
    server: &Server,
    method: reqwest::Method,
    accept: Option<&str>,
    content_type: Option<&str>,
    version: Option<&str>,
    body: Option<String>,
) -> Reply {
    let mut request = reqwest::Client::new()
        .request(method, server.url())
        .bearer_auth(FULL);
    if let Some(accept) = accept {
        request = request.header("accept", accept);
    }
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    if let Some(version) = version {
        request = request.header("mcp-protocol-version", version);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send().await.unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    let (status, content_type, session) = (
        response.status().as_u16(),
        header("content-type"),
        header("mcp-session-id"),
    );
    let raw = response.text().await.unwrap();
    Reply {
        status,
        content_type,
        session,
        body: serde_json::from_str(&raw).unwrap_or(Value::Null),
        raw,
    }
}

const BOTH: &str = "application/json, text/event-stream";
const JSON: &str = "application/json";

async fn post(server: &Server, body: Value) -> Reply {
    send(
        server,
        reqwest::Method::POST,
        Some(BOTH),
        Some(JSON),
        None,
        Some(body.to_string()),
    )
    .await
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": null})
}

fn initialize(version: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"},
    }})
}

#[tokio::test]
async fn initialize_is_stateless_and_negotiates_like_the_sdk() {
    let server = Server::start().await;
    for (asked, answered) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-11-25", "2025-11-25"),
        ("2024-11-05", "2024-11-05"),
        ("2024-10-07", "2024-10-07"),
        ("1999-01-01", "2025-11-25"),
        ("2026-07-28", "2025-11-25"),
    ] {
        let reply = post(&server, initialize(asked)).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type.as_deref(), Some(JSON));
        assert_eq!(reply.session, None, "no session in stateless mode");
        let result = &reply.body["result"];
        assert_eq!(result["protocolVersion"], answered, "{asked}");
        assert_eq!(
            result["capabilities"],
            json!({"tools": {"listChanged": true}})
        );
        assert_eq!(
            result["serverInfo"],
            json!({"name": "nexus-crm", "version": "1.0.0"})
        );
        assert_eq!(
            result["instructions"],
            "Nexus CRM MCP Server – manage job applications, contacts, and documents. All operations are scoped to the authenticated user."
        );
    }
}

#[tokio::test]
async fn header_checks_answer_with_jsonrpc_errors() {
    let server = Server::start().await;
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string();
    for accept in [Some(JSON), Some("text/event-stream"), Some("*/*"), None] {
        let reply = send(
            &server,
            reqwest::Method::POST,
            accept,
            Some(JSON),
            None,
            Some(ping.clone()),
        )
        .await;
        assert_eq!(reply.status, 406);
        assert_eq!(
            reply.body,
            rpc_error(
                -32000,
                "Not Acceptable: Client must accept both application/json and text/event-stream"
            )
        );
    }
    for content_type in [Some("text/plain"), None] {
        let reply = send(
            &server,
            reqwest::Method::POST,
            Some(BOTH),
            content_type,
            None,
            Some(ping.clone()),
        )
        .await;
        assert_eq!(reply.status, 415);
        assert_eq!(
            reply.body,
            rpc_error(
                -32000,
                "Unsupported Media Type: Content-Type must be application/json"
            )
        );
    }
    let reply = send(
        &server,
        reqwest::Method::POST,
        Some(BOTH),
        Some("application/json; charset=utf-8"),
        Some("2024-10-07"),
        Some(ping.clone()),
    )
    .await;
    assert_eq!(reply.status, 200);
    let reply = send(
        &server,
        reqwest::Method::POST,
        Some(BOTH),
        Some(JSON),
        Some("2026-07-28"),
        Some(ping),
    )
    .await;
    assert_eq!(reply.status, 400);
    assert_eq!(
        reply.body["error"]["message"],
        "Bad Request: Unsupported protocol version: 2026-07-28 (supported versions: 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05, 2024-10-07)"
    );
    // A pinned version on initialize itself isn't checked.
    let reply = send(
        &server,
        reqwest::Method::POST,
        Some(BOTH),
        Some(JSON),
        Some("1999-01-01"),
        Some(initialize("2025-06-18").to_string()),
    )
    .await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn bad_bodies_answer_with_parse_errors() {
    let server = Server::start().await;
    for body in ["{oops", ""] {
        let reply = send(
            &server,
            reqwest::Method::POST,
            Some(BOTH),
            Some(JSON),
            None,
            Some(body.to_owned()),
        )
        .await;
        assert_eq!(reply.status, 400);
        assert_eq!(reply.body, rpc_error(-32700, "Parse error: Invalid JSON"));
    }
    for body in [
        json!(5),
        json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "extra": 1}),
        json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": []}),
        json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}, {"x": 1}]),
    ] {
        let reply = post(&server, body.clone()).await;
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(
            reply.body,
            rpc_error(-32700, "Parse error: Invalid JSON-RPC message"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn notifications_and_batches_behave_like_the_sdk() {
    let server = Server::start().await;
    for body in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 9, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 9, "error": {"code": 1, "message": "x"}}),
        json!([]),
    ] {
        let reply = post(&server, body).await;
        assert_eq!(reply.status, 202);
        assert_eq!(reply.raw, "");
    }

    let ping = |id: i64| json!({"jsonrpc": "2.0", "id": id, "method": "ping"});
    let batch = post(
        &server,
        json!([ping(7), {"jsonrpc": "2.0", "method": "notifications/x"}, ping(8)]),
    )
    .await;
    assert_eq!(batch.status, 200);
    let ids: Vec<_> = batch
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, [json!(7), json!(8)]);

    // One request in a batch comes back as a bare object.
    let single = post(&server, json!([ping(7)])).await;
    assert_eq!(single.body["id"], 7);
    assert!(single.body.is_object());

    let init_batch = post(&server, json!([initialize("2025-06-18"), {"jsonrpc": "2.0", "method": "notifications/initialized"}])).await;
    assert_eq!(init_batch.status, 400);
    assert_eq!(
        init_batch.body,
        rpc_error(
            -32600,
            "Invalid Request: Only one initialization request is allowed"
        )
    );
}

#[tokio::test]
async fn unknown_methods_and_malformed_params_are_jsonrpc_errors() {
    let server = Server::start().await;
    for method in [
        "nope/nope",
        "resources/list",
        "prompts/list",
        "logging/setLevel",
    ] {
        let reply = post(
            &server,
            json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": {}}),
        )
        .await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.body["error"],
            json!({"code": -32601, "message": "Method not found"}),
            "{method}"
        );
    }
    let call = |params: Value| json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": params});
    let message = |path: Value, expected: &str, received: &str| {
        serde_json::to_string_pretty(&json!([{
            "expected": expected,
            "code": "invalid_type",
            "path": path,
            "message": format!("Invalid input: expected {expected}, received {received}"),
        }]))
        .unwrap()
    };
    for (body, text) in [
        (
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call"}),
            message(json!(["params"]), "object", "undefined"),
        ),
        (
            call(json!({"arguments": {}})),
            message(json!(["params", "name"]), "string", "undefined"),
        ),
        (
            call(json!({"name": 5})),
            message(json!(["params", "name"]), "string", "number"),
        ),
        (
            call(json!({"name": "get_application", "arguments": "x"})),
            message(json!(["params", "arguments"]), "record", "string"),
        ),
        (
            call(json!({"name": "get_application", "arguments": null})),
            message(json!(["params", "arguments"]), "record", "null"),
        ),
        (
            call(json!({"name": "get_application", "arguments": []})),
            message(json!(["params", "arguments"]), "record", "array"),
        ),
        (
            json!({"jsonrpc": "2.0", "id": 2, "method": "initialize"}),
            message(json!(["params"]), "object", "undefined"),
        ),
        (
            call(
                json!({"name": "get_application", "arguments": {"id": "1"}, "task": {"ttl": 1000}}),
            ),
            "Server does not support task creation (required for tools/call)".to_owned(),
        ),
    ] {
        let reply = post(&server, body.clone()).await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.body["error"],
            json!({"code": -32603, "message": text}),
            "{body}"
        );
    }
}

#[tokio::test]
async fn get_and_delete_are_accepted_and_end_at_once() {
    let server = Server::start().await;
    let reply = send(
        &server,
        reqwest::Method::GET,
        Some("text/event-stream"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.content_type.as_deref(), Some("text/event-stream"));
    assert_eq!(reply.raw, "");
    let reply = send(&server, reqwest::Method::GET, Some(JSON), None, None, None).await;
    assert_eq!(reply.status, 406);
    assert_eq!(
        reply.body,
        rpc_error(
            -32000,
            "Not Acceptable: Client must accept text/event-stream"
        )
    );
    let reply = send(
        &server,
        reqwest::Method::DELETE,
        Some(BOTH),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.raw, "");
    let reply = send(
        &server,
        reqwest::Method::DELETE,
        Some(BOTH),
        None,
        Some("nope"),
        None,
    )
    .await;
    assert_eq!(reply.status, 400);
    let reply = send(&server, reqwest::Method::OPTIONS, None, None, None, None).await;
    assert_eq!(reply.status, 204);
}

#[tokio::test]
async fn requests_without_an_auth_context_are_refused() {
    let server = Server::start().await;
    let response = reqwest::Client::new()
        .post(server.url())
        .header("accept", BOTH)
        .header("content-type", JSON)
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_full_size_upload_fits_the_body_limit() {
    let server = Server::start().await;
    // 10 MiB of PDF-prefixed bytes, base64 encoded: the largest upload old Nexus accepts.
    let mut bytes = b"%PDF-".to_vec();
    bytes.resize(10 * 1024 * 1024, b'x');
    use base64::Engine;
    let content = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let reply = server
        .call(
            "upload_document_content",
            json!({"filename": "big.pdf", "mimeType": "application/pdf", "contentBase64": content}),
        )
        .await
        .ok();
    assert_eq!(reply["size"], 10 * 1024 * 1024);
    let id = reply["id"].as_str().unwrap();
    let download = server
        .call("download_document_content", json!({"id": id}))
        .await
        .ok();
    assert_eq!(download["contentBase64"], content);

    bytes.push(b'x');
    let too_big = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let reply = server
        .call(
            "upload_document_content",
            json!({"filename": "big.pdf", "mimeType": "application/pdf", "contentBase64": too_big}),
        )
        .await;
    assert_eq!(reply.err_text(), "File too large (max 10 MB)");
}
