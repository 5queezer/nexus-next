//! The owner login that replaces better-auth: one passphrase, a server-side session, no accounts.

use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, COOKIE, REFERRER_POLICY, RETRY_AFTER, SET_COOKIE,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use url::Url;
use url::form_urlencoded::byte_serialize;

use crate::admin::verify_passphrase;
use crate::crypto::{escape_html, parse_params, random_bytes, secret_eq, sha256_hex, sign};
use crate::limit::GLOBAL_KEY;
use crate::state::{AuthState, ClientKey};

// `__Host-` makes browsers refuse these unless Secure, Path=/ and without Domain, so a sibling
// subdomain can't plant them.
const SESSION_COOKIE: &str = "__Host-nexus_session";
const LOGIN_CSRF_COOKIE: &str = "__Host-nexus_login_csrf";
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const LOGIN_CSRF_TTL_SECS: u64 = 3600;
/// Well above what one owner has open; the oldest session goes first.
const MAX_SESSIONS: usize = 32;
pub(crate) const MAX_NEXT_BYTES: usize = 8192;

const LOGIN_CSRF_PURPOSE: &str = "login-csrf";
const LOGOUT_CSRF_PURPOSE: &str = "logout-csrf";

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
        "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}"
    );
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
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>{}</title></head>
<body><main>{body}</main></body></html>"#,
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
<form method="post" action="/login"><input type="hidden" name="csrf" value="{csrf}">{next_field}<label>Passphrase <input type="password" name="passphrase" autocomplete="current-password" required autofocus></label> <button type="submit">Log in</button></form>"#
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
        |status, message| login_form(&state, status, message, next.as_deref(), csrf_cookie);

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

    let keys = [client.as_str(), GLOBAL_KEY];
    if let Err(retry) = state.0.login_limit.reserve(&keys, Instant::now()) {
        let mut response = respond(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Please wait and try again.",
        );
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry));
        return response;
    }

    let passphrase = form.get("passphrase").cloned().unwrap_or_default();
    let verified = {
        let _slot = state.0.hash_slot.acquire().await;
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            verify_passphrase(&state.0.passphrase_hash, &passphrase)
        })
        .await
        .unwrap_or(false)
    };
    if !verified {
        return respond(StatusCode::UNAUTHORIZED, "That passphrase is not correct.");
    }

    state.0.login_limit.succeed(&client);
    let session = create_session(&state);
    let response = redirect_after_login(next.as_deref());
    let response = with_cookie(response, SESSION_COOKIE, &session, SESSION_TTL.as_secs());
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
    with_cookie(
        Redirect::to("/login").into_response(),
        SESSION_COOKIE,
        "",
        0,
    )
}

/// Dashboard middleware: passes a logged-in owner through, sends anyone else to `/login`, returning
/// them to the same page afterwards if they were reading one.
pub async fn require_owner_session(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    if current_session(&state, request.headers()).is_some() {
        return next.run(request).await;
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
            trusted_proxy: None,
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
