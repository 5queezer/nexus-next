//! The authorization flow end to end, through the routers, and what real clients need of it.

mod common;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use nexus_auth::issue_api_token;
use nexus_core::repo::api_tokens;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value))
}

#[tokio::test]
async fn full_flow_register_login_consent_token_call_refresh_replay() {
    let h = Harness::new().await;

    // An unauthenticated MCP call says where to find the authorization server.
    let unauthorized = h.send(bearer_req("nope")).await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
    assert!(unauthorized.header("www-authenticate").contains(&format!(
        r#"resource_metadata="{BASE}/.well-known/oauth-protected-resource""#
    )));

    let client = h
        .register_with(json!({
            "client_name": "Synthetic Client",
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
        }))
        .await;
    let client_id = client["client_id"].as_str().unwrap();

    // No session yet: authorize sends the browser to log in and come back.
    let mut browser = Browser::default();
    let path = authorize_path(client_id, "mcp:tools mcp:submissions");
    let to_login = browser.send(&h, get_req(&path)).await;
    assert_eq!(to_login.status, StatusCode::SEE_OTHER);
    assert!(to_login.location().starts_with("/login?next="));

    let form = browser.send(&h, get_req(to_login.location())).await;
    assert_eq!(form.status, StatusCode::OK);
    assert_eq!(form.field("next"), path);

    let logged_in = browser
        .send(
            &h,
            form_req(
                "/login",
                &format!(
                    "csrf={}&next={}&passphrase={}",
                    form.field("csrf"),
                    encode(&form.field("next")),
                    encode(PASSPHRASE)
                ),
            ),
        )
        .await;
    assert_eq!(
        logged_in.status,
        StatusCode::SEE_OTHER,
        "{}",
        logged_in.body
    );
    assert_eq!(logged_in.location(), path);

    // Logged in, the same URL now asks for consent, and issues nothing yet.
    let consent = browser.send(&h, get_req(logged_in.location())).await;
    assert_eq!(consent.status, StatusCode::OK);
    assert!(consent.body.contains("Synthetic Client"));
    assert!(consent.body.contains("https://client.example.test"));
    assert!(consent.body.contains("mcp:tools mcp:submissions"));
    assert!(consent.body.contains("exact application submissions"));
    assert_eq!(h.count("oauth_auth_codes").await, 0);

    let approved = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&consent.field("consent"))),
            ),
        )
        .await;
    assert_eq!(approved.status, StatusCode::SEE_OTHER);
    assert_eq!(approved.header("cache-control"), "no-store");
    let location = approved.location();
    assert!(location.starts_with(REDIRECT));
    assert_eq!(query_param(location, "state").unwrap(), "st-1");
    let code = query_param(location, "code").unwrap();

    let tokens = h.exchange(client_id, &code).await;
    assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.body);
    assert_eq!(tokens.header("cache-control"), "no-store");
    let tokens = tokens.json();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 86400);
    assert_eq!(tokens["scope"], "mcp:tools mcp:submissions");
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert!(access.starts_with("mcp_at_") && access.len() == 7 + 64);
    assert!(refresh.starts_with("mcp_rt_") && refresh.len() == 7 + 64);

    let call = h.whoami(access).await;
    assert_eq!(call.status, StatusCode::OK);
    assert_eq!(
        call.json(),
        json!({ "authType": "oauth", "scopes": ["mcp:tools", "mcp:submissions"] })
    );

    let rotated = h.refresh(client_id, refresh, None).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    let rotated = rotated.json();
    assert_ne!(rotated["refresh_token"], tokens["refresh_token"]);
    assert_eq!(
        h.whoami(rotated["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::OK
    );

    // Past the retry grace, presenting the old refresh token again ends the whole grant.
    h.end_refresh_grace().await;
    h.refresh(client_id, refresh, None)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");
    assert_eq!(
        h.whoami(rotated["access_token"].as_str().unwrap())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    bearer_methods_supported: Vec<String>,
    scopes_supported: Vec<String>,
    resource_name: String,
}

#[tokio::test]
async fn discovery_metadata_has_old_nexus_shape_and_is_usable_by_clients() {
    let h = Harness::new().await;
    let reply = h
        .send(get_req("/.well-known/oauth-authorization-server"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("cache-control"), "public, max-age=3600");
    assert_eq!(
        reply.json(),
        json!({
            "issuer": BASE,
            "authorization_endpoint": format!("{BASE}/api/mcp/authorize"),
            "token_endpoint": format!("{BASE}/api/mcp/token"),
            "registration_endpoint": format!("{BASE}/api/mcp/register"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp:tools", "mcp:submissions"],
        })
    );

    // What an RFC 8414 / MCP authorization client does with it.
    let server: ServerMetadata = serde_json::from_str(&reply.body).unwrap();
    for endpoint in [
        &server.authorization_endpoint,
        &server.token_endpoint,
        &server.registration_endpoint,
    ] {
        let url = Url::parse(endpoint).unwrap();
        assert_eq!(url.scheme(), "https");
        assert!(endpoint.starts_with(&server.issuer));
    }
    assert_eq!(Url::parse(&server.issuer).unwrap().path(), "/");
    assert!(!server.issuer.ends_with('/'));

    for path in [
        "/.well-known/oauth-protected-resource",
        // RFC 9728 §3.1: strict clients append the resource path.
        "/.well-known/oauth-protected-resource/api/mcp",
        "/.well-known/oauth-protected-resource/any/other/path",
    ] {
        let reply = h.send(get_req(path)).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.header("cache-control"), "public, max-age=3600");
        assert_eq!(
            reply.json(),
            json!({
                "resource": format!("{BASE}/api/mcp"),
                "authorization_servers": [BASE],
                "bearer_methods_supported": ["header"],
                "scopes_supported": ["mcp:tools", "mcp:submissions"],
                "resource_name": "Nexus CRM MCP Server",
            })
        );
        let resource: ResourceMetadata = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(resource.authorization_servers[0], server.issuer);
    }

    // The URL in WWW-Authenticate resolves on this router.
    let challenge = h.send(bearer_req("nope")).await;
    let header = challenge.header("www-authenticate");
    let start = header.find("resource_metadata=\"").unwrap() + "resource_metadata=\"".len();
    let url = &header[start..header.len() - 1];
    let path = url.strip_prefix(BASE).unwrap();
    assert_eq!(h.send(get_req(path)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn spoofed_host_headers_cannot_change_the_issuer() {
    let h = Harness::new().await;
    let spoofed = |path: &str| {
        Request::get(path)
            .header("host", "evil.example.test")
            .header("x-forwarded-host", "evil.example.test")
            .header("x-forwarded-proto", "http")
            .header("forwarded", "host=evil.example.test")
            .body(Body::empty())
            .unwrap()
    };
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource",
    ] {
        let reply = h.send(spoofed(path)).await;
        assert!(reply.body.contains(BASE));
        assert!(!reply.body.contains("evil"), "{}", reply.body);
    }
    let mut unauthorized = spoofed("/api/mcp");
    *unauthorized.method_mut() = axum::http::Method::POST;
    let reply = h.send(unauthorized).await;
    assert!(!reply.header("www-authenticate").contains("evil"));
}

#[tokio::test]
async fn missing_or_bad_credentials_get_exactly_old_nexus_401() {
    let h = Harness::new().await;
    let expected_header = format!(
        r#"Bearer realm="{BASE}/api/mcp", resource_metadata="{BASE}/.well-known/oauth-protected-resource""#
    );
    let mut requests = vec![
        (
            "no header",
            Request::post("/api/mcp").body(Body::empty()).unwrap(),
        ),
        ("unknown API token", bearer_req("jt_unknown")),
        ("unknown OAuth token", bearer_req("mcp_at_unknown")),
        (
            "refresh token as access token",
            bearer_req("mcp_rt_unknown"),
        ),
        ("empty bearer", bearer_req("   ")),
    ];
    // The scheme match is case-sensitive in old Nexus.
    for value in ["bearer jt_x", "Basic abc", "Bearerjt_x"] {
        requests.push((
            value,
            Request::post("/api/mcp")
                .header(AUTHORIZATION, value)
                .body(Body::empty())
                .unwrap(),
        ));
    }
    for (name, request) in requests {
        let reply = h.send(request).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(reply.header("www-authenticate"), expected_header, "{name}");
        assert_eq!(reply.header("content-type"), "application/json", "{name}");
        assert_eq!(
            reply.body, "{\"error\":\"Unauthorized – provide Bearer token\"}",
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn api_tokens_are_hashed_routed_by_prefix_and_carry_all_scopes() {
    let h = Harness::new().await;
    let raw = issue_api_token(&h.db, "agent").await.unwrap();
    assert!(raw.starts_with("jt_") && raw.len() == 3 + 64);

    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
        .fetch_all(h.db.pool())
        .await
        .unwrap();
    assert_eq!(stored, [sha256(&raw)]);

    let reply = h.whoami(&raw).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({ "authType": "api_token", "scopes": ["mcp:tools", "mcp:submissions"] })
    );

    // Recorded in the background, as old Nexus's fire-and-forget does.
    let mut touched = false;
    for _ in 0..200 {
        let last: Option<String> = sqlx::query_scalar("SELECT last_used_at FROM api_tokens")
            .fetch_one(h.db.pool())
            .await
            .unwrap();
        if last.is_some() {
            touched = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(touched);

    // One token at a time: issuing again revokes the first.
    let second = issue_api_token(&h.db, "agent").await.unwrap();
    assert_eq!(h.whoami(&raw).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.whoami(&second).await.status, StatusCode::OK);

    // `mcp_at_` always means OAuth, even if the same value is stored as an API token.
    let mut conn = h.db.conn().await.unwrap();
    api_tokens::replace(&mut conn, &sha256("mcp_at_lookalike"), "x")
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        h.whoami("mcp_at_lookalike").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn client_secret_post_and_basic_clients_complete_the_flow() {
    let h = Harness::new().await;

    let post = h
        .register_with(json!({ "redirect_uris": [REDIRECT] }))
        .await;
    assert_eq!(post["token_endpoint_auth_method"], "client_secret_post");
    let id = post["client_id"].as_str().unwrap();
    let secret = post["client_secret"].as_str().unwrap();
    let code = h.code_for(id, "mcp:tools").await;
    let reply = h
        .token(&format!(
            "grant_type=authorization_code&client_id={id}&client_secret={secret}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
            encode(&code),
            encode(REDIRECT)
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let basic = h
        .register_with(json!({
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "client_secret_basic",
        }))
        .await;
    let id = basic["client_id"].as_str().unwrap();
    let secret = basic["client_secret"].as_str().unwrap();
    let code = h.code_for(id, "mcp:tools").await;
    let request = Request::post("/api/mcp/token")
        .header(
            AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{id}:{secret}"))),
        )
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={VERIFIER}",
            encode(&code),
            encode(REDIRECT)
        )))
        .unwrap();
    let reply = h.send(request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn what_old_nexus_accepts_stays_accepted() {
    let h = Harness::new().await;
    let client_id = h.register().await;
    let mut browser = Browser::default();
    browser.login(&h).await;

    // No code_challenge_method, no scope, and an RFC 8707 resource indicator, which old Nexus
    // ignores. The challenge is still treated as S256.
    let path = format!(
        "/api/mcp/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&resource={}",
        encode(REDIRECT),
        encode(&format!("{BASE}/api/mcp")),
    );
    let page = browser.send(&h, get_req(&path)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<code>mcp:tools</code>"));
    let done = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&page.field("consent"))),
            ),
        )
        .await;
    let location = done.location();
    assert_eq!(query_param(location, "state"), None);
    let code = query_param(location, "code").unwrap();

    // A JSON token request with a resource indicator and a stray non-string field.
    let reply = h
        .send(json_req(
            "/api/mcp/token",
            &json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "code": code,
                "redirect_uri": REDIRECT,
                "code_verifier": VERIFIER,
                "resource": format!("{BASE}/api/mcp"),
                "ignored": 1,
            }),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["scope"], "mcp:tools");

    // A refresh with an odd Content-Type, parsed as a form as old Nexus does.
    let refresh = reply.json()["refresh_token"].as_str().unwrap().to_owned();
    let request = Request::post("/api/mcp/token")
        .header(CONTENT_TYPE, "text/plain; charset=weird")
        .body(Body::from(format!(
            "grant_type=refresh_token&client_id={client_id}&refresh_token={refresh}"
        )))
        .unwrap();
    let reply = h.send(request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn native_clients_can_use_any_loopback_port() {
    let h = Harness::new().await;
    let client = h
        .register_with(json!({
            "redirect_uris": ["http://127.0.0.1:39123/callback"],
            "token_endpoint_auth_method": "none",
        }))
        .await;
    let id = client["client_id"].as_str().unwrap();
    let redirect = "http://127.0.0.1:47987/callback";
    let mut browser = Browser::default();
    browser.login(&h).await;
    let page = browser
        .send(
            &h,
            get_req(&format!(
                "/api/mcp/authorize?response_type=code&client_id={id}&redirect_uri={}&code_challenge={CHALLENGE}",
                encode(redirect)
            )),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.header("content-security-policy")
            .contains("form-action 'self' http://127.0.0.1:47987")
    );
    let done = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&page.field("consent"))),
            ),
        )
        .await;
    assert!(done.location().starts_with(redirect));
    let code = query_param(done.location(), "code").unwrap();
    let reply = h
        .token(&format!(
            "grant_type=authorization_code&client_id={id}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
            encode(&code),
            encode(redirect)
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn registration_response_has_old_nexus_shape() {
    let h = Harness::new().await;
    let reply = h
        .register_with(json!({ "redirect_uris": [REDIRECT], "client_name": "Named" }))
        .await;
    let keys: Vec<&str> = reply
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "client_id",
            "client_secret",
            "client_name",
            "redirect_uris",
            "grant_types",
            "response_types",
            "token_endpoint_auth_method",
            "client_id_issued_at",
            "client_secret_expires_at",
        ]
    );
    assert!(reply["client_id"].as_str().unwrap().starts_with("mcp_"));
    assert_eq!(reply["client_id"].as_str().unwrap().len(), 4 + 32);
    assert_eq!(
        reply["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(reply["response_types"], json!(["code"]));
    assert_eq!(reply["client_secret_expires_at"], 0);
    assert!(reply["client_id_issued_at"].as_i64().unwrap() > 1_700_000_000);

    let public = h
        .register_with(json!({
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code"],
        }))
        .await;
    assert!(public.get("client_secret").is_none());
    assert_eq!(public["client_name"], Value::Null);
    assert_eq!(public["grant_types"], json!(["authorization_code"]));
}

#[tokio::test]
async fn logged_out_api_calls_get_401_json_but_pages_still_redirect() {
    let h = Harness::new().await;
    for request in [
        get_req("/api/applications"),
        form_req("/api/applications", "x=1"),
    ] {
        let reply = h.send(request).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(reply.header("content-type").starts_with("application/json"));
        assert_eq!(reply.header("cache-control"), "no-store");
        assert!(reply.headers.get("location").is_none());
        assert_eq!(reply.json()["error"], "Not logged in");
    }
    let page = h.send(get_req("/dashboard")).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location(), "/login?next=%2Fdashboard");

    let mut browser = Browser::default();
    browser.login(&h).await;
    let api = browser.send(&h, get_req("/api/applications")).await;
    assert_eq!(api.status, StatusCode::OK);
}

/// A same-origin path whose first two characters are not a scheme-relative `//` or `/\`.
fn assert_local(location: &str) {
    assert!(location.starts_with('/'), "{location:?}");
    assert!(!location.starts_with("//"), "{location:?}");
    assert!(
        location.bytes().all(|b| b.is_ascii_graphic()),
        "{location:?}"
    );
    assert!(!location.contains('\\'), "{location:?}");
    let joined = Url::parse(BASE).unwrap().join(location).unwrap();
    assert_eq!(joined.origin(), Url::parse(BASE).unwrap().origin());
}

#[tokio::test]
async fn a_foreign_next_never_leaves_the_site_after_a_successful_login() {
    let h = Harness::new().await;
    let long = format!("/{}", "a".repeat(20_000));
    let hostile = [
        "//evil.example",
        "/\\evil.example",
        "\\\\evil.example",
        "https://evil.example",
        "http://evil.example/",
        "javascript:alert(1)",
        "/%2F%2Fevil.example",
        "/\t/evil.example",
        "/\n/evil.example",
        "/\r/evil.example",
        "/\u{0}/evil.example",
        "/\u{7f}x",
        "/\u{202e}x",
        "///evil.example",
        "/ /evil.example",
        long.as_str(),
    ];
    for (index, next) in hostile.iter().enumerate() {
        let ip = format!("203.0.113.{}", index + 1);
        // Through the form the page rendered, as a browser would do it.
        let mut browser = Browser::default();
        let form = browser
            .send(
                &h,
                with_ip(get_req(&format!("/login?next={}", encode(next))), &ip),
            )
            .await;
        let body = format!(
            "csrf={}&passphrase={}&next={}",
            form.field("csrf"),
            encode(PASSPHRASE),
            encode(next)
        );
        let reply = browser
            .send(&h, with_ip(form_req("/login", &body), &ip))
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{next:?}");
        assert_local(reply.location());
        if reply.location() != "/" {
            assert_eq!(reply.location(), *next, "{next:?}");
        }

        // An already logged-in browser opening the hostile link.
        let again = browser
            .send(&h, get_req(&format!("/login?next={}", encode(next))))
            .await;
        if again.status == StatusCode::SEE_OTHER {
            assert_local(again.location());
        }
    }
}

#[tokio::test]
async fn the_stylesheet_needs_no_session_and_pages_need_no_inline_styles() {
    let h = Harness::new().await;
    let css = h.send(get_req("/auth.css")).await;
    assert_eq!(css.status, StatusCode::OK);
    assert!(css.header("content-type").starts_with("text/css"));
    assert!(css.body.contains("prefers-color-scheme: dark"));

    let login = h.send(get_req("/login")).await;
    let csp = login.header("content-security-policy");
    assert!(csp.contains("style-src 'self'"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(csp.contains("default-src 'none'"));
    assert!(login.body.contains(r#"href="/auth.css""#));
    assert!(!login.body.contains("style="));
}
