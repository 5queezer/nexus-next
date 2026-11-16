//! Cloudflare Access as the owner's login: a valid assertion logs in without the passphrase, and
//! anything short of one leaves the passphrase form, since off Access anyone can send the header.

mod common;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use nexus_auth::CfAccess;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};

const AUD: &str = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2";
const TEAM: &str = "owner-team";
const ISS: &str = "https://owner-team.cloudflareaccess.com";
const EMAIL: &str = "owner@example.net";
const KID: &str = "test-kid-1";

/// A throwaway 2048-bit key made for these tests only; it signs nothing anywhere else.
fn key() -> RsaKeyPair {
    RsaKeyPair::from_der(include_bytes!("fixtures/cf-access-test-key.der")).unwrap()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed()
}

fn jwks(kid: &str) -> Value {
    let public: RsaPublicKeyComponents<Vec<u8>> = key().public().into();
    json!({
        "keys": [
            { "kid": "an-ec-key", "kty": "EC", "crv": "P-256", "x": "a", "y": "b" },
            {
                "kid": kid,
                "kty": "RSA",
                "alg": "RS256",
                "use": "sig",
                "e": URL_SAFE_NO_PAD.encode(&public.e),
                "n": URL_SAFE_NO_PAD.encode(&public.n),
            }
        ],
        "public_cert": { "kid": kid, "cert": "-----BEGIN CERTIFICATE-----..." },
    })
}

fn sign(header: &Value, claims: &Value) -> String {
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let key = key();
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn claims() -> Value {
    let now = now();
    json!({
        "aud": [AUD],
        "email": EMAIL,
        "exp": now + 3600,
        "iat": now,
        "nbf": now,
        "iss": ISS,
        "type": "app",
        "identity_nonce": "n",
        "sub": "7335d417-61da-459d-899c-0a01c76a2f94",
        "country": "ES",
    })
}

fn header() -> Value {
    json!({ "alg": "RS256", "kid": KID, "typ": "JWT" })
}

fn config(certs: &Path) -> CfAccess {
    CfAccess {
        aud: AUD.to_owned(),
        team: TEAM.to_owned(),
        emails: vec![
            "someone@example.org".to_owned(),
            "Owner@Example.net".to_owned(),
        ],
        certs_file: certs.to_owned(),
    }
}

/// A harness with Access configured and the key file written.
async fn harness() -> Harness {
    let keys = tempfile::tempdir().unwrap().keep();
    let file = keys.join("certs.json");
    std::fs::write(&file, jwks(KID).to_string()).unwrap();
    Harness::with_cf_access(config(&file)).await
}

fn login_with(token: &str, next: &str) -> Request<Body> {
    Request::get(format!("/login?next={}", encode(next)))
        .header("cf-access-jwt-assertion", token)
        .body(Body::empty())
        .unwrap()
}

fn assert_passphrase_form(reply: &Reply, case: &str) {
    assert_eq!(reply.status, StatusCode::OK, "{case}: {}", reply.body);
    assert!(reply.body.contains(r#"name="passphrase""#), "{case}");
    assert!(
        !reply
            .headers
            .get_all("set-cookie")
            .iter()
            .any(|c| c.to_str().unwrap().starts_with("__Host-nexus_session=")),
        "{case}: no session"
    );
}

#[tokio::test]
async fn a_valid_assertion_logs_the_owner_in_and_consent_is_still_asked() {
    let h = harness().await;
    let id = h.register().await;
    let path = authorize_path(&id, "mcp:tools");

    // Authorize without a session goes to /login, which Access fronts on the public host.
    let first = h.send(get_req(&path)).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert!(first.location().starts_with("/login?next="));

    let mut browser = Browser::default();
    let reply = browser
        .send(&h, login_with(&sign(&header(), &claims()), &path))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), path);
    assert!(browser.cookies.contains_key("__Host-nexus_session"));

    let consent = browser.send(&h, get_req(&path)).await;
    assert_eq!(consent.status, StatusCode::OK);
    assert!(consent.body.contains("Authorize MCP access?"));
    assert_eq!(
        h.count("oauth_auth_codes").await,
        0,
        "no code without the click"
    );
    let done = browser
        .send(
            &h,
            form_req(
                "/api/mcp/authorize",
                &format!("consent={}", encode(&consent.field("consent"))),
            ),
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    assert!(query_param(done.location(), "code").is_some());
}

#[tokio::test]
async fn anything_short_of_a_valid_assertion_leaves_the_passphrase_form() {
    let h = harness().await;
    let now = now();
    let with = |key: &str, value: Value| {
        let mut claims = claims();
        claims[key] = value;
        sign(&header(), &claims)
    };
    let valid = sign(&header(), &claims());
    let (head, rest) = valid.split_once('.').unwrap();
    let (_, signature) = rest.split_once('.').unwrap();
    let mut widened = claims();
    // Everything else valid: only the signature no longer matches.
    widened["sub"] = json!("someone else");
    let swapped_payload = format!(
        "{head}.{}.{signature}",
        URL_SAFE_NO_PAD.encode(widened.to_string())
    );
    let none = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(json!({ "alg": "none", "kid": KID }).to_string()),
        URL_SAFE_NO_PAD.encode(claims().to_string())
    );
    let cases = [
        ("wrong aud", with("aud", json!(["another-application"]))),
        (
            "wrong iss",
            with("iss", json!("https://other-team.cloudflareaccess.com")),
        ),
        ("expired", with("exp", json!(now - 120))),
        ("not yet valid", with("nbf", json!(now + 600))),
        ("wrong email", with("email", json!("intruder@example.com"))),
        ("no email", with("email", Value::Null)),
        ("bad signature", swapped_payload),
        (
            "unknown kid",
            sign(&json!({ "alg": "RS256", "kid": "other" }), &claims()),
        ),
        ("alg none", none),
        (
            "alg HS256",
            sign(&json!({ "alg": "HS256", "kid": KID }), &claims()),
        ),
        ("garbage", "a.b.c".to_owned()),
        ("empty", String::new()),
    ];
    for (case, token) in cases {
        assert_passphrase_form(&h.send(login_with(&token, "/")).await, case);
    }
    // A forged header on the tailnet: no signature at all.
    assert_passphrase_form(
        &h.send(login_with("eyJhbGciOiJSUzI1NiJ9.eyJlbWFpbCI6Im8ifQ.", "/"))
            .await,
        "forged",
    );
    // And within the clock leeway, a token just past `exp` still works.
    let reply = h.send(login_with(&with("exp", json!(now - 30)), "/")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "leeway");
}

#[tokio::test]
async fn without_a_usable_key_file_access_fails_closed_until_one_appears() {
    let keys = tempfile::tempdir().unwrap().keep();
    let file = keys.join("certs.json");
    let h = Harness::with_cf_access(config(&file)).await;
    let token = sign(&header(), &claims());
    assert_passphrase_form(&h.send(login_with(&token, "/")).await, "missing file");

    std::fs::write(&file, "{\"keys\": []}").unwrap();
    assert_passphrase_form(&h.send(login_with(&token, "/")).await, "no keys");

    // The timer replaces the file: the server reads it again without a restart. A rename, as the
    // timer does it, gives the file a new modification time.
    let next = keys.join("certs.json.new");
    std::fs::write(&next, jwks(KID).to_string()).unwrap();
    std::fs::rename(&next, &file).unwrap();
    let reply = h.send(login_with(&token, "/")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
}

#[tokio::test]
async fn without_access_configured_the_header_is_ignored() {
    let h = Harness::new().await;
    assert_passphrase_form(
        &h.send(login_with(&sign(&header(), &claims()), "/")).await,
        "not configured",
    );
}

#[test]
fn a_cloudflare_key_set_parses_and_others_do_not() {
    assert_eq!(nexus_auth::parse_access_keys(&jwks(KID).to_string()), Ok(1));
    assert!(nexus_auth::parse_access_keys("{\"keys\": []}").is_err());
    assert!(nexus_auth::parse_access_keys("<html>").is_err());
}
