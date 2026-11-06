//! Port of `lib/mcp-oauth.ts` and `app/api/mcp/{authorize,register,token}`. The consent step is new
//! in one way: approval is a POST with a session-bound signed token, never a followable GET link.

use std::collections::HashMap;
use std::time::Instant;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, NO_PAD, URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use nexus_core::auth::{AuthContext, AuthType, SCOPE_SUBMISSIONS, SCOPE_TOOLS};
use nexus_core::repo::api_tokens;
use nexus_core::repo::oauth::{
    self as repo, NewAuthCode, NewOAuthClient, NewOAuthToken, OAuthClient,
};
use nexus_core::time::Timestamp;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use url::form_urlencoded::byte_serialize;

use crate::crypto::{
    escape_html, generate_token, parse_params, random_bytes, secret_eq, sha256_base64url,
    sha256_hex, sign,
};
use crate::limit::GLOBAL_KEY;
use crate::session::{MAX_NEXT_BYTES, current_session, html_page};
use crate::state::{AuthState, ClientKey};

const AUTH_CODE_TTL_MINUTES: i64 = 10;
const ACCESS_TOKEN_TTL_HOURS: i64 = 24;
const REFRESH_TOKEN_TTL_DAYS: i64 = 90;
const CONSENT_TTL_MILLIS: i64 = 600_000;
pub const SENSITIVE_CONSENT_VERSION: i64 = 1;

/// Old Nexus reached 33 client rows, largely junk. Real clients number a handful.
const MAX_CLIENTS: i64 = 50;
/// A client this new may be mid-authorization, so a registration flood must not evict it. The
/// registration limit cannot fill the cap within this time.
const EVICTION_MIN_AGE_MINUTES: i64 = 10;
const MAX_CLIENT_NAME_CHARS: usize = 200;
const CHALLENGE_LEN: usize = 43;
/// How long a rotated refresh token may be presented again without counting as a replay. Both MCP
/// SDKs keep the old token when a refresh response is lost, and retry it, and an unattended agent
/// can't re-consent. A grant still has one live refresh token: a retry supersedes the token issued
/// before it, so if a thief won the window, the real client's next refresh exposes them.
const REFRESH_RETRY_GRACE_SECONDS: i64 = 30;

const BASIC_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    NO_PAD.with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub fn effective_mcp_scopes(scopes: &[String], sensitive_consent_version: i64) -> Vec<String> {
    if sensitive_consent_version >= SENSITIVE_CONSENT_VERSION {
        return scopes.to_vec();
    }
    scopes
        .iter()
        .filter(|scope| *scope != SCOPE_SUBMISSIONS)
        .cloned()
        .collect()
}

/// A finished error response; `?` carries it out of a handler.
pub(crate) struct Reject(Box<Response>);

impl IntoResponse for Reject {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<nexus_core::error::Error> for Reject {
    fn from(error: nexus_core::error::Error) -> Self {
        tracing::error!(%error, "auth storage error");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Internal error",
        )
    }
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Reject {
    Reject(Box::new(
        (
            status,
            [(CACHE_CONTROL, "no-store")],
            Json(json!({ "error": error, "error_description": description })),
        )
            .into_response(),
    ))
}

fn bad_request(error: &str, description: &str) -> Reject {
    oauth_error(StatusCode::BAD_REQUEST, error, description)
}

fn too_many_requests(route: &str, source: &str, description: &str, retry_after: u64) -> Reject {
    tracing::warn!(
        event = "rate_limited",
        route,
        source,
        retry_after,
        "rate limit hit"
    );
    Reject(Box::new(
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (RETRY_AFTER, retry_after.to_string()),
                (CACHE_CONTROL, "no-store".to_owned()),
            ],
            Json(json!({ "error": "temporarily_unavailable", "error_description": description })),
        )
            .into_response(),
    ))
}

// ── Discovery ────────────────────────────────────────────────────────────────

fn discovery_response(metadata: Value) -> Response {
    ([(CACHE_CONTROL, "public, max-age=3600")], Json(metadata)).into_response()
}

pub(crate) async fn authorization_server_metadata(State(state): State<AuthState>) -> Response {
    let base = &state.0.base_url;
    discovery_response(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/api/mcp/authorize"),
        "token_endpoint": format!("{base}/api/mcp/token"),
        "registration_endpoint": format!("{base}/api/mcp/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": [SCOPE_TOOLS, SCOPE_SUBMISSIONS],
    }))
}

pub(crate) async fn protected_resource_metadata(State(state): State<AuthState>) -> Response {
    let base = &state.0.base_url;
    discovery_response(json!({
        "resource": format!("{base}/api/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE_TOOLS, SCOPE_SUBMISSIONS],
        "resource_name": "Nexus CRM MCP Server",
    }))
}

// ── Dynamic client registration ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ClientMetadata {
    client_name: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    token_endpoint_auth_method: Option<String>,
}

fn registration_error() -> Reject {
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Failed to register client",
    )
}

fn invalid_metadata(description: &str) -> Reject {
    bad_request("invalid_client_metadata", description)
}

fn validate_registered_redirect(uri: &Value) -> Result<String, Reject> {
    let shown = uri.as_str().map_or_else(|| uri.to_string(), str::to_owned);
    let parsed = uri
        .as_str()
        .and_then(|text| Url::parse(text).ok())
        .ok_or_else(|| invalid_metadata(&format!("Invalid redirect URI: {shown}")))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
        return Err(invalid_metadata(&format!(
            "redirect_uri must use https (got {}:): {shown}",
            parsed.scheme()
        )));
    }
    if parsed.fragment().is_some() {
        return Err(invalid_metadata(&format!(
            "redirect_uri must not contain a fragment: {shown}"
        )));
    }
    Ok(shown)
}

pub(crate) async fn register(
    State(state): State<AuthState>,
    ClientKey(client): ClientKey,
    body: Bytes,
) -> Result<Response, Reject> {
    let now = std::time::Instant::now();
    state
        .0
        .register_ip_limit
        .check(&client, now)
        .and_then(|()| state.0.register_global_limit.check(GLOBAL_KEY, now))
        .map_err(|retry| {
            too_many_requests("register", &client, "Too many registration requests", retry)
        })?;

    let body: Value = serde_json::from_slice(&body)
        .map_err(|_| invalid_metadata("Request body is not valid JSON"))?;
    let uris = body
        .get("redirect_uris")
        .and_then(Value::as_array)
        .filter(|uris| !uris.is_empty())
        .ok_or_else(|| invalid_metadata("redirect_uris is required"))?;
    let redirect_uris = uris
        .iter()
        .map(validate_registered_redirect)
        .collect::<Result<Vec<_>, _>>()?;
    let metadata: ClientMetadata = serde_json::from_value(body.clone())
        .map_err(|_| invalid_metadata("Client metadata has a field of the wrong type"))?;
    if metadata
        .client_name
        .as_ref()
        .is_some_and(|name| name.chars().count() > MAX_CLIENT_NAME_CHARS)
    {
        return Err(invalid_metadata("client_name is too long"));
    }

    let token_endpoint_auth = metadata
        .token_endpoint_auth_method
        .unwrap_or_else(|| "client_secret_post".to_owned());
    let client_secret = (token_endpoint_auth != "none").then(|| hex::encode(random_bytes()));
    let new_client = NewOAuthClient {
        client_id: format!("mcp_{}", hex::encode(&random_bytes()[..16])),
        client_secret_hash: client_secret.as_deref().map(sha256_hex),
        client_name: metadata.client_name,
        redirect_uris,
        grant_types: metadata
            .grant_types
            .unwrap_or_else(|| vec!["authorization_code".to_owned(), "refresh_token".to_owned()]),
        response_types: metadata
            .response_types
            .unwrap_or_else(|| vec!["code".to_owned()]),
        token_endpoint_auth,
    };

    let Some(created) = store_client(&state, &new_client).await.map_err(|error| {
        tracing::error!(%error, "client registration failed");
        registration_error()
    })?
    else {
        return Err(oauth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "Client registration limit reached",
        ));
    };

    tracing::info!(
        event = "client_registered",
        client = %created.client_id,
        public = client_secret.is_none(),
        source = %client,
        "OAuth client registered"
    );
    let mut response = json!({ "client_id": created.client_id });
    if let Some(secret) = client_secret {
        response["client_secret"] = json!(secret);
    }
    response["client_name"] = json!(created.client_name);
    response["redirect_uris"] = json!(created.redirect_uris.0);
    response["grant_types"] = json!(created.grant_types.0);
    response["response_types"] = json!(created.response_types.0);
    response["token_endpoint_auth_method"] = json!(created.token_endpoint_auth);
    response["client_id_issued_at"] = json!(created.created_at.to_datetime().timestamp());
    response["client_secret_expires_at"] = json!(0);
    Ok((
        StatusCode::CREATED,
        [(CACHE_CONTROL, "no-store")],
        Json(response),
    )
        .into_response())
}

/// A used code is kept this long past its expiry: long enough for every token it can have produced
/// to have expired, unless rotation has kept the grant going.
pub(crate) fn used_code_cutoff(now: &Timestamp) -> Timestamp {
    now.plus(Duration::days(-REFRESH_TOKEN_TTL_DAYS))
}

/// Inserts a client, keeping the table at or below `MAX_CLIENTS`. Unused clients are evicted so
/// junk registrations can never lock out a real client; `None` means every client is in use.
async fn store_client(
    state: &AuthState,
    client: &NewOAuthClient,
) -> nexus_core::error::Result<Option<OAuthClient>> {
    let mut tx = state.0.db.begin_write().await?;
    if repo::count_clients(&mut tx).await? >= MAX_CLIENTS {
        let now = Timestamp::now();
        repo::prune_expired(&mut tx, &now, &used_code_cutoff(&now)).await?;
        let cutoff = now.plus(Duration::minutes(-EVICTION_MIN_AGE_MINUTES));
        if !repo::evict_oldest_unused_client(&mut tx, &cutoff).await? {
            return Ok(None);
        }
    }
    let created = repo::insert_client(&mut tx, client).await?;
    tx.commit().await?;
    Ok(Some(created))
}

// ── Redirect URI matching ────────────────────────────────────────────────────

fn is_loopback_redirect_uri(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|parsed| {
        parsed.scheme() == "http" && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
    })
}

fn is_redirect_uri_allowed(registered: &[String], requested: &str) -> bool {
    if registered.iter().any(|uri| uri == requested) {
        return true;
    }
    if !is_loopback_redirect_uri(requested) {
        return false;
    }
    let Ok(requested) = Url::parse(requested) else {
        return false;
    };
    if requested.fragment().is_some() {
        return false;
    }
    registered.iter().any(|uri| {
        is_loopback_redirect_uri(uri)
            && Url::parse(uri).is_ok_and(|registered| {
                registered.host_str() == requested.host_str()
                    && registered.path() == requested.path()
            })
    })
}

// ── Authorization endpoint ───────────────────────────────────────────────────

/// What the owner approves. Signed, like old Nexus's `SubmissionConsent`, and carried through the
/// consent form. `session` makes it double as that form's CSRF token: only the session that was
/// shown the page can submit it.
#[derive(Serialize, Deserialize)]
struct Consent {
    client_id: String,
    session: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
    state: Option<String>,
    expires_at: i64,
}

const CONSENT_PURPOSE: &str = "mcp-consent";

fn sign_consent(state: &AuthState, consent: &Consent) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(consent).expect("consent serializes"));
    let signature = sign(&state.0.key, CONSENT_PURPOSE, &encoded);
    format!("{encoded}.{signature}")
}

fn verify_consent(state: &AuthState, token: &str, session: &str) -> Option<Consent> {
    let (encoded, signature) = token.rsplit_once('.')?;
    if !secret_eq(signature, &sign(&state.0.key, CONSENT_PURPOSE, encoded)) {
        return None;
    }
    let consent: Consent = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
    (consent.expires_at >= Utc::now().timestamp_millis() && secret_eq(&consent.session, session))
        .then_some(consent)
}

fn is_s256_challenge(challenge: &str) -> bool {
    challenge.len() == CHALLENGE_LEN
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn limit_authorize(state: &AuthState, source: &str) -> Result<(), Reject> {
    state
        .0
        .authorize_limit
        .check(source, Instant::now())
        .map_err(|retry| {
            too_many_requests(
                "authorize",
                source,
                "Too many authorization requests",
                retry,
            )
        })
}

/// Validates in old Nexus's order and with its messages. An omitted `code_challenge_method` stays
/// accepted as old Nexus does: the challenge is always checked as S256, so `plain` can't succeed.
pub(crate) async fn authorize(
    State(state): State<AuthState>,
    ClientKey(source): ClientKey,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, Reject> {
    limit_authorize(&state, &source)?;
    let query = uri.query().unwrap_or("");
    // The whole query travels through /login as `next`, which refuses anything longer.
    if query.len() + "/api/mcp/authorize?".len() > MAX_NEXT_BYTES {
        return Err(bad_request("invalid_request", "Request is too long"));
    }
    let params = parse_params(query.as_bytes());
    let param = |name: &str| params.get(name).map(String::as_str);
    let required = |name: &str| param(name).filter(|value| !value.is_empty());

    let (Some(client_id), Some(redirect_uri), Some(code_challenge)) = (
        required("client_id"),
        required("redirect_uri"),
        required("code_challenge"),
    ) else {
        return Err(bad_request(
            "invalid_request",
            "Missing required parameters (client_id, redirect_uri, code_challenge)",
        ));
    };
    let scopes: Vec<&str> = param("scope")
        .unwrap_or(SCOPE_TOOLS)
        .split(' ')
        .filter(|scope| !scope.is_empty())
        .collect();
    if !scopes.contains(&SCOPE_TOOLS)
        || scopes
            .iter()
            .any(|scope| *scope != SCOPE_TOOLS && *scope != SCOPE_SUBMISSIONS)
    {
        return Err(bad_request(
            "invalid_scope",
            "Supported scopes are mcp:tools and mcp:submissions",
        ));
    }
    if param("response_type") != Some("code") {
        return Err(bad_request(
            "unsupported_response_type",
            "Only 'code' is supported",
        ));
    }
    if required("code_challenge_method").is_some_and(|method| method != "S256") {
        return Err(bad_request(
            "invalid_request",
            "Only S256 code_challenge_method is supported",
        ));
    }
    // Any other shape could never match a verifier's digest at the token endpoint.
    if !is_s256_challenge(code_challenge) {
        return Err(bad_request(
            "invalid_request",
            "code_challenge must be a base64url SHA-256 digest",
        ));
    }

    let mut conn = state.0.db.conn().await?;
    let Some(client) = repo::find_client(&mut conn, client_id).await? else {
        return Err(bad_request("invalid_client", "Unknown client_id"));
    };
    drop(conn);
    if !is_redirect_uri_allowed(&client.redirect_uris.0, redirect_uri) {
        return Err(bad_request(
            "invalid_request",
            "redirect_uri not registered for this client",
        ));
    }

    let Some(session) = current_session(&state, &headers) else {
        let next: String =
            byte_serialize(format!("/api/mcp/authorize?{query}").as_bytes()).collect();
        return Ok(Redirect::to(&format!("/login?next={next}")).into_response());
    };

    let scope = scopes.join(" ");
    let consent = sign_consent(
        &state,
        &Consent {
            client_id: client_id.to_owned(),
            session,
            redirect_uri: redirect_uri.to_owned(),
            code_challenge: code_challenge.to_owned(),
            scope: scope.clone(),
            state: required("state").map(str::to_owned),
            expires_at: Utc::now().timestamp_millis() + CONSENT_TTL_MILLIS,
        },
    );
    let client_label = client
        .client_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(client_id);
    let redirect_origin = Url::parse(redirect_uri)
        .map_err(|_| {
            bad_request(
                "invalid_request",
                "redirect_uri not registered for this client",
            )
        })?
        .origin()
        .ascii_serialization();
    let requested_access = if scopes.contains(&SCOPE_SUBMISSIONS) {
        "read and change CRM data, download documents, and access exact application submissions"
    } else {
        "read and change CRM applications and download ordinary documents"
    };
    Ok(html_page(
        StatusCode::OK,
        "Authorize MCP access",
        &format!(
            r#"<h1>Authorize MCP access?</h1>
<p><strong>{client}</strong> ({origin}) is requesting permission to {requested_access}.</p>
<p>Scopes: <code>{scope}</code>. Only continue if you trust this client.</p>
<form method="post" action="/api/mcp/authorize"><input type="hidden" name="consent" value="{consent}"><button type="submit">Allow access</button></form>
<p>You can close this page to deny access.</p>"#,
            client = escape_html(client_label),
            origin = escape_html(&redirect_origin),
            scope = escape_html(&scope),
            consent = escape_html(&consent),
        ),
        Some(&redirect_origin),
    ))
}

fn consent_refused(status: StatusCode, message: &str) -> Reject {
    Reject(Box::new(html_page(
        status,
        "Authorization failed",
        &format!("<h1>Authorization failed</h1><p>{message}</p>"),
        None,
    )))
}

/// The consent form's POST: issues a code for exactly what the signed token says was shown.
pub(crate) async fn approve(
    State(state): State<AuthState>,
    ClientKey(source): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Reject> {
    limit_authorize(&state, &source)?;
    const RESTART: &str = "Start the connection again from your client.";
    let Some(session) = current_session(&state, &headers) else {
        return Err(consent_refused(
            StatusCode::UNAUTHORIZED,
            &format!("Your login has expired. {RESTART}"),
        ));
    };
    let form = parse_params(&body);
    let Some(consent) = form
        .get("consent")
        .and_then(|token| verify_consent(&state, token, &session))
    else {
        return Err(consent_refused(
            StatusCode::FORBIDDEN,
            &format!("This approval is invalid or has expired. {RESTART}"),
        ));
    };

    state.prune_if_due().await?;
    let mut conn = state.0.db.conn().await?;
    // The client can have been evicted since the page was shown.
    let client = repo::find_client(&mut conn, &consent.client_id).await?;
    if !client.is_some_and(|c| is_redirect_uri_allowed(&c.redirect_uris.0, &consent.redirect_uri)) {
        return Err(consent_refused(
            StatusCode::BAD_REQUEST,
            &format!("This client is no longer registered. {RESTART}"),
        ));
    }

    let scopes: Vec<String> = consent.scope.split(' ').map(str::to_owned).collect();
    let code = URL_SAFE_NO_PAD.encode(random_bytes());
    let code_hash = sha256_hex(&code);
    repo::insert_auth_code(
        &mut conn,
        &NewAuthCode {
            code: code_hash.clone(),
            client_id: consent.client_id.clone(),
            redirect_uri: consent.redirect_uri.clone(),
            code_challenge: consent.code_challenge,
            sensitive_consent_version: if consent.scope.split(' ').any(|s| s == SCOPE_SUBMISSIONS) {
                SENSITIVE_CONSENT_VERSION
            } else {
                0
            },
            scopes,
            expires_at: Timestamp::now().plus(Duration::minutes(AUTH_CODE_TTL_MINUTES)),
        },
    )
    .await?;

    tracing::info!(
        event = "consent_granted",
        client = %consent.client_id,
        scopes = %consent.scope,
        grant = grant_label(&code_hash),
        "owner approved an OAuth client"
    );

    let callback = callback_url(&consent.redirect_uri, &code, consent.state.as_deref())
        .ok_or_else(|| consent_refused(StatusCode::BAD_REQUEST, RESTART))?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Redirect::to(callback.as_str()),
    )
        .into_response())
}

/// Enough of a grant id to follow one grant through the logs.
fn grant_label(grant_id: &str) -> &str {
    &grant_id[..grant_id.len().min(12)]
}

/// `URLSearchParams.set` semantics: a registered query keeps its other parameters.
fn callback_url(redirect_uri: &str, code: &str, state: Option<&str>) -> Option<Url> {
    let mut url = Url::parse(redirect_uri).ok()?;
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "code" && !(key == "state" && state.is_some()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    {
        let mut query = url.query_pairs_mut();
        query.clear().extend_pairs(kept).append_pair("code", code);
        if let Some(state) = state {
            query.append_pair("state", state);
        }
    }
    Some(url)
}

// ── Token endpoint ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: i64,
    scope: String,
}

fn token_params(headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    if !is_json {
        // RFC 6749 mandates a form body. Some clients send odd or no Content-Type, so old Nexus
        // doesn't reject on it.
        return parse_params(body);
    }
    let Ok(Value::Object(object)) = serde_json::from_slice(body) else {
        return HashMap::new();
    };
    object
        .into_iter()
        .filter_map(|(key, value)| match value {
            Value::String(text) => Some((key, text)),
            _ => None,
        })
        .collect()
}

/// `client_secret_basic` credentials: `(id, secret)`, percent-decoded per RFC 6749 §2.3.1.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = header.split_at_checked(6)?;
    if !scheme.eq_ignore_ascii_case("basic ") {
        return None;
    }
    let decoded = String::from_utf8(BASIC_BASE64.decode(encoded.trim()).ok()?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    let decode = |text| {
        percent_decode_str(text)
            .decode_utf8()
            .ok()
            .map(String::from)
    };
    Some((decode(id)?, decode(secret)?))
}

/// Port of `verifyClient`. Public clients have no secret to check; the PKCE verifier proves them.
fn client_authenticates(client: &OAuthClient, secret: Option<&str>) -> bool {
    if client.token_endpoint_auth == "none" {
        return true;
    }
    match &client.client_secret_hash {
        Some(hash) => secret.is_some_and(|secret| secret_eq(&sha256_hex(secret), hash)),
        None => true,
    }
}

pub(crate) async fn token(
    State(state): State<AuthState>,
    ClientKey(client_key): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Reject> {
    if let Err(retry) = state.0.token_limit.check(&client_key, Instant::now()) {
        return Err(too_many_requests(
            "token",
            &client_key,
            "Too many token requests",
            retry,
        ));
    }

    let params = token_params(&headers, &body);
    let param = |name: &str| params.get(name).map(String::as_str);
    let required = |name: &str| param(name).filter(|value| !value.is_empty());
    let basic = basic_credentials(&headers);
    let client_id = required("client_id").or(basic.as_ref().map(|(id, _)| id.as_str()));
    let client_secret = param("client_secret").or(basic.as_ref().map(|(_, s)| s.as_str()));

    let Some(client_id) = client_id.filter(|id| !id.is_empty()) else {
        return Err(bad_request("invalid_client", "client_id is required"));
    };

    state.prune_if_due().await?;
    let mut conn = state.0.db.conn().await?;
    let client = repo::find_client(&mut conn, client_id).await?;
    drop(conn);
    if !client.is_some_and(|client| client_authenticates(&client, client_secret)) {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Invalid client credentials",
        ));
    }

    let tokens = match param("grant_type") {
        Some("authorization_code") => {
            let (Some(code), Some(redirect_uri), Some(verifier)) = (
                required("code"),
                required("redirect_uri"),
                required("code_verifier"),
            ) else {
                return Err(bad_request(
                    "invalid_request",
                    "Missing code, redirect_uri, or code_verifier",
                ));
            };
            exchange_auth_code(&state, code, client_id, redirect_uri, verifier)
                .await?
                .ok_or_else(|| {
                    bad_request("invalid_grant", "Invalid or expired authorization code")
                })?
        }
        Some("refresh_token") => {
            let Some(refresh_token) = required("refresh_token") else {
                return Err(bad_request("invalid_request", "Missing refresh_token"));
            };
            let requested: Vec<String> = param("scope")
                .unwrap_or("")
                .split(' ')
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect();
            exchange_refresh_token(&state, refresh_token, client_id, &requested).await?
        }
        _ => {
            return Err(bad_request(
                "unsupported_grant_type",
                "Only authorization_code and refresh_token are supported",
            ));
        }
    };
    Ok(([(CACHE_CONTROL, "no-store")], Json(tokens)).into_response())
}

async fn issue_tokens(
    conn: &mut sqlx::SqliteConnection,
    grant_id: &str,
    client_id: &str,
    access_scopes: Vec<String>,
    refresh_scopes: Vec<String>,
    sensitive_consent_version: i64,
) -> nexus_core::error::Result<TokenResponse> {
    let now = Timestamp::now();
    let access_token = generate_token("mcp_at");
    let refresh_token = generate_token("mcp_rt");
    let scope = access_scopes.join(" ");
    repo::insert_access_token(
        conn,
        &NewOAuthToken {
            token_hash: sha256_hex(&access_token),
            grant_id: grant_id.to_owned(),
            client_id: client_id.to_owned(),
            scopes: access_scopes,
            sensitive_consent_version,
            expires_at: now.plus(Duration::hours(ACCESS_TOKEN_TTL_HOURS)),
        },
    )
    .await?;
    repo::insert_refresh_token(
        conn,
        &NewOAuthToken {
            token_hash: sha256_hex(&refresh_token),
            grant_id: grant_id.to_owned(),
            client_id: client_id.to_owned(),
            scopes: refresh_scopes,
            sensitive_consent_version,
            expires_at: now.plus(Duration::days(REFRESH_TOKEN_TTL_DAYS)),
        },
    )
    .await?;
    Ok(TokenResponse {
        access_token,
        refresh_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_HOURS * 3600,
        scope,
    })
}

/// The code is consumed before PKCE is checked, and that sticks: a wrong verifier burns the code.
/// Presenting a used code again means it leaked, so the tokens it produced are revoked (RFC 6749
/// §4.1.2). Whoever holds a code may do that, whichever client they claim to be.
async fn exchange_auth_code(
    state: &AuthState,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    verifier: &str,
) -> nexus_core::error::Result<Option<TokenResponse>> {
    let code_hash = sha256_hex(code);
    let mut tx = state.0.db.begin_write().await?;
    let Some(auth_code) = repo::consume_auth_code(
        &mut tx,
        &code_hash,
        client_id,
        redirect_uri,
        &Timestamp::now(),
    )
    .await?
    else {
        if let Some(used) = repo::find_auth_code(&mut tx, &code_hash)
            .await?
            .filter(|code| code.used)
        {
            let revoked = repo::revoke_grant(&mut tx, &code_hash).await?;
            tx.commit().await?;
            log_replay("authorization_code", &used.client_id, &code_hash, revoked);
        }
        return Ok(None);
    };
    if !secret_eq(&sha256_base64url(verifier), &auth_code.code_challenge) {
        tx.commit().await?;
        tracing::warn!(
            event = "pkce_failure",
            client = %auth_code.client_id,
            grant = grant_label(&code_hash),
            "wrong code_verifier burned an authorization code"
        );
        return Ok(None);
    }
    let granted = effective_mcp_scopes(&auth_code.scopes.0, auth_code.sensitive_consent_version);
    let tokens = issue_tokens(
        &mut tx,
        &code_hash,
        client_id,
        granted.clone(),
        granted,
        auth_code.sensitive_consent_version,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(tokens))
}

fn log_replay(kind: &str, client: &str, grant_id: &str, revoked: repo::Revoked) {
    tracing::warn!(
        event = "token_replay_detected",
        kind,
        client,
        grant = grant_label(grant_id),
        "a used credential was presented again"
    );
    tracing::warn!(
        event = "grants_revoked",
        reason = "token_replay",
        grant = grant_label(grant_id),
        access_tokens = revoked.access_tokens,
        refresh_tokens = revoked.refresh_tokens,
        "revoked the tokens of one grant"
    );
}

async fn exchange_refresh_token(
    state: &AuthState,
    refresh_token: &str,
    client_id: &str,
    requested: &[String],
) -> Result<TokenResponse, Reject> {
    let invalid = || bad_request("invalid_grant", "Invalid or expired refresh token");
    let now = Timestamp::now();
    let mut tx = state.0.db.begin_write().await.map_err(Reject::from)?;
    let stored = repo::find_refresh_token(&mut tx, &sha256_hex(refresh_token))
        .await?
        .filter(|stored| stored.expires_at >= now)
        .ok_or_else(invalid)?;

    if let Some(rotated_at) = &stored.rotated_at {
        let in_grace = stored.client_id == client_id
            && now <= rotated_at.plus(Duration::seconds(REFRESH_RETRY_GRACE_SECONDS));
        if !in_grace {
            let revoked = repo::revoke_grant(&mut tx, &stored.grant_id).await?;
            tx.commit().await.map_err(nexus_core::error::Error::from)?;
            log_replay(
                "refresh_token",
                &stored.client_id,
                &stored.grant_id,
                revoked,
            );
            return Err(invalid());
        }
    } else if stored.client_id != client_id {
        return Err(invalid());
    }

    let granted = effective_mcp_scopes(&stored.scopes.0, stored.sensitive_consent_version);
    // RFC 6749 §6: a refresh may narrow scope but never widen it. A client wanting more must
    // re-authorize; a silently narrower token would read as success and fail later.
    let beyond_grant: Vec<&str> = requested
        .iter()
        .filter(|scope| !granted.contains(scope))
        .map(String::as_str)
        .collect();
    if !beyond_grant.is_empty() {
        return Err(bad_request(
            "invalid_scope",
            &format!(
                "Requested scope exceeds the granted scope ({}); re-authorization is required to obtain it",
                beyond_grant.join(" ")
            ),
        ));
    }
    let issued = if requested.is_empty() {
        granted
    } else {
        requested.to_vec()
    };

    repo::retire_refresh_tokens(&mut tx, &stored.grant_id, &now).await?;
    // Narrowing applies to the access token only. The rotated refresh token keeps the original
    // grant, so a later refresh isn't permanently burned down to the narrowed set.
    let tokens = issue_tokens(
        &mut tx,
        &stored.grant_id,
        client_id,
        issued,
        stored.scopes.0,
        stored.sensitive_consent_version,
    )
    .await?;
    tx.commit().await.map_err(nexus_core::error::Error::from)?;
    Ok(tokens)
}

// ── Bearer authentication ────────────────────────────────────────────────────

/// Port of `authenticateFromRequest`. Tokens are found by their SHA-256, so a lookup never compares
/// anything an attacker can steer byte by byte.
async fn authenticate(
    state: &AuthState,
    raw: &str,
) -> nexus_core::error::Result<Option<AuthContext>> {
    let hash = sha256_hex(raw);
    let mut conn = state.0.db.conn().await?;

    if raw.starts_with("mcp_at_") {
        let token = repo::find_access_token(&mut conn, &hash)
            .await?
            .filter(|token| token.expires_at >= Timestamp::now());
        return Ok(token.map(|token| AuthContext {
            auth_type: AuthType::OAuth,
            scopes: effective_mcp_scopes(&token.scopes.0, token.sensitive_consent_version),
        }));
    }

    let Some(token) = api_tokens::find_by_hash(&mut conn, &hash).await? else {
        return Ok(None);
    };
    let db = state.0.db.clone();
    tokio::spawn(async move {
        let touched = match db.conn().await {
            Ok(mut conn) => api_tokens::touch_last_used(&mut conn, token.id).await,
            Err(error) => Err(error),
        };
        if let Err(error) = touched {
            tracing::warn!(%error, "could not record API token use");
        }
    });
    Ok(Some(AuthContext {
        auth_type: AuthType::ApiToken,
        scopes: vec![SCOPE_TOOLS.to_owned(), SCOPE_SUBMISSIONS.to_owned()],
    }))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
}

/// Puts `AuthContext` in the request extensions, or answers exactly as old Nexus does. The
/// `WWW-Authenticate` header is how MCP clients discover the OAuth flow (RFC 9728 §5.1).
///
/// Only a presented token that fails counts towards the rate limit, and only after it was checked.
/// So valid tokens are never limited, not even from a source that is being limited, and neither is
/// a client's first tokenless request, which starts discovery.
pub async fn require_mcp_auth(
    State(state): State<AuthState>,
    ClientKey(source): ClientKey,
    mut request: Request,
    next: Next,
) -> Response {
    let authenticated = match bearer_token(request.headers()) {
        Some(raw) => {
            let authenticated = authenticate(&state, raw).await;
            if matches!(authenticated, Ok(None))
                && let Err(retry) = state.0.bearer_failure_limit.check(&source, Instant::now())
            {
                return too_many_requests(
                    "mcp",
                    &source,
                    "Too many failed authentication attempts",
                    retry,
                )
                .into_response();
            }
            authenticated
        }
        None => Ok(None),
    };
    match authenticated {
        Ok(Some(context)) => {
            request.extensions_mut().insert(context);
            next.run(request).await
        }
        Ok(None) => {
            let base = &state.0.base_url;
            (
                StatusCode::UNAUTHORIZED,
                [
                    (CONTENT_TYPE, "application/json".to_owned()),
                    (
                        WWW_AUTHENTICATE,
                        format!(
                            r#"Bearer realm="{base}/api/mcp", resource_metadata="{base}/.well-known/oauth-protected-resource""#
                        ),
                    ),
                ],
                r#"{"error":"Unauthorized – provide Bearer token"}"#,
            )
                .into_response()
        }
        Err(error) => Reject::from(error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::db::Db;

    use super::*;
    use crate::AuthConfig;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn accepts_exact_registered_redirect_uris() {
        assert!(is_redirect_uri_allowed(
            &strings(&["https://client.example.test/cb"]),
            "https://client.example.test/cb"
        ));
    }

    #[test]
    fn accepts_loopback_redirects_with_dynamic_ports() {
        assert!(is_redirect_uri_allowed(
            &strings(&["http://127.0.0.1:39123/callback"]),
            "http://127.0.0.1:47987/callback"
        ));
        assert!(is_redirect_uri_allowed(
            &strings(&["http://localhost:39123/callback"]),
            "http://localhost/callback"
        ));
    }

    #[test]
    fn rejects_every_other_redirect() {
        let registered = strings(&["http://127.0.0.1:39123/callback"]);
        for requested in [
            "http://example.test/callback",
            "http://localhost:39123/callback",
            "http://127.0.0.1:39123/other",
            "http://127.0.0.1:47987/callback#fragment",
            "https://127.0.0.1:39123/callback",
            "http://127.0.0.1.example.test/callback",
            "http://127.0.0.1:39123@example.test/callback",
            "not a url",
        ] {
            assert!(
                !is_redirect_uri_allowed(&registered, requested),
                "{requested}"
            );
        }
        assert!(!is_redirect_uri_allowed(
            &strings(&["https://client.example.test/cb"]),
            "http://127.0.0.1:47987/cb"
        ));
        assert!(!is_redirect_uri_allowed(
            &strings(&["https://client.example.test/cb"]),
            "https://client.example.test/cb/"
        ));
        assert!(!is_redirect_uri_allowed(
            &strings(&["https://client.example.test/cb"]),
            "https://client.example.test/cb?x=1"
        ));
    }

    #[test]
    fn legacy_submissions_scope_is_inactive_until_versioned_consent() {
        let scopes = strings(&[SCOPE_TOOLS, SCOPE_SUBMISSIONS]);
        assert_eq!(effective_mcp_scopes(&scopes, 0), strings(&[SCOPE_TOOLS]));
        assert_eq!(
            effective_mcp_scopes(&scopes, SENSITIVE_CONSENT_VERSION),
            scopes
        );
        assert_eq!(
            effective_mcp_scopes(&scopes, SENSITIVE_CONSENT_VERSION + 1),
            scopes
        );
    }

    #[test]
    fn callback_keeps_other_query_params_and_replaces_code_and_state() {
        let url = callback_url(
            "https://c.example.test/cb?a=1&code=old&state=old",
            "new",
            Some("st"),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://c.example.test/cb?a=1&code=new&state=st"
        );
        let url = callback_url("https://c.example.test/cb", "a b", Some("x&y=z")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://c.example.test/cb?code=a+b&state=x%26y%3Dz"
        );
        let url = callback_url("https://c.example.test/cb", "c", None).unwrap();
        assert_eq!(url.as_str(), "https://c.example.test/cb?code=c");
    }

    #[test]
    fn s256_challenge_shape() {
        assert!(is_s256_challenge(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!is_s256_challenge("too-short"));
        assert!(!is_s256_challenge(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw+cM"
        ));
        assert!(!is_s256_challenge(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM="
        ));
    }

    #[tokio::test]
    async fn consent_tokens_expire_and_bind_to_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = AuthState::new(
            Db::open(dir.path()).await.unwrap(),
            AuthConfig {
                public_base_url: "https://crm.example.test".to_owned(),
                owner_passphrase_hash:
                    "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                trusted_proxy: None,
            },
        )
        .unwrap();
        let token = |expires_at| {
            sign_consent(
                &state,
                &Consent {
                    client_id: "c".to_owned(),
                    session: "session-a".to_owned(),
                    redirect_uri: "https://c.example.test/cb".to_owned(),
                    code_challenge: "x".to_owned(),
                    scope: SCOPE_TOOLS.to_owned(),
                    state: None,
                    expires_at,
                },
            )
        };
        let now = Utc::now().timestamp_millis();
        assert!(verify_consent(&state, &token(now + 60_000), "session-a").is_some());
        assert!(verify_consent(&state, &token(now + 60_000), "session-b").is_none());
        assert!(verify_consent(&state, &token(now - 1), "session-a").is_none());
    }
}
