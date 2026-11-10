#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::header::{CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use http_body_util::BodyExt;
use nexus_auth::{AuthConfig, AuthState, TrustedProxy, require_mcp_auth, require_owner_session};
use nexus_core::auth::{AuthContext, AuthType};
use nexus_core::db::Db;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use tower::ServiceExt;
use url::Url;
use url::form_urlencoded::{byte_serialize, parse};

pub const BASE: &str = "https://crm.example.test";
pub const PASSPHRASE: &str = "correct horse battery staple";
pub const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
pub const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
pub const REDIRECT: &str = "https://client.example.test/callback";
pub const ALLOWED_HOSTS: [&str; 3] = ["client.example.test", "ok.example.test", "a.example.test"];
pub const DEFAULT_IP: &str = "198.51.100.1";

pub fn encode(value: &str) -> String {
    byte_serialize(value.as_bytes()).collect()
}

/// Cheap parameters: these tests verify the flow, not argon2's cost.
pub fn weak_hash() -> String {
    let params = Params::new(8, 1, 1, None).unwrap();
    let salt = SaltString::encode_b64(b"synthetic-salt").unwrap();
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(PASSPHRASE.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

async fn echo_auth(Extension(context): Extension<AuthContext>) -> impl IntoResponse {
    let auth_type = match context.auth_type {
        AuthType::OAuth => "oauth",
        AuthType::ApiToken => "api_token",
    };
    Json(json!({ "authType": auth_type, "scopes": context.scopes }))
}

pub struct Harness {
    pub app: Router,
    pub db: Db,
    pub dir: TempDir,
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_str()
            .unwrap()
    }

    pub fn location(&self) -> &str {
        self.header(LOCATION.as_str())
    }

    /// The value of a hidden form field. Panics if the page has none.
    pub fn field(&self, name: &str) -> String {
        self.try_field(name)
            .unwrap_or_else(|| panic!("no {name} field in {}", self.body))
    }

    pub fn try_field(&self, name: &str) -> Option<String> {
        let marker = format!(r#"name="{name}" value=""#);
        let start = self.body.find(&marker)? + marker.len();
        let end = self.body[start..].find('"')? + start;
        Some(
            self.body[start..end]
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        )
    }

    /// The error code of an OAuth error reply, after checking its status.
    #[track_caller]
    pub fn assert_error(&self, status: StatusCode, error: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.json()["error"], error, "{}", self.body);
    }
}

pub fn with_ip(mut request: Request<Body>, ip: &str) -> Request<Body> {
    let addr: SocketAddr = format!("{}:443", bracket(ip)).parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(addr));
    request
}

fn bracket(ip: &str) -> String {
    if ip.contains(':') {
        format!("[{ip}]")
    } else {
        ip.to_owned()
    }
}

pub fn get_req(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

pub fn form_req(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

pub fn json_req(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn bearer_req(token: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_proxy(None).await
    }

    pub async fn with_proxy(trusted_proxy: Option<TrustedProxy>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let state = AuthState::new(
            db.clone(),
            AuthConfig {
                public_base_url: BASE.to_owned(),
                owner_passphrase_hash: weak_hash(),
                owner_passphrase_file: Some(dir.path().join("owner-passphrase.argon2")),
                trusted_proxy,
                redirect_hosts: ALLOWED_HOSTS.map(str::to_owned).to_vec(),
            },
        )
        .unwrap();
        let app = nexus_auth::router(state.clone())
            .route(
                "/api/mcp",
                any(echo_auth).layer(from_fn_with_state(state.clone(), require_mcp_auth)),
            )
            .route(
                "/dashboard",
                get(|| async { "dashboard" })
                    .post(|| async { "dashboard" })
                    .layer(from_fn_with_state(state.clone(), require_owner_session)),
            )
            .route(
                "/api/applications",
                get(|| async { "[]" }).layer(from_fn_with_state(state, require_owner_session)),
            );
        Self { app, db, dir }
    }

    pub async fn send(&self, request: Request<Body>) -> Reply {
        let request = if request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some()
        {
            request
        } else {
            with_ip(request, DEFAULT_IP)
        };
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// Registers a public client and returns its id.
    pub async fn register(&self) -> String {
        self.register_with(json!({
            "client_name": "Synthetic Client",
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
        }))
        .await["client_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    pub async fn register_with(&self, body: Value) -> Value {
        let reply = self.send(json_req("/api/mcp/register", &body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.json()
    }

    pub async fn token(&self, body: &str) -> Reply {
        self.send(form_req("/api/mcp/token", body)).await
    }

    pub async fn exchange(&self, client_id: &str, code: &str) -> Reply {
        self.token(&format!(
            "grant_type=authorization_code&client_id={client_id}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
            encode(code),
            encode(REDIRECT),
        ))
        .await
    }

    pub async fn refresh(
        &self,
        client_id: &str,
        refresh_token: &str,
        scope: Option<&str>,
    ) -> Reply {
        let scope = scope.map_or(String::new(), |scope| format!("&scope={}", encode(scope)));
        self.token(&format!(
            "grant_type=refresh_token&client_id={client_id}&refresh_token={refresh_token}{scope}"
        ))
        .await
    }

    /// Moves every rotation far enough back that the retry grace is over.
    pub async fn end_refresh_grace(&self) {
        sqlx::query(
            "UPDATE oauth_refresh_tokens SET rotated_at = '2020-01-01T00:00:00.000Z'
             WHERE rotated_at IS NOT NULL",
        )
        .execute(self.db.pool())
        .await
        .unwrap();
    }

    pub async fn live_refresh_tokens(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens WHERE rotated_at IS NULL")
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// What `require_mcp_auth` made of a bearer token.
    pub async fn whoami(&self, token: &str) -> Reply {
        self.send(bearer_req(token)).await
    }

    pub async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// A browser logged in as the owner, then consented: returns the authorization code.
    pub async fn code_for(&self, client_id: &str, scope: &str) -> String {
        let mut browser = Browser::default();
        browser.login(self).await;
        let page = browser
            .send(self, get_req(&authorize_path(client_id, scope)))
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let done = browser
            .send(
                self,
                form_req(
                    "/api/mcp/authorize",
                    &format!("consent={}", encode(&page.field("consent"))),
                ),
            )
            .await;
        assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
        query_param(done.location(), "code").expect("redirect carries a code")
    }

    /// Runs the whole flow and returns the token response.
    pub async fn grant(&self, client_id: &str, scope: &str) -> Value {
        let code = self.code_for(client_id, scope).await;
        let reply = self.exchange(client_id, &code).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        reply.json()
    }
}

pub fn query_param(url: &str, name: &str) -> Option<String> {
    let query = Url::parse(url).ok()?.query()?.to_owned();
    parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

pub fn authorize_path(client_id: &str, scope: &str) -> String {
    format!(
        "/api/mcp/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=st-1&scope={}",
        encode(REDIRECT),
        encode(scope),
    )
}

/// Holds cookies across requests, as a browser would.
#[derive(Default)]
pub struct Browser {
    pub cookies: HashMap<String, String>,
}

impl Browser {
    pub async fn send(&mut self, harness: &Harness, mut request: Request<Body>) -> Reply {
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            request
                .headers_mut()
                .insert(COOKIE, cookie.parse().unwrap());
        }
        let reply = harness.send(request).await;
        for cookie in reply.headers.get_all(SET_COOKIE) {
            let first = cookie.to_str().unwrap().split(';').next().unwrap();
            let (key, value) = first.split_once('=').unwrap();
            if value.is_empty() {
                self.cookies.remove(key);
            } else {
                self.cookies.insert(key.to_owned(), value.to_owned());
            }
        }
        reply
    }

    /// Fetches the login form and posts a passphrase through it.
    pub async fn attempt_login(&mut self, harness: &Harness, passphrase: &str, ip: &str) -> Reply {
        let form = self.send(harness, with_ip(get_req("/login"), ip)).await;
        let body = format!(
            "csrf={}&passphrase={}",
            form.field("csrf"),
            encode(passphrase)
        );
        self.send(harness, with_ip(form_req("/login", &body), ip))
            .await
    }

    pub async fn login(&mut self, harness: &Harness) {
        let reply = self.attempt_login(harness, PASSPHRASE, DEFAULT_IP).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    }
}
