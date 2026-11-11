#![allow(dead_code)]
//! Runs the server on a loopback port with synthetic data, and talks to it over real HTTP.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::Response;
use nexus_core::Db;
use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_mcp::{McpState, Renderer, router};
use serde_json::{Value, json};

pub const OWNER_ID: &str = "owner-synthetic";
pub const OWNER_EMAIL: &str = "owner@example.test";
pub const FULL: &str = "full-token";
pub const TOOLS_ONLY: &str = "tools-token";
pub const SUBMISSIONS_ONLY: &str = "submissions-token";
pub const API_TOKEN: &str = "api-token";

async fn test_auth(mut request: Request, next: Next) -> Response {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let context = match bearer.as_deref() {
        Some(FULL) => Some((AuthType::OAuth, vec![SCOPE_TOOLS, SCOPE_SUBMISSIONS])),
        Some(TOOLS_ONLY) => Some((AuthType::OAuth, vec![SCOPE_TOOLS])),
        Some(SUBMISSIONS_ONLY) => Some((AuthType::OAuth, vec![SCOPE_SUBMISSIONS])),
        Some(API_TOKEN) => Some((AuthType::ApiToken, vec![SCOPE_TOOLS, SCOPE_SUBMISSIONS])),
        _ => None,
    };
    if let Some((auth_type, scopes)) = context {
        request.extensions_mut().insert(AuthContext {
            auth_type,
            scopes: scopes.into_iter().map(str::to_owned).collect(),
        });
    }
    next.run(request).await
}

pub struct Server {
    pub addr: SocketAddr,
    pub db: Db,
    /// The same state the route uses, for what only the owner's paths reach.
    pub state: Arc<McpState>,
    pub dir: tempfile::TempDir,
    http: reqwest::Client,
    token: &'static str,
}

/// One tool result, as a client reads it.
#[derive(Debug)]
pub struct ToolReply {
    pub is_error: bool,
    pub text: String,
    pub structured: Option<Value>,
}

impl ToolReply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }

    pub fn ok(self) -> Value {
        assert!(!self.is_error, "unexpected tool error: {}", self.text);
        self.json()
    }

    pub fn code(&self) -> String {
        assert!(self.is_error, "expected an error: {}", self.text);
        self.json()["error"]["code"]
            .as_str()
            .expect("error code")
            .to_owned()
    }

    pub fn err_text(&self) -> &str {
        assert!(self.is_error, "expected an error: {}", self.text);
        &self.text
    }
}

impl Server {
    pub async fn start() -> Self {
        Self::start_with(Renderer::InProcess).await
    }

    pub async fn start_with(renderer: Renderer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let state = McpState::new(
            db.clone(),
            OWNER_ID.to_owned(),
            OWNER_EMAIL.to_owned(),
            renderer,
        );
        let mcp = router(state.clone());
        let state = Arc::new(state);
        let app = Router::new()
            .nest("/api/mcp", mcp.clone())
            .nest("/mcp", mcp)
            .layer(middleware::from_fn(test_auth));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            db,
            state,
            dir,
            http: reqwest::Client::new(),
            token: FULL,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/api/mcp", self.addr)
    }

    /// A raw JSON-RPC POST: status and parsed body.
    pub async fn post(&self, token: &str, body: &Value) -> (u16, Value) {
        let response = self
            .http
            .post(self.url())
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn call_as(&self, token: &str, name: &str, args: Value) -> ToolReply {
        let (status, body) = self
            .post(
                token,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let result = &body["result"];
        assert!(result.is_object(), "{body}");
        ToolReply {
            is_error: result["isError"] == true,
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            structured: result.get("structuredContent").cloned(),
        }
    }

    pub async fn call(&self, name: &str, args: Value) -> ToolReply {
        self.call_as(self.token, name, args).await
    }

    /// Creates an application and returns its ID.
    pub async fn application(&self, company: &str) -> String {
        let created = self
            .call(
                "create_application",
                json!({"company": company, "role": "Engineer"}),
            )
            .await
            .ok();
        created["id"].as_str().unwrap().to_owned()
    }

    pub async fn pdf(&self, application_ids: Value) -> Value {
        // "%PDF-1.7 synthetic"
        self.call(
            "upload_document_content",
            json!({
                "filename": "cv.pdf",
                "mimeType": "application/pdf",
                "contentBase64": "JVBERi0xLjcgc3ludGhldGlj",
                "applicationIds": application_ids,
            }),
        )
        .await
        .ok()
    }
}

pub fn good_policy() -> Value {
    json!({
        "humanReviewed": true,
        "identityConsistent": true,
        "factsVerified": true,
        "profileConsistencyStatus": "verified",
    })
}

pub fn one_answer() -> Value {
    json!([{"question": "Why here?", "answer": "Because."}])
}
