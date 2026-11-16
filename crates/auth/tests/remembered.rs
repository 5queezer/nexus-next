//! A remembered device: sessions kept in the database, so authorizing a client after a restart
//! asks only for consent.

mod common;

use axum::http::StatusCode;
use common::*;
use nexus_core::repo::owner_login;

#[tokio::test]
async fn a_remembered_session_gets_a_thirty_day_cookie_with_the_same_attributes() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    let reply = browser.login_remembered(&h).await;
    let session = reply
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|cookie| cookie.starts_with("__Host-nexus_session="))
        .unwrap()
        .to_owned();
    assert!(session.ends_with("; Path=/; Max-Age=2592000; HttpOnly; Secure; SameSite=Lax"));
    assert_eq!(h.count("owner_sessions").await, 1);
}

#[tokio::test]
async fn after_a_restart_authorize_skips_the_passphrase_but_still_asks_for_consent() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login_remembered(&h).await;
    let before = browser
        .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
        .await;
    let stale_consent = before.field("consent");

    let h = h.restart().await;
    let page = browser
        .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("Authorize MCP access?"));
    assert_eq!(h.count("oauth_auth_codes").await, 0, "a GET never grants");

    // A consent page shown before the restart was signed by the old process.
    let stale = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&stale_consent)),
            ),
        )
        .await;
    assert_eq!(stale.status, StatusCode::FORBIDDEN);

    let done = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&page.field("consent"))),
            ),
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    let code = query_param(done.location(), "code").unwrap();
    assert_eq!(h.exchange(&id, &code).await.status, StatusCode::OK);
}

#[tokio::test]
async fn an_expired_session_sends_authorize_back_to_the_login() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login_remembered(&h).await;
    sqlx::query("UPDATE owner_sessions SET expires_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
        .execute(h.db.pool())
        .await
        .unwrap();
    let reply = browser
        .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(
        reply
            .location()
            .starts_with("/login?next=%2Fapi%2Fmcp%2Fauthorize")
    );
}

#[tokio::test]
async fn revoking_every_session_from_the_command_line_ends_a_running_servers_sessions() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut first = Browser::default();
    let mut second = Browser::default();
    first.login_remembered(&h).await;
    second.login(&h).await;
    assert_eq!(
        first.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );

    // What `nexus sessions revoke-all` runs, from another process on the same database.
    let mut tx = h.db.begin_write().await.unwrap();
    assert_eq!(owner_login::delete_all_sessions(&mut tx).await.unwrap(), 2);
    tx.commit().await.unwrap();

    for browser in [&mut first, &mut second] {
        assert_eq!(
            browser.send(&h, get_req("/dashboard")).await.status,
            StatusCode::SEE_OTHER
        );
        let reply = browser
            .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
            .await;
        assert!(reply.location().starts_with("/login?next="));
    }
}

#[tokio::test]
async fn logout_and_a_passphrase_change_remove_the_stored_session() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    browser.login_remembered(&h).await;
    let page = browser.send(&h, get_req("/logout")).await;
    browser
        .send(
            &h,
            form_req("/logout", &format!("csrf={}", page.field("csrf"))),
        )
        .await;
    assert_eq!(h.count("owner_sessions").await, 0);

    let mut a = Browser::default();
    let mut b = Browser::default();
    a.login_remembered(&h).await;
    b.login_remembered(&h).await;
    let form = a.send(&h, get_req("/passphrase")).await;
    let body = format!(
        "csrf={}&current={}&new={}&repeat={}",
        form.field("csrf"),
        encode(PASSPHRASE),
        encode("a different long passphrase"),
        encode("a different long passphrase"),
    );
    let changed = a.send(&h, form_req("/passphrase", &body)).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(h.count("owner_sessions").await, 0);
    assert_eq!(
        b.send(&h, get_req("/dashboard")).await.status,
        StatusCode::SEE_OTHER
    );
}
