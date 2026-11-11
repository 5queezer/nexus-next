//! MCP through the whole server stack: real auth layer, limits and paths, over a real socket.

use std::net::SocketAddr;
use std::time::Duration;

use nexus_auth::{AuthConfig, issue_api_token};
use nexus_core::Db;
use nexus_core::blob::sha256_hex;
use nexus_server::config::{Access, ServeConfig};
use nexus_server::serve::{self, MAX_BODY_BYTES};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const FULL: &str = "mcp_at_full_token";
const TOOLS_ONLY: &str = "mcp_at_tools_only_token";

struct Running {
    addr: SocketAddr,
    db: Db,
    _dir: tempfile::TempDir,
}

async fn start(access: Access) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let config = ServeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_path_buf(),
        access,
        owner_id: Some("owner-1".to_owned()),
        owner_email: Some("owner@example.net".to_owned()),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: None,
        log_level: tracing::Level::INFO,
        voice: Default::default(),
        renderer: nexus_mcp::Renderer::current_exe(),
        stream_shutdown: tokio::sync::watch::Sender::new(false),
    };
    let app = serve::build_app(&db, &config).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve::serve(
        listener,
        app,
        std::future::pending(),
        Duration::from_secs(1),
    ));
    for (token, scopes) in [
        (FULL, r#"["mcp:tools","mcp:submissions"]"#),
        (TOOLS_ONLY, r#"["mcp:tools"]"#),
    ] {
        sqlx::query(
            "INSERT INTO oauth_access_tokens
                 (token_hash, client_id, scopes, sensitive_consent_version, expires_at)
             VALUES (?, 'client', ?, 1, '2999-01-01T00:00:00.000Z')",
        )
        .bind(sha256_hex(token.as_bytes()))
        .bind(scopes)
        .execute(db.pool())
        .await
        .unwrap();
    }
    Running {
        addr,
        db,
        _dir: dir,
    }
}

fn owner() -> Access {
    Access::Owner(AuthConfig {
        public_base_url: "https://crm.example.test".to_owned(),
        owner_passphrase_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA"
            .to_owned(),
        owner_passphrase_file: None,
        trusted_proxy: None,
        redirect_hosts: Vec::new(),
    })
}

fn post(
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    body: String,
) -> reqwest::RequestBuilder {
    let mut request = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request
}

fn tool(name: &str, arguments: Value) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
        .to_string()
}

async fn result(response: reqwest::Response) -> Value {
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    body["result"].clone()
}

fn config_with(
    owner_id: Option<&str>,
    owner_email: Option<&str>,
    dir: &std::path::Path,
) -> ServeConfig {
    ServeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.to_path_buf(),
        access: Access::InsecureLocalOnly,
        owner_id: owner_id.map(str::to_owned),
        owner_email: owner_email.map(str::to_owned),
        timezone: nexus_web::Tz::Europe__Madrid,
        calendar_token: None,
        log_level: tracing::Level::INFO,
        voice: Default::default(),
        renderer: nexus_mcp::Renderer::current_exe(),
        stream_shutdown: tokio::sync::watch::Sender::new(false),
    }
}

#[tokio::test]
async fn the_imported_owner_wins_and_configuration_covers_a_fresh_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();

    let mut missing = config_with(None, Some("c@example.net"), dir.path());
    let error = serve::resolve_owner(&db, &mut missing).await.unwrap_err();
    assert!(error.to_string().contains("NEXUS_OWNER_ID"), "{error}");
    let mut missing = config_with(Some("c"), None, dir.path());
    let error = serve::resolve_owner(&db, &mut missing).await.unwrap_err();
    assert!(error.to_string().contains("NEXUS_OWNER_EMAIL"), "{error}");

    let mut fresh = config_with(Some("c"), Some("c@example.net"), dir.path());
    serve::resolve_owner(&db, &mut fresh).await.unwrap();
    assert_eq!(fresh.owner_id.as_deref(), Some("c"));

    // An import with no MCP events: the ID comes from it, the email from configuration.
    sqlx::query("INSERT INTO import_meta (id, manifest_sha256, fingerprint, owner_id) VALUES (1, 'm', 'f', 'imported')")
        .execute(db.pool())
        .await
        .unwrap();
    let mut partial = config_with(Some("c"), Some("c@example.net"), dir.path());
    serve::resolve_owner(&db, &mut partial).await.unwrap();
    assert_eq!(partial.owner_id.as_deref(), Some("imported"));
    assert_eq!(partial.owner_email.as_deref(), Some("c@example.net"));

    sqlx::query("UPDATE import_meta SET owner_email = 'imported@example.net'")
        .execute(db.pool())
        .await
        .unwrap();
    let mut imported = config_with(None, None, dir.path());
    serve::resolve_owner(&db, &mut imported).await.unwrap();
    assert_eq!(
        imported.owner_email.as_deref(),
        Some("imported@example.net")
    );
}

#[tokio::test]
async fn both_paths_require_the_bearer_and_answer_with_it() {
    let server = start(owner()).await;
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    for path in ["/api/mcp", "/mcp"] {
        for token in [None, Some("mcp_at_unknown")] {
            let response = post(server.addr, path, token, list.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401, "{path}");
            let challenge = response.headers()["www-authenticate"].to_str().unwrap();
            assert!(challenge.contains("resource_metadata="), "{challenge}");
        }
        let tools = result(
            post(server.addr, path, Some(FULL), list.clone())
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(tools["tools"].as_array().unwrap().len(), 40, "{path}");
    }
}

#[tokio::test]
async fn the_auth_routes_sit_beside_the_mcp_routes() {
    let server = start(owner()).await;
    let http = reqwest::Client::new();
    let metadata = http
        .get(format!(
            "http://{}/.well-known/oauth-authorization-server",
            server.addr
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), 200);
    let register = post(
        server.addr,
        "/api/mcp/register",
        None,
        json!({"redirect_uris": ["http://127.0.0.1:39123/callback"]}).to_string(),
    )
    .send()
    .await
    .unwrap();
    assert!(register.status().is_success(), "{}", register.status());
}

#[tokio::test]
async fn scopes_come_from_the_real_tokens() {
    let server = start(owner()).await;
    let check = tool("pipeline_healthcheck", json!({}));
    let full = result(
        post(server.addr, "/api/mcp", Some(FULL), check.clone())
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(full["structuredContent"]["healthy"], true);
    let limited = result(
        post(server.addr, "/api/mcp", Some(TOOLS_ONLY), check.clone())
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(limited["isError"], true);
    assert_eq!(
        limited["structuredContent"]["error"]["code"],
        "insufficient_scope"
    );
    let api_token = issue_api_token(&server.db, "test").await.unwrap();
    let api = result(
        post(server.addr, "/api/mcp", Some(&api_token), check)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(api["structuredContent"]["healthy"], true);
}

#[tokio::test]
async fn a_local_run_needs_no_token_and_keeps_the_owner() {
    let server = start(Access::InsecureLocalOnly).await;
    let created = result(
        post(
            server.addr,
            "/mcp",
            None,
            tool(
                "create_application",
                json!({"company": "Acme", "role": "Eng"}),
            ),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(created["structuredContent"]["userId"], "owner-1");
    let id = created["structuredContent"]["id"].as_str().unwrap();
    let event = result(
        post(
            server.addr,
            "/api/mcp",
            None,
            tool(
                "record_application_event",
                json!({"applicationId": id, "type": "note_added", "metadata": {"note": "n"}}),
            ),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        event["structuredContent"]["event"]["actor"],
        "owner@example.net"
    );
}

#[tokio::test]
async fn the_largest_upload_fits_and_anything_over_the_limit_gets_413() {
    let server = start(owner()).await;
    // The longest contentBase64 the schema accepts, with the longest filename around it.
    let content = "A".repeat(13_981_020);
    let upload = tool(
        "upload_document_content",
        json!({"filename": "n".repeat(255), "mimeType": "application/pdf", "contentBase64": content}),
    );
    assert!(upload.len() < MAX_BODY_BYTES);
    let reply = result(
        post(server.addr, "/api/mcp", Some(FULL), upload)
            .send()
            .await
            .unwrap(),
    )
    .await;
    // Past the transport, so the tool itself reports the size.
    assert_eq!(reply["isError"], true);
    assert!(
        reply["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("File too large")
    );

    let too_big = post(
        server.addr,
        "/api/mcp",
        Some(FULL),
        "x".repeat(MAX_BODY_BYTES + 1),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(too_big.status(), 413);
}

#[tokio::test]
async fn get_streams_end_at_once_and_free_their_slots() {
    let server = start(owner()).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/api/mcp", server.addr);
    let gets = (0..serve::MCP.in_flight * 8).map(|_| {
        let request = client
            .get(&url)
            .bearer_auth(FULL)
            .header("accept", "text/event-stream")
            .send();
        tokio::spawn(async move {
            let response = request.await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            assert_eq!(response.text().await.unwrap(), "");
        })
    });
    let all = async {
        for get in gets.collect::<Vec<_>>() {
            get.await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), all)
        .await
        .unwrap();
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string();
    assert_eq!(
        post(server.addr, "/api/mcp", Some(FULL), ping)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}
