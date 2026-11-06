//! Startup configuration for `nexus serve`: flags and environment in, a validated config out.
//! Secrets are only ever read from files, so none appears in the process list.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::Args;
use nexus_auth::{AuthConfig, ConfigError, normalize_base_url};
use nexus_web::Tz;
use tracing::Level;

const MIN_TOKEN_LEN: usize = 32;

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
    /// Most verbose level logged: error, warn, info, debug or trace.
    #[arg(long, env = "NEXUS_LOG_LEVEL", default_value = "info")]
    pub log_level: Level,
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
        })
    }
}

fn auth_problem(error: &ConfigError) -> String {
    match error {
        ConfigError::MissingPassphraseHash(_) => {
            format!("{error}. For a local run without login, pass --insecure-local-only")
        }
        _ => error.to_string(),
    }
}

fn read_token(path: &std::path::Path) -> Result<String, String> {
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read: {}", e.kind()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
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
    if token.chars().all(|c| Some(c) == token.chars().next()) {
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
            log_level: Level::INFO,
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
