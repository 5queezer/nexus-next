//! Voice sessions: `POST /api/voice/session` mints a short-lived OpenAI secret, and
//! `POST /api/voice/tool` runs the function calls the model makes, through an allowlist this server
//! enforces. The browser only relays them, and whatever a job description talks the model into
//! arrives here as just another call. See SPEC section 9b.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use nexus_core::auth::{FormToken, SCOPE_TOOLS};
use nexus_core::repo::cv_revisions;
use nexus_mcp::{CallToolResult, all_tools};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::cv::Cv;
use crate::forms::csrf_ok;

pub const OPENAI_BASE_URL: &str = "https://api.openai.com";

/// Everything a voice session may call. Nothing here writes outside the `cv_*` tables, so a voice
/// session can't mark an application as applied. The definitions sent to OpenAI come from this list.
pub const VOICE_TOOLS: [&str; 8] = [
    "get_cv_source",
    "patch_cv",
    "render_cv_pdf",
    "list_cv_revisions",
    "revert_cv",
    "list_applications_filtered",
    "get_application",
    "list_application_events",
];

/// Arguments the server decides, so the model isn't offered them.
const FORCED_ARGUMENTS: [(&str, &str); 3] = [
    ("patch_cv", "origin"),
    ("render_cv_pdf", "saveToApplicationId"),
    ("render_cv_pdf", "previewPages"),
];

const INSTRUCTIONS: &str = include_str!("voice_instructions.txt");
const TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
/// The API's minimum for a client secret's lifetime.
const MIN_SECRET_SECONDS: u64 = 10;
/// A secret only has to last until the browser has connected.
const MAX_SECRET_SECONDS: u64 = 60;
/// One long job description mustn't burn the budget.
pub const MAX_MODEL_OUTPUT_BYTES: usize = 32 * 1024;
pub const MAX_SESSION_BODY_BYTES: usize = 4 * 1024;
/// A patch can carry a whole source, 256 KiB, in both `oldText` and `newText`, escaped as JSON.
pub const MAX_TOOL_BODY_BYTES: usize = 1024 * 1024;
/// A mint costs nothing itself, but a page bug in a reconnect loop must not open sessions without
/// end. Failed mints count too.
pub const MINTS_PER_HOUR: usize = 12;
const MINT_WINDOW: Duration = Duration::from_secs(3600);
/// A reload leaves its old session's id behind. The oldest goes first.
const MAX_LIVE_SESSIONS: usize = 2;
pub const IDLE_SECONDS: u64 = 300;

const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
/// The answer holds the session's whole configuration, instructions and tools included.
const MAX_UPSTREAM_ANSWER_BYTES: usize = 1024 * 1024;

const CSRF_HEADER: &str = "x-nexus-csrf";
const SESSION_HEADER: &str = "x-nexus-voice-session";

/// The API key. `Debug` doesn't show it, and nothing but the mint's request header reads it.
#[derive(Clone)]
pub struct VoiceKey(String);

impl VoiceKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }
}

impl std::fmt::Debug for VoiceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VoiceKey(hidden)")
    }
}

#[derive(Clone, Debug)]
pub struct VoiceConfig {
    pub key: VoiceKey,
    pub model: String,
    pub voice: String,
    /// `OPENAI_BASE_URL`, unless a local test run set a stub.
    pub base_url: String,
    /// Enforced here: an ephemeral key's expiry limits when a session can start, not how long it
    /// lives. Tool calls stop at this point and the page closes the connection.
    pub lifetime: Duration,
}

impl VoiceConfig {
    /// The origin the page's CSP lets it reach, besides itself.
    pub(crate) fn connect_origin(&self) -> &str {
        &self.base_url
    }
}

struct VoiceSession {
    /// The form token of the login that opened it: another login's calls don't get in.
    form_token: String,
    variant: String,
    expires_at: Instant,
    created_at: Instant,
}

#[derive(Default)]
struct Live {
    sessions: HashMap<String, VoiceSession>,
    mints: VecDeque<Instant>,
}

/// In memory only: a restart ends every session. The page's status check finds that out within
/// about a minute, closes the connection and says so.
#[derive(Clone)]
pub struct Voice {
    config: Option<VoiceConfig>,
    client: Option<Upstream>,
    live: Arc<Mutex<Live>>,
}

impl Voice {
    /// Builds no client without a key, so that nothing can open an outbound connection.
    pub fn new(config: Option<VoiceConfig>) -> Self {
        let client = config.as_ref().map(upstream_client);
        Self {
            config,
            client,
            live: Arc::default(),
        }
    }

    pub(crate) fn config(&self) -> Option<&VoiceConfig> {
        self.config.as_ref()
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// `false` once this hour's mints are used up. Counted before the call, so a failure counts.
    fn take_mint(&self) -> bool {
        let now = Instant::now();
        let mut live = self.live();
        while live
            .mints
            .front()
            .is_some_and(|at| now.duration_since(*at) >= MINT_WINDOW)
        {
            live.mints.pop_front();
        }
        if live.mints.len() >= MINTS_PER_HOUR {
            return false;
        }
        live.mints.push_back(now);
        true
    }

    fn open_session(&self, form_token: &str, variant: &str, lifetime: Duration) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let id = hex::encode(bytes);
        let now = Instant::now();
        let mut live = self.live();
        live.sessions.retain(|_, session| session.expires_at > now);
        while live.sessions.len() >= MAX_LIVE_SESSIONS {
            let oldest = live
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.created_at)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else { break };
            live.sessions.remove(&oldest);
        }
        live.sessions.insert(
            id.clone(),
            VoiceSession {
                form_token: form_token.to_owned(),
                variant: variant.to_owned(),
                expires_at: now + lifetime,
                created_at: now,
            },
        );
        id
    }

    /// The variant of a live session this login opened. Without a token (no login in local mode)
    /// any live session counts.
    fn session_variant(&self, id: &str, form_token: Option<&str>) -> Option<String> {
        let live = self.live();
        let session = live.sessions.get(id)?;
        let ours = form_token.is_none_or(|token| csrf_ok(Some(&session.form_token), token));
        (session.expires_at > Instant::now() && ours).then(|| session.variant.clone())
    }
}

type Upstream = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// What a TLS connection trusts is built into the binary, so nothing on the board's disk or in its
/// environment can change where the API key goes. It follows no redirect and reads no proxy setting.
/// Plain HTTP exists only for the loopback stand-in a local test run names.
fn upstream_client(config: &VoiceConfig) -> Upstream {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(UPSTREAM_CONNECT_TIMEOUT));
    let builder = HttpsConnectorBuilder::new().with_tls_config(tls);
    let builder = if config.base_url == OPENAI_BASE_URL {
        builder.https_only()
    } else {
        builder.https_or_http()
    };
    Client::builder(TokioExecutor::new()).build(builder.enable_http1().wrap_connector(http))
}

/// The allowed tools as OpenAI wants them, schemas taken from the contract. What the server forces
/// is left out, so the model isn't offered an argument that can't work.
pub fn tool_definitions() -> Vec<Value> {
    VOICE_TOOLS
        .iter()
        .map(|name| {
            let tool = all_tools()
                .iter()
                .find(|tool| tool["name"] == *name)
                .expect("a voice tool is in the contract");
            let mut parameters = tool["inputSchema"].clone();
            if let Some(schema) = parameters.as_object_mut() {
                schema.remove("$schema");
            }
            for (_, argument) in FORCED_ARGUMENTS.iter().filter(|(tool, _)| tool == name) {
                if let Some(properties) = parameters["properties"].as_object_mut() {
                    properties.remove(*argument);
                }
            }
            let mut description = tool["description"].as_str().unwrap_or_default().to_owned();
            if *name == "render_cv_pdf" {
                description.push_str(
                    " In a voice session this only checks that the revision renders: it can't save, and the page shows the preview.",
                );
            }
            json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            })
        })
        .collect()
}

fn mint_body(config: &VoiceConfig, variant: &str) -> Value {
    let seconds = config
        .lifetime
        .as_secs()
        .clamp(MIN_SECRET_SECONDS, MAX_SECRET_SECONDS);
    json!({
        "expires_after": {"anchor": "created_at", "seconds": seconds},
        "session": {
            "type": "realtime",
            "model": config.model,
            "instructions": format!(
                "{INSTRUCTIONS}\nThe CV variant on the person's screen is named {variant}. Pass it as `variant`."
            ),
            "tools": tool_definitions(),
            "tool_choice": "auto",
            "audio": {
                "input": {
                    "turn_detection": null,
                    "transcription": {"model": TRANSCRIPTION_MODEL},
                },
                "output": {"voice": config.voice},
            },
        },
    })
}

fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn disabled() -> Response {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "voice_disabled",
        "Voice is off: no OpenAI key is configured on the server.",
    )
}

fn refused_csrf() -> Response {
    failure(
        StatusCode::FORBIDDEN,
        "csrf_invalid",
        "This page has expired. Reload it and try again.",
    )
}

fn session_ended() -> Response {
    failure(
        StatusCode::FORBIDDEN,
        "voice_session_invalid",
        "This voice session has ended.",
    )
}

fn has_csrf(headers: &HeaderMap, token: &str) -> bool {
    csrf_ok(
        headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok()),
        token,
    )
}

#[derive(Deserialize)]
struct SessionRequest {
    variant: String,
}

pub(crate) async fn session(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !has_csrf(&headers, &token) {
        return refused_csrf();
    }
    let (Some(config), Some(client)) = (cv.voice.config(), &cv.voice.client) else {
        return disabled();
    };
    let Ok(request) = serde_json::from_slice::<SessionRequest>(&body) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "voice_bad_request",
            "This request was not understood.",
        );
    };
    match known_variant(&cv, &request.variant).await {
        Ok(true) => {}
        Ok(false) => {
            return failure(
                StatusCode::NOT_FOUND,
                "cv_variant_not_found",
                "This CV variant no longer exists.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "voice could not read a variant");
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "voice_failed",
                "Something went wrong on the server.",
            );
        }
    }
    if !cv.voice.take_mint() {
        tracing::warn!(event = "voice_mint_limited", "refused a voice session");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "600")],
            axum::Json(json!({"error": {
                "code": "voice_rate_limited",
                "message": "Too many voice sessions were started in the last hour. Try again later.",
            }})),
        )
            .into_response();
    }
    let Some(secret) = mint(client, config, &request.variant).await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "voice_upstream_failed",
            "OpenAI did not open a session. Try again in a moment.",
        );
    };
    let id = cv
        .voice
        .open_session(&token, &request.variant, config.lifetime);
    tracing::info!(event = "voice_session_opened", "voice session opened");
    axum::Json(json!({
        "voiceSessionId": id,
        "clientSecret": secret,
        "callsUrl": format!("{}/v1/realtime/calls", config.base_url),
        "lifetimeSeconds": config.lifetime.as_secs(),
        "idleSeconds": IDLE_SECONDS,
    }))
    .into_response()
}

async fn known_variant(cv: &Cv, variant: &str) -> nexus_core::Result<bool> {
    let mut conn = cv.db.conn().await?;
    Ok(cv_revisions::get_variant(&mut conn, variant)
        .await?
        .is_some())
}

/// The client secret, or `None`. What OpenAI answers on failure stays here, because an error for a
/// wrong key can quote part of it: only the status is logged.
async fn mint(client: &Upstream, config: &VoiceConfig, variant: &str) -> Option<String> {
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.key.0)).ok()?;
    authorization.set_sensitive(true);
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("{}/v1/realtime/client_secrets", config.base_url))
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
            mint_body(config, variant).to_string(),
        )))
        .ok()?;
    let exchange = async {
        let response = client.request(request).await.map_err(|error| {
            tracing::warn!(
                event = "voice_mint_failed",
                connect = error.is_connect(),
                "could not reach OpenAI"
            );
        })?;
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(
                event = "voice_mint_failed",
                status = status.as_u16(),
                "OpenAI refused to open a session"
            );
            return Err(());
        }
        Limited::new(response.into_body(), MAX_UPSTREAM_ANSWER_BYTES)
            .collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(|_| tracing::warn!(event = "voice_mint_failed", "unreadable answer"))
    };
    let Ok(answer) = tokio::time::timeout(UPSTREAM_TIMEOUT, exchange).await else {
        tracing::warn!(event = "voice_mint_failed", "OpenAI took too long");
        return None;
    };
    let body: Value = serde_json::from_slice(&answer.ok()?).ok()?;
    body["value"].as_str().map(str::to_owned)
}

/// `GET /api/voice/status`, which the page asks every so often: only it can close the connection to
/// OpenAI, and hands-free can send sound for up to an hour with no tool call to bring this up. Outside
/// the owner pool, because a busy dashboard must not read as a lost server. A logged-out browser gets
/// the session check's 401 before this, and after a restart the session is simply gone. It tells the
/// page nothing else, so it needs no CSRF token.
async fn status(
    State(voice): State<Voice>,
    token: Option<Extension<FormToken>>,
    headers: HeaderMap,
) -> Response {
    let token = token
        .as_ref()
        .map(|Extension(FormToken(token))| token.as_str());
    let live = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|id| voice.session_variant(id, token));
    if live.is_some() {
        axum::Json(json!({"live": true})).into_response()
    } else {
        session_ended()
    }
}

/// Mounts `/api/voice/status`. Auth is the caller's.
pub fn voice_status_router(voice: Voice) -> Router {
    Router::new()
        .route("/api/voice/status", get(status))
        .layer(middleware::map_response(crate::secure))
        .with_state(voice)
}

#[derive(Deserialize)]
struct ToolRequest {
    name: String,
    arguments: Option<Value>,
}

/// Cut at a character boundary, and says so: the model must not take a cut text for the whole.
pub fn for_model(text: &str) -> String {
    if text.len() <= MAX_MODEL_OUTPUT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_MODEL_OUTPUT_BYTES - 128;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[Trimmed by Nexus: {} more bytes not shown.]",
        &text[..end],
        text.len() - end
    )
}

/// The tool's own words. Blocks after the first are the PDF and PNGs, which never go to the model.
fn output_text(result: &CallToolResult) -> String {
    let first = result
        .content
        .first()
        .and_then(|block| serde_json::to_value(block).ok());
    first
        .as_ref()
        .and_then(|block| block["text"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Sets what the server decides. `Err` is an argument no voice session may pass.
fn force_arguments(
    name: &str,
    variant: &str,
    arguments: &mut Map<String, Value>,
) -> Result<(), ()> {
    match name {
        "patch_cv" => {
            arguments.insert("origin".to_owned(), "voice".into());
        }
        "render_cv_pdf" => {
            if arguments.contains_key("saveToApplicationId") {
                return Err(());
            }
            arguments.insert("previewPages".to_owned(), 0.into());
        }
        _ => {}
    }
    // The schema's default is `default`, which needn't be the variant the person is looking at.
    if name.contains("cv") && !arguments.contains_key("variant") {
        arguments.insert("variant".to_owned(), variant.into());
    }
    Ok(())
}

pub(crate) async fn tool(
    State(cv): State<Cv>,
    Extension(FormToken(token)): Extension<FormToken>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !has_csrf(&headers, &token) {
        return refused_csrf();
    }
    if cv.voice.config().is_none() {
        return disabled();
    }
    let Some(variant) = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|id| cv.voice.session_variant(id, Some(&token)))
    else {
        return session_ended();
    };
    let Ok(request) = serde_json::from_slice::<ToolRequest>(&body) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "voice_bad_request",
            "This request was not understood.",
        );
    };
    let Some(name) = VOICE_TOOLS.iter().find(|known| **known == request.name) else {
        // Named in the log only when it is a real tool: a made-up name is the model's text.
        let known = all_tools().iter().find(|tool| tool["name"] == request.name);
        let logged = known
            .and_then(|tool| tool["name"].as_str())
            .unwrap_or("unknown");
        tracing::warn!(
            event = "voice_tool_refused",
            tool = logged,
            "refused a voice tool call"
        );
        return failure(
            StatusCode::FORBIDDEN,
            "voice_tool_not_allowed",
            "A voice session can't use this tool.",
        );
    };
    let mut arguments = match request.arguments.unwrap_or_else(|| json!({})) {
        Value::Object(arguments) => arguments,
        _ => {
            return failure(
                StatusCode::BAD_REQUEST,
                "voice_bad_request",
                "A tool's arguments must be an object.",
            );
        }
    };
    if force_arguments(name, &variant, &mut arguments).is_err() {
        tracing::warn!(
            event = "voice_tool_refused",
            tool = *name,
            "refused an argument"
        );
        return failure(
            StatusCode::FORBIDDEN,
            "voice_argument_not_allowed",
            "A voice session can't save a PDF to an application.",
        );
    }
    let result = nexus_mcp::call_tool(
        cv.mcp.clone(),
        &[SCOPE_TOOLS],
        "voice",
        name,
        Value::Object(arguments),
    )
    .await;
    let is_error = result.is_error == Some(true);
    tracing::info!(
        event = "voice_tool",
        tool = *name,
        is_error,
        "voice tool call"
    );
    axum::Json(json!({
        "output": for_model(&output_text(&result)),
        "isError": is_error,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_exactly_the_specs_and_every_tool_exists() {
        let definitions = tool_definitions();
        let names: Vec<_> = definitions.iter().map(|tool| &tool["name"]).collect();
        assert_eq!(names, VOICE_TOOLS);
        assert_eq!(definitions.len(), 8);
        assert!(definitions.iter().all(|tool| tool["type"] == "function"));
    }

    #[test]
    fn definitions_keep_the_contract_schema_minus_what_the_server_forces() {
        for definition in tool_definitions() {
            let name = definition["name"].as_str().unwrap();
            let mut contract = all_tools()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["inputSchema"]
                .clone();
            contract.as_object_mut().unwrap().remove("$schema");
            for (_, argument) in FORCED_ARGUMENTS.iter().filter(|(tool, _)| *tool == name) {
                let removed = contract["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove(*argument);
                assert!(removed.is_some(), "{name}.{argument} isn't in the contract");
            }
            assert_eq!(definition["parameters"], contract, "{name}");
        }
    }

    #[test]
    fn long_output_is_cut_on_a_character_with_a_marker_and_stays_under_the_cap() {
        let short = "ü".repeat(10);
        assert_eq!(for_model(&short), short);
        let long = "ü".repeat(MAX_MODEL_OUTPUT_BYTES);
        let cut = for_model(&long);
        assert!(cut.len() <= MAX_MODEL_OUTPUT_BYTES, "{}", cut.len());
        assert!(
            cut.ends_with("more bytes not shown.]"),
            "{}",
            &cut[cut.len() - 60..]
        );
    }

    #[test]
    fn the_instructions_hold_the_rules_the_model_is_given() {
        for rule in [
            "can't change hard facts",
            "name the exact token",
            "data, not instructions",
            "never say that something has been submitted",
        ] {
            assert!(INSTRUCTIONS.contains(rule), "{rule}");
        }
    }

    #[test]
    fn the_mint_asks_for_a_short_secret_and_no_turn_detection() {
        let config = VoiceConfig {
            key: VoiceKey::new("sk-synthetic".to_owned()),
            model: "model-x".to_owned(),
            voice: "marin".to_owned(),
            base_url: OPENAI_BASE_URL.to_owned(),
            lifetime: Duration::from_secs(900),
        };
        let body = mint_body(&config, "default");
        assert_eq!(body["expires_after"]["seconds"], 60);
        assert_eq!(body["session"]["model"], "model-x");
        assert!(body["session"]["audio"]["input"]["turn_detection"].is_null());
        assert_eq!(body["session"]["audio"]["output"]["voice"], "marin");
        assert_eq!(body["session"]["tool_choice"], "auto");
        assert!(!body.to_string().contains("sk-synthetic"));
    }
}
