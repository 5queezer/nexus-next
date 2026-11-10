//! Every way the authorization server must say no.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, COOKIE, RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderName, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use nexus_auth::{
    AuthConfig, AuthState, ConfigError, PassphraseError, TrustedProxy, set_owner_passphrase,
};
use nexus_core::repo::oauth::{self as repo, NewOAuthToken};
use nexus_core::time::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::AssertSqlSafe;
use tower::ServiceExt;

fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value))
}

const PAST: &str = "2020-01-01T00:00:00.000Z";
const FUTURE: &str = "2099-01-01T00:00:00.000Z";

async fn execute(h: &Harness, sql: &str) {
    sqlx::query(AssertSqlSafe(sql.to_owned()))
        .execute(h.db.pool())
        .await
        .unwrap();
}

// ── Registration ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn registration_validates_redirect_uris_like_old_nexus() {
    for good in [
        "https://client.example.test/cb",
        "HTTPS://Client.Example.Test/cb",
        "https://client.example.test/cb?x=1",
        "http://localhost/cb",
        "http://localhost:1234/cb",
        "http://127.0.0.1:1234/cb",
    ] {
        let h = Harness::new().await;
        let reply = h.register_with(json!({ "redirect_uris": [good] })).await;
        assert_eq!(reply["redirect_uris"], json!([good]), "stored unchanged");
    }

    let h = Harness::new().await;
    for (body, description) in [
        (json!({}), "redirect_uris is required".to_owned()),
        (
            json!({ "redirect_uris": [] }),
            "redirect_uris is required".to_owned(),
        ),
        (
            json!({ "redirect_uris": "https://a.example.test" }),
            "redirect_uris is required".to_owned(),
        ),
    ] {
        let reply = h
            .send(with_ip(json_req("/api/mcp/register", &body), "203.0.113.1"))
            .await;
        reply.assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
        assert_eq!(reply.json()["error_description"], description);
    }
    for (uri, description) in [
        ("not a url", "Invalid redirect URI: not a url"),
        ("/relative", "Invalid redirect URI: /relative"),
        (
            "http://example.test/cb",
            "redirect_uri must use https (got http:): http://example.test/cb",
        ),
        (
            "http://localhost.example.test/cb",
            "redirect_uri must use https (got http:): http://localhost.example.test/cb",
        ),
        (
            "http://[::1]/cb",
            "redirect_uri must use 127.0.0.1 or localhost for a loopback address: http://[::1]/cb",
        ),
        (
            "ftp://localhost/cb",
            "redirect_uri must use https (got ftp:): ftp://localhost/cb",
        ),
        (
            "javascript:alert(1)",
            "redirect_uri must use https (got javascript:): javascript:alert(1)",
        ),
        (
            "https://client.example.test/cb#fragment",
            "redirect_uri must not contain a fragment: https://client.example.test/cb#fragment",
        ),
        (
            "https://client.example.test/cb#",
            "redirect_uri must not contain a fragment: https://client.example.test/cb#",
        ),
        (
            "http://localhost:1234/cb#fragment",
            "redirect_uri must not contain a fragment: http://localhost:1234/cb#fragment",
        ),
        (
            "custom-scheme://cb",
            "redirect_uri must use https (got custom-scheme:): custom-scheme://cb",
        ),
    ] {
        let h = Harness::new().await;
        let reply = h
            .send(json_req(
                "/api/mcp/register",
                &json!({ "redirect_uris": ["https://ok.example.test/cb", uri] }),
            ))
            .await;
        reply.assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
        assert_eq!(reply.json()["error_description"], description, "{uri}");
        assert_eq!(h.count("oauth_clients").await, 0);
    }
    let h = Harness::new().await;
    h.send(json_req(
        "/api/mcp/register",
        &json!({ "redirect_uris": [123] }),
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
}

#[tokio::test]
async fn malformed_registration_bodies_get_400() {
    let h = Harness::new().await;
    for body in ["{not json", "", "null", "[]", "\"text\""] {
        let request = Request::post("/api/mcp/register")
            .body(Body::from(body))
            .unwrap();
        h.send(with_ip(request, "203.0.113.1"))
            .await
            .assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    for field in [
        json!({ "client_name": 5 }),
        json!({ "grant_types": "authorization_code" }),
        json!({ "token_endpoint_auth_method": ["none"] }),
    ] {
        let mut body = json!({ "redirect_uris": [REDIRECT] });
        body.as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        h.send(with_ip(json_req("/api/mcp/register", &body), "203.0.113.2"))
            .await
            .assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    assert_eq!(h.count("oauth_clients").await, 0);
}

#[tokio::test]
async fn registration_bounds_stored_input() {
    let h = Harness::new().await;
    h.send(json_req(
        "/api/mcp/register",
        &json!({ "redirect_uris": [REDIRECT], "client_name": "x".repeat(201) }),
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");

    let oversized = h
        .send(json_req(
            "/api/mcp/register",
            &json!({ "redirect_uris": [REDIRECT], "padding": "x".repeat(17 * 1024) }),
        ))
        .await;
    assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(h.count("oauth_clients").await, 0);
}

#[tokio::test]
async fn client_secrets_are_stored_hashed_and_public_clients_have_none() {
    let h = Harness::new().await;
    let client = h
        .register_with(json!({ "redirect_uris": [REDIRECT] }))
        .await;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT client_secret_hash FROM oauth_clients WHERE client_id = ?")
            .bind(client["client_id"].as_str().unwrap())
            .fetch_one(h.db.pool())
            .await
            .unwrap();
    assert_eq!(
        stored.unwrap(),
        sha256(client["client_secret"].as_str().unwrap())
    );

    let public = h.register().await;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT client_secret_hash FROM oauth_clients WHERE client_id = ?")
            .bind(public)
            .fetch_one(h.db.pool())
            .await
            .unwrap();
    assert_eq!(stored, None);
}

fn register_from(ip: &str) -> Request<Body> {
    with_ip(
        json_req("/api/mcp/register", &json!({ "redirect_uris": [REDIRECT] })),
        ip,
    )
}

#[tokio::test]
async fn registration_is_rate_limited_per_source_and_globally() {
    let h = Harness::new().await;
    for _ in 0..10 {
        assert_eq!(
            h.send(register_from("203.0.113.1")).await.status,
            StatusCode::CREATED
        );
    }
    let limited = h.send(register_from("203.0.113.1")).await;
    limited.assert_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
    assert!(limited.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    assert_eq!(h.count("oauth_clients").await, 10);

    for i in 0..20 {
        let ip = format!("203.0.113.{}", 2 + i / 5);
        assert_eq!(h.send(register_from(&ip)).await.status, StatusCode::CREATED);
    }
    // 30 in the hour, from any source: a new source is refused too.
    h.send(register_from("203.0.113.200"))
        .await
        .assert_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
    assert_eq!(h.count("oauth_clients").await, 30);
}

async fn fill_clients(h: &Harness, created_at: &str) {
    execute(
        h,
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50)
             INSERT INTO oauth_clients (client_id, redirect_uris, grant_types, response_types, created_at)
             SELECT 'junk_' || i, '[]', '[]', '[]', '{created_at}' FROM n"
        ),
    )
    .await;
}

#[tokio::test]
async fn at_the_cap_the_oldest_unused_client_makes_room() {
    let h = Harness::new().await;
    fill_clients(&h, "2026-01-01T00:00:00.000Z").await;
    execute(&h, "UPDATE oauth_clients SET created_at = '2025-01-01T00:00:00.000Z' WHERE client_id = 'junk_7'").await;

    assert_eq!(
        h.send(register_from(DEFAULT_IP)).await.status,
        StatusCode::CREATED
    );
    assert_eq!(h.count("oauth_clients").await, 50);
    let survivors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients WHERE client_id = 'junk_7'")
            .fetch_one(h.db.pool())
            .await
            .unwrap();
    assert_eq!(survivors, 0);
}

#[tokio::test]
async fn at_the_cap_recent_clients_are_not_evicted_mid_authorization() {
    let h = Harness::new().await;
    fill_clients(&h, Timestamp::now().as_str()).await;
    h.send(register_from(DEFAULT_IP))
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    assert_eq!(h.count("oauth_clients").await, 50);
}

#[tokio::test]
async fn at_the_cap_clients_holding_live_tokens_are_kept_and_expired_ones_are_not() {
    let h = Harness::new().await;
    fill_clients(&h, "2026-01-01T00:00:00.000Z").await;
    execute(
        &h,
        &format!(
            "INSERT INTO oauth_refresh_tokens (token_hash, client_id, scopes, expires_at)
             SELECT 'hash_' || client_id, client_id, '[]', '{FUTURE}' FROM oauth_clients"
        ),
    )
    .await;
    h.send(register_from(DEFAULT_IP))
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");

    execute(
        &h,
        &format!(
            "UPDATE oauth_refresh_tokens SET expires_at = '{PAST}' WHERE client_id = 'junk_3'"
        ),
    )
    .await;
    assert_eq!(
        h.send(register_from(DEFAULT_IP)).await.status,
        StatusCode::CREATED
    );
    assert_eq!(h.count("oauth_clients").await, 50);
    assert_eq!(h.count("oauth_refresh_tokens").await, 49);
}

#[tokio::test]
async fn expired_codes_and_tokens_are_pruned_and_live_ones_kept() {
    let h = Harness::new().await;
    for (table, column) in [
        ("oauth_access_tokens", "token_hash"),
        ("oauth_refresh_tokens", "token_hash"),
    ] {
        for (hash, expires) in [("old", PAST), ("live", FUTURE)] {
            execute(
                &h,
                &format!(
                    "INSERT INTO {table} ({column}, client_id, scopes, expires_at)
                     VALUES ('{hash}', 'c', '[]', '{expires}')"
                ),
            )
            .await;
        }
    }
    let yesterday = Timestamp::now()
        .plus(chrono::Duration::days(-1))
        .to_string();
    for (code, used, expires) in [
        ("used-long-ago", 1, PAST),
        ("unused-expired", 0, yesterday.as_str()),
        ("used-recently", 1, yesterday.as_str()),
        ("live", 0, FUTURE),
    ] {
        execute(
            &h,
            &format!(
                "INSERT INTO oauth_auth_codes (code, client_id, redirect_uri, code_challenge, scopes, expires_at, used)
                 VALUES ('{code}', 'c', 'r', 'x', '[]', '{expires}', {used})"
            ),
        )
        .await;
    }

    // Any token request runs the prune.
    h.token("client_id=mcp_unknown").await;
    assert_eq!(h.count("oauth_access_tokens").await, 1);
    assert_eq!(h.count("oauth_refresh_tokens").await, 1);
    let mut kept: Vec<String> = sqlx::query_scalar("SELECT code FROM oauth_auth_codes")
        .fetch_all(h.db.pool())
        .await
        .unwrap();
    kept.sort();
    assert_eq!(
        kept,
        ["live", "used-recently"],
        "a used code stays to catch its replay"
    );
}

// ── Authorization endpoint ───────────────────────────────────────────────────

#[tokio::test]
async fn authorize_rejects_bad_requests_with_old_nexus_errors() {
    let h = Harness::new().await;
    let id = h.register().await;
    let good = format!(
        "client_id={id}&redirect_uri={}&response_type=code&code_challenge={CHALLENGE}&code_challenge_method=S256",
        encode(REDIRECT)
    );
    let without = |name: &str| {
        good.split('&')
            .filter(|pair| !pair.starts_with(&format!("{name}=")))
            .collect::<Vec<_>>()
            .join("&")
    };
    let cases: Vec<(String, &str)> = vec![
        (without("client_id"), "invalid_request"),
        (without("redirect_uri"), "invalid_request"),
        (without("code_challenge"), "invalid_request"),
        (without("response_type"), "unsupported_response_type"),
        (
            format!("{}&response_type=token", without("response_type")),
            "unsupported_response_type",
        ),
        (format!("{good}&scope=mcp:submissions"), "invalid_scope"),
        (format!("{good}&scope=mcp:tools+admin"), "invalid_scope"),
        (format!("{good}&scope="), "invalid_scope"),
        (
            format!(
                "{}&code_challenge_method=plain",
                without("code_challenge_method")
            ),
            "invalid_request",
        ),
        (
            format!(
                "{}&code_challenge_method=S512",
                without("code_challenge_method")
            ),
            "invalid_request",
        ),
        (
            format!("{}&code_challenge=short", without("code_challenge")),
            "invalid_request",
        ),
        (
            format!("{}&client_id=mcp_unknown", without("client_id")),
            "invalid_client",
        ),
        (
            format!(
                "{}&redirect_uri={}",
                without("redirect_uri"),
                encode("https://evil.example.test/cb")
            ),
            "invalid_request",
        ),
        (
            format!(
                "{}&redirect_uri={}",
                without("redirect_uri"),
                encode("https://client.example.test/callback/")
            ),
            "invalid_request",
        ),
        (
            format!("{good}&state={}", "x".repeat(9000)),
            "invalid_request",
        ),
    ];
    // Logged in, so failures can't be mistaken for the login redirect.
    let mut browser = Browser::default();
    browser.login(&h).await;
    for (query, error) in cases {
        let reply = browser
            .send(&h, get_req(&format!("/api/mcp/authorize?{query}")))
            .await;
        reply.assert_error(StatusCode::BAD_REQUEST, error);
        assert!(!reply.headers.contains_key("location"), "{query}");
    }
}

#[tokio::test]
async fn repeated_parameters_cannot_smuggle_a_second_value() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(
            &h,
            get_req(&format!(
                "{}&code_challenge_method=plain&redirect_uri={}",
                authorize_path(&id, "mcp:tools"),
                encode("https://evil.example.test/cb"),
            )),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "first values win");
    assert!(page.body.contains("https://client.example.test"));
    assert!(!page.body.contains("evil"));
}

#[tokio::test]
async fn consent_is_required_every_time_and_get_never_issues_a_code() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let path = authorize_path(&id, "mcp:tools");
    let first = browser.send(&h, get_req(&path)).await;
    let consent = first.field("consent");

    // Old Nexus's approval link, replayed as a GET, only shows the consent page again.
    let replay = browser
        .send(&h, get_req(&format!("{path}&consent={}", encode(&consent))))
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert!(replay.body.contains("Authorize MCP access?"));
    assert_eq!(h.count("oauth_auth_codes").await, 0);

    // A second authorization by the same client is asked again.
    h.exchange(&id, &h.code_for(&id, "mcp:tools").await).await;
    let again = browser.send(&h, get_req(&path)).await;
    assert!(again.body.contains("Authorize MCP access?"));
}

#[tokio::test]
async fn consent_post_requires_session_and_an_untampered_token_of_that_session() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
        .await;
    let consent = page.field("consent");
    let post = |token: &str| form_req("/api/mcp/authorize", &format!("consent={}", encode(token)));

    // No session: a cross-site POST, whose Lax cookie doesn't come along, ends here.
    assert_eq!(
        h.send(post(&consent)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Missing token: CSRF from a page that can't read this one.
    let empty = browser.send(&h, form_req("/api/mcp/authorize", "")).await;
    assert_eq!(empty.status, StatusCode::FORBIDDEN);

    // Tampered payload, as a client wanting more scope would try.
    let (payload, signature) = consent.rsplit_once('.').unwrap();
    let decoded = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap();
    assert!(decoded.contains("mcp:tools"));
    let widened = decoded.replace("mcp:tools", "mcp:tools mcp:submissions");
    let forged = format!(
        "{}.{signature}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(widened)
    );
    for bad in [
        forged.as_str(),
        "garbage",
        "garbage.garbage",
        &format!("{payload}.{}", "0".repeat(64)),
    ] {
        assert_eq!(
            browser.send(&h, post(bad)).await.status,
            StatusCode::FORBIDDEN,
            "{bad}"
        );
    }

    // A different logged-in session can't use it either.
    let mut other = Browser::default();
    other.login(&h).await;
    assert_eq!(
        other.send(&h, post(&consent)).await.status,
        StatusCode::FORBIDDEN
    );

    assert_eq!(h.count("oauth_auth_codes").await, 0);
    let done = browser.send(&h, post(&consent)).await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn consent_for_a_client_removed_since_the_page_was_shown_fails() {
    let h = Harness::new().await;
    let id = h.register().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(&h, get_req(&authorize_path(&id, "mcp:tools")))
        .await;
    execute(&h, "DELETE FROM oauth_clients").await;
    let reply = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&page.field("consent"))),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.count("oauth_auth_codes").await, 0);
}

#[tokio::test]
async fn consent_page_escapes_reflected_values_and_blocks_framing() {
    let h = Harness::new().await;
    let hostile = r#"<script>alert(1)</script>"' onload=x"#;
    let client = h
        .register_with(json!({
            "client_name": hostile,
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
        }))
        .await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(
            &h,
            get_req(&format!(
                "{}&state={}",
                authorize_path(client["client_id"].as_str().unwrap(), "mcp:tools")
                    .replace("&state=st-1", ""),
                encode(r#""><script>alert(2)</script>"#)
            )),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("<script"), "{}", page.body);
    assert!(
        page.body
            .contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&#39; onload=x")
    );
    let csp = page.header("content-security-policy");
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("form-action 'self' https://client.example.test"));
    assert_eq!(page.header("x-frame-options"), "DENY");
    assert_eq!(page.header("cache-control"), "no-store");
    assert_eq!(page.header("referrer-policy"), "no-referrer");
}

// ── Token endpoint ───────────────────────────────────────────────────────────

#[tokio::test]
async fn replaying_a_code_revokes_everything_from_that_grant_only() {
    let h = Harness::new().await;
    let id = h.register().await;
    let other_grant = h.grant(&id, "mcp:tools").await;

    let code = h.code_for(&id, "mcp:tools").await;
    let first = h.exchange(&id, &code).await.json();
    let rotated = h
        .refresh(&id, first["refresh_token"].as_str().unwrap(), None)
        .await
        .json();

    h.exchange(&id, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    for token in [&first["access_token"], &rotated["access_token"]] {
        assert_eq!(
            h.whoami(token.as_str().unwrap()).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    h.refresh(&id, rotated["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    assert_eq!(
        h.whoami(other_grant["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        h.refresh(&id, other_grant["refresh_token"].as_str().unwrap(), None)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_code_replayed_under_another_client_or_after_expiry_still_revokes() {
    let h = Harness::new().await;
    let id = h.register().await;
    let thief = h.register().await;

    let code = h.code_for(&id, "mcp:tools").await;
    let tokens = h.exchange(&id, &code).await.json();
    h.exchange(&thief, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(
        h.whoami(tokens["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let code = h.code_for(&id, "mcp:tools").await;
    let tokens = h.exchange(&id, &code).await.json();
    let yesterday = Timestamp::now().plus(chrono::Duration::days(-1));
    execute(
        &h,
        &format!("UPDATE oauth_auth_codes SET expires_at = '{yesterday}'"),
    )
    .await;
    h.exchange(&id, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(
        h.whoami(tokens["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_code_exchanges_have_one_winner_and_the_losers_revoke_it() {
    let h = Arc::new(Harness::new().await);
    let id = h.register().await;
    let code = h.code_for(&id, "mcp:tools").await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (h, id, code) = (h.clone(), id.clone(), code.clone());
        tasks.push(tokio::spawn(async move { h.exchange(&id, &code).await }));
    }
    let mut wins = 0;
    for task in tasks {
        let reply = task.await.unwrap();
        if reply.status == StatusCode::OK {
            wins += 1;
        } else {
            reply.assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(h.count("oauth_access_tokens").await, 0);
    assert_eq!(h.count("oauth_refresh_tokens").await, 0);
}

#[tokio::test]
async fn a_lost_refresh_response_can_be_retried_within_the_grace() {
    let h = Harness::new().await;
    let id = h.register().await;
    let first = h.grant(&id, "mcp:tools mcp:submissions").await;
    let old = first["refresh_token"].as_str().unwrap();

    // The server rotated, but the client never saw this response. It still holds `old`.
    let lost = h.refresh(&id, old, None).await.json();
    let retried = h.refresh(&id, old, None).await;
    assert_eq!(retried.status, StatusCode::OK, "{}", retried.body);
    let retried = retried.json();
    assert_eq!(retried["scope"], "mcp:tools mcp:submissions");
    assert_ne!(retried["refresh_token"], lost["refresh_token"]);
    assert_eq!(
        h.whoami(retried["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(h.live_refresh_tokens().await, 1, "no branch in the grant");

    // The client carries on normally, long after the grace.
    h.end_refresh_grace().await;
    let next = h
        .refresh(&id, retried["refresh_token"].as_str().unwrap(), None)
        .await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
}

#[tokio::test]
async fn a_thief_who_uses_the_grace_is_caught_at_the_real_clients_next_refresh() {
    let h = Harness::new().await;
    let id = h.register().await;
    let first = h.grant(&id, "mcp:tools").await;
    let stolen = first["refresh_token"].as_str().unwrap();

    let real_client = h.refresh(&id, stolen, None).await.json();
    let thief = h.refresh(&id, stolen, None).await.json();
    assert_eq!(h.live_refresh_tokens().await, 1);

    // A day later the real client refreshes with a token the thief's retry superseded.
    h.end_refresh_grace().await;
    h.refresh(&id, real_client["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    assert_eq!(
        h.whoami(thief["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    h.refresh(&id, thief["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
}

#[tokio::test]
async fn a_retry_after_the_grace_revokes_the_grant_and_a_new_authorization_recovers() {
    let h = Harness::new().await;
    let id = h.register().await;
    let first = h.grant(&id, "mcp:tools").await;
    let old = first["refresh_token"].as_str().unwrap();
    let rotated = h.refresh(&id, old, None).await.json();
    let other_grant = h.grant(&id, "mcp:tools").await;

    h.end_refresh_grace().await;
    h.refresh(&id, old, None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    assert_eq!(
        h.whoami(rotated["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    h.refresh(&id, rotated["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(
        h.whoami(other_grant["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK,
        "only that grant ends"
    );

    let fresh = h.grant(&id, "mcp:tools").await;
    assert_eq!(
        h.whoami(fresh["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_grace_is_for_the_same_client_and_does_not_restart_on_retries() {
    let h = Harness::new().await;
    let id = h.register().await;
    let thief = h.register().await;
    let first = h.grant(&id, "mcp:tools").await;
    let old = first["refresh_token"].as_str().unwrap();
    h.refresh(&id, old, None).await;

    let rotated_at = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT rotated_at FROM oauth_refresh_tokens WHERE token_hash = ?",
        )
        .bind(sha256(old))
        .fetch_one(h.db.pool())
        .await
        .unwrap()
    };
    let first_rotation = rotated_at().await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(h.refresh(&id, old, None).await.status, StatusCode::OK);
    assert_eq!(rotated_at().await, first_rotation);

    h.refresh(&thief, old, None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(h.count("oauth_refresh_tokens").await, 0, "and it revokes");
}

#[tokio::test]
async fn a_widening_retry_within_the_grace_changes_nothing() {
    let h = Harness::new().await;
    let id = h.register().await;
    let first = h.grant(&id, "mcp:tools").await;
    let old = first["refresh_token"].as_str().unwrap();
    let rotated = h.refresh(&id, old, None).await.json();
    h.refresh(&id, old, Some("mcp:tools mcp:submissions"))
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_scope");
    assert_eq!(
        h.refresh(&id, rotated["refresh_token"].as_str().unwrap(), None)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_refreshes_leave_one_live_token() {
    let h = Arc::new(Harness::new().await);
    let id = h.register().await;
    let refresh = h.grant(&id, "mcp:tools").await["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (h, id, refresh) = (h.clone(), id.clone(), refresh.clone());
        tasks.push(tokio::spawn(
            async move { h.refresh(&id, &refresh, None).await },
        ));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().status, StatusCode::OK);
    }
    assert_eq!(h.live_refresh_tokens().await, 1);
}

#[tokio::test]
async fn codes_are_bound_to_client_redirect_and_challenge() {
    let h = Harness::new().await;
    let id = h.register().await;
    let other = h.register().await;
    let code = h.code_for(&id, "mcp:tools").await;

    h.exchange(&other, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    h.token(&format!(
        "grant_type=authorization_code&client_id={id}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
        encode(&code),
        encode("https://client.example.test/other"),
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    // A wrong verifier burns the code: the right one fails afterwards.
    h.token(&format!(
        "grant_type=authorization_code&client_id={id}&code={}&redirect_uri={}&code_verifier=wrong-verifier",
        encode(&code),
        encode(REDIRECT),
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    h.exchange(&id, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(h.count("oauth_access_tokens").await, 0);
}

#[tokio::test]
async fn plain_pkce_cannot_succeed() {
    let h = Harness::new().await;
    let id = h.register().await;
    let code = h.code_for(&id, "mcp:tools").await;
    // Under `plain`, the verifier would equal the challenge.
    h.token(&format!(
        "grant_type=authorization_code&client_id={id}&code={}&redirect_uri={}&code_verifier={CHALLENGE}",
        encode(&code),
        encode(REDIRECT),
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
}

#[tokio::test]
async fn expired_codes_and_tokens_fail() {
    let h = Harness::new().await;
    let id = h.register().await;

    let code = h.code_for(&id, "mcp:tools").await;
    execute(
        &h,
        &format!("UPDATE oauth_auth_codes SET expires_at = '{PAST}'"),
    )
    .await;
    h.exchange(&id, &code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    let tokens = h.grant(&id, "mcp:tools").await;
    execute(
        &h,
        &format!("UPDATE oauth_access_tokens SET expires_at = '{PAST}'"),
    )
    .await;
    assert_eq!(
        h.whoami(tokens["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    execute(
        &h,
        &format!("UPDATE oauth_refresh_tokens SET expires_at = '{PAST}'"),
    )
    .await;
    h.refresh(&id, tokens["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
}

#[tokio::test]
async fn codes_and_tokens_are_hashed_at_rest() {
    let h = Harness::new().await;
    let id = h.register().await;
    let code = h.code_for(&id, "mcp:tools").await;
    let stored: String = sqlx::query_scalar("SELECT code FROM oauth_auth_codes")
        .fetch_one(h.db.pool())
        .await
        .unwrap();
    assert_eq!(stored, sha256(&code));
    assert_ne!(stored, code);

    let tokens = h.exchange(&id, &code).await.json();
    for (table, field) in [
        ("oauth_access_tokens", "access_token"),
        ("oauth_refresh_tokens", "refresh_token"),
    ] {
        let stored: String =
            sqlx::query_scalar(AssertSqlSafe(format!("SELECT token_hash FROM {table}")))
                .fetch_one(h.db.pool())
                .await
                .unwrap();
        assert_eq!(stored, sha256(tokens[field].as_str().unwrap()));
    }
}

#[tokio::test]
async fn token_endpoint_authenticates_confidential_clients() {
    let h = Harness::new().await;
    let client = h
        .register_with(json!({ "redirect_uris": [REDIRECT] }))
        .await;
    let id = client["client_id"].as_str().unwrap();
    let secret = client["client_secret"].as_str().unwrap();

    h.token("grant_type=authorization_code")
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_client");
    for body in [
        format!("client_id={id}"),
        format!("client_id={id}&client_secret=wrong"),
        format!("client_id={id}&client_secret={secret}x"),
        "client_id=mcp_unknown".to_owned(),
    ] {
        h.token(&body)
            .await
            .assert_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    let basic = |user: &str, pass: &str| {
        Request::post("/api/mcp/token")
            .header(
                AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{pass}"))),
            )
            .body(Body::empty())
            .unwrap()
    };
    h.send(basic(id, "wrong"))
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "invalid_client");
    h.send(basic(id, secret))
        .await
        .assert_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");

    // Malformed Basic header: no credentials, not a crash.
    let request = Request::post("/api/mcp/token")
        .header(AUTHORIZATION, "Basic %%%")
        .body(Body::empty())
        .unwrap();
    h.send(request)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_client");

    h.token(&format!(
        "client_id={id}&client_secret={secret}&grant_type=password"
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    h.token(&format!(
        "client_id={id}&client_secret={secret}&grant_type=authorization_code"
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    h.token(&format!(
        "client_id={id}&client_secret={secret}&grant_type=refresh_token"
    ))
    .await
    .assert_error(StatusCode::BAD_REQUEST, "invalid_request");
}

#[tokio::test]
async fn a_refresh_token_belongs_to_its_client() {
    let h = Harness::new().await;
    let id = h.register().await;
    let other = h.register().await;
    let tokens = h.grant(&id, "mcp:tools").await;
    let refresh = tokens["refresh_token"].as_str().unwrap();
    h.refresh(&other, refresh, None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    // The failed attempt doesn't consume it.
    assert_eq!(h.refresh(&id, refresh, None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn token_endpoint_is_rate_limited_per_source() {
    let h = Harness::new().await;
    for _ in 0..20 {
        h.token("client_id=mcp_unknown")
            .await
            .assert_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    let limited = h.token("client_id=mcp_unknown").await;
    limited.assert_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
    assert_eq!(
        limited.json()["error_description"],
        "Too many token requests"
    );
    assert!(limited.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    assert_eq!(limited.header("cache-control"), "no-store");

    let other = with_ip(
        form_req("/api/mcp/token", "client_id=mcp_unknown"),
        "203.0.113.9",
    );
    assert_eq!(h.send(other).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authorize_is_rate_limited_per_source_on_get_and_post() {
    let h = Harness::new().await;
    for _ in 0..30 {
        h.send(get_req("/api/mcp/authorize")).await;
        h.send(form_req("/api/mcp/authorize", "")).await;
    }
    let limited = h.send(get_req("/api/mcp/authorize")).await;
    limited.assert_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
    assert!(limited.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    h.send(form_req("/api/mcp/authorize", ""))
        .await
        .assert_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");

    let other = h
        .send(with_ip(get_req("/api/mcp/authorize"), "203.0.113.9"))
        .await;
    other.assert_error(StatusCode::BAD_REQUEST, "invalid_request");
}

#[tokio::test]
async fn failed_bearer_checks_are_limited_and_valid_tokens_never_are() {
    let h = Harness::new().await;
    let id = h.register().await;
    let token = h.grant(&id, "mcp:tools").await["access_token"]
        .as_str()
        .unwrap()
        .to_owned();

    for i in 0..30 {
        let reply = h.whoami(&format!("jt_guess{i}")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    for guess in ["jt_more", "mcp_at_more"] {
        let limited = h.whoami(guess).await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    }

    // The limited source's valid token still works, well past the failure limit.
    for _ in 0..50 {
        assert_eq!(h.whoami(&token).await.status, StatusCode::OK);
    }
    // A client starting discovery gets its 401, not a 429.
    let tokenless = h
        .send(Request::post("/api/mcp").body(Body::empty()).unwrap())
        .await;
    assert_eq!(tokenless.status, StatusCode::UNAUTHORIZED);
    assert!(tokenless.headers.contains_key("www-authenticate"));

    let elsewhere = with_ip(bearer_req("jt_elsewhere"), "203.0.113.9");
    assert_eq!(h.send(elsewhere).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn successful_bearer_traffic_does_not_count_towards_the_failure_limit() {
    let h = Harness::new().await;
    let raw = nexus_auth::issue_api_token(&h.db, "agent").await.unwrap();
    for _ in 0..40 {
        assert_eq!(h.whoami(&raw).await.status, StatusCode::OK);
    }
    assert_eq!(
        h.whoami("jt_one_typo").await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ── Scopes ───────────────────────────────────────────────────────────────────

async fn scopes_of(h: &Harness, access_token: &Value) -> Value {
    h.whoami(access_token.as_str().unwrap()).await.json()["scopes"].clone()
}

#[tokio::test]
async fn a_client_gets_only_the_scopes_consented_to() {
    let h = Harness::new().await;
    let id = h.register().await;
    let tools = h.grant(&id, "mcp:tools").await;
    assert_eq!(tools["scope"], "mcp:tools");
    assert_eq!(
        scopes_of(&h, &tools["access_token"]).await,
        json!(["mcp:tools"])
    );

    // Asking for scope at the token endpoint of a code exchange changes nothing.
    let code = h.code_for(&id, "mcp:tools").await;
    let reply = h
        .token(&format!(
            "grant_type=authorization_code&client_id={id}&code={}&redirect_uri={}&code_verifier={VERIFIER}&scope={}",
            encode(&code),
            encode(REDIRECT),
            encode("mcp:tools mcp:submissions"),
        ))
        .await;
    assert_eq!(reply.json()["scope"], "mcp:tools");

    // Re-authorizing with the added scope, through consent, is how to gain it.
    let both = h.grant(&id, "mcp:tools mcp:submissions").await;
    assert_eq!(both["scope"], "mcp:tools mcp:submissions");
    let version: i64 = sqlx::query_scalar(
        "SELECT sensitive_consent_version FROM oauth_access_tokens ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(h.db.pool())
    .await
    .unwrap();
    assert_eq!(version, nexus_auth::SENSITIVE_CONSENT_VERSION);
}

#[tokio::test]
async fn refresh_cannot_widen_and_a_rejected_widening_keeps_the_token() {
    let h = Harness::new().await;
    let id = h.register().await;
    let tools = h.grant(&id, "mcp:tools").await;
    let refresh = tools["refresh_token"].as_str().unwrap();

    let refused = h
        .refresh(&id, refresh, Some("mcp:tools mcp:submissions"))
        .await;
    refused.assert_error(StatusCode::BAD_REQUEST, "invalid_scope");
    assert_eq!(
        refused.json()["error_description"],
        "Requested scope exceeds the granted scope (mcp:submissions); re-authorization is required to obtain it"
    );
    assert_eq!(h.count("oauth_access_tokens").await, 1);

    let same = h.refresh(&id, refresh, Some("mcp:tools")).await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.body);
    assert_eq!(same.json()["scope"], "mcp:tools");
}

#[tokio::test]
async fn narrowing_applies_to_the_access_token_and_the_grant_survives_rotation() {
    let h = Harness::new().await;
    let id = h.register().await;
    let both = h.grant(&id, "mcp:tools mcp:submissions").await;

    let narrow = h
        .refresh(
            &id,
            both["refresh_token"].as_str().unwrap(),
            Some("mcp:tools"),
        )
        .await
        .json();
    assert_eq!(narrow["scope"], "mcp:tools");
    assert_eq!(
        scopes_of(&h, &narrow["access_token"]).await,
        json!(["mcp:tools"])
    );

    let restored = h
        .refresh(&id, narrow["refresh_token"].as_str().unwrap(), None)
        .await
        .json();
    assert_eq!(restored["scope"], "mcp:tools mcp:submissions");
}

#[tokio::test]
async fn pre_consent_version_submission_grants_stay_narrow() {
    let h = Harness::new().await;
    let id = h.register().await;
    let legacy = |hash: String| NewOAuthToken {
        token_hash: hash,
        grant_id: "legacy".to_owned(),
        client_id: id.clone(),
        scopes: vec!["mcp:tools".to_owned(), "mcp:submissions".to_owned()],
        sensitive_consent_version: 0,
        expires_at: Timestamp::parse(FUTURE).unwrap(),
    };
    let mut conn = h.db.conn().await.unwrap();
    repo::insert_access_token(&mut conn, &legacy(sha256("mcp_at_legacy")))
        .await
        .unwrap();
    repo::insert_refresh_token(&mut conn, &legacy(sha256("mcp_rt_legacy")))
        .await
        .unwrap();
    drop(conn);

    assert_eq!(
        h.whoami("mcp_at_legacy").await.json()["scopes"],
        json!(["mcp:tools"])
    );
    h.refresh(&id, "mcp_rt_legacy", Some("mcp:submissions"))
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_scope");
    let refreshed = h.refresh(&id, "mcp_rt_legacy", None).await.json();
    assert_eq!(refreshed["scope"], "mcp:tools");
}

// ── Login ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn session_cookie_has_the_required_attributes() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    let reply = browser.attempt_login(&h, PASSPHRASE, DEFAULT_IP).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/");
    let cookies: Vec<&str> = reply
        .headers
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    let session = cookies
        .iter()
        .find(|cookie| cookie.starts_with("__Host-nexus_session="))
        .unwrap();
    let value = session
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1;
    assert_eq!(value.len(), 64);
    assert!(session.ends_with("; Path=/; Max-Age=604800; HttpOnly; Secure; SameSite=Lax"));
}

#[tokio::test]
async fn wrong_passphrases_and_forged_login_posts_fail() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    let wrong = browser
        .attempt_login(&h, "not the passphrase", DEFAULT_IP)
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(!browser.cookies.contains_key("__Host-nexus_session"));
    assert!(
        !wrong.body.contains("not the passphrase"),
        "never reflected"
    );

    let form = browser.send(&h, get_req("/login")).await;
    let csrf = form.field("csrf");
    let pass = encode(PASSPHRASE);

    // No token.
    let reply = browser
        .send(&h, form_req("/login", &format!("passphrase={pass}")))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Wrong token.
    let reply = browser
        .send(
            &h,
            form_req(
                "/login",
                &format!("csrf={}&passphrase={pass}", "0".repeat(64)),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Token without its cookie, the cross-site case.
    let reply = h
        .send(form_req(
            "/login",
            &format!("csrf={csrf}&passphrase={pass}"),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Token tied to another browser's cookie.
    let mut other = Browser::default();
    other.send(&h, get_req("/login")).await;
    let reply = other
        .send(
            &h,
            form_req("/login", &format!("csrf={csrf}&passphrase={pass}")),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(!other.cookies.contains_key("__Host-nexus_session"));

    // Oversized passphrases fail without being hashed; an empty one simply fails.
    let form = browser.send(&h, get_req("/login")).await;
    let long = "x".repeat(2000);
    let reply = browser
        .send(
            &h,
            form_req(
                "/login",
                &format!("csrf={}&passphrase={long}", form.field("csrf")),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = browser
        .send(
            &h,
            form_req(
                "/login",
                &format!("csrf={}&passphrase=", form.field("csrf")),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn forged_login_posts_do_not_consume_login_attempts() {
    let h = Harness::new().await;
    for _ in 0..50 {
        let reply = h.send(form_req("/login", "csrf=x&passphrase=y")).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }
    Browser::default().login(&h).await;
}

#[tokio::test]
async fn login_backs_off_per_source_even_for_the_right_passphrase() {
    let h = Harness::new().await;
    let attacker = "203.0.113.5";
    let mut browser = Browser::default();
    for _ in 0..6 {
        let reply = browser
            .attempt_login(&h, "wrong guess here", attacker)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let blocked = browser.attempt_login(&h, PASSPHRASE, attacker).await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(blocked.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    assert!(!browser.cookies.contains_key("__Host-nexus_session"));

    // The owner elsewhere is unaffected, and a success clears their own count.
    let mut owner = Browser::default();
    owner.login(&h).await;
}

#[tokio::test]
async fn login_backs_off_globally_across_sources() {
    let h = Harness::new().await;
    exhaust_global_backoff(&h).await;
    let mut browser = Browser::default();
    let blocked = browser
        .attempt_login(&h, PASSPHRASE, "198.51.100.200")
        .await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
}

async fn exhaust_global_backoff(h: &Harness) {
    for i in 0..21 {
        let reply = Browser::default()
            .attempt_login(h, "wrong guess here", &format!("203.0.113.{i}"))
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }
}

#[tokio::test]
async fn a_known_device_can_log_in_through_an_attack_on_the_global_backoff() {
    let h = Harness::new().await;
    let mut owner = Browser::default();
    owner.login(&h).await;
    let cookie = owner
        .cookies
        .get("__Host-nexus_device")
        .expect("login marks the device")
        .clone();
    assert_eq!(cookie.len(), 64);

    // Logging out leaves the mark, and the owner is back at the login form when the attack starts.
    let page = owner.send(&h, get_req("/logout")).await;
    owner
        .send(
            &h,
            form_req("/logout", &format!("csrf={}", page.field("csrf"))),
        )
        .await;
    assert!(owner.cookies.contains_key("__Host-nexus_device"));
    exhaust_global_backoff(&h).await;

    let blocked = Browser::default()
        .attempt_login(&h, PASSPHRASE, "198.51.100.200")
        .await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);

    // A forged mark doesn't help.
    let mut forged = Browser::default();
    forged
        .cookies
        .insert("__Host-nexus_device".to_owned(), "0".repeat(64));
    let reply = forged.attempt_login(&h, PASSPHRASE, "198.51.100.201").await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);

    let reply = owner.attempt_login(&h, PASSPHRASE, "198.51.100.202").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
}

#[tokio::test]
async fn a_known_device_still_needs_the_passphrase_and_has_its_own_backoff() {
    let h = Harness::new().await;
    let mut device = Browser::default();
    device.login(&h).await;
    let page = device.send(&h, get_req("/logout")).await;
    device
        .send(
            &h,
            form_req("/logout", &format!("csrf={}", page.field("csrf"))),
        )
        .await;

    // Rotating source addresses doesn't escape the device bucket.
    for i in 0..6 {
        let reply = device
            .attempt_login(&h, "wrong guess here", &format!("203.0.113.{i}"))
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {i}");
        assert!(!device.cookies.contains_key("__Host-nexus_session"));
    }
    let reply = device.attempt_login(&h, PASSPHRASE, "203.0.113.50").await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);

    // Its failures don't count against anyone else.
    Browser::default().login(&h).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_guesses_cannot_outrun_the_limiter() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    let form = browser.send(&h, get_req("/login")).await;
    let cookie = browser
        .cookies
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .next()
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..40 {
        let mut request = with_ip(
            form_req(
                "/login",
                &format!("csrf={}&passphrase=wrong", form.field("csrf")),
            ),
            "203.0.113.5",
        );
        request
            .headers_mut()
            .insert(COOKIE, cookie.parse().unwrap());
        let app = h.app.clone();
        tasks.push(tokio::spawn(async move {
            app.oneshot(request).await.unwrap().status()
        }));
    }
    let mut verified = 0;
    for task in tasks {
        match task.await.unwrap() {
            StatusCode::UNAUTHORIZED => verified += 1,
            status => assert_eq!(status, StatusCode::TOO_MANY_REQUESTS),
        }
    }
    assert_eq!(verified, 6, "5 free attempts, then the first with a delay");
}

fn cf_proxy() -> TrustedProxy {
    TrustedProxy {
        header: HeaderName::from_static("cf-connecting-ip"),
        peers: vec!["127.0.0.1".parse().unwrap()],
    }
}

async fn guess(
    h: &Harness,
    peer: &str,
    header: Option<(&str, &str)>,
    form: &Reply,
    cookie: &str,
) -> StatusCode {
    let mut request = with_ip(
        form_req(
            "/login",
            &format!("csrf={}&passphrase=wrong", form.field("csrf")),
        ),
        peer,
    );
    request
        .headers_mut()
        .insert(COOKIE, cookie.parse().unwrap());
    if let Some((name, value)) = header {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    h.send(request).await.status
}

async fn login_form_and_cookie(h: &Harness) -> (Reply, String) {
    let mut browser = Browser::default();
    let form = browser.send(h, get_req("/login")).await;
    let cookie = browser
        .cookies
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .next()
        .unwrap();
    (form, cookie)
}

#[tokio::test]
async fn forwarded_headers_are_ignored_by_default() {
    let h = Harness::new().await;
    let (form, cookie) = login_form_and_cookie(&h).await;
    for i in 0..6 {
        let ip = format!("192.0.2.{i}");
        let status = guess(
            &h,
            "127.0.0.1",
            Some(("x-forwarded-for", &ip)),
            &form,
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let status = guess(
        &h,
        "127.0.0.1",
        Some(("x-forwarded-for", "192.0.2.99")),
        &form,
        &cookie,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "rotating the header changes nothing"
    );
}

#[tokio::test]
async fn a_configured_proxy_header_is_trusted_only_from_its_peers() {
    let h = Harness::with_proxy(Some(cf_proxy())).await;
    let (form, cookie) = login_form_and_cookie(&h).await;
    let via = |ip: &'static str| Some(("cf-connecting-ip", ip));

    for _ in 0..6 {
        assert_eq!(
            guess(&h, "127.0.0.1", via("192.0.2.1"), &form, &cookie).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        guess(&h, "127.0.0.1", via("192.0.2.1"), &form, &cookie).await,
        StatusCode::TOO_MANY_REQUESTS,
    );
    assert_eq!(
        guess(&h, "127.0.0.1", via("192.0.2.2"), &form, &cookie).await,
        StatusCode::UNAUTHORIZED,
        "another client behind the proxy has its own count"
    );

    // Someone reaching the server directly can't rotate their key with the header.
    for i in 0..6 {
        let ip: &'static str = Box::leak(format!("192.0.2.{}", 10 + i).into_boxed_str());
        assert_eq!(
            guess(&h, "203.0.113.7", via(ip), &form, &cookie).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        guess(&h, "203.0.113.7", via("192.0.2.50"), &form, &cookie).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // From a trusted peer with no usable header, the peer itself is the key.
    assert_eq!(
        guess(&h, "127.0.0.1", via("not-an-ip"), &form, &cookie).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_next_cannot_redirect_off_site() {
    let h = Harness::new().await;
    for bad in [
        "//evil.example.test",
        "https://evil.example.test",
        "/\\evil.example.test",
        "/\t/evil.example.test",
        "javascript:alert(1)",
    ] {
        let mut browser = Browser::default();
        let form = browser
            .send(&h, get_req(&format!("/login?next={}", encode(bad))))
            .await;
        assert!(form.try_field("next").is_none(), "{bad}");
        let reply = browser
            .send(
                &h,
                form_req(
                    "/login",
                    &format!(
                        "csrf={}&next={}&passphrase={}",
                        form.field("csrf"),
                        encode(bad),
                        encode(PASSPHRASE)
                    ),
                ),
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{bad}");
        assert_eq!(reply.location(), "/", "{bad}");

        // Already logged in, the GET redirect is held to the same rule.
        let reply = browser
            .send(&h, get_req(&format!("/login?next={}", encode(bad))))
            .await;
        assert_eq!(reply.location(), "/", "{bad}");
    }

    let mut browser = Browser::default();
    browser.login(&h).await;
    let reply = browser
        .send(
            &h,
            get_req(&format!("/login?next={}", encode("/dashboard?a=1"))),
        )
        .await;
    assert_eq!(reply.location(), "/dashboard?a=1");
}

#[tokio::test]
async fn login_page_escapes_next_and_blocks_framing() {
    let h = Harness::new().await;
    let hostile = r#"/x"><script>alert(1)</script>"#;
    let page = h
        .send(get_req(&format!("/login?next={}", encode(hostile))))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("<script"), "{}", page.body);
    assert_eq!(page.field("next"), hostile);
    let csp = page.header("content-security-policy");
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("form-action 'self'"));
    assert_eq!(page.header("x-frame-options"), "DENY");
    assert_eq!(page.header("cache-control"), "no-store");
}

#[tokio::test]
async fn logout_needs_csrf_and_revokes_on_the_server() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    assert_eq!(
        browser.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );

    let page = browser.send(&h, get_req("/logout")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.header("content-security-policy")
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(
        browser.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK,
        "GET doesn't log out"
    );

    let forged = browser.send(&h, form_req("/logout", "csrf=x")).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(
        browser.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );

    let stolen_cookie = browser.cookies.clone();
    let done = browser
        .send(
            &h,
            form_req("/logout", &format!("csrf={}", page.field("csrf"))),
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    assert_eq!(done.location(), "/login");
    assert!(done.header("set-cookie").contains("Max-Age=0"));

    // The old cookie is dead even if a copy survived.
    let mut replay = Browser {
        cookies: stolen_cookie,
    };
    let reply = replay.send(&h, get_req("/dashboard")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn dashboard_middleware_redirects_to_login_and_back() {
    let h = Harness::new().await;
    let reply = h.send(get_req("/dashboard?a=1")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/login?next=%2Fdashboard%3Fa%3D1");

    let post = h.send(form_req("/dashboard", "")).await;
    assert_eq!(post.location(), "/login");

    let forged = Request::get("/dashboard")
        .header(COOKIE, format!("__Host-nexus_session={}", "0".repeat(64)))
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(forged).await.status, StatusCode::SEE_OTHER);

    // A bearer token is not a browser session.
    let id = h.register().await;
    let tokens = h.grant(&id, "mcp:tools").await;
    let bearer = Request::get("/dashboard")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(bearer).await.status, StatusCode::SEE_OTHER);

    let mut browser = Browser::default();
    browser.login(&h).await;
    let reply = browser.send(&h, get_req("/dashboard")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, "dashboard");
}

#[tokio::test]
async fn sessions_do_not_authenticate_mcp_calls() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let reply = browser
        .send(&h, Request::post("/api/mcp").body(Body::empty()).unwrap())
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

// ── Startup and operator functions ───────────────────────────────────────────

#[tokio::test]
async fn auth_state_refuses_to_build_without_a_valid_hash_or_base_url() {
    let h = Harness::new().await;
    let config = |hash: &str, base: &str| AuthConfig {
        public_base_url: base.to_owned(),
        owner_passphrase_hash: hash.to_owned(),
        owner_passphrase_file: None,
        trusted_proxy: None,
        redirect_hosts: Vec::new(),
    };
    for hash in ["", "plaintext passphrase", "$2b$12$abcdefghijklmnopqrstuv"] {
        assert!(matches!(
            AuthState::new(h.db.clone(), config(hash, BASE)),
            Err(ConfigError::InvalidPassphraseHash)
        ));
    }
    assert!(matches!(
        AuthState::new(
            h.db.clone(),
            config(&weak_hash(), "http://crm.example.test")
        ),
        Err(ConfigError::InvalidBaseUrl)
    ));
    assert!(AuthState::new(h.db.clone(), config(&weak_hash(), BASE)).is_ok());
}

#[tokio::test]
async fn changing_the_passphrase_revokes_all_grants_and_api_tokens() {
    let h = Harness::new().await;
    let id = h.register().await;
    let tokens = h.grant(&id, "mcp:tools").await;
    let pending_code = h.code_for(&id, "mcp:tools").await;
    let api_token = nexus_auth::issue_api_token(&h.db, "agent").await.unwrap();
    let path = h.dir.path().join("hash");

    // A refused passphrase revokes nothing.
    assert!(matches!(
        set_owner_passphrase(&h.db, &path, "too short").await,
        Err(PassphraseError::TooShort)
    ));
    assert_eq!(h.whoami(&api_token).await.status, StatusCode::OK);

    set_owner_passphrase(&h.db, &path, PASSPHRASE)
        .await
        .unwrap();

    assert_eq!(h.whoami(&api_token).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.whoami(tokens["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    h.refresh(&id, tokens["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    h.exchange(&id, &pending_code)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(h.count("oauth_clients").await, 1, "clients stay registered");

    // The client can authorize again.
    let again = h.grant(&id, "mcp:tools").await;
    assert_eq!(
        h.whoami(again["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn set_owner_passphrase_writes_a_private_hash_that_logs_in() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hash");
    let store = tempfile::tempdir().unwrap();
    let db = nexus_core::db::Db::open(store.path()).await.unwrap();
    assert!(matches!(
        set_owner_passphrase(&db, &path, "too short").await,
        Err(PassphraseError::TooShort)
    ));
    assert!(matches!(
        set_owner_passphrase(&db, &path, &"x".repeat(1025)).await,
        Err(PassphraseError::TooLong)
    ));
    assert!(!path.exists());

    set_owner_passphrase(&db, &path, PASSPHRASE).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.starts_with("$argon2id$"));
    assert!(!stored.contains(PASSPHRASE));
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no temp file left"
    );

    // Replacing keeps the mode, and the hash really verifies through the login route.
    set_owner_passphrase(&db, &path, PASSPHRASE).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let harness = Harness::new().await;
    let state = AuthState::new(
        harness.db.clone(),
        AuthConfig {
            public_base_url: BASE.to_owned(),
            owner_passphrase_hash: std::fs::read_to_string(&path).unwrap().trim().to_owned(),
            owner_passphrase_file: None,
            trusted_proxy: None,
            redirect_hosts: Vec::new(),
        },
    )
    .unwrap();
    let app = nexus_auth::router(state);
    let h = Harness {
        app,
        db: harness.db,
        dir: harness.dir,
    };
    Browser::default().login(&h).await;
    let mut wrong = Browser::default();
    let reply = wrong
        .attempt_login(&h, "another passphrase", DEFAULT_IP)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

// ── Changing the passphrase in the browser ───────────────────────────────────

const NEW_PASSPHRASE: &str = "an entirely different passphrase";

fn hash_file(h: &Harness) -> std::path::PathBuf {
    h.dir.path().join("owner-passphrase.argon2")
}

/// Fetches the change form and posts it.
async fn change_passphrase(
    browser: &mut Browser,
    h: &Harness,
    current: &str,
    new: &str,
    repeat: &str,
) -> Reply {
    let page = browser.send(h, get_req("/passphrase")).await;
    let body = format!(
        "csrf={}&current={}&new={}&repeat={}",
        page.field("csrf"),
        encode(current),
        encode(new),
        encode(repeat)
    );
    browser.send(h, form_req("/passphrase", &body)).await
}

#[tokio::test]
async fn the_owner_changes_the_passphrase_in_the_browser() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::new().await;
    let id = h.register().await;
    let tokens = h.grant(&id, "mcp:tools").await;
    let api_token = nexus_auth::issue_api_token(&h.db, "agent").await.unwrap();
    let mut browser = Browser::default();
    browser.login(&h).await;
    let mut other = Browser::default();
    other.login(&h).await;

    let page = browser.send(&h, get_req("/passphrase")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.header("cache-control"), "no-store");
    for said in [
        "Each client must be authorized again",
        "Every browser session ends too, this one included",
        r#"name="current" autocomplete="current-password""#,
        r#"name="new" autocomplete="new-password""#,
        r#"name="repeat" autocomplete="new-password""#,
    ] {
        assert!(page.body.contains(said), "{said}");
    }

    let done =
        change_passphrase(&mut browser, &h, PASSPHRASE, NEW_PASSPHRASE, NEW_PASSPHRASE).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert!(done.body.contains("Passphrase changed"));
    assert!(!done.body.contains(NEW_PASSPHRASE));
    assert!(done.header("set-cookie").contains("Max-Age=0"));
    assert_eq!(
        std::fs::metadata(hash_file(&h))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // Every session is over, the other browser's too, and so is every credential.
    for session in [&mut browser, &mut other] {
        let reply = session.send(&h, get_req("/dashboard")).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
    }
    assert_eq!(h.whoami(&api_token).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.whoami(tokens["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    h.refresh(&id, tokens["refresh_token"].as_str().unwrap(), None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    // The new passphrase logs in without a restart, and the old one doesn't.
    let mut fresh = Browser::default();
    let old = fresh.attempt_login(&h, PASSPHRASE, DEFAULT_IP).await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);
    let new = fresh.attempt_login(&h, NEW_PASSPHRASE, DEFAULT_IP).await;
    assert_eq!(new.status, StatusCode::SEE_OTHER, "{}", new.body);
}

#[tokio::test]
async fn a_refused_passphrase_change_changes_nothing() {
    let h = Harness::new().await;
    let api_token = nexus_auth::issue_api_token(&h.db, "agent").await.unwrap();
    let mut browser = Browser::default();
    browser.login(&h).await;

    for (current, new, repeat, status, says) in [
        (
            "not the passphrase at all",
            NEW_PASSPHRASE,
            NEW_PASSPHRASE,
            StatusCode::UNAUTHORIZED,
            "The current passphrase is not correct",
        ),
        (
            PASSPHRASE,
            NEW_PASSPHRASE,
            "another passphrase entirely",
            StatusCode::BAD_REQUEST,
            "The two new passphrases differ",
        ),
        (
            PASSPHRASE,
            "too short",
            "too short",
            StatusCode::BAD_REQUEST,
            "at least 16 characters",
        ),
        (
            PASSPHRASE,
            "abcabcabcabcabcabc",
            "abcabcabcabcabcabc",
            StatusCode::BAD_REQUEST,
            "one short pattern repeated",
        ),
    ] {
        let reply = change_passphrase(&mut browser, &h, current, new, repeat).await;
        assert_eq!(reply.status, status, "{}", reply.body);
        assert!(reply.body.contains(says), "{}", reply.body);
        for typed in [current, new, repeat] {
            assert!(!reply.body.contains(typed), "{typed} is echoed");
        }
    }

    // Without the form's own token nothing happens, whatever else is right.
    let fields = format!(
        "current={}&new={new}&repeat={new}",
        encode(PASSPHRASE),
        new = encode(NEW_PASSPHRASE)
    );
    let logout_csrf = browser.send(&h, get_req("/logout")).await.field("csrf");
    for csrf in ["", "csrf=x&", &format!("csrf={logout_csrf}&")] {
        let forged = browser
            .send(&h, form_req("/passphrase", &format!("{csrf}{fields}")))
            .await;
        assert_eq!(forged.status, StatusCode::FORBIDDEN, "{csrf}");
    }

    assert!(!hash_file(&h).exists());
    assert_eq!(h.whoami(&api_token).await.status, StatusCode::OK);
    assert_eq!(
        browser.send(&h, get_req("/dashboard")).await.status,
        StatusCode::OK
    );
    Browser::default().login(&h).await;
}

#[tokio::test]
async fn changing_the_passphrase_needs_a_session() {
    let h = Harness::new().await;
    let page = h.send(get_req("/passphrase")).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location(), "/login?next=/passphrase");

    // Even a real token is worth nothing without the session it belongs to.
    let mut browser = Browser::default();
    browser.login(&h).await;
    let csrf = browser.send(&h, get_req("/passphrase")).await.field("csrf");
    let body = format!(
        "csrf={csrf}&current={}&new={new}&repeat={new}",
        encode(PASSPHRASE),
        new = encode(NEW_PASSPHRASE)
    );
    let reply = h.send(form_req("/passphrase", &body)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/login");
    assert!(!hash_file(&h).exists());
    Browser::default().login(&h).await;
}

#[tokio::test]
async fn guesses_at_the_current_passphrase_meet_the_login_backoff() {
    let h = Harness::new().await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    for _ in 0..6 {
        let reply = change_passphrase(
            &mut browser,
            &h,
            "a guess at the passphrase",
            NEW_PASSPHRASE,
            NEW_PASSPHRASE,
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    // Now even the right one waits, as at the login.
    let locked =
        change_passphrase(&mut browser, &h, PASSPHRASE, NEW_PASSPHRASE, NEW_PASSPHRASE).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.header(RETRY_AFTER.as_str()).parse::<u64>().unwrap() > 0);
    assert!(locked.body.contains("Too many attempts"));
    assert!(!hash_file(&h).exists());
}

#[tokio::test]
async fn a_passphrase_from_the_environment_cannot_be_changed_in_the_browser() {
    let harness = Harness::new().await;
    let state = AuthState::new(
        harness.db.clone(),
        AuthConfig {
            public_base_url: BASE.to_owned(),
            owner_passphrase_hash: weak_hash(),
            owner_passphrase_file: None,
            trusted_proxy: None,
            redirect_hosts: Vec::new(),
        },
    )
    .unwrap();
    let h = Harness {
        app: nexus_auth::router(state),
        db: harness.db,
        dir: harness.dir,
    };
    let mut browser = Browser::default();
    browser.login(&h).await;

    let page = browser.send(&h, get_req("/passphrase")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("NEXUS_OWNER_PASSPHRASE_HASH"));
    assert!(!page.body.contains("<form"));
    let reply = browser.send(&h, form_req("/passphrase", "")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(!reply.body.contains("<form"));
    Browser::default().login(&h).await;
}

// ── Redirect allowlist ───────────────────────────────────────────────────────

#[tokio::test]
async fn only_listed_hosts_and_loopback_can_register() {
    let h = Harness::new().await;
    for good in [
        REDIRECT,
        "https://client.example.test:8443/cb",
        "http://127.0.0.1:39123/callback",
        "http://localhost:39123/callback",
    ] {
        h.register_with(json!({ "redirect_uris": [good] })).await;
    }
    for evil in [
        "https://evil.test/cb",
        "https://client.example.test.evil.test/cb",
        "https://sub.client.example.test/cb",
        "https://client.example.test./cb",
        "https://client.example.test@evil.test/cb",
        "https://client.example.test;script-src.evil.test/cb",
        "https://client.example.test,evil.test/cb",
        "https://xn--laude-0ye.ai/cb",
    ] {
        let reply = h
            .send(json_req(
                "/api/mcp/register",
                &json!({ "redirect_uris": [REDIRECT, evil] }),
            ))
            .await;
        reply.assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
        assert!(
            reply.json()["error_description"]
                .as_str()
                .unwrap()
                .starts_with("redirect_uri host is not allowed: "),
            "{evil}: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn a_refused_registration_spends_no_budget() {
    let h = Harness::new().await;
    for i in 0..100 {
        let refused = with_ip(
            json_req(
                "/api/mcp/register",
                &json!({ "redirect_uris": ["https://evil.test/cb"] }),
            ),
            &format!("203.0.113.{i}"),
        );
        assert_eq!(h.send(refused).await.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(h.count("oauth_clients").await, 0);
    assert_eq!(
        h.send(register_from("203.0.113.1")).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn a_client_registered_before_the_list_is_refused_at_authorize() {
    let h = Harness::new().await;
    execute(
        &h,
        r#"INSERT INTO oauth_clients (client_id, redirect_uris, grant_types, response_types)
           VALUES ('old', '["https://evil.test/cb"]', '[]', '[]')"#,
    )
    .await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let reply = browser
        .send(
            &h,
            get_req(&format!(
                "/api/mcp/authorize?client_id=old&redirect_uri={}&code_challenge={CHALLENGE}&response_type=code",
                encode("https://evil.test/cb")
            )),
        )
        .await;
    reply.assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(reply.body.contains("evil.test"), "{}", reply.body);
    assert_eq!(h.count("oauth_auth_codes").await, 0);
}

#[tokio::test]
async fn a_client_name_cannot_hold_control_or_format_characters() {
    let h = Harness::new().await;
    for name in [
        "Claude\u{202e}ia.edualc",
        "Cl\u{200b}aude",
        "Claude\n",
        "Claude\u{7f}",
        "\u{feff}Claude",
    ] {
        h.send(json_req(
            "/api/mcp/register",
            &json!({ "client_name": name, "redirect_uris": [REDIRECT] }),
        ))
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    assert_eq!(h.count("oauth_clients").await, 0);
    // Other scripts and ordinary punctuation are fine, and the page isolates a stored name.
    let client = h
        .register_with(json!({
            "client_name": "Клиент — שלום",
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
        }))
        .await;
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(
            &h,
            get_req(&authorize_path(
                client["client_id"].as_str().unwrap(),
                "mcp:tools",
            )),
        )
        .await;
    assert!(
        page.body.contains("<bdi>Клиент — שלום</bdi>"),
        "{}",
        page.body
    );
}

// ── Grants ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn one_grant_can_be_listed_and_revoked_without_touching_the_others() {
    let h = Harness::new().await;
    let id = h.register().await;
    let first = h.grant(&id, "mcp:tools mcp:submissions").await;
    let second = h.grant(&id, "mcp:tools").await;
    let token = |grant: &Value| grant["access_token"].as_str().unwrap().to_owned();

    let grants = nexus_auth::list_grants(&h.db).await.unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().all(|g| g.client_id == id
        && g.client_name == "Synthetic Client"
        && g.redirect_origin == "https://client.example.test"
        && g.id.len() == 12));
    assert!(
        grants
            .iter()
            .any(|g| g.scopes == "mcp:tools mcp:submissions")
    );

    assert!(matches!(
        nexus_auth::revoke_grant(&h.db, "").await,
        Err(nexus_auth::RevokeError::NotFound)
    ));
    assert!(matches!(
        nexus_auth::revoke_grant(&h.db, "0000000000000").await,
        Err(nexus_auth::RevokeError::NotFound)
    ));

    let target = grants.iter().find(|g| g.scopes == "mcp:tools").unwrap();
    let revoked = nexus_auth::revoke_grant(&h.db, &target.id).await.unwrap();
    assert_eq!((revoked.access_tokens, revoked.refresh_tokens), (1, 1));
    assert_eq!(
        h.whoami(&token(&second)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.whoami(&token(&first)).await.status, StatusCode::OK);
    assert_eq!(nexus_auth::list_grants(&h.db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn set_passphrase_refuses_short_and_repetitive_passphrases() {
    let h = Harness::new().await;
    let path = h.dir.path().join("hash");
    assert!(matches!(
        set_owner_passphrase(&h.db, &path, "fifteen chars..").await,
        Err(PassphraseError::TooShort)
    ));
    for weak in ["aaaaaaaaaaaaaaaa", "abcdabcdabcdabcd"] {
        assert!(matches!(
            set_owner_passphrase(&h.db, &path, weak).await,
            Err(PassphraseError::Repetitive)
        ));
    }
    assert!(!path.exists());
}
