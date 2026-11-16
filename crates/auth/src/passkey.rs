//! Passkey login (WebAuthn) beside the passphrase, which stays the fallback and the way back in.
//!
//! The owner adds passkeys at `/login/passkeys` while logged in, giving the passphrase again so that
//! a stolen session alone can't add a lasting way in. Everything lives under `/login/`, on the
//! public origin: a passkey belongs to the host it was made on. A ceremony's state stays on the
//! server, for a few minutes and one use.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nexus_core::repo::owner_login::{self, OwnerPasskey};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Uuid, Webauthn, WebauthnBuilder,
};

use crate::crypto::{escape_html, parse_params, random_bytes, secret_eq, sha256_hex, sign};
use crate::session::{
    attempt, attempt_succeeded, cookie_value, create_session, current_passphrase_tag,
    current_session, html_page_with_passkeys, known_device, logged_in, origin_of, reserve_attempt,
    safe_next, with_cookie,
};
use crate::state::{AuthState, ClientKey};

const CEREMONY_TTL: Duration = Duration::from_secs(300);
/// Unauthenticated starts beyond this push out the oldest; the owner's own is seconds old.
const MAX_LOGINS: usize = 32;
const MAX_PASSKEYS: usize = 16;
const MAX_NAME_CHARS: usize = 64;
const LOGIN_COOKIE: &str = "__Host-nexus_passkey";
const PASSKEYS_CSRF_PURPOSE: &str = "passkeys-csrf";
/// The owner's WebAuthn user handle. There is one owner, so it is a constant.
const OWNER_HANDLE: Uuid = Uuid::from_u128(0x6e65_7875_735f_6f77_6e65_7200_0000_0001);

pub(crate) struct Ceremonies {
    /// `None` when the base URL can't be a WebAuthn origin; the routes then say so.
    webauthn: Option<Webauthn>,
    /// By session hash: the name the owner gave and the state of the registration.
    registrations: Mutex<HashMap<String, (Instant, String, PasskeyRegistration)>>,
    /// By the hash of the `LOGIN_COOKIE` value.
    logins: Mutex<HashMap<String, (Instant, PasskeyAuthentication)>>,
}

impl Ceremonies {
    pub(crate) fn new(base_url: &str) -> Self {
        let webauthn = Url::parse(base_url).ok().and_then(|origin| {
            let rp_id = origin.host_str()?.to_owned();
            WebauthnBuilder::new(&rp_id, &origin)
                .ok()?
                .rp_name("Nexus")
                .build()
                .ok()
        });
        Self {
            webauthn,
            registrations: Mutex::default(),
            logins: Mutex::default(),
        }
    }
}

fn reply(status: StatusCode, value: Value) -> Response {
    (
        status,
        [
            (CACHE_CONTROL, "no-store"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Json(value),
    )
        .into_response()
}

fn refused(status: StatusCode, code: &str, message: &str) -> Response {
    reply(status, json!({ "error": code, "message": message }))
}

fn unavailable() -> Response {
    refused(
        StatusCode::NOT_FOUND,
        "passkeys_unavailable",
        "This server's address can't be used for passkeys.",
    )
}

fn failed(error: impl std::fmt::Display) -> Response {
    tracing::error!(event = "passkey_failed", %error, "a passkey request failed");
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "passkey_failed",
        "That did not work. The server log says why.",
    )
}

fn too_many(retry: u64) -> Response {
    let mut response = refused(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        "Too many attempts. Wait a little and try again.",
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry));
    response
}

fn credential_id(passkey: &Passkey) -> String {
    URL_SAFE_NO_PAD.encode::<&[u8]>(passkey.cred_id().as_ref())
}

fn to_json(passkey: &Passkey) -> String {
    serde_json::to_string(passkey).expect("a derived struct with string keys always serializes")
}

/// The stored passkeys with their credentials. One that can't be read is logged and left out.
async fn stored(state: &AuthState) -> nexus_core::error::Result<Vec<(OwnerPasskey, Passkey)>> {
    let mut conn = state.0.db.conn().await?;
    let rows = owner_login::passkeys(&mut conn).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| match serde_json::from_str(&row.passkey) {
            Ok(passkey) => Some((row, passkey)),
            Err(error) => {
                tracing::error!(event = "passkey_unreadable", name = %row.name, %error, "a stored passkey can't be read");
                None
            }
        })
        .collect())
}

pub(crate) async fn script() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("passkey.js"),
    )
}

// ── Logging in ──────────────────────────────────────────────────────────────

pub(crate) async fn login_start(
    State(state): State<AuthState>,
    ClientKey(source): ClientKey,
) -> Response {
    if let Err(retry) = state.0.authorize_limit.check(&source, Instant::now()) {
        return too_many(retry);
    }
    let Some(webauthn) = &state.0.passkeys.webauthn else {
        return unavailable();
    };
    let passkeys: Vec<Passkey> = match stored(&state).await {
        Ok(found) => found.into_iter().map(|(_, passkey)| passkey).collect(),
        Err(error) => return failed(error),
    };
    if passkeys.is_empty() {
        return refused(
            StatusCode::NOT_FOUND,
            "no_passkeys",
            "No passkey is registered. Log in with the passphrase.",
        );
    }
    let (options, ceremony) = match webauthn.start_passkey_authentication(&passkeys) {
        Ok(started) => started,
        Err(error) => return failed(error),
    };
    let id = hex::encode(random_bytes());
    {
        let now = Instant::now();
        let mut logins = state.0.passkeys.logins.lock().unwrap();
        logins.retain(|_, (at, _)| now.duration_since(*at) < CEREMONY_TTL);
        if logins.len() >= MAX_LOGINS {
            let oldest = logins
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                logins.remove(&oldest);
            }
        }
        logins.insert(sha256_hex(&id), (now, ceremony));
    }
    with_cookie(
        reply(StatusCode::OK, json!(options)),
        LOGIN_COOKIE,
        &id,
        CEREMONY_TTL.as_secs(),
    )
}

#[derive(Deserialize)]
struct LoginFinish {
    credential: PublicKeyCredential,
    #[serde(default)]
    remember: bool,
    #[serde(default)]
    next: String,
}

/// Needs no CSRF token: only a page of this origin can get the browser to sign the challenge.
pub(crate) async fn login_finish(
    State(state): State<AuthState>,
    ClientKey(client): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(webauthn) = &state.0.passkeys.webauthn else {
        return unavailable();
    };
    let ceremony = cookie_value(&headers, LOGIN_COOKIE)
        .and_then(|id| {
            let mut logins = state.0.passkeys.logins.lock().unwrap();
            logins.remove(&sha256_hex(id))
        })
        .filter(|(at, _)| at.elapsed() < CEREMONY_TTL);
    let Some((_, ceremony)) = ceremony else {
        return refused(
            StatusCode::FORBIDDEN,
            "passkey_expired",
            "This passkey prompt expired. Try again.",
        );
    };
    let Ok(finish) = serde_json::from_slice::<LoginFinish>(&body) else {
        return refused(
            StatusCode::BAD_REQUEST,
            "passkey_bad_request",
            "The browser sent something unexpected.",
        );
    };

    let known_device = known_device(&state, &headers);
    if let Err(retry) = reserve_attempt(&state, &client, known_device) {
        return too_many(retry);
    }
    let result = match webauthn.finish_passkey_authentication(&finish.credential, &ceremony) {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(event = "login_failure", source = %client, known_device, method = "passkey", %error, "passkey refused");
            return refused(
                StatusCode::UNAUTHORIZED,
                "passkey_refused",
                "That passkey was not accepted.",
            );
        }
    };
    let id = URL_SAFE_NO_PAD.encode::<&[u8]>(result.cred_id().as_ref());
    let found = match stored(&state).await {
        Ok(found) => found,
        Err(error) => return failed(error),
    };
    // Removed since the prompt was shown.
    let Some((_, mut passkey)) = found.into_iter().find(|(row, _)| row.credential_id == id) else {
        tracing::warn!(event = "login_failure", source = %client, known_device, method = "passkey", "passkey no longer registered");
        return refused(
            StatusCode::UNAUTHORIZED,
            "passkey_refused",
            "That passkey was not accepted.",
        );
    };
    let updated = (passkey.update_credential(&result) == Some(true)).then(|| to_json(&passkey));
    let recorded = async {
        let mut conn = state.0.db.conn().await?;
        owner_login::passkey_used(&mut conn, &id, updated.as_deref()).await
    };
    if let Err(error) = recorded.await {
        return failed(error);
    }

    attempt_succeeded(&state, &client, known_device);
    let tag = current_passphrase_tag(&state);
    let (session, max_age) = match create_session(&state, finish.remember, &tag).await {
        Ok(session) => session,
        Err(error) => return failed(error),
    };
    tracing::info!(event = "login_success", source = %client, known_device, remembered = finish.remember, method = "passkey", "owner logged in");
    let next = safe_next(&state.0.base_url, &finish.next).unwrap_or_else(|| "/".to_owned());
    let response = logged_in(
        &state,
        reply(StatusCode::OK, json!({ "redirect": next })),
        &session,
        max_age,
    );
    with_cookie(response, LOGIN_COOKIE, "", 0)
}

// ── Managing passkeys ───────────────────────────────────────────────────────

fn csrf_for(state: &AuthState, session: &str) -> String {
    sign(&state.0.key, PASSKEYS_CSRF_PURPOSE, session)
}

fn csrf_valid(state: &AuthState, session: &str, csrf: &str) -> bool {
    secret_eq(csrf, &csrf_for(state, session))
}

async fn manage(state: &AuthState, session: &str, status: StatusCode, message: &str) -> Response {
    let found = async {
        let mut conn = state.0.db.conn().await?;
        owner_login::passkeys(&mut conn).await
    };
    let passkeys = match found.await {
        Ok(passkeys) => passkeys,
        Err(error) => {
            tracing::error!(event = "passkey_failed", %error, "could not list passkeys");
            Vec::new()
        }
    };
    let csrf = csrf_for(state, session);
    let list: String = passkeys
        .iter()
        .map(|passkey| {
            let used = passkey
                .last_used_at
                .as_ref()
                .map_or_else(|| "never used".to_owned(), |at| format!("last used {}", &at.as_str()[..10]));
            format!(
                r#"<li><strong><bdi>{name}</bdi></strong> <span class="hint">added {added}, {used}</span><form method="post" action="/login/passkeys/delete"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="id" value="{id}"><button type="submit" class="secondary">Remove</button></form></li>"#,
                name = escape_html(&passkey.name),
                added = &passkey.created_at.as_str()[..10],
                id = escape_html(&passkey.credential_id),
            )
        })
        .collect();
    let list = if list.is_empty() {
        "<p>No passkey yet.</p>".to_owned()
    } else {
        format!(r#"<ul class="passkeys">{list}</ul>"#)
    };
    let message = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<p role="alert">{}</p>"#, escape_html(message))
    };
    html_page_with_passkeys(
        status,
        "Passkeys",
        &format!(
            r#"<h1>Passkeys</h1>{message}
<p>A passkey logs you in with a fingerprint, face or your phone instead of the passphrase. The passphrase keeps working, and is how you get back in without your passkeys.</p>
{list}
<h2>Add a passkey</h2>
<div id="passkey-add" data-csrf="{csrf}" data-origin="{origin}" hidden><label for="passkey-name">Name</label><input id="passkey-name" type="text" maxlength="{MAX_NAME_CHARS}" value="This device" required><label for="passkey-passphrase">Passphrase</label><input id="passkey-passphrase" type="password" autocomplete="current-password" required><button type="button">Add a passkey</button><p id="passkey-status" role="status"></p></div>
<noscript><p>Adding a passkey needs JavaScript.</p></noscript>
<p class="hint"><a href="/">Back to Nexus</a></p>"#,
            origin = escape_html(&origin_of(&state.0.base_url)),
        ),
    )
}

pub(crate) async fn manage_page(State(state): State<AuthState>, headers: HeaderMap) -> Response {
    let Some(session) = current_session(&state, &headers).await else {
        return Redirect::to("/login?next=/login/passkeys").into_response();
    };
    manage(&state, &session, StatusCode::OK, "").await
}

pub(crate) async fn delete(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = current_session(&state, &headers).await else {
        return Redirect::to("/login?next=/login/passkeys").into_response();
    };
    let form = parse_params(&body);
    if !form
        .get("csrf")
        .is_some_and(|csrf| csrf_valid(&state, &session, csrf))
    {
        return manage(
            &state,
            &session,
            StatusCode::FORBIDDEN,
            "Your form expired. Please try again.",
        )
        .await;
    }
    let id = form.get("id").cloned().unwrap_or_default();
    let removed = async {
        let mut conn = state.0.db.conn().await?;
        owner_login::delete_passkey(&mut conn, &id).await
    };
    match removed.await {
        Ok(true) => {
            tracing::warn!(event = "passkey_removed", "owner removed a passkey");
            Redirect::to("/login/passkeys").into_response()
        }
        Ok(false) => {
            manage(
                &state,
                &session,
                StatusCode::NOT_FOUND,
                "That passkey was already removed.",
            )
            .await
        }
        Err(error) => {
            tracing::error!(event = "passkey_failed", %error, "could not remove a passkey");
            manage(
                &state,
                &session,
                StatusCode::INTERNAL_SERVER_ERROR,
                "The passkey was not removed. The server log says why.",
            )
            .await
        }
    }
}

#[derive(Deserialize)]
struct RegisterStart {
    csrf: String,
    passphrase: String,
    name: String,
}

pub(crate) async fn register_start(
    State(state): State<AuthState>,
    ClientKey(client): ClientKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = current_session(&state, &headers).await else {
        return refused(StatusCode::UNAUTHORIZED, "not_logged_in", "Log in first.");
    };
    let Some(webauthn) = &state.0.passkeys.webauthn else {
        return unavailable();
    };
    let Ok(start) = serde_json::from_slice::<RegisterStart>(&body) else {
        return refused(
            StatusCode::BAD_REQUEST,
            "passkey_bad_request",
            "The browser sent something unexpected.",
        );
    };
    if !csrf_valid(&state, &session, &start.csrf) {
        return refused(
            StatusCode::FORBIDDEN,
            "form_expired",
            "This page expired. Reload it and try again.",
        );
    }
    let name = start.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return refused(
            StatusCode::BAD_REQUEST,
            "passkey_name_invalid",
            &format!("Give the passkey a name of 1 to {MAX_NAME_CHARS} characters."),
        );
    }
    match attempt(
        &state,
        &client,
        known_device(&state, &headers),
        start.passphrase,
    )
    .await
    {
        Ok(_) => {}
        Err(Some(retry)) => return too_many(retry),
        Err(None) => {
            return refused(
                StatusCode::UNAUTHORIZED,
                "passphrase_wrong",
                "The passphrase is not correct. No passkey was added.",
            );
        }
    }
    let existing = match stored(&state).await {
        Ok(found) => found,
        Err(error) => return failed(error),
    };
    if existing.len() >= MAX_PASSKEYS {
        return refused(
            StatusCode::CONFLICT,
            "too_many_passkeys",
            &format!("There are already {MAX_PASSKEYS} passkeys. Remove one first."),
        );
    }
    let exclude = existing
        .iter()
        .map(|(_, passkey)| passkey.cred_id().clone())
        .collect();
    let (options, ceremony) = match webauthn.start_passkey_registration(
        OWNER_HANDLE,
        "owner",
        "Nexus owner",
        Some(exclude),
    ) {
        Ok(started) => started,
        Err(error) => return failed(error),
    };
    {
        let now = Instant::now();
        let mut registrations = state.0.passkeys.registrations.lock().unwrap();
        registrations.retain(|_, (at, _, _)| now.duration_since(*at) < CEREMONY_TTL);
        registrations.insert(session, (now, name.to_owned(), ceremony));
    }
    reply(StatusCode::OK, json!(options))
}

#[derive(Deserialize)]
struct RegisterFinish {
    csrf: String,
    credential: RegisterPublicKeyCredential,
}

pub(crate) async fn register_finish(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = current_session(&state, &headers).await else {
        return refused(StatusCode::UNAUTHORIZED, "not_logged_in", "Log in first.");
    };
    let Some(webauthn) = &state.0.passkeys.webauthn else {
        return unavailable();
    };
    let Ok(finish) = serde_json::from_slice::<RegisterFinish>(&body) else {
        return refused(
            StatusCode::BAD_REQUEST,
            "passkey_bad_request",
            "The browser sent something unexpected.",
        );
    };
    if !csrf_valid(&state, &session, &finish.csrf) {
        return refused(
            StatusCode::FORBIDDEN,
            "form_expired",
            "This page expired. Reload it and try again.",
        );
    }
    let ceremony = state
        .0
        .passkeys
        .registrations
        .lock()
        .unwrap()
        .remove(&session)
        .filter(|(at, _, _)| at.elapsed() < CEREMONY_TTL);
    let Some((_, name, ceremony)) = ceremony else {
        return refused(
            StatusCode::FORBIDDEN,
            "passkey_expired",
            "This passkey prompt expired. Try again.",
        );
    };
    let passkey = match webauthn.finish_passkey_registration(&finish.credential, &ceremony) {
        Ok(passkey) => passkey,
        Err(error) => {
            tracing::warn!(event = "passkey_refused", %error, "a new passkey was not accepted");
            return refused(
                StatusCode::BAD_REQUEST,
                "passkey_refused",
                "That passkey was not accepted.",
            );
        }
    };
    let saved = async {
        let mut tx = state.0.db.begin_write().await?;
        owner_login::insert_passkey(&mut tx, &credential_id(&passkey), &name, &to_json(&passkey))
            .await?;
        tx.commit().await?;
        Ok::<_, nexus_core::error::Error>(())
    };
    if let Err(error) = saved.await {
        return failed(error);
    }
    tracing::warn!(event = "passkey_added", "owner added a passkey");
    reply(StatusCode::OK, json!({ "ok": true }))
}
