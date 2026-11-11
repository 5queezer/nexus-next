//! The owner login that replaces better-auth: one passphrase, a server-side session, no accounts.

use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, REFERRER_POLICY, RETRY_AFTER,
    SET_COOKIE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde_json::json;
use url::Url;
use url::form_urlencoded::byte_serialize;

use nexus_core::auth::FormToken;
use nexus_core::repo::oauth;
use tokio::sync::OwnedSemaphorePermit;

use crate::admin::{
    MIN_PASSPHRASE_CHARS, PassphraseError, set_owner_passphrase, verify_passphrase,
};
use crate::crypto::{escape_html, parse_params, random_bytes, secret_eq, sha256_hex, sign};
use crate::limit::GLOBAL_KEY;
use crate::state::{AuthState, ClientKey};

// `__Host-` makes browsers refuse these unless Secure, Path=/ and without Domain, so a sibling
// subdomain can't plant them.
const SESSION_COOKIE: &str = "__Host-nexus_session";
const LOGIN_CSRF_COOKIE: &str = "__Host-nexus_login_csrf";
const DEVICE_COOKIE: &str = "__Host-nexus_device";
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const LOGIN_CSRF_TTL_SECS: u64 = 3600;
const DEVICE_TTL_SECS: u64 = 400 * 24 * 3600;
/// Well above what one owner has open; the oldest session goes first.
const MAX_SESSIONS: usize = 32;
pub(crate) const MAX_NEXT_BYTES: usize = 8192;

const LOGIN_CSRF_PURPOSE: &str = "login-csrf";
const LOGOUT_CSRF_PURPOSE: &str = "logout-csrf";
const FORM_CSRF_PURPOSE: &str = "form-csrf";
const PASSPHRASE_CSRF_PURPOSE: &str = "passphrase-csrf";
const DEVICE_PURPOSE: &str = "known-device";
/// The limiter bucket of browsers holding the device cookie.
const DEVICE_KEY: &str = "known-device";

/// Marks a browser that has logged in before. An attacker can hold the global backoff open with a
/// few requests, which would lock the owner out of re-authorizing clients, so a marked browser's
/// attempts count on a bucket of their own. It grants nothing else: the passphrase is still needed.
/// Keyed by the stored hash, it survives restarts and dies when the passphrase changes, except in
/// the browser that changed it, which gets the new one.
fn device_token(state: &AuthState) -> String {
    let hash = state.0.passphrase_hash.lock().unwrap();
    sign(hash.as_bytes(), DEVICE_PURPOSE, "")
}

pub(crate) async fn stylesheet() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("auth.css"),
    )
}

/// A page that can't be framed, cached or indexed. `form_target` is an extra origin forms may lead
/// to: Chrome applies `form-action` to redirects, so the consent form must name the client's.
///
/// `Referrer-Policy: no-referrer` makes browsers send `Origin: null` on these forms' POSTs, so the
/// handlers can't use an Origin check. CSRF tokens and SameSite do that job.
pub(crate) fn html_page(
    status: StatusCode,
    title: &str,
    body: &str,
    form_target: Option<&str>,
) -> Response {
    let form_action = form_target.map_or("'self'".to_owned(), |origin| format!("'self' {origin}"));
    let csp = format!(
        "default-src 'none'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}"
    );
    let label = nexus_core::env_label().map(escape_html);
    let (prefix, tag) = label.map_or_else(Default::default, |label| {
        (
            format!("[{label}] "),
            format!(r#" <span class="env">{label}</span>"#),
        )
    });
    (
        status,
        [
            (CACHE_CONTROL, "no-store".to_owned()),
            (CONTENT_SECURITY_POLICY, csp),
            (X_FRAME_OPTIONS, "DENY".to_owned()),
            (REFERRER_POLICY, "no-referrer".to_owned()),
            (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        Html(format!(
            r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><link rel="stylesheet" href="/auth.css"><title>{prefix}{}</title></head>
<body><main><p class="brand">Nexus{tag}</p>{body}</main></body></html>"#,
            escape_html(title)
        )),
    )
        .into_response()
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn with_cookie(mut response: Response, name: &str, value: &str, max_age: u64) -> Response {
    let cookie =
        format!("{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax");
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie names and values here are ASCII"),
    );
    response
}

/// The hash identifying the caller's live session, if any. Sessions are looked up by hash, so no
/// comparison runs against the raw id.
pub(crate) fn current_session(state: &AuthState, headers: &HeaderMap) -> Option<String> {
    let hash = sha256_hex(cookie_value(headers, SESSION_COOKIE)?);
    let now = Instant::now();
    let mut sessions = state.0.sessions.lock().unwrap();
    match sessions.get(&hash) {
        Some(expiry) if now < *expiry => Some(hash),
        Some(_) => {
            sessions.remove(&hash);
            None
        }
        None => None,
    }
}

fn create_session(state: &AuthState) -> String {
    let id = hex::encode(random_bytes());
    let now = Instant::now();
    let mut sessions = state.0.sessions.lock().unwrap();
    sessions.retain(|_, expiry| now < *expiry);
    if sessions.len() >= MAX_SESSIONS {
        let oldest = sessions
            .iter()
            .min_by_key(|(_, expiry)| **expiry)
            .map(|(hash, _)| hash.clone());
        if let Some(oldest) = oldest {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(sha256_hex(&id), now + SESSION_TTL);
    id
}

/// `next` as a same-origin path, or `None`. Printable ASCII only: browsers strip tabs and newlines
/// before parsing a URL, which would turn `/\t/host` into `//host`, and treat `\` as `/`.
fn safe_next(base_url: &str, next: &str) -> Option<String> {
    if next.len() > MAX_NEXT_BYTES
        || !next.starts_with('/')
        || next.starts_with("//")
        || next.contains('\\')
        || !next.bytes().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    let base = Url::parse(base_url).ok()?;
    (base.join(next).ok()?.origin() == base.origin()).then(|| next.to_owned())
}

/// The wait as a person reads it, rounded up so that trying again at that moment works.
fn wait_text(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds} second{}", if seconds == 1 { "" } else { "s" }),
        _ => {
            let minutes = seconds.div_ceil(60);
            format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
        }
    }
}

fn login_form(
    state: &AuthState,
    status: StatusCode,
    message: &str,
    next: Option<&str>,
    csrf_cookie: Option<&str>,
) -> Response {
    let cookie = csrf_cookie
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .map_or_else(|| hex::encode(random_bytes()), str::to_owned);
    let csrf = sign(&state.0.key, LOGIN_CSRF_PURPOSE, &cookie);
    let next_field = next.map_or(String::new(), |next| {
        format!(
            r#"<input type="hidden" name="next" value="{}">"#,
            escape_html(next)
        )
    });
    let message = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<p role="alert">{}</p>"#, escape_html(message))
    };
    let page = html_page(
        status,
        "Log in to Nexus",
        &format!(
            r#"<h1>Log in to Nexus</h1>{message}
<form method="post" action="/login"><input type="hidden" name="csrf" value="{csrf}">{next_field}<label for="passphrase">Passphrase</label><input id="passphrase" type="password" name="passphrase" autocomplete="current-password" required autofocus><button type="submit">Log in</button></form>"#
        ),
        None,
    );
    with_cookie(page, LOGIN_CSRF_COOKIE, &cookie, LOGIN_CSRF_TTL_SECS)
}

fn redirect_after_login(next: Option<&str>) -> Response {
    (
        [(CACHE_CONTROL, "no-store")],
        Redirect::to(next.unwrap_or("/")),
    )
        .into_response()
}

pub(crate) async fn login_page(
    State(state): State<AuthState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let params = parse_params(uri.query().unwrap_or("").as_bytes());
    let next = params
        .get("next")
        .and_then(|next| safe_next(&state.0.base_url, next));
    if current_session(&state, &headers).is_some() {
        return redirect_after_login(next.as_deref());
    }
    login_form(
        &state,
        StatusCode::OK,
        "",
        next.as_deref(),
        cookie_value(&headers, LOGIN_CSRF_COOKIE),
    )
}

/// One attempt at the owner passphrase, counted by the login backoff. `Ok` holds the hash slot, so
/// a change can follow before anyone else's attempt is checked. `Err` carries the wait if the
/// backoff refused the attempt, and `None` if the passphrase was wrong.
async fn attempt(
    state: &AuthState,
    client: &str,
    known_device: bool,
    passphrase: String,
) -> Result<OwnedSemaphorePermit, Option<u64>> {
    let source = if known_device { DEVICE_KEY } else { client };
    let keys: &[&str] = if known_device {
        &[source]
    } else {
        &[source, GLOBAL_KEY]
    };
    if let Err(retry) = state.0.login_limit.reserve(keys, Instant::now()) {
        tracing::warn!(
            event = "login_lockout",
            source = %client,
            known_device,
            retry_after = retry,
            "login refused by backoff"
        );
        return Err(Some(retry));
    }

    let slot = state.0.hash_slot.clone().acquire_owned().await;
    let slot = slot.map_err(|_| None)?;
    let hash = state.0.passphrase_hash.lock().unwrap().clone();
    let verified = tokio::task::spawn_blocking(move || verify_passphrase(&hash, &passphrase))
        .await
        .unwrap_or(false);
    if !verified {
        tracing::warn!(
            event = "login_failure",
            source = %client,
            known_device,
            "wrong owner passphrase"
        );
        return Err(None);
    }
    state.0.login_limit.succeed(source, !known_device);
    Ok(slot)
}

impl AuthState {
    /// The passphrase, asked again by a form outside this crate before it changes a secret, so that
    /// a stolen session alone can't. A guess counts on the login backoff like any other. `Err` is
    /// the status and the words for the person.
    pub async fn confirm_passphrase(
        &self,
        parts: &mut Parts,
        passphrase: String,
    ) -> Result<(), (StatusCode, String)> {
        let Ok(ClientKey(client)) = ClientKey::from_request_parts(parts, self).await;
        let known_device = cookie_value(&parts.headers, DEVICE_COOKIE)
            .is_some_and(|cookie| secret_eq(cookie, &device_token(self)));
        match attempt(self, &client, known_device, passphrase).await {
            Ok(_) => Ok(()),
            Err(Some(retry)) => Err((
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many attempts. Try again in {}.", wait_text(retry)),
            )),
            Err(None) => Err((
                StatusCode::UNAUTHORIZED,
                "The passphrase is not correct.".to_owned(),
            )),
        }
    }
}

fn locked_out(respond: impl Fn(StatusCode, &str) -> Response, retry: u64) -> Response {
    let mut response = respond(
        StatusCode::TOO_MANY_REQUESTS,
        &format!("Too many attempts. Try again in {}.", wait_text(retry)),
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry));
    response
}

pub(crate) async fn login(
    State(state): State<AuthState>,
    ClientKey(client): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let form = parse_params(&body);
    let next = form
        .get("next")
        .and_then(|next| safe_next(&state.0.base_url, next));
    let csrf_cookie = cookie_value(&headers, LOGIN_CSRF_COOKIE);
    let respond =
        |status, message: &str| login_form(&state, status, message, next.as_deref(), csrf_cookie);

    // A forged POST never reaches the hash or the limiter, so it can't lock the owner out. A stale
    // form (the key changes on restart) lands here too, and gets a fresh one.
    let csrf_valid = csrf_cookie
        .zip(form.get("csrf"))
        .is_some_and(|(cookie, csrf)| {
            secret_eq(csrf, &sign(&state.0.key, LOGIN_CSRF_PURPOSE, cookie))
        });
    if !csrf_valid {
        return respond(
            StatusCode::FORBIDDEN,
            "Your login form expired. Please try again.",
        );
    }

    let known_device = cookie_value(&headers, DEVICE_COOKIE)
        .is_some_and(|cookie| secret_eq(cookie, &device_token(&state)));
    let passphrase = form.get("passphrase").cloned().unwrap_or_default();
    match attempt(&state, &client, known_device, passphrase).await {
        Ok(_) => {}
        Err(Some(retry)) => return locked_out(respond, retry),
        Err(None) => {
            return respond(StatusCode::UNAUTHORIZED, "That passphrase is not correct.");
        }
    }

    tracing::info!(event = "login_success", source = %client, known_device, "owner logged in");
    let session = create_session(&state);
    let response = redirect_after_login(next.as_deref());
    let response = with_cookie(response, SESSION_COOKIE, &session, SESSION_TTL.as_secs());
    let response = with_cookie(
        response,
        DEVICE_COOKIE,
        &device_token(&state),
        DEVICE_TTL_SECS,
    );
    with_cookie(response, LOGIN_CSRF_COOKIE, "", 0)
}

pub(crate) async fn logout_page(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let Some(session) = current_session(&state, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let csrf = sign(&state.0.key, LOGOUT_CSRF_PURPOSE, &session);
    html_page(
        StatusCode::OK,
        "Log out of Nexus",
        &format!(
            r#"<h1>Log out of Nexus?</h1>
<form method="post" action="/logout"><input type="hidden" name="csrf" value="{csrf}"><button type="submit">Log out</button></form>"#
        ),
        None,
    )
}

pub(crate) async fn logout(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = current_session(&state, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let form = parse_params(&body);
    let csrf_valid = form
        .get("csrf")
        .is_some_and(|csrf| secret_eq(csrf, &sign(&state.0.key, LOGOUT_CSRF_PURPOSE, &session)));
    if !csrf_valid {
        return html_page(
            StatusCode::FORBIDDEN,
            "Log out of Nexus",
            r#"<h1>Your request expired</h1><p><a href="/logout">Try again</a></p>"#,
            None,
        );
    }
    state.0.sessions.lock().unwrap().remove(&session);
    state.0.logouts.send_modify(|count| *count += 1);
    with_cookie(
        Redirect::to("/login").into_response(),
        SESSION_COOKIE,
        "",
        0,
    )
}

fn passphrase_form(
    state: &AuthState,
    session: &str,
    status: StatusCode,
    message: &str,
) -> Response {
    let title = "Change the passphrase";
    if state.0.passphrase_file.is_none() {
        return html_page(
            status,
            title,
            r#"<h1>Change the passphrase</h1><p>This server takes its passphrase from <code>NEXUS_OWNER_PASSPHRASE_HASH</code>, so it can only be changed on the host.</p><p><a href="/">Back to Nexus</a></p>"#,
            None,
        );
    }
    let csrf = sign(&state.0.key, PASSPHRASE_CSRF_PURPOSE, session);
    let message = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<p role="alert">{}</p>"#, escape_html(message))
    };
    html_page(
        status,
        title,
        &format!(
            r#"<h1>Change the passphrase</h1>{message}
<p>This signs out every connected client, such as Claude.ai and other MCP clients, and revokes the API token. Each client must be authorized again. Every browser session ends too, this one included, and you log in with the new passphrase.</p>
<form method="post" action="/passphrase"><input type="hidden" name="csrf" value="{csrf}"><label for="current">Current passphrase</label><input id="current" type="password" name="current" autocomplete="current-password" required autofocus><label for="new">New passphrase</label><input id="new" type="password" name="new" autocomplete="new-password" minlength="{MIN_PASSPHRASE_CHARS}" required><label for="repeat">Repeat the new passphrase</label><input id="repeat" type="password" name="repeat" autocomplete="new-password" required><button type="submit">Change passphrase</button></form>
<p class="hint">At least {MIN_PASSPHRASE_CHARS} characters. <a href="/">Cancel</a></p>"#
        ),
        None,
    )
}

pub(crate) async fn passphrase_page(
    State(state): State<AuthState>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = current_session(&state, &headers) else {
        return Redirect::to("/login?next=/passphrase").into_response();
    };
    passphrase_form(&state, &session, StatusCode::OK, "")
}

/// Changes the passphrase for a logged-in owner who also knows the current one, so a stolen
/// session alone can't. Guesses at the current one count on the login backoff. Every session ends,
/// this one included: the owner logs in again, which shows that the new passphrase works.
pub(crate) async fn change_passphrase(
    State(state): State<AuthState>,
    ClientKey(client): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = current_session(&state, &headers) else {
        return Redirect::to("/login").into_response();
    };
    let form = parse_params(&body);
    let respond = |status, message: &str| passphrase_form(&state, &session, status, message);
    let csrf_valid = form.get("csrf").is_some_and(|csrf| {
        secret_eq(csrf, &sign(&state.0.key, PASSPHRASE_CSRF_PURPOSE, &session))
    });
    if !csrf_valid {
        return respond(
            StatusCode::FORBIDDEN,
            "Your form expired. Please try again.",
        );
    }
    let Some(path) = &state.0.passphrase_file else {
        return respond(StatusCode::CONFLICT, "");
    };
    let field = |name| form.get(name).cloned().unwrap_or_default();
    let new = field("new");
    if new != field("repeat") {
        return respond(
            StatusCode::BAD_REQUEST,
            "The two new passphrases differ. Nothing was changed.",
        );
    }

    let known_device = cookie_value(&headers, DEVICE_COOKIE)
        .is_some_and(|cookie| secret_eq(cookie, &device_token(&state)));
    let slot = match attempt(&state, &client, known_device, field("current")).await {
        Ok(slot) => slot,
        Err(Some(retry)) => return locked_out(respond, retry),
        Err(None) => {
            return respond(
                StatusCode::UNAUTHORIZED,
                "The current passphrase is not correct. Nothing was changed.",
            );
        }
    };
    // In a task of its own, so that a client who leaves, or the deadline of the pool, can't stop
    // the change between the file and the memory of this process.
    let change = {
        let (state, path) = (state.clone(), path.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let hash = set_owner_passphrase(&state.0.db, &path, &new).await?;
            *state.0.passphrase_hash.lock().unwrap() = hash;
            state.0.sessions.lock().unwrap().clear();
            state.0.logouts.send_modify(|count| *count += 1);
            tracing::warn!(
                event = "passphrase_changed",
                source = %client,
                "owner passphrase changed in the browser: all sessions ended"
            );
            // A session that was still open could approve a client after the first revocation.
            let again = async {
                let mut tx = state.0.db.begin_write().await?;
                oauth::revoke_all(&mut tx).await?;
                tx.commit().await.map_err(nexus_core::error::Error::from)
            };
            if let Err(error) = again.await {
                tracing::error!(event = "passphrase_change_failed", %error, "could not revoke again after ending the sessions");
            }
            Ok(())
        })
    };
    match change.await.unwrap_or(Err(PassphraseError::Task)) {
        Ok(()) => {}
        Err(
            e
            @ (PassphraseError::TooShort | PassphraseError::Repetitive | PassphraseError::TooLong),
        ) => {
            return respond(
                StatusCode::BAD_REQUEST,
                &format!("The new {e}. Nothing was changed."),
            );
        }
        Err(e) => {
            tracing::error!(event = "passphrase_change_failed", error = %e, "could not change the owner passphrase");
            return respond(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The passphrase was not changed. Connected clients may have been signed out. The server log says why.",
            );
        }
    };
    let response = with_cookie(
        html_page(
            StatusCode::OK,
            "Passphrase changed",
            r#"<h1>Passphrase changed</h1><p>Every session has ended and every connected client is signed out. Authorize each client again when it asks.</p><p><a href="/login">Log in with the new passphrase</a></p>"#,
            None,
        ),
        SESSION_COOKIE,
        "",
        0,
    );
    // The old mark died with the old hash, and this browser has just shown the passphrase.
    with_cookie(
        response,
        DEVICE_COOKIE,
        &device_token(&state),
        DEVICE_TTL_SECS,
    )
}

/// Dashboard middleware: passes a logged-in owner through with their session's `FormToken`, sends anyone else to `/login`, returning
/// them to the same page afterwards if they were reading one. `/api/*` callers get a 401 JSON error
/// instead, since a script can't follow a login page.
pub async fn require_owner_session(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(session) = current_session(&state, request.headers()) {
        let token = sign(&state.0.key, FORM_CSRF_PURPOSE, &session);
        request.extensions_mut().insert(FormToken(token));
        return next.run(request).await;
    }
    if request.uri().path().starts_with("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            [
                (CACHE_CONTROL, "no-store"),
                (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            Json(json!({ "error": "Not logged in" })),
        )
            .into_response();
    }
    let target = request
        .uri()
        .path_and_query()
        .map_or("/", |target| target.as_str());
    match request.method() {
        &Method::GET | &Method::HEAD if safe_next(&state.0.base_url, target).is_some() => {
            let target: String = byte_serialize(target.as_bytes()).collect();
            Redirect::to(&format!("/login?next={target}")).into_response()
        }
        _ => Redirect::to("/login").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::db::Db;

    use super::*;
    use crate::AuthConfig;

    const BASE: &str = "https://crm.example.test";

    #[test]
    fn safe_next_accepts_only_local_paths() {
        for good in [
            "/",
            "/applications?page=2",
            "/api/mcp/authorize?a=b%20c&d=/x",
        ] {
            assert_eq!(safe_next(BASE, good).as_deref(), Some(good));
        }
        for bad in [
            "",
            "applications",
            "//evil.example.test",
            "//evil.example.test/path",
            "/\\evil.example.test",
            "\\\\evil.example.test",
            "/\t/evil.example.test",
            "/\n/evil.example.test",
            " /x",
            "/x y",
            "https://evil.example.test",
            "javascript:alert(1)",
            "/caf\u{e9}",
        ] {
            assert_eq!(safe_next(BASE, bad), None, "{bad:?}");
        }
        assert_eq!(
            safe_next(BASE, &format!("/{}", "a".repeat(MAX_NEXT_BYTES))),
            None
        );
    }

    #[test]
    fn waits_are_worded_for_people() {
        assert_eq!(wait_text(1), "1 second");
        assert_eq!(wait_text(59), "59 seconds");
        assert_eq!(wait_text(60), "1 minute");
        assert_eq!(wait_text(61), "2 minutes");
        assert_eq!(wait_text(3600), "60 minutes");
    }

    #[test]
    fn cookies_are_read_from_any_header_and_exact_name() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("x_a=1; a=2"));
        headers.append(COOKIE, HeaderValue::from_static("b=3"));
        assert_eq!(cookie_value(&headers, "a"), Some("2"));
        assert_eq!(cookie_value(&headers, "b"), Some("3"));
        assert_eq!(cookie_value(&headers, "c"), None);
    }

    async fn state() -> (AuthState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        let config = AuthConfig {
            public_base_url: BASE.to_owned(),
            owner_passphrase_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA"
                .to_owned(),
            owner_passphrase_file: None,
            trusted_proxy: None,
            redirect_hosts: Vec::new(),
        };
        (AuthState::new(db, config).unwrap(), dir)
    }

    fn cookie_headers(id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE}={id}")).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn the_device_mark_survives_restarts_and_dies_with_the_passphrase() {
        let (first, dir) = state().await;
        let db = Db::open(dir.path()).await.unwrap();
        let config = |hash: &str| AuthConfig {
            public_base_url: BASE.to_owned(),
            owner_passphrase_hash: hash.to_owned(),
            owner_passphrase_file: None,
            trusted_proxy: None,
            redirect_hosts: Vec::new(),
        };
        let restarted =
            AuthState::new(db.clone(), config(&first.0.passphrase_hash.lock().unwrap())).unwrap();
        assert_ne!(first.0.key, restarted.0.key);
        assert_eq!(device_token(&first), device_token(&restarted));

        let rotated = AuthState::new(
            db,
            config("$argon2id$v=19$m=8,t=1,p=1$b3RoZXJzYWx0$AAAAAAAAAAAAAAAAAAAAAA"),
        )
        .unwrap();
        assert_ne!(device_token(&first), device_token(&rotated));
    }

    #[tokio::test]
    async fn an_expired_session_is_refused_and_removed() {
        let (state, _dir) = state().await;
        let id = create_session(&state);
        let headers = cookie_headers(&id);
        assert!(current_session(&state, &headers).is_some());

        let hash = sha256_hex(&id);
        state
            .0
            .sessions
            .lock()
            .unwrap()
            .insert(hash.clone(), Instant::now());
        assert!(current_session(&state, &headers).is_none());
        assert!(!state.0.sessions.lock().unwrap().contains_key(&hash));
    }

    #[tokio::test]
    async fn sessions_are_stored_hashed_and_capped_oldest_first() {
        let (state, _dir) = state().await;
        let first = create_session(&state);
        assert!(!state.0.sessions.lock().unwrap().contains_key(&first));
        for _ in 0..MAX_SESSIONS {
            create_session(&state);
        }
        assert_eq!(state.0.sessions.lock().unwrap().len(), MAX_SESSIONS);
        assert!(current_session(&state, &cookie_headers(&first)).is_none());
    }
}
