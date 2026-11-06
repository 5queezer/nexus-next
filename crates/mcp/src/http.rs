//! The HTTP surface of old Nexus's stateless, JSON-response TS SDK transport: its header checks,
//! JSON-RPC error bodies, batches, GET and DELETE. Single requests then go to rmcp's stateless
//! service, except `tools/list`: rmcp's `Tool` has no `execution` field, so the frozen tools are
//! served as they are.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{ACCEPT, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::get;
use nexus_core::auth::AuthContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};

use crate::{Handler, McpState, frozen_tools};

/// Old Nexus had no body limit. This keeps the largest legal upload (about 14 MB of base64) well
/// inside it while bounding what one request can make the board hold in memory.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

type Rmcp = StreamableHttpService<Handler, LocalSessionManager>;

/// Serves the MCP endpoint at this router's root. Requests must carry an `AuthContext` extension,
/// which the auth layer sets.
pub fn router(state: McpState) -> Router {
    let state = Arc::new(state);
    let handler = Handler { state };
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(MAX_BODY_BYTES)
        .disable_allowed_hosts();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .route("/", get(handle_get).post(handle_post).delete(handle_delete))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(service)
}

fn json_response(status: StatusCode, body: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("valid response")
}

fn error_response(status: StatusCode, code: i64, message: &str) -> Response<Body> {
    json_response(
        status,
        &json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": null}),
    )
}

fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("valid response")
}

fn header<'a>(headers: &'a HeaderMap, name: impl axum::http::header::AsHeaderName) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn protocol_error(headers: &HeaderMap) -> Option<Response<Body>> {
    let version = headers.get("mcp-protocol-version")?.to_str().unwrap_or("");
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return None;
    }
    let message = format!(
        "Bad Request: Unsupported protocol version: {version} (supported versions: {})",
        SUPPORTED_PROTOCOL_VERSIONS.join(", ")
    );
    Some(error_response(StatusCode::BAD_REQUEST, -32000, &message))
}

fn unauthenticated(request: &Request) -> Option<Response<Body>> {
    if request.extensions().get::<AuthContext>().is_some() {
        return None;
    }
    Some(json_response(
        StatusCode::UNAUTHORIZED,
        &json!({"error": "Unauthorized – provide Bearer token"}),
    ))
}

/// Old Nexus closed the transport as soon as the handler returned, so a GET gets an event stream
/// that ends at once.
async fn handle_get(request: Request) -> Response<Body> {
    if let Some(response) = unauthenticated(&request) {
        return response;
    }
    if !header(request.headers(), ACCEPT).contains("text/event-stream") {
        return error_response(
            StatusCode::NOT_ACCEPTABLE,
            -32000,
            "Not Acceptable: Client must accept text/event-stream",
        );
    }
    if let Some(response) = protocol_error(request.headers()) {
        return response;
    }
    Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache, no-transform")
        .header(CONNECTION, "keep-alive")
        .body(Body::empty())
        .expect("valid response")
}

async fn handle_delete(request: Request) -> Response<Body> {
    if let Some(response) = unauthenticated(&request) {
        return response;
    }
    protocol_error(request.headers()).unwrap_or_else(|| empty(StatusCode::OK))
}

fn is_id(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(_)) => true,
        Some(Value::Number(n)) => n.is_i64() || n.is_u64(),
        _ => false,
    }
}

/// Port of the SDK's strict `JSONRPCMessageSchema`: no extra top-level keys.
fn is_jsonrpc_message(message: &Value) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    let has_only = |allowed: &[&str]| object.keys().all(|key| allowed.contains(&key.as_str()));
    let params_ok = object.get("params").is_none_or(Value::is_object);
    if object.get("jsonrpc") != Some(&json!("2.0")) {
        return false;
    }
    if object.get("method").is_some_and(Value::is_string) {
        return params_ok
            && if object.contains_key("id") {
                is_id(object.get("id")) && has_only(&["jsonrpc", "id", "method", "params"])
            } else {
                has_only(&["jsonrpc", "method", "params"])
            };
    }
    if object.get("result").is_some_and(Value::is_object) {
        return is_id(object.get("id")) && has_only(&["jsonrpc", "id", "result"]);
    }
    object.get("error").is_some_and(Value::is_object)
        && has_only(&["jsonrpc", "id", "error"])
        && object.get("id").is_none_or(|id| is_id(Some(id)))
}

fn method(message: &Value) -> Option<&str> {
    message.get("method")?.as_str()
}

fn is_request(message: &Value) -> bool {
    method(message).is_some() && message.get("id").is_some()
}

async fn answer(service: &Rmcp, parts: &axum::http::request::Parts, message: Value) -> Result<Value, Response<Body>> {
    let id = message["id"].clone();
    match method(&message) {
        Some("tools/list") => {
            Ok(json!({"result": {"tools": frozen_tools()}, "jsonrpc": "2.0", "id": id}))
        }
        Some("initialize" | "ping" | "tools/call") => {
            let mut inner = Request::post("/")
                .header(HOST, "localhost")
                .header(ACCEPT, "application/json, text/event-stream")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(message.to_string()))
                .expect("valid request");
            *inner.extensions_mut() = parts.extensions.clone();
            let response = service.handle(inner).await;
            let status = response.status();
            let body = to_bytes(Body::new(response.into_body()), MAX_BODY_BYTES)
                .await
                .unwrap_or_else(|_| Bytes::new());
            match serde_json::from_slice(&body) {
                Ok(value) => Ok(value),
                Err(_) => Err(Response::builder()
                    .status(status)
                    .body(Body::from(body))
                    .expect("valid response")),
            }
        }
        _ => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "Method not found"},
        })),
    }
}

async fn handle_post(State(service): State<Rmcp>, request: Request) -> Response<Body> {
    if let Some(response) = unauthenticated(&request) {
        return response;
    }
    let (parts, body) = request.into_parts();
    let accept = header(&parts.headers, ACCEPT);
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return error_response(
            StatusCode::NOT_ACCEPTABLE,
            -32000,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        );
    }
    if !header(&parts.headers, CONTENT_TYPE).contains("application/json") {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
        );
    }
    let parsed = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes),
        Err(_) => return empty(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let Ok(parsed) = parsed else {
        return error_response(StatusCode::BAD_REQUEST, -32700, "Parse error: Invalid JSON");
    };
    let messages = match parsed {
        Value::Array(messages) => messages,
        message => vec![message],
    };
    if !messages.iter().all(is_jsonrpc_message) {
        return error_response(
            StatusCode::BAD_REQUEST,
            -32700,
            "Parse error: Invalid JSON-RPC message",
        );
    }
    let is_initialize = messages
        .iter()
        .any(|message| is_request(message) && method(message) == Some("initialize"));
    if is_initialize && messages.len() > 1 {
        return error_response(
            StatusCode::BAD_REQUEST,
            -32600,
            "Invalid Request: Only one initialization request is allowed",
        );
    }
    if !is_initialize && let Some(response) = protocol_error(&parts.headers) {
        return response;
    }
    let requests: Vec<Value> = messages.into_iter().filter(is_request).collect();
    if requests.is_empty() {
        return empty(StatusCode::ACCEPTED);
    }
    let mut responses = Vec::with_capacity(requests.len());
    for message in requests {
        match answer(&service, &parts, message).await {
            Ok(value) => responses.push(value),
            Err(response) => return response,
        }
    }
    if responses.len() == 1 {
        json_response(StatusCode::OK, &responses[0])
    } else {
        json_response(StatusCode::OK, &Value::Array(responses))
    }
}
