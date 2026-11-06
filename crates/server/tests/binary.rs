//! The real `nexus` binary: subcommands, startup refusals, and login over a real socket.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexus_core::Db;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PASSPHRASE: &str = "correct horse battery staple";
const NEW_PASSPHRASE: &str = "a different long passphrase";
const BASE: &str = "https://crm.example.test";
const CALENDAR_TOKEN: &str = "cal-0123456789abcdef0123456789abcdef";

fn nexus() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nexus"));
    command.env_clear();
    command
}

fn with_stdin(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn set_passphrase(dir: &Path, passphrase: &str) -> Output {
    let mut command = nexus();
    command.args(["set-passphrase", "--data-dir", dir.to_str().unwrap()]);
    with_stdin(command, &format!("{passphrase}\n"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn serve_output(args: &[&str], dir: &Path) -> Output {
    nexus()
        .args(["serve", "--bind", "127.0.0.1:0", "--data-dir"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn serve_refuses_to_start_without_its_settings_and_says_why() {
    let dir = tempfile::tempdir().unwrap();

    let no_passphrase = serve_output(&["--base-url", BASE], dir.path());
    assert!(!no_passphrase.status.success());
    let error = text(&no_passphrase.stderr);
    assert!(error.contains("nexus set-passphrase"), "{error}");
    assert!(error.contains("--insecure-local-only"), "{error}");
    assert!(
        !dir.path().join("nexus.db").exists(),
        "refused before touching data"
    );

    let no_base_url = serve_output(&[], dir.path());
    assert!(!no_base_url.status.success());
    assert!(text(&no_base_url.stderr).contains("NEXUS_PUBLIC_BASE_URL"));

    let exposed = nexus()
        .args(["serve", "--insecure-local-only", "--bind", "0.0.0.0:0"])
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!exposed.status.success());
    assert!(text(&exposed.stderr).contains("loopback"));

    // The base URL has one environment name, and the old one is gone.
    let old_name = nexus()
        .env("NEXUS_BASE_URL", BASE)
        .args(["serve", "--bind", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!old_name.status.success());
    assert!(text(&old_name.stderr).contains("NEXUS_PUBLIC_BASE_URL"));
}

#[test]
fn the_passphrase_never_comes_from_argv() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["--passphrase", PASSPHRASE], vec![PASSPHRASE]] {
        let output = nexus()
            .arg("set-passphrase")
            .arg("--data-dir")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!dir.path().join("owner-passphrase.argon2").exists());

    let short = set_passphrase(dir.path(), "too short");
    assert!(!short.status.success());
    assert!(!text(&short.stderr).contains("too short\n"));
    assert!(!dir.path().join("owner-passphrase.argon2").exists());

    let empty = with_stdin(
        {
            let mut command = nexus();
            command.args(["set-passphrase", "--data-dir", dir.path().to_str().unwrap()]);
            command
        },
        "",
    );
    assert!(!empty.status.success());

    let overridden = {
        let mut command = nexus();
        command.env("NEXUS_OWNER_PASSPHRASE_HASH", "x").args([
            "set-passphrase",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ]);
        with_stdin(command, &format!("{PASSPHRASE}\n"))
    };
    assert!(!overridden.status.success());
    assert!(text(&overridden.stderr).contains("NEXUS_OWNER_PASSPHRASE_HASH"));
}

struct Server {
    child: Child,
    addr: SocketAddr,
    logs: Arc<Mutex<String>>,
}

impl Server {
    fn start(dir: &Path, envs: &[(&str, &str)]) -> Self {
        let token_file = dir.join("calendar-token");
        std::fs::write(&token_file, CALENDAR_TOKEN).unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut command = nexus();
        command
            .env("NEXUS_PUBLIC_BASE_URL", BASE)
            .env("NEXUS_OWNER_ID", "owner-1")
            .env("NEXUS_OWNER_EMAIL", "owner@example.net")
            .env("NEXUS_TRUSTED_IP_HEADER", "none")
            .envs(envs.iter().copied())
            .args(["serve", "--bind", "127.0.0.1:0", "--data-dir"])
            .arg(dir)
            .arg("--calendar-token-file")
            .arg(&token_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().unwrap();

        let logs = Arc::new(Mutex::new(String::new()));
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let sink = logs.clone();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                let mut logs = sink.lock().unwrap();
                logs.push_str(&line);
                logs.push('\n');
            }
        });

        let started = Instant::now();
        let addr = loop {
            if let Some(addr) = logs
                .lock()
                .unwrap()
                .lines()
                .find(|line| line.contains("listening"))
                .and_then(|line| {
                    line.split("addr=")
                        .nth(1)?
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
            {
                break addr;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "did not start");
            std::thread::sleep(Duration::from_millis(10));
        };
        Self { child, addr, logs }
    }

    /// Stops it with SIGTERM, as systemd does, and returns everything it logged.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            self.child.wait().unwrap().success(),
            "clean exit on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
        self.logs.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn field(&self, name: &str) -> String {
        let marker = format!(r#"name="{name}" value=""#);
        let start = self.body.find(&marker).expect(name) + marker.len();
        let end = self.body[start..].find('"').unwrap() + start;
        self.body[start..end].to_owned()
    }
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: evil.example.test\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("{head}\r\n{body}").as_bytes())
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.to_lowercase(), value.to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// A cookie jar, the way a browser keeps one.
#[derive(Default)]
struct Browser(HashMap<String, String>);

impl Browser {
    async fn send(
        &mut self,
        addr: SocketAddr,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: &str,
    ) -> Reply {
        let cookie = self
            .0
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let mut headers = extra.to_vec();
        if !cookie.is_empty() {
            headers.push(("Cookie", &cookie));
        }
        let reply = request(addr, method, path, &headers, body).await;
        for (key, value) in &reply.headers {
            if key == "set-cookie" {
                let (name, value) = value.split(';').next().unwrap().split_once('=').unwrap();
                if value.is_empty() {
                    self.0.remove(name);
                } else {
                    self.0.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        reply
    }

    async fn login(&mut self, addr: SocketAddr, passphrase: &str, extra: &[(&str, &str)]) -> Reply {
        let form = self.send(addr, "GET", "/login", extra, "").await;
        assert_eq!(form.status, 200, "{}", form.body);
        let body = format!(
            "csrf={}&passphrase={}",
            form.field("csrf"),
            passphrase.replace(' ', "+")
        );
        let mut headers = extra.to_vec();
        headers.push(("Content-Type", "application/x-www-form-urlencoded"));
        self.send(addr, "POST", "/login", &headers, &body).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_passphrase_serve_login_dashboard_and_passphrase_change() {
    let dir = tempfile::tempdir().unwrap();
    let hash_file = dir.path().join("owner-passphrase.argon2");

    let output = set_passphrase(dir.path(), PASSPHRASE);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("revoked all OAuth grants and API tokens"),
        "{stderr}"
    );
    assert!(stderr.contains("Restart nexus"), "{stderr}");
    assert!(!stderr.contains(PASSPHRASE), "{stderr}");
    let metadata = std::fs::metadata(&hash_file).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert!(
        std::fs::read_to_string(&hash_file)
            .unwrap()
            .starts_with("$argon2id$")
    );

    let issued = nexus()
        .args(["issue-token", "--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(issued.status.success(), "{}", text(&issued.stderr));
    let token = text(&issued.stdout).trim().to_owned();
    assert!(token.starts_with("jt_") && token.len() == 67);
    assert!(!text(&issued.stderr).contains(&token), "only stdout has it");

    let server = Server::start(dir.path(), &[]);
    let addr = server.addr;

    // Open: health and the secret-URL calendar. Neither needs a session.
    let health = request(addr, "GET", "/healthz", &[], "").await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
    let calendar = request(
        addr,
        "GET",
        &format!("/calendar.ics?token={CALENDAR_TOKEN}"),
        &[],
        "",
    )
    .await;
    assert_eq!(calendar.status, 200);
    assert_eq!(
        request(addr, "GET", "/calendar.ics?token=wrong", &[], "")
            .await
            .status,
        404
    );

    // Logged out: the dashboard redirects, carrying the page to return to; the API answers 401 JSON.
    let mut browser = Browser::default();
    let dashboard = browser.send(addr, "GET", "/queue", &[], "").await;
    assert_eq!(dashboard.status, 303);
    assert_eq!(dashboard.header("location"), Some("/login?next=%2Fqueue"));
    let api = browser
        .send(addr, "GET", "/api/applications", &[], "")
        .await;
    assert_eq!(api.status, 401);
    assert_eq!(api.header("location"), None);
    assert!(api.body.contains("Not logged in"), "{}", api.body);

    // The issuer comes from configuration, whatever Host says (this client sends a hostile one).
    let metadata = request(
        addr,
        "GET",
        "/.well-known/oauth-authorization-server",
        &[],
        "",
    )
    .await;
    assert!(
        metadata.body.contains(BASE) && !metadata.body.contains("evil"),
        "{}",
        metadata.body
    );

    // Login sets the cookies through the whole middleware stack.
    let wrong = browser.login(addr, "not the passphrase", &[]).await;
    assert_eq!(wrong.status, 401);
    let page = browser.send(addr, "GET", "/login", &[], "").await;
    assert!(
        page.header("content-security-policy")
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(page.header("x-frame-options"), Some("DENY"));

    let done = browser.login(addr, PASSPHRASE, &[]).await;
    assert_eq!(done.status, 303, "{}", done.body);
    assert_eq!(done.header("location"), Some("/"));
    let session_cookie = done
        .headers
        .iter()
        .find(|(key, value)| key == "set-cookie" && value.starts_with("__Host-nexus_session="))
        .map(|(_, value)| value.clone())
        .unwrap();
    assert!(
        session_cookie.contains("HttpOnly; Secure; SameSite=Lax"),
        "{session_cookie}"
    );

    // Logged in: the dashboard and API answer.
    for path in ["/", "/queue", "/api/applications", "/assets/app.css"] {
        let reply = browser.send(addr, "GET", path, &[], "").await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
    }

    // Backoff keys on the real socket peer, and forwarded headers are ignored by default.
    let mut attacker = Browser::default();
    for i in 0..6 {
        let ip = format!("192.0.2.{i}");
        let reply = attacker
            .login(addr, "another wrong guess", &[("X-Forwarded-For", &ip)])
            .await;
        // The owner's failed attempt above was cleared by the success, so these are all free.
        assert_eq!(reply.status, 401, "attempt {i}");
    }
    let blocked = attacker
        .login(addr, PASSPHRASE, &[("X-Forwarded-For", "192.0.2.99")])
        .await;
    assert_eq!(blocked.status, 429);
    assert!(blocked.header("retry-after").is_some());

    // Auth routes keep their body limits; the logging layer sees them by template.
    let huge = "x".repeat(70 * 1024);
    let reply = request(addr, "POST", "/login", &[], &huge).await;
    assert_eq!(reply.status, 413);
    let session_value = session_cookie
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_owned();

    let logs = server.stop();
    assert!(logs.contains("route=/login"), "{logs}");
    assert!(logs.contains("route=/healthz"), "{logs}");
    assert!(logs.contains("source=127.0.0.1"), "{logs}");
    assert!(
        logs.contains("login_failure") && logs.contains("login_lockout"),
        "{logs}"
    );
    assert!(logs.contains("login_success"), "{logs}");
    assert!(
        !logs.contains("unknown"),
        "the peer address reached the limiter:\n{logs}"
    );
    for secret in [
        PASSPHRASE,
        "another wrong guess",
        session_value.as_str(),
        CALENDAR_TOKEN,
        token.as_str(),
        "192.0.2.",
        "evil.example.test",
    ] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }

    // Changing the passphrase revokes the API token; after a restart only the new one logs in.
    let output = set_passphrase(dir.path(), NEW_PASSPHRASE);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let db = Db::open(dir.path()).await.unwrap();
    let api_tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(api_tokens, 0);
    db.close().await;

    let server = Server::start(dir.path(), &[]);
    let addr = server.addr;
    let dashboard = browser.send(addr, "GET", "/", &[], "").await;
    assert_eq!(dashboard.status, 303, "the old session did not survive");
    assert_eq!(browser.login(addr, PASSPHRASE, &[]).await.status, 401);
    assert_eq!(browser.login(addr, NEW_PASSPHRASE, &[]).await.status, 303);
    assert_eq!(browser.send(addr, "GET", "/", &[], "").await.status, 200);
    server.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_ip_header_separates_clients_behind_a_tunnel() {
    let dir = tempfile::tempdir().unwrap();
    assert!(set_passphrase(dir.path(), PASSPHRASE).status.success());
    let server = Server::start(
        dir.path(),
        &[("NEXUS_TRUSTED_IP_HEADER", "CF-Connecting-IP")],
    );
    let addr = server.addr;

    let attacker = [("CF-Connecting-IP", "192.0.2.1")];
    let mut browser = Browser::default();
    for _ in 0..6 {
        assert_eq!(
            browser.login(addr, "a wrong guess", &attacker).await.status,
            401
        );
    }
    assert_eq!(browser.login(addr, PASSPHRASE, &attacker).await.status, 429);

    // The owner comes through the same tunnel, from another address.
    let owner = [("CF-Connecting-IP", "198.51.100.7")];
    assert_eq!(
        Browser::default()
            .login(addr, PASSPHRASE, &owner)
            .await
            .status,
        303
    );

    let logs = server.stop();
    assert!(logs.contains("source=192.0.2.1"), "{logs}");
    assert!(logs.contains("source=198.51.100.7"), "{logs}");
}

#[test]
fn set_passphrase_refuses_weak_ones_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for weak in [
        "only fifteen ch",
        "aaaaaaaaaaaaaaaaaaaaaaaa",
        "abcabcabcabcabcabc",
    ] {
        let output = set_passphrase(dir.path(), weak);
        assert!(!output.status.success(), "{weak}");
        assert!(!dir.path().join("owner-passphrase.argon2").exists());
    }
}

#[test]
fn an_https_base_url_makes_the_operator_decide_about_the_trusted_header() {
    let dir = tempfile::tempdir().unwrap();
    assert!(set_passphrase(dir.path(), PASSPHRASE).status.success());
    let output = serve_output(&["--base-url", BASE], dir.path());
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("NEXUS_TRUSTED_IP_HEADER"));
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_are_listed_and_revoked_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let db = nexus_core::Db::open(dir.path()).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, grant_types, response_types)
             VALUES ('mcp_one', 'Name\u{1b}[31m\u{202e}', '[]', '[]', '[]');
         INSERT INTO oauth_auth_codes (code, client_id, redirect_uri, code_challenge, scopes, expires_at, used)
             VALUES ('abcdef0123456789', 'mcp_one', 'https://claude.ai/cb?x=1', 'c', '[]', '2999-01-01T00:00:00.000Z', 1);
         INSERT INTO oauth_access_tokens (token_hash, grant_id, client_id, scopes, expires_at)
             VALUES ('a', 'abcdef0123456789', 'mcp_one', '[\"mcp:tools\"]', '2999-01-01T00:00:00.000Z');
         INSERT INTO oauth_refresh_tokens (token_hash, grant_id, client_id, scopes, expires_at)
             VALUES ('r', 'abcdef0123456789', 'mcp_one', '[\"mcp:tools\"]', '2999-01-01T00:00:00.000Z');",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let grants = |args: &[&str]| {
        nexus()
            .arg("grants")
            .args(args)
            .arg("--data-dir")
            .arg(dir.path())
            .output()
            .unwrap()
    };
    let list = grants(&["list"]);
    assert!(list.status.success(), "{}", text(&list.stderr));
    let printed = text(&list.stdout);
    assert!(printed.starts_with("abcdef012345\tmcp_one\t"), "{printed}");
    assert!(printed.contains("https://claude.ai\t"), "{printed}");
    assert!(printed.contains("mcp:tools"), "{printed}");
    assert!(
        !printed.contains('\u{1b}') && !printed.contains('\u{202e}'),
        "a stored name must not reach the terminal as control characters: {printed:?}"
    );

    assert!(!grants(&["revoke", "ffffffffffff"]).status.success());
    let revoke = grants(&["revoke", "abcdef012345"]);
    assert!(revoke.status.success(), "{}", text(&revoke.stderr));
    assert!(text(&revoke.stdout).contains("revoked 1 access and 1 refresh tokens"));
    assert_eq!(text(&grants(&["list"]).stdout), "");
}
