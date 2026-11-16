//! Passkeys, end to end with a software authenticator: adding one needs a session, the page's CSRF
//! token and the passphrase; logging in with one needs nothing else, works once per prompt and
//! only for this origin.

mod common;

use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use serde_json::{Value, json};
use url::Url;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;

fn origin() -> Url {
    Url::parse(BASE).unwrap()
}

fn data_csrf(page: &Reply) -> String {
    let marker = r#"data-csrf=""#;
    let start = page.body.find(marker).expect("the page has the add box") + marker.len();
    let end = page.body[start..].find('"').unwrap() + start;
    page.body[start..end].to_owned()
}

/// A logged-in browser adds a passkey held by `authenticator`.
async fn add_passkey(h: &Harness, browser: &mut Browser, authenticator: &mut SoftPasskey) {
    let page = browser.send(h, get_req("/login/passkeys")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = data_csrf(&page);
    let start = browser
        .send(
            h,
            json_req(
                "/login/passkeys/register/start",
                &json!({ "csrf": csrf, "passphrase": PASSPHRASE, "name": "Phone" }),
            ),
        )
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let credential = authenticator
        .do_registration(origin(), serde_json::from_str(&start.body).unwrap())
        .unwrap();
    let finish = browser
        .send(
            h,
            json_req(
                "/login/passkeys/register/finish",
                &json!({ "csrf": csrf, "credential": credential }),
            ),
        )
        .await;
    assert_eq!(finish.status, StatusCode::OK, "{}", finish.body);
}

/// Starts a passkey login in `browser` and returns the authenticator's answer.
async fn sign_in(
    h: &Harness,
    browser: &mut Browser,
    authenticator: &mut SoftPasskey,
    origin: Url,
) -> Value {
    let start = browser
        .send(h, json_req("/login/passkey/start", &json!({})))
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let credential = authenticator
        .do_authentication(origin, serde_json::from_str(&start.body).unwrap())
        .unwrap();
    json!(credential)
}

#[tokio::test]
async fn a_passkey_logs_the_owner_in_and_the_passphrase_still_works() {
    let h = Harness::new().await;
    let mut authenticator = SoftPasskey::new(true);
    let mut owner = Browser::default();
    owner.login(&h).await;
    add_passkey(&h, &mut owner, &mut authenticator).await;
    assert_eq!(h.count("owner_passkeys").await, 1);
    let page = owner.send(&h, get_req("/login/passkeys")).await;
    assert!(page.body.contains("<bdi>Phone</bdi>"));

    let mut phone = Browser::default();
    let form = phone.send(&h, get_req("/login")).await;
    assert!(form.body.contains(r#"id="passkey-login""#));
    assert!(
        form.header("content-security-policy")
            .contains("script-src 'self'; connect-src 'self'")
    );
    let credential = sign_in(&h, &mut phone, &mut authenticator, origin()).await;
    let done = phone
        .send(
            &h,
            json_req(
                "/login/passkey/finish",
                &json!({ "credential": credential, "remember": true, "next": "/dashboard" }),
            ),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(done.json()["redirect"], "/dashboard");
    assert!(done.headers.get_all("set-cookie").iter().any(|c| {
        c.to_str()
            .unwrap()
            .contains("Max-Age=2592000; HttpOnly; Secure; SameSite=Lax")
    }));
    assert_eq!(
        phone.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );

    // The same answer again: the prompt it belonged to is used up.
    let replay = phone
        .send(
            &h,
            json_req(
                "/login/passkey/finish",
                &json!({ "credential": credential }),
            ),
        )
        .await;
    replay.assert_error(StatusCode::FORBIDDEN, "passkey_expired");

    let mut laptop = Browser::default();
    laptop.login(&h).await;
    assert_eq!(
        laptop.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_foreign_next_is_dropped_after_a_passkey_login() {
    let h = Harness::new().await;
    let mut authenticator = SoftPasskey::new(true);
    let mut owner = Browser::default();
    owner.login(&h).await;
    add_passkey(&h, &mut owner, &mut authenticator).await;
    let mut other = Browser::default();
    let credential = sign_in(&h, &mut other, &mut authenticator, origin()).await;
    let done = other
        .send(
            &h,
            json_req(
                "/login/passkey/finish",
                &json!({ "credential": credential, "next": "//evil.example.test/x" }),
            ),
        )
        .await;
    assert_eq!(done.json()["redirect"], "/");
}

#[tokio::test]
async fn a_passkey_answer_for_another_origin_or_without_its_prompt_is_refused() {
    let h = Harness::new().await;
    let mut authenticator = SoftPasskey::new(true);
    let mut owner = Browser::default();
    owner.login(&h).await;
    add_passkey(&h, &mut owner, &mut authenticator).await;

    // The authenticator itself won't sign for another site, as a browser won't. An answer whose
    // client data was rewritten to name another origin is refused by the server.
    let mut phished = Browser::default();
    let mut credential = sign_in(&h, &mut phished, &mut authenticator, origin()).await;
    let field = &mut credential["response"]["clientDataJSON"];
    let client_data = String::from_utf8(URL_SAFE_NO_PAD.decode(field.as_str().unwrap()).unwrap())
        .unwrap()
        .replace(BASE, "https://crm.example.test.evil.example");
    *field = json!(URL_SAFE_NO_PAD.encode(client_data));
    let reply = phished
        .send(
            &h,
            json_req(
                "/login/passkey/finish",
                &json!({ "credential": credential }),
            ),
        )
        .await;
    reply.assert_error(StatusCode::UNAUTHORIZED, "passkey_refused");
    assert!(!phished.cookies.contains_key("__Host-nexus_session"));

    // An answer posted from a browser that never started the prompt.
    let mut started = Browser::default();
    let credential = sign_in(&h, &mut started, &mut authenticator, origin()).await;
    let reply = h
        .send(json_req(
            "/login/passkey/finish",
            &json!({ "credential": credential }),
        ))
        .await;
    reply.assert_error(StatusCode::FORBIDDEN, "passkey_expired");
}

#[tokio::test]
async fn adding_a_passkey_needs_a_session_the_page_token_and_the_passphrase() {
    let h = Harness::new().await;
    let anonymous = h
        .send(json_req(
            "/login/passkeys/register/start",
            &json!({ "csrf": "x", "passphrase": PASSPHRASE, "name": "n" }),
        ))
        .await;
    anonymous.assert_error(StatusCode::UNAUTHORIZED, "not_logged_in");
    let page = h.send(get_req("/login/passkeys")).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location(), "/login?next=/login/passkeys");

    let mut owner = Browser::default();
    owner.login(&h).await;
    let csrf = data_csrf(&owner.send(&h, get_req("/login/passkeys")).await);
    let start = |csrf: &str, passphrase: &str, name: &str| {
        json_req(
            "/login/passkeys/register/start",
            &json!({ "csrf": csrf, "passphrase": passphrase, "name": name }),
        )
    };
    owner
        .send(&h, start("forged", PASSPHRASE, "n"))
        .await
        .assert_error(StatusCode::FORBIDDEN, "form_expired");
    owner
        .send(&h, start(&csrf, "a stolen session only", "n"))
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "passphrase_wrong");
    owner
        .send(&h, start(&csrf, PASSPHRASE, "  "))
        .await
        .assert_error(StatusCode::BAD_REQUEST, "passkey_name_invalid");

    // A finish with no started registration.
    let mut authenticator = SoftPasskey::new(true);
    let mut other = Browser::default();
    other.login(&h).await;
    let other_csrf = data_csrf(&other.send(&h, get_req("/login/passkeys")).await);
    let options = other.send(&h, start(&other_csrf, PASSPHRASE, "n")).await;
    let credential = authenticator
        .do_registration(origin(), serde_json::from_str(&options.body).unwrap())
        .unwrap();
    owner
        .send(
            &h,
            json_req(
                "/login/passkeys/register/finish",
                &json!({ "csrf": csrf, "credential": credential }),
            ),
        )
        .await
        .assert_error(StatusCode::FORBIDDEN, "passkey_expired");
    assert_eq!(h.count("owner_passkeys").await, 0);
}

#[tokio::test]
async fn a_removed_passkey_no_longer_logs_in() {
    let h = Harness::new().await;
    let mut authenticator = SoftPasskey::new(true);
    let mut owner = Browser::default();
    owner.login(&h).await;
    add_passkey(&h, &mut owner, &mut authenticator).await;

    let page = owner.send(&h, get_req("/login/passkeys")).await;
    let forged = owner
        .send(
            &h,
            form_req(
                "/login/passkeys/delete",
                &format!("csrf=x&id={}", encode(&page.field("id"))),
            ),
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(h.count("owner_passkeys").await, 1);

    // A prompt started before the removal can't finish after it.
    let mut late = Browser::default();
    let credential = sign_in(&h, &mut late, &mut authenticator, origin()).await;
    let removed = owner
        .send(
            &h,
            form_req(
                "/login/passkeys/delete",
                &format!(
                    "csrf={}&id={}",
                    encode(&page.field("csrf")),
                    encode(&page.field("id"))
                ),
            ),
        )
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    assert_eq!(h.count("owner_passkeys").await, 0);
    late.send(
        &h,
        json_req(
            "/login/passkey/finish",
            &json!({ "credential": credential }),
        ),
    )
    .await
    .assert_error(StatusCode::UNAUTHORIZED, "passkey_refused");

    h.send(json_req("/login/passkey/start", &json!({})))
        .await
        .assert_error(StatusCode::NOT_FOUND, "no_passkeys");
    let form = h.send(get_req("/login")).await;
    assert!(!form.body.contains("passkey-login"));
    assert!(
        !form
            .header("content-security-policy")
            .contains("script-src")
    );
}

#[tokio::test]
async fn the_script_is_served_without_a_session_and_pages_stay_free_of_inline_code() {
    let h = Harness::new().await;
    let script = h.send(get_req("/login/passkey.js")).await;
    assert_eq!(script.status, StatusCode::OK);
    assert!(script.header("content-type").starts_with("text/javascript"));
    assert_eq!(script.header("x-content-type-options"), "nosniff");

    let mut owner = Browser::default();
    owner.login(&h).await;
    let page = owner.send(&h, get_req("/login/passkeys")).await;
    assert!(!page.body.contains("<script>"));
    assert!(!page.body.contains(" on"));
    assert!(
        page.header("content-security-policy")
            .contains("default-src 'none'")
    );
}

async fn change_passphrase_in(h: &Harness, browser: &mut Browser) {
    let form = browser.send(h, get_req("/passphrase")).await;
    let body = format!(
        "csrf={}&current={}&new={}&repeat={}",
        form.field("csrf"),
        encode(PASSPHRASE),
        encode("a different long passphrase"),
        encode("a different long passphrase"),
    );
    let changed = browser.send(h, form_req("/passphrase", &body)).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
}

#[tokio::test]
async fn a_passphrase_change_removes_every_passkey_a_leaked_passphrase_could_have_added() {
    let h = Harness::new().await;
    let mut intruder_key = SoftPasskey::new(true);
    let mut intruder = Browser::default();
    intruder.login(&h).await;
    add_passkey(&h, &mut intruder, &mut intruder_key).await;

    // The owner notices and changes the passphrase in the browser.
    let mut owner = Browser::default();
    owner.login(&h).await;
    change_passphrase_in(&h, &mut owner).await;
    assert_eq!(h.count("owner_passkeys").await, 0);
    h.send(json_req("/login/passkey/start", &json!({})))
        .await
        .assert_error(StatusCode::NOT_FOUND, "no_passkeys");

    // On the host, the same: `nexus set-passphrase` runs this.
    let mut key = SoftPasskey::new(true);
    let mut again = Browser::default();
    let form = again.send(&h, get_req("/login")).await;
    let body = format!(
        "csrf={}&passphrase={}",
        form.field("csrf"),
        encode("a different long passphrase")
    );
    assert_eq!(
        again.send(&h, form_req("/login", &body)).await.status,
        StatusCode::SEE_OTHER
    );
    let page = again.send(&h, get_req("/login/passkeys")).await;
    let csrf = data_csrf(&page);
    let start = again
        .send(
            &h,
            json_req(
                "/login/passkeys/register/start",
                &json!({ "csrf": csrf, "passphrase": "a different long passphrase", "name": "n" }),
            ),
        )
        .await;
    let credential = key
        .do_registration(origin(), serde_json::from_str(&start.body).unwrap())
        .unwrap();
    again
        .send(
            &h,
            json_req(
                "/login/passkeys/register/finish",
                &json!({ "csrf": csrf, "credential": credential }),
            ),
        )
        .await;
    assert_eq!(h.count("owner_passkeys").await, 1);
    nexus_auth::set_owner_passphrase(
        &h.db,
        &h.dir.path().join("host.argon2"),
        "yet another long passphrase",
    )
    .await
    .unwrap();
    assert_eq!(h.count("owner_passkeys").await, 0);
}

#[tokio::test]
async fn a_registration_started_before_a_passphrase_change_cannot_finish_after_it() {
    let h = Harness::new().await;
    let mut key = SoftPasskey::new(true);
    let mut intruder = Browser::default();
    intruder.login(&h).await;
    let csrf = data_csrf(&intruder.send(&h, get_req("/login/passkeys")).await);
    let start = intruder
        .send(
            &h,
            json_req(
                "/login/passkeys/register/start",
                &json!({ "csrf": csrf, "passphrase": PASSPHRASE, "name": "n" }),
            ),
        )
        .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    let credential = key
        .do_registration(origin(), serde_json::from_str(&start.body).unwrap())
        .unwrap();

    let mut owner = Browser::default();
    owner.login(&h).await;
    change_passphrase_in(&h, &mut owner).await;

    // The intruder's session ended with the change, so the finish is refused either way.
    let finish = intruder
        .send(
            &h,
            json_req(
                "/login/passkeys/register/finish",
                &json!({ "csrf": csrf, "credential": credential }),
            ),
        )
        .await;
    assert!(finish.status.is_client_error(), "{}", finish.body);
    assert_eq!(h.count("owner_passkeys").await, 0);
}

#[tokio::test]
async fn removing_a_lost_devices_passkey_logs_out_every_other_browser() {
    let h = Harness::new().await;
    let mut key = SoftPasskey::new(true);
    let mut owner = Browser::default();
    owner.login(&h).await;
    add_passkey(&h, &mut owner, &mut key).await;

    let mut lost = Browser::default();
    let credential = sign_in(&h, &mut lost, &mut key, origin()).await;
    let done = lost
        .send(
            &h,
            json_req(
                "/login/passkey/finish",
                &json!({ "credential": credential, "remember": true }),
            ),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(
        lost.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );
    // The same lost device, also logged in with the passphrase in another browser.
    let mut lost_too = Browser::default();
    lost_too.login(&h).await;

    let page = owner.send(&h, get_req("/login/passkeys")).await;
    let removed = owner
        .send(
            &h,
            form_req(
                "/login/passkeys/delete",
                &format!(
                    "csrf={}&id={}",
                    encode(&page.field("csrf")),
                    encode(&page.field("id"))
                ),
            ),
        )
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    for browser in [&mut lost, &mut lost_too] {
        assert_eq!(
            browser.send(&h, get_req("/dashboard")).await.status,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        owner.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );
}
