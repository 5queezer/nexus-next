//! A bare MCP server for parity runs and manual testing, with fixed bearer tokens in place of the
//! real auth layer. Configuration comes from the environment, so nothing private is on a command
//! line: NEXUS_DATA_DIR, NEXUS_OWNER_ID, NEXUS_OWNER_EMAIL, MCP_PORT, MCP_TOKEN_FULL,
//! MCP_TOKEN_TOOLS.

use std::env::var;
use std::path::PathBuf;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONNECTION};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use nexus_core::Db;
use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_mcp::{McpState, router};

#[derive(Clone)]
struct Tokens {
    full: String,
    tools: String,
}

async fn bearer(State(tokens): State<Tokens>, mut request: Request, next: Next) -> Response {
    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let scopes = if token == tokens.full {
        vec![SCOPE_TOOLS.to_owned(), SCOPE_SUBMISSIONS.to_owned()]
    } else if token == tokens.tools {
        vec![SCOPE_TOOLS.to_owned()]
    } else {
        return (
            StatusCode::UNAUTHORIZED,
            [(CONNECTION, "close")],
            "{\"error\":\"Unauthorized\"}",
        )
            .into_response();
    };
    request.extensions_mut().insert(AuthContext {
        auth_type: AuthType::OAuth,
        scopes,
    });
    let mut response = next.run(request).await;
    // A parity reset restarts this server, and a client must not reuse a socket across that.
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = Db::open(&PathBuf::from(var("NEXUS_DATA_DIR")?)).await?;
    let state = McpState {
        db,
        owner_id: var("NEXUS_OWNER_ID")?,
        owner_email: var("NEXUS_OWNER_EMAIL")?,
    };
    let tokens = Tokens {
        full: var("MCP_TOKEN_FULL")?,
        tools: var("MCP_TOKEN_TOOLS")?,
    };
    let mcp = router(state);
    let app = Router::new()
        .nest("/api/mcp", mcp.clone())
        .nest("/mcp", mcp)
        .layer(middleware::from_fn_with_state(tokens, bearer));
    let listener =
        tokio::net::TcpListener::bind(("127.0.0.1", var("MCP_PORT")?.parse::<u16>()?)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
