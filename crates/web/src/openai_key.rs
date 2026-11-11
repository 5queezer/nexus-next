//! The owner's page for the OpenAI key. `GET /openai-key` says whether a key is set and where it
//! comes from, never the key. Its two forms set or remove the one kept in the data directory, and
//! each asks for the passphrase again, so a stolen session alone can't swap the key or read
//! anything from it. See SPEC section 9b.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use chrono_tz::Tz;
use leptos::prelude::*;
use nexus_auth::AuthState;
use nexus_core::Db;
use nexus_core::auth::FormToken;

use crate::cv::{awaiting_count, expired_form};
use crate::dashboard::{PageError, with_form_token};
use crate::format;
use crate::forms::{Flash, Typed, csrf_ok};
use crate::views::{self, Nav, NavCounts};
use crate::voice::{MintError, Voice, VoiceKey};

#[derive(Clone)]
struct KeyPage {
    db: Db,
    tz: Tz,
    voice: Voice,
    auth: Option<AuthState>,
}

/// Mounts `/openai-key`. Auth is the caller's. Without `auth` (no login in local mode) there is no
/// passphrase to ask for, so the page shows the state and takes no key.
pub fn openai_key_router(db: Db, tz: Tz, voice: Voice, auth: Option<AuthState>) -> Router {
    with_form_token(
        Router::new()
            .route("/openai-key", get(page).post(set))
            .route("/openai-key/remove", post(remove)),
    )
    .layer(middleware::map_response(crate::secure))
    .with_state(KeyPage {
        db,
        tz,
        voice,
        auth,
    })
}

const NO_LOGIN: &str = "This server runs without a login, so the key can only be set on the host.";

async fn render(
    page: &KeyPage,
    token: &str,
    flash: Option<Flash>,
) -> Result<Html<String>, PageError> {
    let counts = NavCounts {
        overdue: 0,
        awaiting: awaiting_count(&page.db).await?,
    };
    let state = page.voice.key_state();
    let voice = if state.stored.is_some() || state.startup {
        "Voice is on."
    } else {
        "Voice is off."
    };
    let source = match (&state.stored, state.startup) {
        (Some((last4, set_at)), startup) => format!(
            "A key ending in {last4} was set here on {}.{}",
            format::date_time(set_at, page.tz),
            if startup {
                " It is used instead of the key file the server was started with."
            } else {
                ""
            }
        ),
        (None, true) => {
            "The key comes from the file the server was started with. A key set here is used instead of it."
                .to_owned()
        }
        (None, false) => "No key is set.".to_owned(),
    };
    let flash = flash.map(|flash| {
        view! {
            <p class=if flash.ok { "banner" } else { "banner warn" } role="status">{flash.text}</p>
        }
    });
    let forms = if page.auth.is_some() {
        let remove = state.stored.is_some().then(|| view! {
            <h2>"Remove the key set here"</h2>
            <form method="post" action="/openai-key/remove" class="act card" autocomplete="off">
                <input type="hidden" name="csrf" value=token.to_owned() />
                <label>"Your passphrase"
                    <input type="password" name="passphrase" autocomplete="current-password" required />
                </label>
                <button type="submit" class="danger">"Remove key"</button>
            </form>
        });
        view! {
            <h2>"Set a new key"</h2>
            <form method="post" action="/openai-key" class="act card" autocomplete="off">
                <input type="hidden" name="csrf" value=token.to_owned() />
                <label>"OpenAI API key"
                    <input type="password" name="key" autocomplete="off" required />
                </label>
                <label>"Your passphrase"
                    <input type="password" name="passphrase" autocomplete="current-password" required />
                </label>
                <label class="check">
                    <input type="checkbox" name="unchecked" value="1" />
                    "Store it without asking OpenAI whether it accepts the key"
                </label>
                <p class="hint">"The key is kept in a file on this server and used from the next conversation on. No page shows it again."</p>
                <button type="submit">"Store key"</button>
            </form>
            {remove}
        }
        .into_any()
    } else {
        view! { <p class="hint">{NO_LOGIN}</p> }.into_any()
    };
    let body = view! {
        <h1>"OpenAI key"</h1>
        <p class="lede">{format!("{voice} {source}")}</p>
        {flash}
        {forms}
    };
    Ok(Html(views::page("OpenAI key", Nav::None, counts, body)))
}

async fn page(
    State(page): State<KeyPage>,
    Extension(FormToken(token)): Extension<FormToken>,
    Query(query): Query<Typed>,
) -> Result<Html<String>, PageError> {
    let done = match query.get("done").map(String::as_str) {
        Some("set") => Some("The key is stored and in use."),
        Some("removed") => Some("The key set here was removed."),
        _ => None,
    };
    let flash = done.map(|text| Flash {
        ok: true,
        text: text.to_owned(),
    });
    render(&page, &token, flash).await
}

/// The page again, with why nothing changed.
async fn refuse(page: &KeyPage, token: &str, status: StatusCode, text: &str) -> Response {
    let flash = Flash {
        ok: false,
        text: text.to_owned(),
    };
    match render(page, token, Some(flash)).await {
        Ok(html) => (status, html).into_response(),
        Err(error) => error.into_response(),
    }
}

/// What both forms need before anything else: this session's token, then the passphrase. `Some`
/// is the refusal to send.
async fn unconfirmed(
    page: &KeyPage,
    token: &str,
    parts: &mut Parts,
    form: &Typed,
) -> Option<Response> {
    if !csrf_ok(form.get("csrf").map(String::as_str), token) {
        return Some(expired_form());
    }
    let Some(auth) = &page.auth else {
        return Some(refuse(page, token, StatusCode::CONFLICT, NO_LOGIN).await);
    };
    let passphrase = form.get("passphrase").cloned().unwrap_or_default();
    if let Err((status, text)) = auth.confirm_passphrase(parts, passphrase).await {
        let text = format!("{text} Nothing was changed.");
        return Some(refuse(page, token, status, &text).await);
    }
    None
}

/// Stores a key OpenAI accepts. One it refuses is never stored. When OpenAI can't be asked, the
/// owner may store the key unchecked, which is the only way in while the network is down.
async fn set(
    State(page): State<KeyPage>,
    Extension(FormToken(token)): Extension<FormToken>,
    mut parts: Parts,
    Form(form): Form<Typed>,
) -> Response {
    if let Some(response) = unconfirmed(&page, &token, &mut parts, &form).await {
        return response;
    }
    let refuse = |status, text: &'static str| refuse(&page, &token, status, text);
    let Some(key) = form.get("key").and_then(|key| VoiceKey::parse(key)) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "That does not look like an OpenAI key. Nothing was changed.",
        )
        .await;
    };
    let checked = !form.contains_key("unchecked");
    if checked {
        match page.voice.check_key(&key).await {
            Ok(()) => {}
            Err(MintError::Refused) => {
                return refuse(
                    StatusCode::BAD_REQUEST,
                    "OpenAI does not accept this key. Nothing was changed.",
                )
                .await;
            }
            Err(MintError::Failed) => {
                return refuse(
                    StatusCode::BAD_GATEWAY,
                    "The key could not be checked with OpenAI, so nothing was changed. Try again, or tick the box to store it without the check.",
                )
                .await;
            }
        }
    }
    if let Err(error) = page.voice.store_key(key).await {
        tracing::error!(event = "openai_key_store_failed", %error, "could not store the OpenAI key");
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The key was not stored. The server log says why.",
        )
        .await;
    }
    tracing::warn!(
        event = "openai_key_set",
        checked,
        "the owner set the OpenAI key in the browser"
    );
    Redirect::to("/openai-key?done=set").into_response()
}

async fn remove(
    State(page): State<KeyPage>,
    Extension(FormToken(token)): Extension<FormToken>,
    mut parts: Parts,
    Form(form): Form<Typed>,
) -> Response {
    if let Some(response) = unconfirmed(&page, &token, &mut parts, &form).await {
        return response;
    }
    if let Err(error) = page.voice.remove_key() {
        tracing::error!(event = "openai_key_remove_failed", %error, "could not remove the OpenAI key");
        return refuse(
            &page,
            &token,
            StatusCode::INTERNAL_SERVER_ERROR,
            "The key was not removed. The server log says why.",
        )
        .await;
    }
    tracing::warn!(
        event = "openai_key_removed",
        "the owner removed the OpenAI key set in the browser"
    );
    Redirect::to("/openai-key?done=removed").into_response()
}
