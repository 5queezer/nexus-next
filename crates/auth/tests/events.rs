//! Security events reach the log, and nothing secret or client-supplied comes with them. A single
//! test, because it installs the process-wide tracing subscriber.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::*;
use nexus_auth::{issue_api_token, set_owner_passphrase};
use serde_json::json;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const HOSTILE_NAME: &str = "HOSTILE-CLIENT-NAME\nevent=forged";
const WRONG_GUESS: &str = "WRONG-GUESS-PASSPHRASE";

#[tokio::test]
async fn security_events_are_logged_without_secrets_or_client_text() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::INFO)
        .init();

    let h = Harness::new().await;
    let client = h
        .register_with(json!({
            "client_name": HOSTILE_NAME,
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "HOSTILE-METHOD",
        }))
        .await;
    let id = client["client_id"].as_str().unwrap();
    let secret = client["client_secret"].as_str().unwrap();

    let mut browser = Browser::default();
    let wrong = browser.attempt_login(&h, WRONG_GUESS, "203.0.113.5").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    for _ in 0..5 {
        browser.attempt_login(&h, WRONG_GUESS, "203.0.113.5").await;
    }
    let locked = browser.attempt_login(&h, WRONG_GUESS, "203.0.113.5").await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);

    let code = h.code_for(id, "mcp:tools").await;
    let body = format!(
        "grant_type=authorization_code&client_id={id}&client_secret={secret}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
        encode(&code),
        encode(REDIRECT)
    );
    let tokens = h.token(&body).await.json();
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    h.token(&body)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    let code2 = h.code_for(id, "mcp:tools").await;
    let tokens2 = h
        .token(&format!(
            "grant_type=authorization_code&client_id={id}&client_secret={secret}&code={}&redirect_uri={}&code_verifier={VERIFIER}",
            encode(&code2),
            encode(REDIRECT)
        ))
        .await
        .json();
    let refresh2 = tokens2["refresh_token"].as_str().unwrap();
    let refresh_body = format!(
        "grant_type=refresh_token&client_id={id}&client_secret={secret}&refresh_token={refresh2}"
    );
    h.token(&refresh_body).await;
    h.end_refresh_grace().await;
    h.token(&refresh_body)
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_grant");

    for i in 0..31 {
        h.send(with_ip(
            bearer_req(&format!("jt_guessed_token_{i}")),
            "203.0.113.6",
        ))
        .await;
    }

    let api_token = issue_api_token(&h.db, "HOSTILE-TOKEN-NAME").await.unwrap();
    set_owner_passphrase(&h.db, &h.dir.path().join("hash"), PASSPHRASE)
        .await
        .unwrap();

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for event in [
        "client_registered",
        "login_failure",
        "login_lockout",
        "login_success",
        "consent_granted",
        "token_replay_detected",
        "grants_revoked",
        "rate_limited",
        "api_token_issued",
        "revoked all OAuth grants and API tokens",
    ] {
        assert!(logs.contains(event), "no {event} in:\n{logs}");
    }
    assert!(logs.contains("authorization_code") && logs.contains("refresh_token"));
    assert!(logs.contains("source=203.0.113.5"), "{logs}");
    assert!(logs.contains(id), "events name the registered client");

    for secret in [
        PASSPHRASE,
        WRONG_GUESS,
        "HOSTILE",
        "forged",
        "client.example.test",
        "jt_guessed",
        "st-1",
        secret,
        code.as_str(),
        code2.as_str(),
        access,
        refresh,
        refresh2,
        api_token.as_str(),
        VERIFIER,
        CHALLENGE,
    ] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }
}
