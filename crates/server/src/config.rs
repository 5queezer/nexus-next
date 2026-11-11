//! Startup configuration for `nexus serve`: flags and environment in, a validated config out.
//! Secrets are only ever read from files, so none appears in the process list.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use nexus_auth::{AuthConfig, ConfigError, normalize_base_url};
use nexus_mcp::Renderer;
use nexus_web::{OPENAI_BASE_URL, Tz, VoiceConfig, VoiceKey};
use tokio::sync::watch;
use tracing::Level;

const MIN_TOKEN_LEN: usize = 32;
const MAX_VOICE_SESSION_SECONDS: u64 = 3600;
const MAX_ENV_LABEL_CHARS: usize = 24;

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Address to listen on. Keep it on loopback and publish through a tunnel or Tailscale.
    #[arg(long, env = "NEXUS_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Where nexus.db and blobs/ live.
    #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
    pub data_dir: PathBuf,
    /// Public origin clients reach this server at, for example https://nexus.example.net. It is the
    /// OAuth issuer, so it is required unless --insecure-local-only is set.
    #[arg(long, env = "NEXUS_PUBLIC_BASE_URL")]
    pub base_url: Option<String>,
    /// The owner's ID in old Nexus, for a database no import wrote. MCP responses carry it as
    /// `userId`. An imported database's own value wins.
    #[arg(long, env = "NEXUS_OWNER_ID")]
    pub owner_id: Option<String>,
    /// The owner's email, for a database whose import had no MCP events to read it from. New events
    /// record it as `actor`, and their request hashes cover it. An imported value wins.
    #[arg(long, env = "NEXUS_OWNER_EMAIL")]
    pub owner_email: Option<String>,
    /// IANA time zone for dates shown on the dashboard and in the calendar feed.
    #[arg(long, env = "NEXUS_TIMEZONE", default_value = "Europe/Madrid")]
    pub timezone: String,
    /// File holding the secret token of /calendar.ics. Without it the feed is not served.
    #[arg(long, env = "NEXUS_CALENDAR_TOKEN_FILE")]
    pub calendar_token_file: Option<PathBuf>,
    /// For local runs only: serve the dashboard and JSON API without any login, and no OAuth routes.
    /// Needs a loopback --bind.
    #[arg(long)]
    pub insecure_local_only: bool,
    /// Shown on every page and in its title, such as `Staging`, so that this server is not taken
    /// for production. Leave it unset or empty in production.
    #[arg(long, env = "NEXUS_ENV_LABEL", value_parser = env_label)]
    pub env_label: Option<String>,
    /// Most verbose level logged: error, warn, info, debug or trace.
    #[arg(long, env = "NEXUS_LOG_LEVEL", default_value = "info")]
    pub log_level: Level,
    /// File holding an OpenAI API key. Read once at start. Without it voice is off and Nexus makes
    /// no outbound call, until the owner sets a key in the browser. See deploy/voice.conf for what
    /// turning it on gives up.
    #[arg(long, env = "NEXUS_OPENAI_API_KEY_FILE")]
    pub openai_api_key_file: Option<PathBuf>,
    /// The OpenAI Realtime model for voice sessions.
    #[arg(
        long,
        env = "NEXUS_REALTIME_MODEL",
        default_value = "gpt-realtime-2.1-mini"
    )]
    pub realtime_model: String,
    /// The voice the model speaks with.
    #[arg(long, env = "NEXUS_REALTIME_VOICE", default_value = "marin")]
    pub realtime_voice: String,
    /// How long a voice session may last, in seconds. Nexus enforces this itself.
    #[arg(long, env = "NEXUS_VOICE_SESSION_SECONDS", default_value_t = 900)]
    pub voice_session_seconds: u64,
    /// For tests: where voice's OpenAI calls go. Refused unless --insecure-local-only is set, so a
    /// real server can't be pointed elsewhere.
    #[arg(long, hide = true)]
    pub voice_test_base_url: Option<String>,
}

/// Who may reach the dashboard and JSON API.
#[derive(Debug)]
pub enum Access {
    /// The owner, through the login page. OAuth routes are served.
    Owner(AuthConfig),
    /// Anyone who can reach loopback. No login, no OAuth routes.
    InsecureLocalOnly,
}

#[derive(Debug)]
pub struct ServeConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub access: Access,
    pub owner_id: Option<String>,
    pub owner_email: Option<String>,
    pub timezone: Tz,
    pub calendar_token: Option<String>,
    pub log_level: Level,
    /// Without a key here or one the owner set in the browser, voice is off: no outbound call.
    pub voice: VoiceConfig,
    pub renderer: Renderer,
    /// Set when shutdown starts, which ends the open live streams so that the drain can finish.
    pub stream_shutdown: watch::Sender<bool>,
}

impl ServeArgs {
    /// Checks every setting and reports all problems at once.
    pub fn validate(self) -> Result<ServeConfig, String> {
        let mut problems = Vec::new();

        if self.insecure_local_only && !self.bind.ip().is_loopback() {
            problems.push(format!(
                "--insecure-local-only needs a loopback --bind, got {}",
                self.bind
            ));
        }
        if self.data_dir.as_os_str().is_empty() {
            problems.push("--data-dir must not be empty".to_owned());
        }
        let timezone = self.timezone.parse::<Tz>().unwrap_or_else(|_| {
            problems.push(format!(
                "--timezone: {:?} is not an IANA time zone name (e.g. Europe/Madrid)",
                self.timezone
            ));
            Tz::UTC
        });
        let access = if self.insecure_local_only {
            if let Some(Err(e)) = self.base_url.as_deref().map(normalize_base_url) {
                problems.push(e.to_string());
            }
            Some(Access::InsecureLocalOnly)
        } else if let Some(url) = &self.base_url {
            AuthConfig::from_env(url, &self.data_dir)
                .map(Access::Owner)
                .map_err(|e| problems.push(auth_problem(&e)))
                .ok()
        } else {
            problems.push(
                "--base-url or NEXUS_PUBLIC_BASE_URL is required: it is the OAuth issuer"
                    .to_owned(),
            );
            None
        };
        let calendar_token = self.calendar_token_file.as_ref().map(|path| {
            read_token(path).unwrap_or_else(|e| {
                problems.push(format!("--calendar-token-file {}: {e}", path.display()));
                String::new()
            })
        });
        let voice = self.voice_config(&mut problems).unwrap_or_default();

        let (true, Some(access)) = (problems.is_empty(), access) else {
            return Err(problems.join("\n"));
        };
        Ok(ServeConfig {
            bind: self.bind,
            data_dir: self.data_dir,
            access,
            owner_id: self.owner_id.filter(|value| !value.trim().is_empty()),
            owner_email: self.owner_email.filter(|value| !value.trim().is_empty()),
            timezone,
            calendar_token,
            log_level: self.log_level,
            voice,
            renderer: Renderer::current_exe(),
            stream_shutdown: watch::Sender::new(false),
        })
    }

    fn voice_config(&self, problems: &mut Vec<String>) -> Option<VoiceConfig> {
        let base_url = match &self.voice_test_base_url {
            Some(url) if !self.insecure_local_only => {
                problems.push(format!(
                    "--voice-test-base-url {url} needs --insecure-local-only"
                ));
                return None;
            }
            Some(url) if !is_loopback_http(url) => {
                problems.push(format!(
                    "--voice-test-base-url must be http://127.0.0.1:<port>, got {url}"
                ));
                return None;
            }
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => OPENAI_BASE_URL.to_owned(),
        };
        if !(1..=MAX_VOICE_SESSION_SECONDS).contains(&self.voice_session_seconds) {
            problems.push(format!(
                "--voice-session-seconds must be from 1 to {MAX_VOICE_SESSION_SECONDS}"
            ));
        }
        let mut key = None;
        if let Some(path) = &self.openai_api_key_file {
            match read_token(path) {
                Ok(text) => key = Some(VoiceKey::new(text)),
                Err(e) => problems.push(format!("--openai-api-key-file {}: {e}", path.display())),
            }
        }
        Some(VoiceConfig {
            key,
            model: self.realtime_model.clone(),
            voice: self.realtime_voice.clone(),
            base_url,
            lifetime: Duration::from_secs(self.voice_session_seconds),
        })
    }
}

fn env_label(raw: &str) -> Result<String, String> {
    let label = raw.trim();
    if label.chars().count() > MAX_ENV_LABEL_CHARS || label.chars().any(char::is_control) {
        return Err(format!(
            "must be plain text of at most {MAX_ENV_LABEL_CHARS} characters"
        ));
    }
    Ok(label.to_owned())
}

fn is_loopback_http(url: &str) -> bool {
    url.strip_prefix("http://")
        .and_then(|rest| rest.trim_end_matches('/').rsplit_once(':'))
        .is_some_and(|(host, port)| host == "127.0.0.1" && port.parse::<u16>().is_ok())
}

fn auth_problem(error: &ConfigError) -> String {
    match error {
        ConfigError::MissingPassphraseHash(_) => {
            format!("{error}. For a local run without login, pass --insecure-local-only")
        }
        _ => error.to_string(),
    }
}

/// Whether a secret file's mode keeps it from other users. systemd hands a credential over as 0440, in
/// a directory only the service can reach, and its group is the service's own, so there the group may
/// read. Nowhere else.
fn secret_mode_ok(mode: u32, in_credentials: bool) -> bool {
    let denied = if in_credentials { 0o037 } else { 0o077 };
    mode & denied == 0
}

fn read_token(path: &std::path::Path) -> Result<String, String> {
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read: {}", e.kind()))?
        .permissions()
        .mode();
    let in_credentials =
        std::env::var_os("CREDENTIALS_DIRECTORY").is_some_and(|dir| path.starts_with(dir));
    if !secret_mode_ok(mode, in_credentials) {
        return Err(format!(
            "mode {:o} lets other users read the secret, use 0400 or 0600",
            mode & 0o777
        ));
    }
    let token = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {}", e.kind()))?;
    let token = token.trim();
    if token.len() < MIN_TOKEN_LEN || token.contains(char::is_whitespace) {
        return Err(format!(
            "must hold one token of at least {MIN_TOKEN_LEN} characters without whitespace"
        ));
    }
    if token.chars().all(|c| token.starts_with(c)) {
        return Err("is one repeated character: use `openssl rand -hex 32`".to_owned());
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A local run, which needs no passphrase, for the tests about other settings.
    fn args() -> ServeArgs {
        ServeArgs {
            bind: "127.0.0.1:8080".parse().unwrap(),
            data_dir: "/var/lib/nexus".into(),
            base_url: None,
            owner_id: None,
            owner_email: None,
            timezone: "Europe/Madrid".into(),
            calendar_token_file: None,
            insecure_local_only: true,
            env_label: None,
            log_level: Level::INFO,
            openai_api_key_file: None,
            realtime_model: "gpt-realtime-2.1-mini".into(),
            realtime_voice: "marin".into(),
            voice_session_seconds: 900,
            voice_test_base_url: None,
        }
    }

    fn owner_args(data_dir: &std::path::Path) -> ServeArgs {
        ServeArgs {
            data_dir: data_dir.to_path_buf(),
            base_url: Some("https://nexus.example.net/".into()),
            insecure_local_only: false,
            ..args()
        }
    }

    fn token_file(mode: u32, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    #[test]
    fn the_env_label_is_short_plain_text() {
        assert_eq!(env_label(" Staging ").unwrap(), "Staging");
        assert_eq!(env_label("").unwrap(), "");
        assert_eq!(env_label("<b>").unwrap(), "<b>");
        assert!(env_label(&"x".repeat(MAX_ENV_LABEL_CHARS)).is_ok());
        assert!(env_label(&"x".repeat(MAX_ENV_LABEL_CHARS + 1)).is_err());
        for bad in ["Stag\ning", "Stag\u{1b}[31m", "a\tb"] {
            assert!(env_label(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn local_runs_need_no_passphrase_or_base_url() {
        let config = args().validate().unwrap();
        assert!(config.calendar_token.is_none());
        assert!(matches!(config.access, Access::InsecureLocalOnly));
    }

    #[test]
    fn serving_for_real_needs_a_passphrase_and_says_how_to_set_one() {
        let dir = tempfile::tempdir().unwrap();
        let error = owner_args(dir.path()).validate().unwrap_err();
        assert!(error.contains("nexus set-passphrase"), "{error}");
        assert!(error.contains("--insecure-local-only"), "{error}");

        fs::write(
            dir.path().join("owner-passphrase.argon2"),
            "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA",
        )
        .unwrap();
        fs::set_permissions(
            dir.path().join("owner-passphrase.argon2"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let mut a = owner_args(dir.path());
        a.base_url = Some("http://localhost:3000/".into());
        let config = a.validate().unwrap();
        let Access::Owner(auth) = config.access else {
            panic!("expected owner access");
        };
        assert_eq!(auth.public_base_url, "http://localhost:3000");
    }

    #[test]
    fn serving_for_real_needs_a_base_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = owner_args(dir.path());
        a.base_url = None;
        assert!(a.validate().unwrap_err().contains("NEXUS_PUBLIC_BASE_URL"));
    }

    #[test]
    fn a_bad_base_url_is_refused_in_either_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut owner = owner_args(dir.path());
        owner.base_url = Some("http://nexus.example.net".into());
        assert!(owner.validate().unwrap_err().contains("base URL"));
        let mut local = args();
        local.base_url = Some("http://nexus.example.net".into());
        assert!(local.validate().unwrap_err().contains("base URL"));
    }

    #[test]
    fn insecure_mode_requires_loopback() {
        let mut a = args();
        a.insecure_local_only = true;
        a.bind = "0.0.0.0:8080".parse().unwrap();
        assert!(a.validate().unwrap_err().contains("loopback"));
        let mut a = args();
        a.insecure_local_only = true;
        a.bind = "[::1]:8080".parse().unwrap();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn token_file_is_read_and_trimmed() {
        let secret = "0123456789abcdef".repeat(3);
        let (_dir, path) = token_file(0o400, &format!("{secret}\n"));
        let mut a = args();
        a.calendar_token_file = Some(path);
        assert_eq!(a.validate().unwrap().calendar_token.unwrap(), secret);
    }

    #[test]
    fn bad_token_files_are_rejected_without_echoing_the_secret() {
        let weak = "short-secret-value";
        let loose = "b".repeat(40);
        for (mode, content, needle) in [
            (0o400, weak, "at least"),
            (
                0o400,
                "two words in one token file of enough length",
                "whitespace",
            ),
            (0o644, loose.as_str(), "other users"),
            (0o400, loose.as_str(), "repeated"),
        ] {
            let (_dir, path) = token_file(mode, content);
            let mut a = args();
            a.calendar_token_file = Some(path);
            let error = a.validate().unwrap_err();
            assert!(error.contains(needle), "{error}");
            assert!(!error.contains(content), "{error}");
        }
        let mut a = args();
        a.calendar_token_file = Some("/nonexistent/token".into());
        assert!(a.validate().unwrap_err().contains("cannot read"));
    }

    #[test]
    fn voice_is_off_without_a_key_file_and_the_default_target_is_openai() {
        assert!(args().validate().unwrap().voice.key.is_none());
        let secret = "sk-synthetic-0123456789abcdefghijklmnop";
        let (_dir, path) = token_file(0o400, secret);
        let mut a = args();
        a.openai_api_key_file = Some(path);
        let voice = a.validate().unwrap().voice;
        assert!(voice.key.is_some());
        assert_eq!(voice.base_url, OPENAI_BASE_URL);
        assert_eq!(voice.lifetime, Duration::from_secs(900));
        assert_eq!(voice.model, "gpt-realtime-2.1-mini");
        assert_eq!(voice.voice, "marin");
        assert!(
            !format!("{voice:?}").contains(secret),
            "Debug shows the key"
        );
    }

    #[test]
    fn a_loose_key_file_is_refused_without_echoing_it() {
        let secret = "sk-synthetic-0123456789abcdefghijklmnop";
        let (_dir, path) = token_file(0o644, secret);
        let mut a = args();
        a.openai_api_key_file = Some(path);
        let error = a.validate().unwrap_err();
        assert!(error.contains("other users"), "{error}");
        assert!(!error.contains(secret), "{error}");
    }

    #[test]
    fn only_a_local_run_may_point_voice_elsewhere_and_only_at_loopback() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("owner-passphrase.argon2"),
            "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$AAAAAAAAAAAAAAAAAAAAAA",
        )
        .unwrap();
        fs::set_permissions(
            dir.path().join("owner-passphrase.argon2"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let mut owner = owner_args(dir.path());
        owner.voice_test_base_url = Some("http://127.0.0.1:9".into());
        let error = owner.validate().unwrap_err();
        assert!(error.contains("--insecure-local-only"), "{error}");

        for bad in [
            "https://evil.example",
            "http://evil.example:80",
            "http://127.0.0.1",
        ] {
            let mut a = args();
            a.voice_test_base_url = Some(bad.into());
            assert!(a.validate().is_err(), "{bad}");
        }

        let (_key_dir, path) = token_file(0o400, "sk-synthetic-0123456789abcdefghijklmnop");
        let mut a = args();
        a.openai_api_key_file = Some(path);
        a.voice_test_base_url = Some("http://127.0.0.1:9/".into());
        assert_eq!(a.validate().unwrap().voice.base_url, "http://127.0.0.1:9");
    }

    #[test]
    fn a_voice_session_lasts_at_most_an_hour() {
        for bad in [0, 3601] {
            let mut a = args();
            a.voice_session_seconds = bad;
            assert!(a.validate().unwrap_err().contains("voice-session-seconds"));
        }
    }

    #[test]
    fn only_a_systemd_credential_may_be_readable_by_its_group() {
        assert!(secret_mode_ok(0o600, false));
        assert!(secret_mode_ok(0o400, false));
        assert!(!secret_mode_ok(0o440, false));
        assert!(secret_mode_ok(0o440, true));
        assert!(!secret_mode_ok(0o460, true));
        assert!(!secret_mode_ok(0o444, true));
    }

    #[test]
    fn timezone_must_be_an_iana_name() {
        assert_eq!(args().validate().unwrap().timezone, Tz::Europe__Madrid);
        let mut a = args();
        a.timezone = "Mars/Olympus".into();
        assert!(a.validate().unwrap_err().contains("--timezone"));
    }

    #[test]
    fn all_problems_are_reported_together() {
        let mut a = args();
        a.base_url = Some("http://example.net".into());
        a.calendar_token_file = Some("/nonexistent/token".into());
        let error = a.validate().unwrap_err();
        assert_eq!(error.lines().count(), 2, "{error}");
    }
}
