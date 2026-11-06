#![allow(dead_code)]
//! The real `nexus serve` on a loopback port with synthetic data, a real API token, and the MCP
//! calls an agent makes. Kept apart from `binary.rs` because these servers get killed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqliteConnection};

pub const PASSPHRASE: &str = "synthetic stress passphrase for tests";

pub fn nexus() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nexus"));
    command.env_clear();
    command
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One data directory and the server on it. Restarts reuse the port.
pub struct Node {
    pub dir: PathBuf,
    pub log: PathBuf,
    pub addr: SocketAddr,
    extra_args: Vec<String>,
    child: Option<Child>,
}

impl Node {
    /// Sets the passphrase and issues the API token, then starts serving. Returns the token.
    pub async fn create(root: &Path, extra_args: &[&str]) -> (Self, String) {
        let dir = root.join("data");
        fs::create_dir_all(&dir).unwrap();
        let mut set = nexus()
            .args(["set-passphrase", "--data-dir"])
            .arg(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        writeln!(set.stdin.take().unwrap(), "{PASSPHRASE}").unwrap();
        assert!(set.wait().unwrap().success());
        let issued = nexus()
            .args(["issue-token", "--data-dir"])
            .arg(&dir)
            .output()
            .unwrap();
        assert!(issued.status.success());
        let token = String::from_utf8(issued.stdout).unwrap().trim().to_owned();
        let port = free_port();
        let mut node = Self {
            dir,
            log: root.join("server.log"),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            extra_args: extra_args.iter().map(|&s| s.to_owned()).collect(),
            child: None,
        };
        node.start().await;
        (node, token)
    }

    pub async fn start(&mut self) {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = nexus()
            .env("NEXUS_PUBLIC_BASE_URL", format!("http://{}", self.addr))
            .env("NEXUS_OWNER_ID", "owner-synthetic")
            .env("NEXUS_OWNER_EMAIL", "owner@example.test")
            .arg("serve")
            .args(["--bind", &self.addr.to_string(), "--data-dir"])
            .arg(&self.dir)
            .args(&self.extra_args)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.child = Some(child);
        let health = format!("http://{}/healthz", self.addr);
        let http = reqwest::Client::new();
        let started = Instant::now();
        loop {
            if let Ok(reply) = http
                .get(&health)
                .timeout(Duration::from_secs(1))
                .send()
                .await
                && reply.status().is_success()
            {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub fn kill9(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    pub fn terminate(&mut self) {
        let mut child = self.child.take().unwrap();
        let term = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        assert!(term.unwrap().success());
        assert!(child.wait().unwrap().success(), "clean exit on SIGTERM");
    }

    pub fn logs(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// A read-only connection for audits, safe while the server runs.
    pub async fn audit_connection(&self) -> SqliteConnection {
        Self::audit_of(&self.dir).await
    }

    /// Fails on corruption or a dangling foreign key.
    pub async fn assert_database_sound(&self) {
        let mut conn = self.audit_connection().await;
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        let dangling = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut conn)
            .await
            .unwrap();
        assert!(dangling.is_empty(), "{} dangling rows", dangling.len());
    }

    /// Every visible blob matches its address and every document's file exists. Returns the visible
    /// blob count, unreferenced blobs among them, and leftover temp files.
    pub async fn assert_blobs_sound(&self) -> (usize, usize, usize) {
        let blobs = self.dir.join("blobs");
        let mut addresses = Vec::new();
        let mut temp_files = 0;
        if let Ok(entries) = fs::read_dir(&blobs) {
            for entry in entries {
                let name = entry.unwrap().file_name().into_string().unwrap();
                if name.starts_with(".tmp-") {
                    temp_files += 1;
                } else {
                    let bytes = fs::read(blobs.join(&name)).unwrap();
                    assert_eq!(nexus_core::blob::sha256_hex(&bytes), name, "half-written");
                    addresses.push(name);
                }
            }
        }
        let mut conn = self.audit_connection().await;
        let referenced: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT blob_sha256 FROM documents WHERE blob_sha256 IS NOT NULL",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        for sha256 in &referenced {
            assert!(addresses.contains(sha256), "document without its blob");
        }
        let orphans = addresses.len() - referenced.len();
        (addresses.len(), orphans, temp_files)
    }

    pub async fn count(&self, sql: &str) -> i64 {
        let mut conn = self.audit_connection().await;
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&mut conn)
            .await
            .unwrap()
    }

    /// A read-only connection to any data directory, for restored copies.
    pub async fn audit_of(dir: &Path) -> SqliteConnection {
        SqliteConnectOptions::new()
            .filename(dir.join("nexus.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap()
    }

    /// Peak resident memory in MiB, and current WAL size in MiB.
    pub fn peak_rss_and_wal(&self) -> (u64, u64) {
        let status = fs::read_to_string(format!("/proc/{}/status", self.pid())).unwrap_or_default();
        let peak = status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0);
        let wal = File::open(self.dir.join("nexus.db-wal"))
            .and_then(|file| file.metadata())
            .map_or(0, |meta| meta.len());
        (peak / 1024, wal / (1024 * 1024))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A tool result, as a client reads it.
#[derive(Debug, Clone)]
pub struct Reply {
    pub is_error: bool,
    pub text: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }

    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// An agent: the bearer token and the MCP endpoint.
#[derive(Clone)]
pub struct Agent {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Agent {
    pub fn new(addr: SocketAddr, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: format!("http://{addr}/api/mcp"),
            token: token.to_owned(),
        }
    }

    /// `Err` when no answer arrived or it was not a JSON-RPC result: an unacknowledged call.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Reply, String> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(120))
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|e| e.to_string())?;
        let parsed: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let result = &parsed["result"];
        if status != 200 || !result.is_object() {
            return Err(format!("{status}: {text}"));
        }
        Ok(Reply {
            is_error: result["isError"] == true,
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// The whole tool result: `isError` and every content block.
    pub async fn raw(&self, tool: &str, args: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(120))
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let parsed: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        parsed["result"].clone()
    }

    /// A call that must succeed.
    pub async fn ok(&self, tool: &str, args: Value) -> Value {
        let reply = self.call(tool, args).await.expect("answered");
        assert!(!reply.is_error, "{tool}: {}", reply.text);
        reply.json()
    }
}

/// `nexus cv-facts lock`, as the owner runs it: read what it prints, then confirm that list by its
/// hash. Returns all it printed.
pub fn lock_confirmed(dir: &Path) -> String {
    let run = |args: &[&str]| {
        nexus()
            .arg("cv-facts")
            .args(args)
            .arg("--data-dir")
            .arg(dir)
            .output()
            .unwrap()
    };
    let shown = run(&["lock"]);
    let mut printed = String::from_utf8(shown.stdout).unwrap();
    if !shown.status.success() {
        let said = String::from_utf8(shown.stderr).unwrap();
        let hash = said
            .split("--confirm ")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .unwrap_or_else(|| panic!("no hash in {said}"));
        let confirmed = run(&["lock", "--confirm", hash]);
        assert!(confirmed.status.success(), "{confirmed:?}");
        printed.push_str(&String::from_utf8(confirmed.stdout).unwrap());
    }
    printed
}

const REDIRECT: &str = "http://127.0.0.1:9/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// A `<input name=... value=...>` from a served page.
pub fn field(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\"");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("{name} in {page}"));
    let tag_start = page[..at].rfind('<').unwrap();
    let tag_end = at + page[at..].find('>').unwrap();
    let tag = &page[tag_start..tag_end];
    let value = tag.split("value=\"").nth(1).unwrap();
    value.split('"').next().unwrap().to_owned()
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

pub struct Browser {
    pub http: reqwest::Client,
    pub base: String,
    pub cookies: Vec<(String, String)>,
}

impl Browser {
    pub fn cookie_header(&self) -> String {
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        pairs.join("; ")
    }

    pub async fn keep_cookies(&mut self, response: &reqwest::Response) {
        for cookie in response.headers().get_all(SET_COOKIE) {
            let first = cookie.to_str().unwrap().split(';').next().unwrap();
            let (key, value) = first.split_once('=').unwrap();
            self.cookies.retain(|(k, _)| k != key);
            if !value.is_empty() {
                self.cookies.push((key.to_owned(), value.to_owned()));
            }
        }
    }

    pub async fn get(&mut self, path: &str) -> (reqwest::StatusCode, String) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .header(COOKIE, self.cookie_header())
            .send()
            .await
            .unwrap();
        self.keep_cookies(&response).await;
        (response.status(), response.text().await.unwrap())
    }

    pub async fn post_form(&mut self, path: &str, body: String) -> reqwest::Response {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header(COOKIE, self.cookie_header())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        self.keep_cookies(&response).await;
        response
    }
}

/// Dynamic registration, owner login, consent and PKCE: what Hermes does at its one reconnect.
pub async fn oauth_token(base: &str) -> String {
    let http = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let registered = http
        .post(format!("{base}/api/mcp/register"))
        .header("content-type", "application/json")
        .body(
            json!({
                "client_name": "Scripted Hermes",
                "redirect_uris": [REDIRECT],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    let registered: Value = serde_json::from_str(&registered.text().await.unwrap()).unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_owned();

    let mut browser = Browser {
        http: http.clone(),
        base: base.to_owned(),
        cookies: Vec::new(),
    };
    let (_, login) = browser.get("/login").await;
    let body = form(&[("csrf", &field(&login, "csrf")), ("passphrase", PASSPHRASE)]);
    let logged_in = browser.post_form("/login", body).await;
    assert_eq!(logged_in.status(), 303);

    let query = form(&[
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", REDIRECT),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("state", "st-1"),
        ("scope", "mcp:tools"),
    ]);
    let (status, consent) = browser.get(&format!("/api/mcp/authorize?{query}")).await;
    assert_eq!(status, 200, "{consent}");
    let body = form(&[("consent", &field(&consent, "consent"))]);
    let granted = browser.post_form("/api/mcp/authorize", body).await;
    assert_eq!(granted.status(), 303);
    let location = granted.headers()[LOCATION].to_str().unwrap().to_owned();
    let code = url::Url::parse(&location)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();

    let token = http
        .post(format!("{base}/api/mcp/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ]))
        .send()
        .await
        .unwrap();
    let token: Value = serde_json::from_str(&token.text().await.unwrap()).unwrap();
    token["access_token"].as_str().unwrap().to_owned()
}

pub fn good_policy() -> Value {
    json!({
        "humanReviewed": true,
        "identityConsistent": true,
        "factsVerified": true,
        "profileConsistencyStatus": "verified",
    })
}

/// A small pseudo-random source: tests only need varied timing and sizes.
pub fn jitter(seed: &mut u64, below: u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed % below
}
